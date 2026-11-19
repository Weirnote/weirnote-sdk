import CryptoKit
import Foundation
import Testing
@testable import DeviceEnrollment

private final class RenewalProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() {}
    override func startLoading() {
        do {
            let path = request.url!.path
            let response: [String: Any]
            if path == "/session/challenge" || path == "/enrollment/challenge" {
                let ttl = Int(request.url!.host!.split(separator: ".")[0]) ?? 120
                let endpoint = path == "/session/challenge" ? "/session/renew" : "/enroll"
                let issuedAt = request.url!.host!.hasPrefix("overflow") ? Int.min : 10
                let expiresAt = request.url!.host!.hasPrefix("overflow") ? Int.max : 10 + ttl
                let audienceHost = request.url!.host == "moved.example.test" ? "original.example.test" : request.url!.host!
                response = ["nonce": "fresh", "audience": "https://" + audienceHost + endpoint, "issued_at": issuedAt, "expires_at": expiresAt]
            } else if path == "/session/renew" || path == "/enroll" {
                var data = request.httpBody ?? Data()
                if let stream = request.httpBodyStream {
                    stream.open()
                    defer { stream.close() }
                    var bytes = [UInt8](repeating: 0, count: 1024)
                    while stream.hasBytesAvailable {
                        let count = stream.read(&bytes, maxLength: bytes.count)
                        if count <= 0 { break }
                        data.append(contentsOf: bytes.prefix(count))
                    }
                }
                let body = try JSONSerialization.jsonObject(with: data) as! [String: String]
                let headerPart = body["proof"]!.split(separator: ".")[0]
                var encoded = String(headerPart).replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
                encoded += String(repeating: "=", count: (4 - encoded.count % 4) % 4)
                let header = try JSONSerialization.jsonObject(with: Data(base64Encoded: encoded)!) as! [String: Any]
                #expect(header["typ"] as? String == (path == "/session/renew" ? "device-renewal+jwt" : "device-enrollment+jwt"))
                if request.url!.host == "moved.example.test" {
                    var payload = String(body["proof"]!.split(separator: ".")[1])
                        .replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
                    payload += String(repeating: "=", count: (4 - payload.count % 4) % 4)
                    let claims = try JSONSerialization.jsonObject(with: Data(base64Encoded: payload)!) as! [String: Any]
                    #expect(claims["aud"] as? String == "https://original.example.test/session/renew")
                }
                let jwk = header["jwk"] as! [String: String]
                let canonical = try JSONSerialization.data(withJSONObject: jwk, options: [.sortedKeys, .withoutEscapingSlashes])
                response = ["access_token": "new-token", "expires_in": 900, "user_id": "user", "device_id": Data(SHA256.hash(data: canonical)).base64URL]
            } else {
                Issue.record("Unexpected IdP or enrollment request: \(path)")
                throw EnrollmentError.invalidResponse
            }
            client!.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: 200, httpVersion: nil, headerFields: nil)!, cacheStoragePolicy: .notAllowed)
            client!.urlProtocol(self, didLoad: try JSONSerialization.data(withJSONObject: response))
            client!.urlProtocolDidFinishLoading(self)
        } catch {
            client!.urlProtocol(self, didFailWithError: error)
        }
    }
}

@Test func existingDeviceRenewsAtMovedTransportWithOriginalLogicalAudience() async throws {
    let configuration = URLSessionConfiguration.ephemeral
    configuration.protocolClasses = [RenewalProtocol.self]
    let transport = URLSession(configuration: configuration)
    defer { transport.invalidateAndCancel() }
    let identity = DeviceIdentity(key: P256.Signing.PrivateKey())
    let old = try EnrollmentClient(idpURL: URL(string: "https://idp.example.test")!,
        apiURL: URL(string: "https://original.example.test")!, transport: transport,
        enrollmentAudience: "https://original.example.test/enroll")
    let moved = try EnrollmentClient(idpURL: URL(string: "https://idp.example.test")!,
        apiURL: URL(string: "https://moved.example.test")!, transport: transport,
        enrollmentAudience: "https://original.example.test/enroll")
    let before = try await old.renew(identity: identity)
    let after = try await moved.renew(identity: identity)
    #expect(before.userID == after.userID)
    #expect(before.deviceID == identity.deviceID)
    #expect(after.deviceID == identity.deviceID)
}

