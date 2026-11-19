import Foundation
import Testing
@testable import DeviceEnrollment

private struct BrowserFixture: Decodable {
    let verifier: String
    let challenge: String
    let state: String
    let code: String
    let clientID: String
    let redirectURI: String
    let tokenResponse: TokenResponse

    struct TokenResponse: Codable {
        let accessToken: String
        let tokenType: String
        let expiresIn: Int

        enum CodingKeys: String, CodingKey {
            case accessToken = "access_token"
            case tokenType = "token_type"
            case expiresIn = "expires_in"
        }
    }

    enum CodingKeys: String, CodingKey {
        case verifier, challenge, state, code
        case clientID = "client_id"
        case redirectURI = "redirect_uri"
        case tokenResponse = "token_response"
    }
}

private func browserFixture() throws -> BrowserFixture {
    var root = URL(fileURLWithPath: #filePath)
    for _ in 0..<5 { root.deleteLastPathComponent() }
    return try JSONDecoder().decode(BrowserFixture.self,
                                    from: Data(contentsOf: root.appendingPathComponent("docs/fixtures/browser-login.json")))
}

private func browserConfiguration() throws -> BrowserLoginConfiguration {
    let fixture = try browserFixture()
    return try BrowserLoginConfiguration(
        idpURL: URL(string: "https://idp.example.test:18081")!,
        clientID: fixture.clientID,
        redirectURI: URL(string: fixture.redirectURI)!
    )
}

private func queryValue(_ url: URL, _ name: String) -> String {
    URLComponents(url: url, resolvingAgainstBaseURL: false)!.queryItems!.first { $0.name == name }!.value!
}

private func callback(_ start: BrowserAuthorizationStart, code: String? = nil, error: String? = nil) throws -> URL {
    let fixture = try browserFixture()
    var components = URLComponents(url: try browserConfiguration().redirectURI, resolvingAgainstBaseURL: false)!
    if let code {
        components.queryItems = [URLQueryItem(name: "code", value: code),
                                 URLQueryItem(name: "state", value: queryValue(start.authorizationURL, "state"))]
    } else {
        components.queryItems = [URLQueryItem(name: "error", value: error ?? "access_denied"),
                                 URLQueryItem(name: "state", value: queryValue(start.authorizationURL, "state"))]
    }
    _ = fixture // Keep this helper tied to the shared public fixture values.
    return components.url!
}

private func requestBody(_ request: URLRequest) -> Data {
    if let body = request.httpBody { return body }
    guard let stream = request.httpBodyStream else { return Data() }
    stream.open()
    defer { stream.close() }
    var result = Data()
    var buffer = [UInt8](repeating: 0, count: 1024)
    while stream.hasBytesAvailable {
        let count = stream.read(&buffer, maxLength: buffer.count)
        guard count > 0 else { break }
        result.append(buffer, count: count)
    }
    return result
}

private func session() -> URLSession {
    let configuration = URLSessionConfiguration.ephemeral
    configuration.protocolClasses = [TokenProtocol.self]
    return URLSession(configuration: configuration)
}

private struct MockResponse: Sendable {
    let status: Int
    let data: Data
    let headers: [String: String]
    let delayNanoseconds: UInt64

    init(status: Int = 200, data: Data, headers: [String: String] = [:], delayNanoseconds: UInt64 = 0) {
        self.status = status
        self.data = data
        self.headers = headers
        self.delayNanoseconds = delayNanoseconds
    }
}

private final class TokenProtocol: URLProtocol, @unchecked Sendable {
    nonisolated(unsafe) static var handler: (@Sendable (URLRequest) -> MockResponse)?
    nonisolated(unsafe) static var requestCount = 0
    private static let lock = NSLock()

    static func install(_ handler: @escaping @Sendable (URLRequest) -> MockResponse) {
        lock.lock()
        self.handler = handler
        requestCount = 0
        lock.unlock()
    }

    static func count() -> Int {
        lock.lock()
        defer { lock.unlock() }
        return requestCount
    }

    override class func canInit(with request: URLRequest) -> Bool {
        ["idp.example.test", "enrollment.example.test"].contains(request.url?.host ?? "")
    }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        Self.lock.lock()
        Self.requestCount += 1
        let handler = Self.handler
        Self.lock.unlock()
        guard let handler else { return }
        let response = handler(request)
        let delivery = TokenDelivery(protocolInstance: self, response: response)
        if response.delayNanoseconds == 0 {
            delivery.send()
        } else {
            DispatchQueue.global().asyncAfter(deadline: .now() + .nanoseconds(Int(response.delayNanoseconds))) {
                delivery.send()
            }
        }
    }

    override func stopLoading() {
        // Intentionally do not suppress a queued result: cancellation must reject
        // a response even when the underlying transport chooses to complete it.
    }
}

