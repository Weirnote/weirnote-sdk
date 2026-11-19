import CryptoKit
import Foundation
import Security

/// Fixed, non-sensitive errors from the browser authorization flow.
/// None of these cases include a URL, authorization code, verifier, or token.
public enum BrowserLoginError: Error, Equatable, LocalizedError, Sendable {
    case invalidConfiguration
    case randomnessFailure
    case invalidVerifier
    case invalidCallback
    case callbackMismatch
    case stateMismatch
    case authorizationDenied
    case serviceUnavailable
    case authorizationFailed
    case expired
    case cancelled
    case exchangeAlreadyStarted
    case exchangeFailed
    case invalidTokenResponse

    public var errorDescription: String? {
        switch self {
        case .invalidConfiguration: return "The browser login configuration is invalid."
        case .randomnessFailure: return "Could not start a secure browser login."
        case .invalidVerifier: return "The PKCE verifier is invalid."
        case .invalidCallback: return "The browser returned an invalid callback."
        case .callbackMismatch: return "The browser returned to an unexpected callback."
        case .stateMismatch: return "The browser login state did not match."
        case .authorizationDenied: return "Browser login was cancelled or denied."
        case .serviceUnavailable: return "The sign-in service is temporarily unavailable. Please try again."
        case .authorizationFailed: return "Browser authorization failed. Please try again."
        case .expired: return "The browser login has expired."
        case .cancelled: return "The browser login was cancelled."
        case .exchangeAlreadyStarted: return "This browser login is no longer active."
        case .exchangeFailed: return "The browser token exchange failed."
        case .invalidTokenResponse: return "The browser token response is invalid."
        }
    }
}

public struct BrowserLoginConfiguration: Sendable {
    public enum Flow: Sendable { case generic, appleWeb }
    public let idpURL: URL
    public let clientID: String
    public let redirectURI: URL
    public let tokenOrigin: URL
    public let flow: Flow

    public var callbackURLScheme: String { redirectURI.scheme! }

    public init(idpURL: URL, clientID: String, redirectURI: URL,
                tokenOrigin: URL? = nil, flow: Flow = .generic) throws {
        guard Self.isHTTPOrigin(idpURL),
              !clientID.isEmpty,
              let scheme = redirectURI.scheme,
              scheme == clientID.lowercased() + ".login",
              redirectURI.host == nil,
              redirectURI.user == nil,
              redirectURI.password == nil,
              redirectURI.port == nil,
              redirectURI.path == "/oauth/callback",
              redirectURI.query == nil,
              redirectURI.fragment == nil
        else { throw BrowserLoginError.invalidConfiguration }
        self.idpURL = idpURL
        self.clientID = clientID
        self.redirectURI = redirectURI
        guard Self.isHTTPOrigin(tokenOrigin ?? idpURL) else { throw BrowserLoginError.invalidConfiguration }
        self.tokenOrigin = tokenOrigin ?? idpURL
        self.flow = flow
    }

    private static func isHTTPOrigin(_ url: URL) -> Bool {
        ["http", "https"].contains(url.scheme)
            && url.host != nil
            && url.user == nil
            && url.password == nil
            && (url.path.isEmpty || url.path == "/")
            && url.query == nil
            && url.fragment == nil
    }
}

public struct BrowserAuthorizationStart: Sendable {
    public let authorizationURL: URL
    public let transaction: BrowserAuthorizationTransaction
}

public struct BrowserTokenResponse: Sendable, Equatable {
    public let accessToken: String
    public let expiresIn: Int
}

public struct BrowserAppleCredentialResponse: Decodable, Sendable, Equatable {
    public let identityToken: String
    public let authorizationCode: String
    enum CodingKeys: String, CodingKey {
        case identityToken = "identity_token"
        case authorizationCode = "authorization_code"
    }
}