private final class AppleEnrollmentProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() {}

    override func startLoading() {
        do {
            let path = request.url!.path
            let response: [String: Any]
            if path == "/enrollment/challenge" {
                response = ["nonce": "apple-nonce-vector", "audience": "https://" + request.url!.host! + "/enroll",
                            "issued_at": 10, "expires_at": 610]
            } else if path == "/enroll" {
                var data = request.httpBody ?? Data()
                if let stream = request.httpBodyStream {
                    stream.open()
                    defer { stream.close() }
                    var bytes = [UInt8](repeating: 0, count: 1024)
                    while stream.hasBytesAvailable {
                        let count = stream.read(&bytes, maxLength: bytes.count)
                        if count <= 0 { break }
                        data.append(contentsOf: bytes.prefix(count))
                    }
                }
                let body = try JSONSerialization.jsonObject(with: data) as! [String: String]
                #expect(Set(body.keys) == Set(["proof", "authorization_code"]))
                #expect(body["authorization_code"] == "synthetic-authorization-code")
                let proofParts = body["proof"]!.split(separator: ".")
                var encodedClaims = String(proofParts[1]).replacingOccurrences(of: "-", with: "+")
                    .replacingOccurrences(of: "_", with: "/")
                encodedClaims += String(repeating: "=", count: (4 - encodedClaims.count % 4) % 4)
                let claims = try JSONSerialization.jsonObject(with: Data(base64Encoded: encodedClaims)!) as! [String: Any]
                #expect(claims["nonce"] as? String == "apple-nonce-vector")
                #expect(claims["idp_token"] as? String == "synthetic-apple-identity-token")

                var encodedHeader = String(proofParts[0]).replacingOccurrences(of: "-", with: "+")
                    .replacingOccurrences(of: "_", with: "/")
                encodedHeader += String(repeating: "=", count: (4 - encodedHeader.count % 4) % 4)
                let header = try JSONSerialization.jsonObject(with: Data(base64Encoded: encodedHeader)!) as! [String: Any]
                let isReuse = request.url!.host == "reuse.apple.example.test"
                #expect(header["typ"] as? String == (isReuse ? "device-enrollment-reuse+jwt" : "device-enrollment+jwt"))
                if isReuse {
                    let reuse = claims["reuse"] as! [String: Any]
                    #expect(Set(reuse.keys) == Set(["expected_user_id", "require_existing_binding"]))
                    #expect(reuse["expected_user_id"] as? String == "user")
                    #expect(reuse["require_existing_binding"] as? Bool == true)
                } else {
                    #expect(claims["reuse"] == nil, "fresh clients must retain the legacy proof contract")
                }
                let jwk = header["jwk"] as! [String: String]
                let canonical = try JSONSerialization.data(withJSONObject: jwk,
                    options: [.sortedKeys, .withoutEscapingSlashes])
                response = ["access_token": "session", "expires_in": 900, "user_id": "user",
                            "device_id": Data(SHA256.hash(data: canonical)).base64URL]
            } else {
                Issue.record("Unexpected Apple enrollment request: \(path)")
                throw EnrollmentError.invalidResponse
            }
            client!.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: 200,
                httpVersion: nil, headerFields: nil)!, cacheStoragePolicy: .notAllowed)
            client!.urlProtocol(self, didLoad: try JSONSerialization.data(withJSONObject: response))
            client!.urlProtocolDidFinishLoading(self)
        } catch {
            client!.urlProtocol(self, didFailWithError: error)
        }
    }
}

private final class ConflictErrorProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() {}

    override func startLoading() {
        let detail: Any
        var statusCode = 409
        switch request.url!.host {
        case "apple-rejected.conflict.test":
            statusCode = 401
            detail = ["code": "apple_authorization_rejected", "message": "secret-provider-body"]
        case "apple-outage.conflict.test":
            statusCode = 503
            detail = ["code": "apple_provider_unavailable", "message": "secret-provider-body"]
        case "unknown-authorization.conflict.test":
            statusCode = 401
            detail = ["code": "future_authorization_code", "message": "secret-provider-body"]
        case "device-bound.conflict.test":
            detail = ["code": "device_bound_to_other_user",
                      "message": "Account deletion is still in progress; retry enrollment later"]
        case "reuse.conflict.test":
            detail = ["code": "device_reuse_rejected", "message": "Existing device binding cannot be reused"]
        case "deletion.conflict.test":
            detail = ["code": "account_deletion_in_progress",
                      "message": "Device already enrolled to another user; choose a different device profile"]
        case "unknown-code.conflict.test":
            detail = ["code": "future_conflict_code",
                      "message": "Device already enrolled to another user; choose a different device profile"]
        case "unknown.conflict.test":
            detail = ["code": "future_conflict_code", "message": "A new conflict condition"]
        case "legacy-device-bound.conflict.test":
            detail = "Device already enrolled to another user; choose a different device profile"
        case "legacy-unknown.conflict.test":
            detail = "The device already enrolled to another user"
        default:
            Issue.record("Unexpected conflict test host: \(request.url!.host ?? "nil")")
            return
        }

        do {
            let body = try JSONSerialization.data(withJSONObject: ["detail": detail])
            client!.urlProtocol(self, didReceive: HTTPURLResponse(url: request.url!, statusCode: statusCode,
                httpVersion: nil, headerFields: nil)!, cacheStoragePolicy: .notAllowed)
            client!.urlProtocol(self, didLoad: body)
            client!.urlProtocolDidFinishLoading(self)
        } catch {
            client!.urlProtocol(self, didFailWithError: error)
        }
    }
}

