import CryptoKit
import Foundation

public struct EnrollmentChallenge: Codable, Sendable {
    public let nonce: String
    public let audience: String
    public let issuedAt: Int
    public let expiresAt: Int
    enum CodingKeys: String, CodingKey {
        case nonce, audience
        case issuedAt = "issued_at", expiresAt = "expires_at"
    }
    public init(nonce: String, audience: String, issuedAt: Int, expiresAt: Int) {
        self.nonce = nonce; self.audience = audience; self.issuedAt = issuedAt; self.expiresAt = expiresAt
    }
}

public struct APISession: Codable, Sendable {
    public let accessToken: String
    public let expiresIn: Int
    public let userID: String
    public let deviceID: String
    enum CodingKeys: String, CodingKey {
        case accessToken = "access_token", expiresIn = "expires_in"
        case userID = "user_id", deviceID = "device_id"
    }
}

public struct UserCounter: Codable, Sendable {
    public let userID: String
    public let value: Int
    enum CodingKeys: String, CodingKey { case userID = "user_id", value }
}

public struct AppleEnrollmentPreparation: Sendable {
    public let challenge: EnrollmentChallenge
    public let requestNonce: String

    public init(challenge: EnrollmentChallenge, requestNonce: String) {
        self.challenge = challenge
        self.requestNonce = requestNonce
    }
}

private struct IdPToken: Decodable { let access_token: String }

private struct ServerErrorPayload: Decodable {
    let detail: ServerErrorDetail?
}

private enum ServerErrorDetail: Decodable {
    case message(String)
    case coded(code: String?, message: String?)

    private enum CodingKeys: String, CodingKey { case code, message }

    init(from decoder: Decoder) throws {
        if let container = try? decoder.singleValueContainer(),
           let message = try? container.decode(String.self) {
            self = .message(message)
            return
        }

        let container = try decoder.container(keyedBy: CodingKeys.self)
        self = .coded(code: try container.decodeIfPresent(String.self, forKey: .code),
                      message: try container.decodeIfPresent(String.self, forKey: .message))
    }

    var message: String? {
        switch self {
        case .message(let message): message
        case .coded(_, let message): message
        }
    }
}

private final class RejectRedirects: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    static let shared = RejectRedirects()

    func urlSession(_ session: URLSession, task: URLSessionTask,
                    willPerformHTTPRedirection response: HTTPURLResponse,
                    newRequest request: URLRequest,
                    completionHandler: @escaping (URLRequest?) -> Void) {
        completionHandler(nil)
    }
}

/// Network operations are independent of the CLI and Keychain for app reuse.
public struct EnrollmentClient: Sendable {
    public let idpURL: URL
    public let apiURL: URL
    private let transport: URLSession
    private let enrollmentAudience: String

    public init(idpURL: URL, apiURL: URL, transport: URLSession = .shared,
                enrollmentAudience: String? = nil,
                allowsInsecureHTTPForDevelopment: Bool = false) throws {
        for url in [idpURL, apiURL] {
            try EnrollmentEndpointPolicy.validateOrigin(url, allowsInsecureHTTPForDevelopment: allowsInsecureHTTPForDevelopment)
        }
        self.idpURL = idpURL; self.apiURL = apiURL; self.transport = transport
        let audience = enrollmentAudience ?? apiURL.appendingPathComponent("enroll").absoluteString
        guard let audienceURL = URL(string: audience), ["http", "https"].contains(audienceURL.scheme),
              audienceURL.host != nil, audienceURL.path == "/enroll", audienceURL.query == nil,
              audienceURL.fragment == nil, audienceURL.user == nil, audienceURL.password == nil
        else { throw EnrollmentError.invalidURL }
        self.enrollmentAudience = audience
    }

