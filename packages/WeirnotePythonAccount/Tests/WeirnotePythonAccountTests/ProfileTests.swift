import CryptoKit
import DeviceEnrollment
import Foundation
import Security
import Testing
@testable import WeirnotePythonAccount

private final class Store: ProfileStore, @unchecked Sendable {
    private let lock = NSLock()
    private var values: [String: Data] = [:]
    private let locked: Bool

    init(locked: Bool = false) { self.locked = locked }

    func read(_ account: String) throws -> Data? {
        if locked { throw EnrollmentError.keychain(errSecInteractionNotAllowed) }
        return lock.withLock { values[account] }
    }

    func insert(_ data: Data, account: String) throws -> Bool {
        lock.withLock {
            guard values[account] == nil else { return false }
            values[account] = data
            return true
        }
    }

    func delete(_ account: String) throws {
        lock.withLock { _ = values.removeValue(forKey: account) }
    }
}

private final class EnrollmentFixture: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() {}

    override func startLoading() {
        do {
            let url = try #require(request.url)
            let origin = try #require(URL(string: request.value(forHTTPHeaderField: "X-Fixture-Origin") ?? ""))
            #expect(url.scheme == origin.scheme && url.host == origin.host && url.port == origin.port)
            let proofOrigin = try #require(request.value(forHTTPHeaderField: "X-Fixture-Proof-Origin"))
            let response: [String: Any]
            var status = 200
            switch url.path {
            case "/enrollment/challenge", "/session/challenge":
                let path = url.path == "/enrollment/challenge" ? "/enroll" : "/session/renew"
                response = ["nonce": "fixture-challenge", "audience": proofOrigin + path,
                            "issued_at": 100, "expires_at": 220]
            case "/v1/apple/login/token":
                response = ["identity_token": "fixture-apple-identity", "authorization_code": "fixture-auth-code"]
            case "/enroll", "/session/renew":
                switch request.value(forHTTPHeaderField: "X-Fixture-Mode") {
                case "revoked":
                    status = 401
                    response = ["detail": "device_unknown"]
                case "offline":
                    throw URLError(.notConnectedToInternet)
                default:
                    let body = try JSONDecoder().decode([String: String].self, from: try bodyData())
                    let proof = try #require(body["proof"]).split(separator: ".")
                    try #require(proof.count == 3)
                    let header = try proofFields(proof[0])
                    let claims = try proofFields(proof[1])
                    #expect(claims["aud"] as? String == proofOrigin + url.path)
                    let jwk = try #require(header["jwk"] as? [String: String])
                    let canonical = try JSONSerialization.data(withJSONObject: jwk, options: .sortedKeys)
                    #expect(header["typ"] as? String == (url.path == "/enroll" ? "device-enrollment+jwt" : "device-renewal+jwt"))
                    if url.path == "/enroll" {
                        #expect(body["authorization_code"] == "fixture-auth-code")
                    }
                    response = ["access_token": url.path == "/enroll" ? "enrolled" : "renewed",
                                "expires_in": 100, "user_id": "fixture-user",
                                "device_id": Data(SHA256.hash(data: canonical)).base64URL]
                }
            default:
                throw URLError(.unsupportedURL)
            }
            let http = try #require(HTTPURLResponse(url: url, statusCode: status, httpVersion: nil, headerFields: nil))
            client?.urlProtocol(self, didReceive: http, cacheStoragePolicy: .notAllowed)
            client?.urlProtocol(self, didLoad: try JSONSerialization.data(withJSONObject: response))
            client?.urlProtocolDidFinishLoading(self)
        } catch {
            client?.urlProtocol(self, didFailWithError: error)
        }
    }

    private func proofFields(_ segment: Substring) throws -> [String: Any] {
        let encoded = segment.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        let padded = encoded + String(repeating: "=", count: (4 - encoded.count % 4) % 4)
        let bytes = try #require(Data(base64Encoded: padded))
        let decoded = try JSONSerialization.jsonObject(with: bytes)
        return try #require(decoded as? [String: Any])
    }

    private func bodyData() throws -> Data {
        if let bytes = request.httpBody { return bytes }
        let stream = try #require(request.httpBodyStream)
        stream.open()
        defer { stream.close() }
        var result = Data()
        var buffer = [UInt8](repeating: 0, count: 1024)
        while stream.hasBytesAvailable {
            let count = stream.read(&buffer, maxLength: buffer.count)
            guard count > 0 else { break }
            result.append(contentsOf: buffer.prefix(count))
        }
        return result
    }
}