@Test func clientRenewsWithoutRequestingAnIdentityTokenAndPreservesSessionKeys() async throws {
    let config = URLSessionConfiguration.ephemeral
    config.protocolClasses = [RenewalProtocol.self]
    let transport = URLSession(configuration: config)
    defer { transport.invalidateAndCancel() }
    let client = try EnrollmentClient(idpURL: URL(string: "https://idp.example.test")!,
        apiURL: URL(string: "https://enrollment.example.test")!, transport: transport)
    let identity = DeviceIdentity(key: P256.Signing.PrivateKey())
    let session = try await client.renew(identity: identity)
    #expect(session.deviceID == identity.deviceID)
    #expect(session.userID == "user")
    let json = try JSONSerialization.jsonObject(with: JSONEncoder().encode(session)) as! [String: Any]
    #expect(Set(json.keys) == Set(["access_token", "expires_in", "user_id", "device_id"]))
}

@Test func liveLocalEnrollmentAndRenewal() async throws {
    guard let base = ProcessInfo.processInfo.environment["ENROLLMENT_LIVE_URL"],
          let idp = ProcessInfo.processInfo.environment["IDP_LIVE_URL"] else { return }
    let client = try EnrollmentClient(idpURL: URL(string: idp)!, apiURL: URL(string: base)!)
    let identity = DeviceIdentity(key: P256.Signing.PrivateKey())
    let first = try await client.login(username: "alice", identity: identity)
    let renewed = try await client.renew(identity: identity)
    #expect(first.userID == renewed.userID)
    #expect(renewed.deviceID == identity.deviceID)
    #expect(first.accessToken != renewed.accessToken)
    #expect(try await client.counter(token: renewed.accessToken).userID == first.userID)
}


@Test(arguments: [(false, 600, true), (false, 900, true), (false, 901, false), (true, 120, true), (true, 121, false)])
func clientChecksChallengeLifetimeByPurpose(renewal: Bool, ttl: Int, accepted: Bool) async throws {
    let config = URLSessionConfiguration.ephemeral
    config.protocolClasses = [RenewalProtocol.self]
    let transport = URLSession(configuration: config)
    defer { transport.invalidateAndCancel() }
    let client = try EnrollmentClient(idpURL: URL(string: "https://idp.example.test")!,
        apiURL: URL(string: "https://\(ttl).example.test")!, transport: transport)
    let identity = DeviceIdentity(key: P256.Signing.PrivateKey())
    do {
        _ = try await (renewal ? client.renew(identity: identity) : client.enroll(idpToken: "synthetic", identity: identity))
        #expect(accepted)
    } catch EnrollmentError.invalidResponse {
        #expect(!accepted)
    }
}

@Test func appleChallengeUsesLowercaseHexSHA256NonceVector() async throws {
    let config = URLSessionConfiguration.ephemeral
    config.protocolClasses = [RenewalProtocol.self]
    let transport = URLSession(configuration: config)
    defer { transport.invalidateAndCancel() }
    let client = try EnrollmentClient(idpURL: URL(string: "https://idp.example.test")!,
        apiURL: URL(string: "https://example.test")!, transport: transport)
    let preparation = try await client.appleChallenge()
    #expect(preparation.challenge.nonce == "fresh")
    #expect(preparation.requestNonce == "d098ab5e44b9aabb755f76d806598f43573c662b35e4a2eab1e312ec9ad195e2")
}

@Test func appleCompletionSendsAuthorizationCodeAndProofWithRawNonce() async throws {
    let config = URLSessionConfiguration.ephemeral
    config.protocolClasses = [AppleEnrollmentProtocol.self]
    let transport = URLSession(configuration: config)
    defer { transport.invalidateAndCancel() }
    let client = try EnrollmentClient(idpURL: URL(string: "https://idp.example.test")!,
        apiURL: URL(string: "https://apple.example.test")!, transport: transport)
    let identity = DeviceIdentity(key: P256.Signing.PrivateKey())
    let preparation = try await client.appleChallenge()
    #expect(preparation.requestNonce == "45f9e344d4cb5de7ef7514e47a5f789d8dfb3a95e254a1467cf952a6c0a480ad")
    let session = try await client.completeAppleEnrollment(challenge: preparation.challenge,
        identityToken: "synthetic-apple-identity-token", authorizationCode: "synthetic-authorization-code",
        identity: identity)
    #expect(session.deviceID == identity.deviceID)
    #expect(session.userID == "user")
}