    private func request<T: Decodable>(_ base: URL, _ path: String, method: String = "GET",
                                       body: [String: String]? = nil, token: String? = nil) async throws -> T {
        var request = URLRequest(url: base.appendingPathComponent(path))
        request.httpMethod = method
        request.timeoutInterval = 15
        if let body {
            request.httpBody = try JSONEncoder().encode(body)
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        if let token { request.setValue("Bearer " + token, forHTTPHeaderField: "Authorization") }
        let (data, response) = try await transport.data(for: request, delegate: RejectRedirects.shared)
        guard let response = response as? HTTPURLResponse else { throw EnrollmentError.invalidResponse }
        guard (200..<300).contains(response.statusCode) else {
            let payload = try? JSONDecoder().decode(ServerErrorPayload.self, from: data)
            throw EnrollmentError.server(Self.mapServerError(statusCode: response.statusCode,
                                                               detail: payload?.detail))
        }
        do { return try JSONDecoder().decode(T.self, from: data) }
        catch { throw EnrollmentError.invalidResponse }
    }

    /// Fetches a dummy IdP token, then exchanges a device-signed proof for an API session.
    public func login(username: String, identity: DeviceIdentity) async throws -> APISession {
        let token: IdPToken = try await request(idpURL, "token", method: "POST", body: ["username": username])
        return try await enroll(idpToken: token.access_token, identity: identity)
    }

    /// An actual app can supply its real IdP token here instead of calling the dummy login.
    public func enroll(idpToken: String, identity: DeviceIdentity) async throws -> APISession {
        let challenge = try await verifiedEnrollmentChallenge()
        return try await completeEnrollment(challenge: challenge, idpToken: idpToken,
                                            authorizationCode: nil, identity: identity)
    }

    /// Fetches the challenge before Apple authorization and derives the nonce
    /// Apple must sign into its identity token.
    public func appleChallenge() async throws -> AppleEnrollmentPreparation {
        let challenge = try await verifiedEnrollmentChallenge()
        let digest = SHA256.hash(data: Data(challenge.nonce.utf8))
        let requestNonce = digest.map { String(format: "%02x", $0) }.joined()
        return AppleEnrollmentPreparation(challenge: challenge, requestNonce: requestNonce)
    }

    /// Completes challenge-first Apple enrollment. The device proof retains the
    /// raw server nonce; only the Apple request uses the derived hash.
    public func completeAppleEnrollment(challenge: EnrollmentChallenge, identityToken: String,
                                        authorizationCode: String, identity: DeviceIdentity) async throws -> APISession {
        guard !identityToken.isEmpty, !authorizationCode.isEmpty else { throw EnrollmentError.invalidResponse }
        return try await completeEnrollment(challenge: challenge, idpToken: identityToken,
                                            authorizationCode: authorizationCode, identity: identity)
    }

    /// Existing bindings are required atomically at enrollment commit. The
    /// distinct signed proof type fails closed against servers predating reuse.
    public func completeAppleEnrollment(challenge: EnrollmentChallenge, identityToken: String,
                                        authorizationCode: String, identity: DeviceIdentity,
                                        reuse: EnrollmentReuseIntent?) async throws -> APISession {
        guard !identityToken.isEmpty, !authorizationCode.isEmpty else { throw EnrollmentError.invalidResponse }
        return try await completeEnrollment(challenge: challenge, idpToken: identityToken,
                                            authorizationCode: authorizationCode, identity: identity, reuse: reuse)
    }

    private func verifiedEnrollmentChallenge() async throws -> EnrollmentChallenge {
        let challenge: EnrollmentChallenge = try await request(apiURL, "enrollment/challenge", method: "POST")
        guard challenge.audience == enrollmentAudience,
              Self.hasValidLifetime(challenge, maximum: 900) else { throw EnrollmentError.invalidResponse }
        return challenge
    }

    private func completeEnrollment(challenge: EnrollmentChallenge, idpToken: String,
                                    authorizationCode: String?, identity: DeviceIdentity,
                                    reuse: EnrollmentReuseIntent? = nil) async throws -> APISession {
        guard challenge.audience == enrollmentAudience,
              Self.hasValidLifetime(challenge, maximum: 900) else { throw EnrollmentError.invalidResponse }
        let proof = try identity.enrollmentProof(idpToken: idpToken, challenge: challenge, reuse: reuse)
        var body = ["proof": proof]
        if let authorizationCode { body["authorization_code"] = authorizationCode }
        let session: APISession = try await request(apiURL, "enroll", method: "POST", body: body)
        guard session.deviceID == identity.deviceID else { throw EnrollmentError.invalidResponse }
        return session
    }

    /// Prove possession of an enrolled device key without requesting an IdP token.
    public func renew(identity: DeviceIdentity) async throws -> APISession {
        let challenge: EnrollmentChallenge = try await request(apiURL, "session/challenge", method: "POST")
        let renewalAudience = URL(string: enrollmentAudience)!.deletingLastPathComponent()
            .appendingPathComponent("session/renew").absoluteString
        guard challenge.audience == renewalAudience,
              Self.hasValidLifetime(challenge, maximum: 120) else { throw EnrollmentError.invalidResponse }
        let proof = try identity.renewalProof(challenge: challenge)
        let session: APISession = try await request(apiURL, "session/renew", method: "POST", body: ["proof": proof])
        guard session.deviceID == identity.deviceID else { throw EnrollmentError.invalidResponse }
        return session
    }

    public func counter(token: String) async throws -> UserCounter {
        try await request(apiURL, "counter", token: token)
    }

    /// Deliberately does not retry: another POST could increment twice after a lost response.
    public func increment(token: String) async throws -> UserCounter {
        try await request(apiURL, "counter/increment", method: "POST", token: token)
    }

    private static func hasValidLifetime(_ challenge: EnrollmentChallenge, maximum: Int) -> Bool {
        let (lifetime, overflow) = challenge.expiresAt.subtractingReportingOverflow(challenge.issuedAt)
        return !overflow && lifetime > 0 && lifetime <= maximum
    }

    private static func mapServerError(statusCode: Int, detail: ServerErrorDetail?) -> EnrollmentServerError {
        if statusCode == 401, case .coded("apple_authorization_rejected", _) = detail {
            return .appleAuthorizationRejected
        }
        if statusCode == 409 {
            switch detail {
            case .coded(let code?, _):
                switch code {
                case "device_bound_to_other_user": return .deviceBoundToOtherUser
                case "device_reuse_rejected": return .deviceReuseRejected
                case "account_deletion_in_progress": return .deletionInProgress
                default: return .httpStatus(statusCode)
                }
            case .message(let message):
                // Compatibility for older servers that returned only a detail string.
                switch message.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
                case "device already enrolled to another user; choose a different device profile":
                    return .deviceBoundToOtherUser
                case "account deletion is still in progress; retry enrollment later":
                    return .deletionInProgress
                default: return .httpStatus(statusCode)
                }
            case .coded, nil:
                return .httpStatus(statusCode)
            }
        }

        return EnrollmentServerError.map(statusCode: statusCode, detail: detail?.message ?? "")
    }
}