private final class TokenDelivery: @unchecked Sendable {
    private let protocolInstance: TokenProtocol
    private let response: MockResponse

    init(protocolInstance: TokenProtocol, response: MockResponse) {
        self.protocolInstance = protocolInstance
        self.response = response
    }

    func send() {
        guard let url = protocolInstance.request.url,
              let http = HTTPURLResponse(url: url, statusCode: response.status,
                                         httpVersion: "HTTP/1.1", headerFields: response.headers)
        else { return }
        protocolInstance.client?.urlProtocol(protocolInstance, didReceive: http, cacheStoragePolicy: .notAllowed)
        protocolInstance.client?.urlProtocol(protocolInstance, didLoad: response.data)
        protocolInstance.client?.urlProtocolDidFinishLoading(protocolInstance)
    }
}

private final class TestClock: @unchecked Sendable {
    private let lock = NSLock()
    private var date: Date
    init(_ date: Date) { self.date = date }
    func now() -> Date { lock.withLock { date } }
    func advance(seconds: TimeInterval) { lock.withLock { date.addTimeInterval(seconds) } }
}

private func expectBrowserError(_ expected: BrowserLoginError,
                                _ operation: () async throws -> Void) async {
    do {
        try await operation()
        Issue.record("Expected \(expected)")
    } catch let error as BrowserLoginError {
        #expect(error == expected)
    } catch {
        Issue.record("Unexpected error: \(error.localizedDescription)")
    }
}