@Test func rejectsOverflowingChallengeLifetime() async throws {
    let config = URLSessionConfiguration.ephemeral
    config.protocolClasses = [RenewalProtocol.self]
    let transport = URLSession(configuration: config)
    defer { transport.invalidateAndCancel() }
    let client = try EnrollmentClient(idpURL: URL(string: "https://idp.example.test")!,
        apiURL: URL(string: "https://overflow.example.test")!, transport: transport)
    do {
        _ = try await client.appleChallenge()
        Issue.record("An overflowing challenge lifetime must be rejected.")
    } catch EnrollmentError.invalidResponse {}
}

@Test func enrollmentClientUsesMachineCodesFor409ConflictsAndBoundsUnknowns() async throws {
    let cases: [(host: String, expected: EnrollmentServerError)] = [
        ("apple-rejected.conflict.test", .appleAuthorizationRejected),
        ("apple-outage.conflict.test", .unavailable(statusCode: 503)),
        ("unknown-authorization.conflict.test", .httpStatus(401)),
        ("device-bound.conflict.test", .deviceBoundToOtherUser),
        ("deletion.conflict.test", .deletionInProgress),
        ("unknown-code.conflict.test", .httpStatus(409)),
        ("unknown.conflict.test", .httpStatus(409)),
        ("legacy-device-bound.conflict.test", .deviceBoundToOtherUser),
        ("legacy-unknown.conflict.test", .httpStatus(409))
    ]

    for testCase in cases {
        let config = URLSessionConfiguration.ephemeral
        config.protocolClasses = [ConflictErrorProtocol.self]
        let transport = URLSession(configuration: config)
        let client = try EnrollmentClient(idpURL: URL(string: "https://idp.example.test")!,
            apiURL: URL(string: "https://\(testCase.host)")!, transport: transport)
        do {
            _ = try await client.appleChallenge()
            Issue.record("A conflict response should be surfaced as a typed error.")
        } catch EnrollmentError.server(let error) {
            #expect(error == testCase.expected)
            #expect(!error.localizedDescription.contains("secret-provider-body"))
        }
        transport.invalidateAndCancel()
    }
}

@Test func appleReuseIntentTravelsInsideSignedProofWithoutUnsignedFlags() async throws {
    let configuration = URLSessionConfiguration.ephemeral
    configuration.protocolClasses = [AppleEnrollmentProtocol.self]
    let transport = URLSession(configuration: configuration)
    defer { transport.invalidateAndCancel() }
    let client = try EnrollmentClient(idpURL: URL(string: "https://idp.example.test")!,
        apiURL: URL(string: "https://reuse.apple.example.test")!, transport: transport)
    let preparation = try await client.appleChallenge()
    let identity = DeviceIdentity(key: P256.Signing.PrivateKey())
    let session = try await client.completeAppleEnrollment(challenge: preparation.challenge,
        identityToken: "synthetic-apple-identity-token", authorizationCode: "synthetic-authorization-code",
        identity: identity, reuse: EnrollmentReuseIntent(expectedUserID: "user"))
    #expect(session.userID == "user")
    #expect(session.deviceID == identity.deviceID)
}

@Test func reuseBindingRefusalIsTypedForSafeFreshKeyFallback() async throws {
    let configuration = URLSessionConfiguration.ephemeral
    configuration.protocolClasses = [ConflictErrorProtocol.self]
    let transport = URLSession(configuration: configuration)
    defer { transport.invalidateAndCancel() }
    let client = try EnrollmentClient(idpURL: URL(string: "https://idp.example.test")!,
        apiURL: URL(string: "https://reuse.conflict.test")!, transport: transport)
    let challenge = EnrollmentChallenge(nonce: "fixture", audience: "https://reuse.conflict.test/enroll",
                                        issuedAt: 10, expiresAt: 130)
    do {
        _ = try await client.completeAppleEnrollment(challenge: challenge, identityToken: "fixture-token",
            authorizationCode: "fixture-code", identity: DeviceIdentity(key: P256.Signing.PrivateKey()),
            reuse: EnrollmentReuseIntent(expectedUserID: "fixture-account"))
        Issue.record("expected binding refusal")
    } catch EnrollmentError.server(let error) {
        #expect(error == .deviceReuseRejected)
    }
}
