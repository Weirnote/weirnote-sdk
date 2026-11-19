import Foundation
import Testing
@testable import DeviceEnrollment

private final class AccountURLProtocol: URLProtocol, @unchecked Sendable {
    struct Stub: Sendable {
        let status: Int
        let body: Data
    }

    private static let lock = NSLock()
    nonisolated(unsafe) private static var stubs: [String: Stub] = [:]
    nonisolated(unsafe) private static var failureHosts: Set<String> = []
    nonisolated(unsafe) private static var requests: [String: [URLRequest]] = [:]

    static func install(host: String, status: Int, body: String = "") {
        lock.lock()
        stubs[host] = Stub(status: status, body: Data(body.utf8))
        failureHosts.remove(host)
        requests[host] = []
        lock.unlock()
    }

    static func installTransportFailure(host: String) {
        lock.lock()
        stubs.removeValue(forKey: host)
        failureHosts.insert(host)
        requests[host] = []
        lock.unlock()
    }

    static func captured(host: String) -> [URLRequest] {
        lock.lock()
        defer { lock.unlock() }
        return requests[host] ?? []
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() {}

    override func startLoading() {
        guard let requestURL = request.url, let host = requestURL.host else {
            client?.urlProtocol(self, didFailWithError: AccountClientError.invalidResponse)
            return
        }
        Self.lock.lock()
        let stub = Self.stubs[host]
        let shouldFail = Self.failureHosts.contains(host)
        Self.requests[host, default: []].append(request)
        Self.lock.unlock()
        if shouldFail {
            client?.urlProtocol(self, didFailWithError: URLError(.networkConnectionLost))
            return
        }
        guard let stub,
              let response = HTTPURLResponse(url: requestURL, statusCode: stub.status,
                                             httpVersion: "HTTP/1.1", headerFields: nil) else {
            client?.urlProtocol(self, didFailWithError: AccountClientError.invalidResponse)
            return
        }
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        if !stub.body.isEmpty { client?.urlProtocol(self, didLoad: stub.body) }
        client?.urlProtocolDidFinishLoading(self)
    }
}

private func accountClient(host: String) throws -> AccountClient {
    let configuration = URLSessionConfiguration.ephemeral
    configuration.protocolClasses = [AccountURLProtocol.self]
    let session = URLSession(configuration: configuration)
    return try AccountClient(accountOrigin: URL(string: "https://\(host)")!, transport: session)
}

@Test func profileDecodesKnownSyncStatusAndIgnoresUnknownFeatures() async throws {
    let host = "profile.account.test"
    AccountURLProtocol.install(host: host, status: 200, body: #"{"user_id":"user-1","identities":["apple"],"devices":[{"device_id":"device-1","caller":true}],"features":{"sync":"available","future_feature":{"added":true}}}"#)
    let client = try accountClient(host: host)
    let profile = try await client.profile(token: "secret-token")

    #expect(profile.userID == "user-1")
    #expect(profile.identities == ["apple"])
    #expect(profile.devices.first?.deviceID == "device-1")
    #expect(profile.syncAvailability == .available)
    let request = try #require(AccountURLProtocol.captured(host: host).first)
    #expect(request.httpMethod == "GET")
    #expect(request.url?.path == "/v1/account")
    #expect(request.value(forHTTPHeaderField: "Authorization") == "Bearer secret-token")
    #expect(request.value(forHTTPHeaderField: "Cache-Control") == "no-store")
    #expect(request.httpBody == nil)
}

@Test(arguments: ["new_status", "", "subscription_required"])
func profileUnknownOrNonAvailableStatusMapsToUnavailable(status: String) async throws {
    let host = "status-\(status.replacingOccurrences(of: "_", with: "-")).account.test"
    let escaped = status.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"")
    AccountURLProtocol.install(host: host, status: 200, body: #"{"user_id":"u","identities":[],"devices":[],"features":{"sync":"\#(escaped)","extra":"ignored"}}"#)
    let profile = try await accountClient(host: host).profile(token: "token")
    #expect(profile.syncAvailability == (status == "subscription_required" ? .subscriptionRequired : .unavailable))
}

@Test(arguments: [(202, true), (204, true), (503, false), (500, false), (504, false), (401, false)])
func deleteUsesContractStatuses(status: Int, accepted: Bool) async throws {
    let host = "delete-\(status).account.test"
    AccountURLProtocol.install(host: host, status: status, body: #"{"code":"private detail"}"#)
    let client = try accountClient(host: host)
    do {
        let result = try await client.deleteAccount(token: "sensitive-bearer")
        #expect(accepted)
        if status == 202 { #expect(result == .accepted) }
        if status == 204 { #expect(result == .completed) }
    } catch let error as AccountClientError {
        #expect(!accepted)
        if status == 503 { #expect(error == .deletionUnavailable) }
        if status == 500 || status == 504 { #expect(error == .serviceUnavailable) }
        if status == 401 { #expect(error == .deletionOutcomeUnknown) }
        #expect(!(error.localizedDescription.contains("private detail")))
        #expect(!(error.localizedDescription.contains("sensitive-bearer")))
    }
    let request = try #require(AccountURLProtocol.captured(host: host).first)
    #expect(request.httpMethod == "DELETE")
    #expect(request.url?.path == "/v1/account")
    #expect(request.value(forHTTPHeaderField: "Authorization") == "Bearer sensitive-bearer")
    #expect(request.httpBody == nil)
}

@Test func deleteTransportLossHasUnknownOutcomeWithoutExposingSecrets() async throws {
    let host = "delete-lost-response.account.test"
    AccountURLProtocol.installTransportFailure(host: host)
    do {
        _ = try await accountClient(host: host).deleteAccount(token: "sensitive-bearer")
        Issue.record("Expected transport failure")
    } catch let error as AccountClientError {
        #expect(error == .deletionOutcomeUnknown)
        #expect(!error.localizedDescription.contains("sensitive-bearer"))
    }
}

@Test(arguments: [(401, AccountClientError.unauthorized),
                  (402, AccountClientError.subscriptionRequired),
                  (500, AccountClientError.serviceUnavailable),
                  (503, AccountClientError.serviceUnavailable),
                  (504, AccountClientError.serviceUnavailable)])
func profileUsesTypedErrorStatuses(status: Int, expected: AccountClientError) async throws {
    let host = "profile-error-\(status).account.test"
    AccountURLProtocol.install(host: host, status: status, body: #"{"code":"do not expose"}"#)
    do {
        _ = try await accountClient(host: host).profile(token: "secret")
        Issue.record("Expected HTTP \(status) to fail")
    } catch let error as AccountClientError {
        #expect(error == expected)
        #expect(!error.localizedDescription.contains("do not expose"))
        #expect(!error.localizedDescription.contains("secret"))
    }
}

@Test func clientDoesNotAcceptRedirectStatusAsProfileSuccess() async throws {
    let host = "redirect.account.test"
    AccountURLProtocol.install(host: host, status: 302)
    do {
        _ = try await accountClient(host: host).profile(token: "secret")
        Issue.record("Expected a redirect response to be rejected")
    } catch let error as AccountClientError {
        #expect(error == .httpStatus(302))
    }
}

@Test(arguments: ["http://account.test", "https://account.test/path", "https://user:pass@account.test"])
func accountOriginRejectsInsecureOrNonOriginURLs(value: String) throws {
    let origin = try #require(URL(string: value))
    #expect(throws: AccountClientError.invalidOrigin) {
        try AccountClient(accountOrigin: origin)
    }
}

@Test(arguments: [204, 401, 503, 302])
func unbindUsesOwnDeviceEndpointAndBoundedErrors(status: Int) async throws {
    let host = "unbind-\(status).account.test"
    AccountURLProtocol.install(host: host, status: status, body: "private response")
    do {
        try await accountClient(host: host).unbindCurrentDevice(token: "secret")
        #expect(status == 204)
    } catch let error as AccountClientError {
        #expect(status != 204)
        if status == 401 { #expect(error == .unauthorized) }
        if status == 503 { #expect(error == .serviceUnavailable) }
        if status == 302 { #expect(error == .httpStatus(302)) }
        #expect(!error.localizedDescription.contains("private response"))
    }
    let request = try #require(AccountURLProtocol.captured(host: host).first)
    #expect(request.httpMethod == "DELETE")
    #expect(request.url?.path == "/v1/devices/current")
    #expect(request.url?.query == nil)
    #expect(request.httpBody == nil)
    #expect(request.value(forHTTPHeaderField: "Authorization") == "Bearer secret")
}

@Test func mcpManagementUsesSessionAndDoesNotRedisplayRawTokens() async throws {
    let host = "mcp.account.test"
    let metadata = #"""
    {"id":"00000000-0000-4000-8000-000000000001","client_name":"Client","issued_at":"2026-10-12T00:00:00+00:00","expires_at":"2026-11-11T00:00:00+00:00","revoked_at":null}
    """#
    let secret = "wnmcp_"+String(repeating: "x", count: 43)
    AccountURLProtocol.install(host: host, status: 201, body: String(metadata.dropLast())+",\"token\":\"\(secret)\"}")
    let client = try accountClient(host: host)
    let receipt = try await client.createMCPToken(token: "normal-session", clientName: "Client")
    #expect(receipt.token == secret)
    #expect(!String(describing: receipt).contains(secret))
    let request = try #require(AccountURLProtocol.captured(host: host).first)
    #expect(request.url?.path == "/v1/account/mcp-tokens")
    #expect(request.value(forHTTPHeaderField: "Authorization") == "Bearer normal-session")
    var body = request.httpBody ?? Data()
    if let stream = request.httpBodyStream {
        stream.open()
        defer { stream.close() }
        var buffer = [UInt8](repeating: 0, count: 4096)
        while stream.hasBytesAvailable {
            let count = stream.read(&buffer, maxLength: buffer.count)
            if count <= 0 { break }
            body.append(contentsOf: buffer.prefix(count))
        }
    }
    let parsed = try JSONSerialization.jsonObject(with: body) as! [String: Any]
    #expect(parsed["expires_in_days"] as? Int == 30)
    #expect(parsed["user_id"] == nil)
    AccountURLProtocol.install(host: host, status: 200, body: "{\"tokens\":[\(metadata)]}")
    let list = try await client.mcpTokens(token: "normal-session")
    #expect(list == [receipt.metadata])
    AccountURLProtocol.install(host: host, status: 204)
    try await client.revokeMCPToken(token: "normal-session", id: receipt.metadata.id)
    #expect(AccountURLProtocol.captured(host: host).first?.httpMethod == "DELETE")
}

@Test func mcpManagementValidationAndErrorsRemainBounded() async throws {
    let host = "mcp-errors.account.test"
    let client = try accountClient(host: host)
    for name in ["", "a\nb", String(repeating: "x", count: 81)] {
        await #expect(throws: AccountClientError.invalidResponse) {
            try await client.createMCPToken(token: "session", clientName: name)
        }
    }
    await #expect(throws: AccountClientError.invalidResponse) { try await client.revokeMCPToken(token: "session", id: "../../evil") }
    for status in [401, 404, 503] {
        AccountURLProtocol.install(host: host, status: status, body: "PRIVATE_SECRET")
        let expected: AccountClientError = status == 401 ? .unauthorized : status == 503 ? .serviceUnavailable : .httpStatus(status)
        await #expect(throws: expected) { try await client.mcpTokens(token: "session") }
    }
    AccountURLProtocol.install(host: host, status: 201, body: #"{"token":"malformed"}"#)
    await #expect(throws: AccountClientError.invalidResponse) { try await client.createMCPToken(token: "session", clientName: "Client") }
}

@Test(arguments: [
    (#""#, false, []),
    (#","platforms":["mac","ios"],"platforms_complete":true"#, true, ["mac", "ios"]),
    (#","platforms":["mac","watch"],"platforms_complete":true"#, false, ["mac"]),
    (#","platforms":[],"platforms_complete":true"#, false, []),
    (#","platforms":"mac","platforms_complete":true"#, false, []),
    (#","platforms":["ios"],"platforms_complete":"true""#, false, ["ios"])
] as [(String, Bool, [String])])
func platformMetadataIsOptionalAndUnknownCoverageIsIncomplete(extra: String, complete: Bool, known: [String]) throws {
    let json = #"{"user_id":"u","identities":[],"devices":[{"device_id":"d","caller":true}],"features":{"sync":"available"}\#(extra)}"#
    let profile = try JSONDecoder().decode(AccountProfile.self, from: Data(json.utf8))
    #expect(profile.platformsComplete == complete)
    #expect(Set(profile.platforms.map(\.rawValue)) == Set(known))
    #expect(profile.syncAvailability == .available)
}

@Test(arguments: [204, 404, 401, 503])
func platformRegistrationUsesOnlyCallerEndpoint(status: Int) async throws {
    let host = "platform-\(status).account.test"
    AccountURLProtocol.install(host: host, status: status)
    do {
        try await accountClient(host: host).registerCurrentDevicePlatform(token: "caller-token", platform: .ios)
        #expect(status == 204)
    } catch { #expect(status != 204) }
    let request = try #require(AccountURLProtocol.captured(host: host).first)
    #expect(request.url?.path == "/v1/devices/current/platform")
    #expect(request.httpMethod == "PUT")
    #expect(request.url?.query == nil)
    #expect(request.value(forHTTPHeaderField: "Authorization") == "Bearer caller-token")
    let body: Data
    if let data = request.httpBody { body = data }
    else if let stream = request.httpBodyStream {
        stream.open(); defer { stream.close() }
        var bytes = [UInt8](repeating: 0, count: 256)
        let length = stream.read(&bytes, maxLength: bytes.count)
        body = Data(bytes.prefix(max(0, length)))
    } else { body = Data() }
    #expect(try JSONSerialization.jsonObject(with: body) as? [String: String] == ["platform": "ios"])
}

private func trialContractFixtures() throws -> [String: Any] {
    let path = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
        .deletingLastPathComponent().deletingLastPathComponent()
        .deletingLastPathComponent().deletingLastPathComponent()
        .appendingPathComponent("docs/fixtures/sync-trial-contract.json")
    return try #require(JSONSerialization.jsonObject(with: Data(contentsOf: path)) as? [String: Any])
}

private func fixtureData(_ value: Any) throws -> Data {
    try JSONSerialization.data(withJSONObject: value)
}

@Test func syncAccessFixturesDecodeWithoutChangingLegacyAvailability() throws {
    let fixtures = try trialContractFixtures()
    let profiles = try #require(fixtures["profiles"] as? [[String: Any]])
    for fixture in profiles {
        let name = try #require(fixture["name"] as? String)
        let json = try #require(fixture["profile"] as? [String: Any])
        let profile = try JSONDecoder().decode(AccountProfile.self, from: fixtureData(json))
        let details = try #require(profile.syncAccess)
        let expectedAvailability = try #require((json["features"] as? [String: Any])?["sync"] as? String)
        #expect(profile.syncAvailability.rawValue == expectedAvailability, "\(name)")
        #expect(profile.experimentalMCPAvailable)
        #expect(profile.platformsComplete)
        #expect(details.evaluatedAt.timeIntervalSince1970 > 0)
        let expectedAccess = try #require(json["sync_access"] as? [String: Any])
        #expect(details.state?.rawValue == expectedAccess["state"] as? String, "\(name)")
        #expect(details.trialEligible == expectedAccess["trial_eligible"] as? Bool, "\(name)")
        #expect((details.trialStartedAt != nil) == (expectedAccess["trial_started_at"] is String), "\(name)")
        #expect((details.trialEndsAt != nil) == (expectedAccess["trial_ends_at"] is String), "\(name)")
        if name == "store_trial_canceled" || name == "subscribed_canceled" || name == "web_subscribed_canceled" {
            #expect(details.willRenew == false)
            #expect(details.renewsAt == nil)
            #expect(details.accessEndsAt != nil)
        }
        if name == "web_subscribed_renewing" {
            #expect(details.willRenew == true)
            #expect(details.renewsAt != nil)
        }
        if name == "expired_store_trial" || name == "expired_paid" || name == "disqualified_server_trial" {
            #expect(details.state == .expired)
            #expect(profile.syncAvailability == .subscriptionRequired)
        }
        if name == "exempt" { #expect(details.enforcement == .exempt); #expect(details.state == nil) }
        if name == "unavailable" { #expect(details.state == nil); #expect(details.trialEligible == nil) }
    }
    let legacy = try #require(fixtures["legacy_profile"])
    let oldProfile = try JSONDecoder().decode(AccountProfile.self, from: fixtureData(legacy))
    #expect(oldProfile.syncAccess == nil)
    #expect(oldProfile.syncAvailability == .subscriptionRequired)
}

@Test func unknownAndMalformedTrialMetadataFailClosed() throws {
    let fixtures = try trialContractFixtures()
    let base = try #require((fixtures["profiles"] as? [[String: Any]])?.first?["profile"] as? [String: Any])
    var future = base
    var futureAccess = try #require(future["sync_access"] as? [String: Any])
    futureAccess["state"] = "future_grant"
    futureAccess["trial_offers"] = ["mac": "future_offer", "ios": "server"]
    futureAccess["future_field"] = ["present": true]
    future["sync_access"] = futureAccess
    let futureProfile = try JSONDecoder().decode(AccountProfile.self, from: fixtureData(future))
    #expect(futureProfile.syncAvailability == .subscriptionRequired)
    #expect(futureProfile.syncAccess == nil)
    futureAccess["state"] = "none"
    future["sync_access"] = futureAccess
    let unknownOffer = try JSONDecoder().decode(AccountProfile.self, from: fixtureData(future))
    #expect(unknownOffer.syncAccess?.trialOffers?.mac == nil)
    for invalid in ["2030-01-15T00:00:00-06:00", "not-a-date", "2030-99-99T00:00:00Z"] {
        var malformed = base
        var access = try #require(malformed["sync_access"] as? [String: Any])
        access["evaluated_at"] = invalid
        malformed["sync_access"] = access
        let profile = try JSONDecoder().decode(AccountProfile.self, from: fixtureData(malformed))
        #expect(profile.syncAccess == nil)
        #expect(profile.syncAvailability == .subscriptionRequired)
    }
}

@Test(arguments: [200, 201])
func startTrialUsesAccountOriginBearerAndImmutableReceipt(status: Int) async throws {
    let host = "trial-success-\(status).account.test"
    let fixtures = try trialContractFixtures()
    let result = try #require(fixtures[status == 200 ? "start_retry" : "start_new"] as? [String: Any])
    let body = try #require(result["body"])
    AccountURLProtocol.install(host: host, status: status, body: String(decoding: try fixtureData(body), as: UTF8.self))
    let access = try await accountClient(host: host).startSyncTrial(token: "trial-bearer")
    #expect(access.state == .serverTrial)
    #expect(access.trialEligible == false)
    #expect(access.trialStartedAt != nil)
    #expect(access.trialEndsAt == nil)
    #expect(access.accessEndsAt == nil)
    let request = try #require(AccountURLProtocol.captured(host: host).first)
    #expect(request.httpMethod == "POST")
    #expect(request.url?.absoluteString == "https://\(host)/v1/account/sync-trial")
    #expect(request.value(forHTTPHeaderField: "Authorization") == "Bearer trial-bearer")
    #expect(request.value(forHTTPHeaderField: "Cache-Control") == "no-store")
    #expect(request.httpBody == nil)
}

@Test(arguments: [
    (401, "unauthorized", AccountClientError.unauthorized),
    (409, "trial_already_used", AccountClientError.trialAlreadyUsed),
    (409, "store_trial_required", AccountClientError.storeTrialRequired),
    (409, "platform_registration_required", AccountClientError.platformRegistrationRequired),
    (409, "access_already_active", AccountClientError.accessAlreadyActive),
    (503, "entitlement_unavailable", AccountClientError.serviceUnavailable),
    (409, "future_code", AccountClientError.httpStatus(409)),
    (302, "redirect", AccountClientError.httpStatus(302))
])
func startTrialErrorsStayBounded(status: Int, code: String, expected: AccountClientError) async throws {
    let host = "trial-error-\(status)-\(code.replacingOccurrences(of: "_", with: "-")).account.test"
    AccountURLProtocol.install(host: host, status: status, body: #"{"code":"\#(code)","secret":"PRIVATE_BODY"}"#)
    await #expect(throws: expected) { try await accountClient(host: host).startSyncTrial(token: "SECRET_BEARER") }
    #expect(!expected.localizedDescription.contains("PRIVATE_BODY"))
    #expect(!expected.localizedDescription.contains("SECRET_BEARER"))
}

@Test func malformedTrialReceiptAndLostResponseAreSafeToRetry() async throws {
    let host = "trial-malformed.account.test"
    AccountURLProtocol.install(host: host, status: 201, body: #"{"sync_access":{"state":"none"}}"#)
    await #expect(throws: AccountClientError.invalidResponse) {
        try await accountClient(host: host).startSyncTrial(token: "bearer")
    }
    AccountURLProtocol.installTransportFailure(host: host)
    do {
        _ = try await accountClient(host: host).startSyncTrial(token: "bearer")
        Issue.record("Expected transport loss")
    } catch let error as AccountClientError {
        #expect(error == .trialOutcomeUnknown)
        #expect(!error.localizedDescription.contains("bearer"))
    }
}

@Test func settingsWirePreservesUnknownMembersAndStableEdit() async throws {
    let host = "settings.account.test"
    let fixture = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
        .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        .appendingPathComponent("docs/fixtures/account-settings.json")
    let raw = try String(contentsOf: fixture, encoding: .utf8)
    AccountURLProtocol.install(host: host, status: 200, body: raw)
    let client = try accountClient(host: host)
    let received = try await client.settings(token: "fixture")
    let edit = try #require(received.edit)
    let retried = try await client.settings(token: "fixture", edit: edit)
    #expect(retried.edit == edit)
    let request = try #require(AccountURLProtocol.captured(host: host).last)
    #expect(request.url?.path == "/v1/account/settings")
    #expect(request.httpMethod == "PUT")
    #expect(request.value(forHTTPHeaderField: "Authorization") == "Bearer fixture")
    var body = request.httpBody ?? Data()
    if let stream = request.httpBodyStream {
        stream.open()
        defer { stream.close() }
        var buffer = [UInt8](repeating: 0, count: 4096)
        while stream.hasBytesAvailable {
            let count = stream.read(&buffer, maxLength: buffer.count)
            if count <= 0 { break }
            body.append(contentsOf: buffer.prefix(count))
        }
    }
    let uploaded = try JSONDecoder().decode(AccountSettingsEdit.self, from: body)
    #expect(uploaded == edit)
}

@Test func settingsRejectUnsupportedVersionsAndPropagateOlderServer() async throws {
    let host = "old-settings.account.test"
    let client = try accountClient(host: host)
    for body in [#"{"version":2,"edit":null}"#, #"{"version":1,"edit":{"version":2,"edited_at_ms":10,"edit_id":"00000000-0000-0000-0000-000000000001","settings":{}}}"#] {
        AccountURLProtocol.install(host: host, status: 200, body: body)
        await #expect(throws: AccountClientError.invalidResponse) { try await client.settings(token: "fixture") }
    }
    for status in [404, 401, 503] {
        AccountURLProtocol.install(host: host, status: status)
        let expected: AccountClientError = status == 401 ? .unauthorized : (status == 503 ? .serviceUnavailable : .httpStatus(status))
        await #expect(throws: expected) { try await client.settings(token: "fixture") }
    }
}

@Test func settingsDecimalDomainIsExactAndBounded() throws {
    for value in ["0.12345678901234567890123456789", "1e-128", "1e127", "9007199254740993", "1.0000"] {
        #expect(AccountSettingsNumbers.supports(value))
        try AccountSettingsNumbers.validateJSON(Data(("{\"nested\":[" + value + "]}").utf8))
    }
    for value in ["1e-129", "1e128", "123456789012345678901234567890123456789", "0.123456789012345678901234567890123456789"] {
        #expect(!AccountSettingsNumbers.supports(value))
        #expect(throws: AccountClientError.invalidResponse) {
            try AccountSettingsNumbers.validateJSON(Data(("{\"nested\":[" + value + "]}").utf8))
        }
    }
    try AccountSettingsNumbers.validateJSON(Data(#"{"literal":"1e99999","escaped":"\"123456789012345678901234567890123456789"}"#.utf8))
}