@Suite(.serialized) struct BrowserLoginTests {
@Test func appleWebRequiresHashedNonceAndRedeemsCredentialBodyOnce() async throws {
    let generic = try browserConfiguration()
    let config = try BrowserLoginConfiguration(idpURL: generic.idpURL, clientID: generic.clientID,
        redirectURI: generic.redirectURI, tokenOrigin: URL(string: "https://enrollment.example.test")!, flow: .appleWeb)
    let client = BrowserLoginClient(configuration: config, session: session())
    for nonce in [nil, "raw-nonce", String(repeating: "A", count: 64)] as [String?] {
        await expectBrowserError(.invalidConfiguration) { _ = try await client.beginAuthorization(nonce: nonce) }
    }
    TokenProtocol.install { request in
        #expect(request.url?.absoluteString == "https://enrollment.example.test/v1/apple/login/token")
        return MockResponse(data: Data("{\"identity_token\":\"synthetic.identity.token\",\"authorization_code\":\"synthetic-code\"}".utf8))
    }
    let start = try await client.beginAuthorization(nonce: String(repeating: "a", count: 64))
    #expect(queryValue(start.authorizationURL, "nonce") == String(repeating: "a", count: 64))
    #expect(start.authorizationURL.path == "/v1/apple/login/start")
    let url = try callback(start, code: browserFixture().code)
    let credential = try await start.transaction.exchangeAppleCredential(callbackURL: url)
    #expect(credential.identityToken == "synthetic.identity.token")
    #expect(credential.authorizationCode == "synthetic-code")
    await expectBrowserError(.exchangeAlreadyStarted) { _ = try await start.transaction.exchangeAppleCredential(callbackURL: url) }
    #expect(TokenProtocol.count() == 1)
}

@Test func appleWebRejectsIncompleteOrOversizedCredentialBodies() async throws {
    let generic = try browserConfiguration()
    let config = try BrowserLoginConfiguration(idpURL: generic.idpURL, clientID: generic.clientID,
        redirectURI: generic.redirectURI, flow: .appleWeb)
    let client = BrowserLoginClient(configuration: config, session: session())
    for body in [
        ["identity_token": "", "authorization_code": "code"],
        ["identity_token": "token", "authorization_code": ""],
        ["identity_token": String(repeating: "t", count: 20001), "authorization_code": "code"],
        ["identity_token": "token", "authorization_code": String(repeating: "c", count: 4097)],
    ] {
        let data = try JSONSerialization.data(withJSONObject: body)
        TokenProtocol.install { _ in MockResponse(data: data) }
        let start = try await client.beginAuthorization(nonce: String(repeating: "a", count: 64))
        let url = try callback(start, code: browserFixture().code)
        await expectBrowserError(.invalidTokenResponse) { _ = try await start.transaction.exchangeAppleCredential(callbackURL: url) }
        await expectBrowserError(.exchangeAlreadyStarted) { _ = try await start.transaction.exchangeAppleCredential(callbackURL: url) }
    }
}

@Test func pkceMatchesRFC7636AppendixBFixture() throws {
    let fixture = try browserFixture()
    #expect(try PKCE.challenge(for: fixture.verifier) == fixture.challenge)
    #expect(throws: BrowserLoginError.invalidVerifier) { try PKCE.challenge(for: "short") }
}

@Test func authorizationUsesFreshStateAndS256Challenge() async throws {
    let client = BrowserLoginClient(configuration: try browserConfiguration(), session: session())
    let first = try await client.beginAuthorization()
    let second = try await client.beginAuthorization()
    #expect(queryValue(first.authorizationURL, "state") != queryValue(second.authorizationURL, "state"))
    #expect(queryValue(first.authorizationURL, "response_type") == "code")
    #expect(queryValue(first.authorizationURL, "code_challenge_method") == "S256")
    let challenge = queryValue(first.authorizationURL, "code_challenge")
    #expect(challenge.count == 43)
    #expect(try browserConfiguration().callbackURLScheme == "com.waterfallelephant.weirnote.devtest.login")
}

@Test func exchangesExactCallbackAndUsesFormEncodedRequest() async throws {
    let fixture = try browserFixture()
    TokenProtocol.install { request in
        #expect(request.httpMethod == "POST")
        #expect(request.value(forHTTPHeaderField: "Content-Type") == "application/x-www-form-urlencoded")
        let body = String(decoding: requestBody(request), as: UTF8.self)
        let fields = URLComponents(string: "?" + body)!.queryItems!
        #expect(Set(fields.map(\.name)) == Set(["grant_type", "client_id", "redirect_uri", "code", "code_verifier"]))
        #expect(fields.first { $0.name == "grant_type" }?.value == "authorization_code")
        #expect(fields.first { $0.name == "code" }?.value == fixture.code)
        #expect(fields.first { $0.name == "code_verifier" }?.value?.count == 43)
        return MockResponse(data: try! JSONEncoder().encode(fixture.tokenResponse))
    }
    let client = BrowserLoginClient(configuration: try browserConfiguration(), session: session())
    let start = try await client.beginAuthorization()
    let token = try await start.transaction.exchange(callbackURL: callback(start, code: fixture.code))
    #expect(token == BrowserTokenResponse(accessToken: fixture.tokenResponse.accessToken, expiresIn: fixture.tokenResponse.expiresIn))
    #expect(TokenProtocol.count() == 1)
}

@Test func rejectsWrongMissingMalformedDuplicateAndUnexpectedCallbacks() async throws {
    let fixture = try browserFixture()
    let client = BrowserLoginClient(configuration: try browserConfiguration(), session: session())

    let wrongState = try await client.beginAuthorization()
    var components = URLComponents(url: try browserConfiguration().redirectURI, resolvingAgainstBaseURL: false)!
    components.queryItems = [URLQueryItem(name: "code", value: fixture.code), URLQueryItem(name: "state", value: fixture.state)]
    await expectBrowserError(.stateMismatch) { _ = try await wrongState.transaction.exchange(callbackURL: components.url!) }
    await expectBrowserError(.exchangeAlreadyStarted) { _ = try await wrongState.transaction.exchange(callbackURL: try callback(wrongState, code: fixture.code)) }

    let missing = try await client.beginAuthorization()
    components.queryItems = [URLQueryItem(name: "code", value: fixture.code)]
    await expectBrowserError(.invalidCallback) { _ = try await missing.transaction.exchange(callbackURL: components.url!) }

    let duplicate = try await client.beginAuthorization()
    let duplicateURL = URL(string: "\(try browserConfiguration().redirectURI.absoluteString)?code=\(fixture.code)&state=\(queryValue(duplicate.authorizationURL, "state"))&state=again")!
    await expectBrowserError(.invalidCallback) { _ = try await duplicate.transaction.exchange(callbackURL: duplicateURL) }

    let malformed = try await client.beginAuthorization()
    let malformedURL = URL(string: "\(try browserConfiguration().redirectURI.absoluteString)?code=%GG&state=\(queryValue(malformed.authorizationURL, "state"))")!
    await expectBrowserError(.invalidCallback) { _ = try await malformed.transaction.exchange(callbackURL: malformedURL) }

    let wrongApp = try await client.beginAuthorization()
    let wrongURL = URL(string: "other.login:/oauth/callback?code=\(fixture.code)&state=\(queryValue(wrongApp.authorizationURL, "state"))")!
    await expectBrowserError(.callbackMismatch) { _ = try await wrongApp.transaction.exchange(callbackURL: wrongURL) }

    let authority = try await client.beginAuthorization()
    let authorityURL = URL(string: "\(try browserConfiguration().callbackURLScheme):///oauth/callback?code=\(fixture.code)&state=\(queryValue(authority.authorizationURL, "state"))")!
    await expectBrowserError(.callbackMismatch) { _ = try await authority.transaction.exchange(callbackURL: authorityURL) }
}

@Test func callbackErrorsAreCategorizedWithoutExchangingAndRemainSingleUse() async throws {
    TokenProtocol.install { _ in MockResponse(data: Data()) }
    let client = BrowserLoginClient(configuration: try browserConfiguration(), session: session())
    let cases: [(String, BrowserLoginError)] = [
        ("access_denied", .authorizationDenied), ("server_error", .serviceUnavailable),
        ("temporarily_unavailable", .serviceUnavailable), ("invalid_request", .authorizationFailed),
        ("PRIVATE-unknown", .invalidCallback),
    ]
    for (category, expected) in cases {
        let start = try await client.beginAuthorization()
        await expectBrowserError(expected) { _ = try await start.transaction.exchange(callbackURL: try callback(start, error: category)) }
        await expectBrowserError(.exchangeAlreadyStarted) { _ = try await start.transaction.exchange(callbackURL: try callback(start, code: browserFixture().code)) }
    }
    let mismatched = try await client.beginAuthorization()
    let untrusted = URL(string: "\(try browserConfiguration().redirectURI.absoluteString)?error=server_error&state=wrong")!
    await expectBrowserError(.stateMismatch) { _ = try await mismatched.transaction.exchange(callbackURL: untrusted) }
    let duplicate = try await client.beginAuthorization()
    let duplicateURL = URL(string: "\(try callback(duplicate, error: "server_error").absoluteString)&error=access_denied")!
    await expectBrowserError(.invalidCallback) { _ = try await duplicate.transaction.exchange(callbackURL: duplicateURL) }
    #expect(TokenProtocol.count() == 0)
}

@Test func cancellationExpiryAndBrowserDenialClearTheTransaction() async throws {
    let fixture = try browserFixture()
    let clock = TestClock(Date(timeIntervalSince1970: 100))
    let client = BrowserLoginClient(configuration: try browserConfiguration(), session: session(), now: { clock.now() })

    let cancelled = try await client.beginAuthorization()
    await cancelled.transaction.cancel()
    await expectBrowserError(.exchangeAlreadyStarted) { _ = try await cancelled.transaction.exchange(callbackURL: try callback(cancelled, code: fixture.code)) }

    let denied = try await client.beginAuthorization()
    await expectBrowserError(.authorizationDenied) { _ = try await denied.transaction.exchange(callbackURL: try callback(denied, error: "access_denied")) }
    await expectBrowserError(.exchangeAlreadyStarted) { _ = try await denied.transaction.exchange(callbackURL: try callback(denied, code: fixture.code)) }

    let expired = try await client.beginAuthorization()
    clock.advance(seconds: 301)
    await expectBrowserError(.expired) { _ = try await expired.transaction.exchange(callbackURL: try callback(expired, code: fixture.code)) }
}

@Test func exchangeIsSingleUseAndCancellationWinsOverACompletedTransport() async throws {
    let fixture = try browserFixture()
    TokenProtocol.install { _ in
        MockResponse(data: try! JSONEncoder().encode(fixture.tokenResponse), delayNanoseconds: 100_000_000)
    }
    let client = BrowserLoginClient(configuration: try browserConfiguration(), session: session())
    let start = try await client.beginAuthorization()
    let first = Task { try await start.transaction.exchange(callbackURL: try callback(start, code: fixture.code)) }
    for _ in 0..<100 where TokenProtocol.count() == 0 { try await Task.sleep(for: .milliseconds(2)) }
    #expect(TokenProtocol.count() == 1)
    let second = Task { try await start.transaction.exchange(callbackURL: try callback(start, code: fixture.code)) }
    await start.transaction.cancel()
    await expectBrowserError(.cancelled) { _ = try await first.value }
    await expectBrowserError(.exchangeAlreadyStarted) { _ = try await second.value }
    #expect(TokenProtocol.count() == 1)
}

@Test func tokenAndRedirectFailuresAreRedactedAndDoNotRetry() async throws {
    let fixture = try browserFixture()
    TokenProtocol.install { _ in
        MockResponse(status: 400, data: Data("{\"error\":\"invalid_grant\",\"detail\":\"fixture-token\"}".utf8))
    }
    let client = BrowserLoginClient(configuration: try browserConfiguration(), session: session())
    let failed = try await client.beginAuthorization()
    do {
        _ = try await failed.transaction.exchange(callbackURL: callback(failed, code: fixture.code))
        Issue.record("Expected the invalid_grant response to fail")
    } catch let error as BrowserLoginError {
        #expect(error == .exchangeFailed)
        #expect(!error.localizedDescription.contains("fixture-token"))
    }
    #expect(TokenProtocol.count() == 1)

    TokenProtocol.install { _ in
        MockResponse(status: 302, data: Data(), headers: ["Location": "https://attacker.example.test/token"])
    }
    let redirected = try await client.beginAuthorization()
    await expectBrowserError(.exchangeFailed) { _ = try await redirected.transaction.exchange(callbackURL: try callback(redirected, code: fixture.code)) }
    #expect(TokenProtocol.count() == 1)
}
}