private func profile(_ store: Store, origin: String = "https://fixture.example",
                     name: String = "default", mode: String = "", audience: String? = nil,
                     proofOrigin: String = "https://fixture.example") throws -> Profile {
    let configuration = try ProfileConfiguration(origin: origin, profile: name,
                                                   bundleID: ProfileConfiguration.bundleID, allowsInsecureHTTP: false)
    let session = URLSessionConfiguration.ephemeral
    session.protocolClasses = [EnrollmentFixture.self]
    session.httpAdditionalHeaders = ["X-Fixture-Mode": mode, "X-Fixture-Proof-Origin": proofOrigin,
                                     "X-Fixture-Origin": configuration.origin.absoluteString]
    return try Profile(configuration: configuration, store: store, transport: URLSession(configuration: session),
                       enrollmentAudience: audience)
}

@MainActor
private func callback(_ start: URL, _ redirect: URL) throws -> URL {
    let query = try #require(URLComponents(url: start, resolvingAgainstBaseURL: false)?.queryItems)
    let state = try #require(query.first { $0.name == "state" }?.value)
    #expect(start.path == "/v1/apple/login/start")
    #expect(query.first { $0.name == "code_challenge_method" }?.value == "S256")
    #expect(query.first { $0.name == "nonce" }?.value?.count == 64)
    #expect(query.first { $0.name == "redirect_uri" }?.value == redirect.absoluteString)
    let code = String(repeating: "A", count: 43)
    return try #require(URL(string: "\(redirect.absoluteString)?code=\(code)&state=\(state)"))
}

@Test @MainActor func profileEnrollmentRenewalIsolationAndSignOut() async throws {
    let store = Store()
    let first = try profile(store)
    let enrolled = try await first.signIn(present: callback)
    #expect(enrolled.accessToken == "enrolled")

    let reopened = try profile(store, origin: "https://FIXTURE.example:443/")
    let renewed = try await reopened.renew()
    #expect(renewed.accessToken == "renewed")
    #expect(renewed.deviceID == enrolled.deviceID && renewed.userID == enrolled.userID)

    let other = try profile(store, name: "other")
    await #expect(throws: ProfileFailure.notSignedIn) { try await other.renew() }
    let otherDevice = try await other.signIn(present: callback)
    #expect(otherDevice.deviceID != enrolled.deviceID)

    let otherOrigin = try profile(store, origin: "https://other.example")
    await #expect(throws: ProfileFailure.notSignedIn) { try await otherOrigin.renew() }
    try reopened.signOut()
    await #expect(throws: ProfileFailure.notSignedIn) { try await first.renew() }
    #expect(try await other.renew().deviceID == otherDevice.deviceID)
}

@Test @MainActor func cancellationAndWrongStateLeaveProfileUnsigned() async throws {
    let account = try profile(Store())
    await #expect(throws: ProfileFailure.cancelled) {
        try await account.signIn { _, _ in throw ProfileFailure.cancelled }
    }
    await #expect(throws: BrowserLoginError.stateMismatch) {
        try await account.signIn { _, redirect in
            try #require(URL(string: "\(redirect.absoluteString)?code=fixture&state=wrong"))
        }
    }
    await #expect(throws: ProfileFailure.notSignedIn) { try await account.renew() }
}

@Test @MainActor func movedTransportRequiresExplicitProofAudience() async throws {
    let store = Store()
    let moved = "https://moved.example"
    let unconfigured = try profile(store, origin: moved)
    await #expect(throws: EnrollmentError.self) {
        try await unconfigured.signIn(present: callback)
    }
    let configured = try profile(store, origin: moved, audience: "https://fixture.example/enroll")
    let enrolled = try await configured.signIn(present: callback)
    await #expect(throws: EnrollmentError.self) { try await unconfigured.renew() }
    #expect(try await configured.renew().deviceID == enrolled.deviceID)
}

@Test @MainActor func lockedRevokedAndOutageRemainDistinct() async throws {
    let locked = try profile(Store(locked: true))
    let store = Store()
    _ = try await profile(store).signIn(present: callback)
    for (account, failure) in [(locked, ProfileFailure.keychainUnavailable),
                               (try profile(store, mode: "revoked"), .deviceRevoked),
                               (try profile(store, mode: "offline"), .unavailable)] {
        do {
            _ = try await account.renew()
            Issue.record("Renewal unexpectedly succeeded")
        } catch {
            #expect(ProfileFailure.classify(error) == failure)
        }
    }
    #expect(try await profile(store).renew().accessToken == "renewed")
}