/// Makes independent browser authorization transactions. Supply a URLSession
/// configured with URLProtocol mocks when testing the token exchange.
public actor BrowserLoginClient {
    private let configuration: BrowserLoginConfiguration
    private let session: URLSession
    private let now: @Sendable () -> Date

    public init(configuration: BrowserLoginConfiguration, session: URLSession = .shared,
                now: @escaping @Sendable () -> Date = { Date() }) {
        self.configuration = configuration
        self.session = session
        self.now = now
    }

    public func beginAuthorization(nonce: String? = nil) throws -> BrowserAuthorizationStart {
        let verifier = try Self.secureRandom(count: 32).base64URL
        let state = try Self.secureRandom(count: 32).base64URL
        let challenge = try PKCE.challenge(for: verifier)
        let expiresAt = now().addingTimeInterval(300)

        let path = configuration.flow == .appleWeb ? "v1/apple/login/start" : "authorize"
        var components = URLComponents(url: configuration.idpURL.appendingPathComponent(path), resolvingAgainstBaseURL: false)!
        components.queryItems = [
            URLQueryItem(name: "response_type", value: "code"),
            URLQueryItem(name: "client_id", value: configuration.clientID),
            URLQueryItem(name: "redirect_uri", value: configuration.redirectURI.absoluteString),
            URLQueryItem(name: "state", value: state),
            URLQueryItem(name: "code_challenge", value: challenge),
            URLQueryItem(name: "code_challenge_method", value: "S256"),
        ]
        if configuration.flow == .appleWeb {
            guard let nonce, nonce.utf8.count == 64,
                  nonce.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }) else {
                throw BrowserLoginError.invalidConfiguration
            }
            components.queryItems?.append(URLQueryItem(name: "nonce", value: nonce))
        }
        guard let authorizationURL = components.url else { throw BrowserLoginError.invalidConfiguration }
        let transaction = BrowserAuthorizationTransaction(
            configuration: configuration, session: session, verifier: verifier, state: state,
            expiresAt: expiresAt, now: now
        )
        return BrowserAuthorizationStart(authorizationURL: authorizationURL, transaction: transaction)
    }

    private static func secureRandom(count: Int) throws -> Data {
        var bytes = Data(repeating: 0, count: count)
        let status = bytes.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, count, $0.baseAddress!) }
        guard status == errSecSuccess else { throw BrowserLoginError.randomnessFailure }
        return bytes
    }
}

/// A single-use PKCE transaction. Its secret material stays private and is
/// erased after any terminal result, including cancellation and network errors.
public actor BrowserAuthorizationTransaction {
    private struct Material: Sendable {
        let verifier: String
        let state: String
        let expiresAt: Date
    }

    private enum Phase: Sendable {
        case pending(Material)
        case exchanging(Material)
        case terminal
    }

    private struct Exchange: Sendable {
        let material: Material
        let code: String
    }

    private let configuration: BrowserLoginConfiguration
    private let session: URLSession
    private let now: @Sendable () -> Date
    private var phase: Phase

    fileprivate init(configuration: BrowserLoginConfiguration, session: URLSession, verifier: String,
                     state: String, expiresAt: Date, now: @escaping @Sendable () -> Date) {
        self.configuration = configuration
        self.session = session
        self.now = now
        self.phase = .pending(Material(verifier: verifier, state: state, expiresAt: expiresAt))
    }

    /// Cancels locally even if the browser or a URLSession task ignores cancellation.
    public func cancel() {
        phase = .terminal
    }

    /// Validates one exact callback and exchanges its code one time.
    public func exchange(callbackURL: URL) async throws -> BrowserTokenResponse {
        let data = try await exchangeData(callbackURL: callbackURL)
        let decoded: TokenPayload
        do { decoded = try JSONDecoder().decode(TokenPayload.self, from: data) }
        catch { throw BrowserLoginError.invalidTokenResponse }
        guard !decoded.accessToken.isEmpty, decoded.tokenType == "Bearer", decoded.expiresIn > 0 else {
            throw BrowserLoginError.invalidTokenResponse
        }
        return BrowserTokenResponse(accessToken: decoded.accessToken, expiresIn: decoded.expiresIn)
    }

    public func exchangeAppleCredential(callbackURL: URL) async throws -> BrowserAppleCredentialResponse {
        let data = try await exchangeData(callbackURL: callbackURL)
        let result: BrowserAppleCredentialResponse
        do { result = try JSONDecoder().decode(BrowserAppleCredentialResponse.self, from: data) }
        catch { throw BrowserLoginError.invalidTokenResponse }
        guard !result.identityToken.isEmpty, result.identityToken.utf8.count <= 20000,
              !result.authorizationCode.isEmpty, result.authorizationCode.utf8.count <= 4096 else {
            throw BrowserLoginError.invalidTokenResponse
        }
        return result
    }

    private func exchangeData(callbackURL: URL) async throws -> Data {
        let exchange: Exchange
        do {
            exchange = try beginExchange(callbackURL: callbackURL)
        } catch {
            phase = .terminal
            throw error
        }
        do {
            let token = try await exchangeCode(exchange)
            guard case .exchanging(let current) = phase, current.state == exchange.material.state else {
                throw BrowserLoginError.cancelled
            }
            guard now() <= exchange.material.expiresAt else {
                throw BrowserLoginError.expired
            }
            phase = .terminal
            return token
        } catch let error as BrowserLoginError {
            if case .terminal = phase { throw BrowserLoginError.cancelled }
            phase = .terminal
            throw error
        } catch {
            if case .terminal = phase { throw BrowserLoginError.cancelled }
            phase = .terminal
            throw BrowserLoginError.exchangeFailed
        }
    }

    private func beginExchange(callbackURL: URL) throws -> Exchange {
        guard case .pending(let material) = phase else { throw BrowserLoginError.exchangeAlreadyStarted }
        guard now() <= material.expiresAt else {
            phase = .terminal
            throw BrowserLoginError.expired
        }
        let code = try validate(callbackURL: callbackURL, expectedState: material.state)
        // Validate before this assignment, but set it before an await so concurrent
        // callbacks cannot both send a token request.
        phase = .exchanging(material)
        return Exchange(material: material, code: code)
    }

    private func validate(callbackURL: URL, expectedState: String) throws -> String {
        guard let expected = URLComponents(url: configuration.redirectURI, resolvingAgainstBaseURL: false),
              let callback = URLComponents(url: callbackURL, resolvingAgainstBaseURL: false),
              callback.scheme == expected.scheme,
              callback.host == nil, callback.user == nil, callback.password == nil, callback.port == nil,
              callback.path == expected.path,
              callback.fragment == nil,
              callbackBaseString(callback) == configuration.redirectURI.absoluteString
        else { throw BrowserLoginError.callbackMismatch }

        let fields = try Self.parseCallbackFields(callback.percentEncodedQuery)
        guard fields["state"] == expectedState else { throw BrowserLoginError.stateMismatch }
        if let error = fields["error"] {
            guard !error.isEmpty else { throw BrowserLoginError.invalidCallback }
            switch error {
            case "access_denied": throw BrowserLoginError.authorizationDenied
            case "server_error", "temporarily_unavailable": throw BrowserLoginError.serviceUnavailable
            case "invalid_request": throw BrowserLoginError.authorizationFailed
            default: throw BrowserLoginError.invalidCallback
            }
        }
        guard let code = fields["code"], PKCE.isUnreserved(code), code.count == 43 else {
            throw BrowserLoginError.invalidCallback
        }
        return code
    }

    private static func parseCallbackFields(_ query: String?) throws -> [String: String] {
        guard let query, !query.isEmpty else { throw BrowserLoginError.invalidCallback }
        var fields: [String: String] = [:]
        for pair in query.split(separator: "&", omittingEmptySubsequences: false) {
            let pieces = pair.split(separator: "=", maxSplits: 1, omittingEmptySubsequences: false)
            guard pieces.count == 2,
                  let name = strictPercentDecode(String(pieces[0])),
                  let value = strictPercentDecode(String(pieces[1])),
                  !name.isEmpty, !value.isEmpty,
                  fields[name] == nil
            else { throw BrowserLoginError.invalidCallback }
            fields[name] = value
        }
        let names = Set(fields.keys)
        guard names == Set(["code", "state"]) || names == Set(["error", "state"]) else {
            throw BrowserLoginError.invalidCallback
        }
        return fields
    }

    private func callbackBaseString(_ components: URLComponents) -> String? {
        var base = components
        base.percentEncodedQuery = nil
        return base.url?.absoluteString
    }

    private static func strictPercentDecode(_ raw: String) -> String? {
        let bytes = Array(raw.utf8)
        var index = 0
        while index < bytes.count {
            if bytes[index] == 37 { // "%"
                guard index + 2 < bytes.count,
                      Self.hex(bytes[index + 1]) != nil, Self.hex(bytes[index + 2]) != nil else { return nil }
                index += 3
            } else {
                index += 1
            }
        }
        return raw.removingPercentEncoding
    }

    private static func hex(_ byte: UInt8) -> UInt8? {
        switch byte {
        case 48...57: return byte - 48
        case 65...70: return byte - 55
        case 97...102: return byte - 87
        default: return nil
        }
    }

    private func exchangeCode(_ exchange: Exchange) async throws -> Data {
        let path = configuration.flow == .appleWeb ? "v1/apple/login/token" : "oauth/token"
        var request = URLRequest(url: configuration.tokenOrigin.appendingPathComponent(path))
        request.httpMethod = "POST"
        request.timeoutInterval = 15
        request.setValue("application/x-www-form-urlencoded", forHTTPHeaderField: "Content-Type")
        request.setValue("no-store", forHTTPHeaderField: "Cache-Control")
        request.httpBody = Self.formBody([
            ("grant_type", "authorization_code"),
            ("client_id", configuration.clientID),
            ("redirect_uri", configuration.redirectURI.absoluteString),
            ("code", exchange.code),
            ("code_verifier", exchange.material.verifier),
        ])
        let (data, response) = try await session.data(for: request, delegate: NoRedirectDelegate())
        guard let response = response as? HTTPURLResponse, (200..<300).contains(response.statusCode) else {
            throw BrowserLoginError.exchangeFailed
        }
        guard data.count <= 30000 else { throw BrowserLoginError.invalidTokenResponse }
        return data
    }

    private static func formBody(_ fields: [(String, String)]) -> Data {
        var components = URLComponents()
        components.queryItems = fields.map { URLQueryItem(name: $0.0, value: $0.1) }
        return Data((components.percentEncodedQuery ?? "").utf8)
    }

    private struct TokenPayload: Decodable {
        let accessToken: String
        let tokenType: String
        let expiresIn: Int
        enum CodingKeys: String, CodingKey {
            case accessToken = "access_token"
            case tokenType = "token_type"
            case expiresIn = "expires_in"
        }
    }
}

private final class NoRedirectDelegate: NSObject, URLSessionTaskDelegate {
    func urlSession(_ session: URLSession, task: URLSessionTask,
                    willPerformHTTPRedirection response: HTTPURLResponse,
                    newRequest request: URLRequest,
                    completionHandler: @escaping @Sendable (URLRequest?) -> Void) {
        completionHandler(nil)
    }
}

public enum PKCE {
    /// RFC 7636 S256: base64url(SHA256(ASCII(verifier))) without padding.
    public static func challenge(for verifier: String) throws -> String {
        guard verifier.count >= 43, verifier.count <= 128, isUnreserved(verifier) else {
            throw BrowserLoginError.invalidVerifier
        }
        return Data(SHA256.hash(data: Data(verifier.utf8))).base64URL
    }

    fileprivate static func isUnreserved(_ value: String) -> Bool {
        guard value.utf8.count == value.count else { return false }
        return value.utf8.allSatisfy { byte in
            (48...57).contains(byte) || (65...90).contains(byte) || (97...122).contains(byte)
                || byte == 45 || byte == 46 || byte == 95 || byte == 126
        }
    }
}
