import CryptoKit
import Foundation
import Testing
@testable import DeviceEnrollment

private func decode(_ string: Substring) -> Data {
    var value = String(string).replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
    value += String(repeating: "=", count: (4 - value.count % 4) % 4)
    return Data(base64Encoded: value)!
}

@Test func proofUsesJWSRawSignatureAndPublicKey() throws {
    let key = P256.Signing.PrivateKey()
    let device = DeviceIdentity(key: key)
    let challenge = EnrollmentChallenge(nonce: "one-use", audience: "https://example.test/enroll", issuedAt: 10, expiresAt: 130)
    let proof = try device.enrollmentProof(idpToken: "idp.jwt.signature", challenge: challenge)
    let parts = proof.split(separator: ".")
    #expect(parts.count == 3)
    let header = try JSONSerialization.jsonObject(with: decode(parts[0])) as! [String: Any]
    #expect(header["alg"] as? String == "ES256")
    #expect(header["typ"] as? String == "device-enrollment+jwt")
    let jwk = header["jwk"] as! [String: String]
    #expect(Set(jwk.keys) == Set(["kty", "crv", "x", "y"]))
    let publicBytes = Data([4]) + decode(Substring(jwk["x"]!)) + decode(Substring(jwk["y"]!))
    let publicKey = try P256.Signing.PublicKey(x963Representation: publicBytes)
    let signature = try P256.Signing.ECDSASignature(rawRepresentation: decode(parts[2]))
    #expect(signature.rawRepresentation.count == 64)
    #expect(publicKey.isValidSignature(signature, for: Data("\(parts[0]).\(parts[1])".utf8)))
    #expect(!publicKey.isValidSignature(signature, for: Data("tampered".utf8)))
    let claims = try JSONSerialization.jsonObject(with: decode(parts[1])) as! [String: Any]
    #expect(claims["idp_token"] as? String == "idp.jwt.signature")
    #expect(claims["nonce"] as? String == "one-use")
    #expect(claims["exp"] as? Int == 130)
    let canonical = try JSONSerialization.data(withJSONObject: jwk, options: [.sortedKeys, .withoutEscapingSlashes])
    #expect(device.deviceID == Data(SHA256.hash(data: canonical)).base64URL)
    let restored = try DeviceIdentity(key: P256.Signing.PrivateKey(rawRepresentation: key.rawRepresentation))
    #expect(restored.deviceID == device.deviceID)
    #expect(DeviceIdentity(key: P256.Signing.PrivateKey()).deviceID != device.deviceID)
}

@Test func rejectsInvalidServerURLs() throws {
    #expect(throws: EnrollmentError.self) {
        try EnrollmentClient(idpURL: URL(string: "file:///tmp/token")!, apiURL: URL(string: "https://example.test")!)
    }
}

@Test func credentialOriginsRequireHTTPSOrExplicitIsolatedDevelopmentOptIn() throws {
    #expect(throws: EnrollmentError.self) {
        try EnrollmentEndpointPolicy.validateOrigin(URL(string: "http://127.0.0.1:8002")!)
    }
    try EnrollmentEndpointPolicy.validateOrigin(URL(string: "http://127.0.0.1:8002")!,
                                               allowsInsecureHTTPForDevelopment: true)
    try EnrollmentEndpointPolicy.validateOrigin(URL(string: "http://[::1]:8002")!,
                                               allowsInsecureHTTPForDevelopment: true)
    try EnrollmentEndpointPolicy.validateOrigin(URL(string: "http://dev.example.ts.net:8002")!,
                                               allowsInsecureHTTPForDevelopment: true)
    #expect(throws: EnrollmentError.self) {
        try EnrollmentEndpointPolicy.validateOrigin(URL(string: "http://example.com")!,
                                                    allowsInsecureHTTPForDevelopment: true)
    }
    try EnrollmentEndpointPolicy.validateOrigin(URL(string: "https://example.com")!)
}

@Test func serverErrorsAreTypedAndDescriptionsDoNotExposeServerBodies() {
    #expect(EnrollmentServerError.map(statusCode: 401, detail: "Challenge expired or already used") == .challengeExpiredOrUsed)
    #expect(EnrollmentServerError.map(statusCode: 401, detail: "invalid_proof") == .invalidProof)
    #expect(EnrollmentServerError.map(statusCode: 401, detail: "device_unknown") == .deviceUnknown)
    #expect(EnrollmentServerError.map(statusCode: 401, detail: "account_deleting") == .accountDeleting)
    #expect(EnrollmentServerError.map(statusCode: 409, detail: "Device already enrolled to another user; choose a different device profile") == .deviceBoundToOtherUser)
    #expect(EnrollmentServerError.map(statusCode: 409, detail: "Account deletion is still in progress; retry enrollment later") == .deletionInProgress)
    #expect(EnrollmentServerError.map(statusCode: 503, detail: "provider secret=should-not-appear") == .unavailable(statusCode: 503))
    #expect(EnrollmentError.http(503, "authorization_code=secret").localizedDescription == "HTTP 503.")
    #expect(EnrollmentError.server(.httpStatus(500)).localizedDescription == "The server returned HTTP 500.")
}

@Test func renewalProofHasDistinctTypeAudienceAndNoIdentityToken() throws {
    let key = P256.Signing.PrivateKey()
    let device = DeviceIdentity(key: key)
    let challenge = EnrollmentChallenge(nonce: "renew-once", audience: "https://example.test/session/renew", issuedAt: 10, expiresAt: 130)
    let proof = try device.renewalProof(challenge: challenge)
    let parts = proof.split(separator: ".")
    let header = try JSONSerialization.jsonObject(with: decode(parts[0])) as! [String: Any]
    let claims = try JSONSerialization.jsonObject(with: decode(parts[1])) as! [String: Any]
    #expect(header["typ"] as? String == "device-renewal+jwt")
    #expect(claims["aud"] as? String == challenge.audience)
    #expect(claims["idp_token"] == nil)
    #expect(key.publicKey.isValidSignature(try P256.Signing.ECDSASignature(rawRepresentation: decode(parts[2])), for: Data("\(parts[0]).\(parts[1])".utf8)))
}

@Test func reuseIntentAndProofTypeAreCoveredByDeviceSignature() throws {
    let key = P256.Signing.PrivateKey()
    let identity = DeviceIdentity(key: key)
    let challenge = EnrollmentChallenge(nonce: "reuse-once", audience: "https://example.test/enroll",
                                        issuedAt: 10, expiresAt: 130)
    let proof = try identity.enrollmentProof(idpToken: "fixture.identity.token", challenge: challenge,
                                            reuse: EnrollmentReuseIntent(expectedUserID: "fixture-account"))
    let parts = proof.split(separator: ".")
    var header = try JSONSerialization.jsonObject(with: decode(parts[0])) as! [String: Any]
    var claims = try JSONSerialization.jsonObject(with: decode(parts[1])) as! [String: Any]
    let reuse = claims["reuse"] as! [String: Any]
    #expect(header["typ"] as? String == "device-enrollment-reuse+jwt")
    #expect(reuse["expected_user_id"] as? String == "fixture-account")
    #expect(reuse["require_existing_binding"] as? Bool == true)
    let signature = try P256.Signing.ECDSASignature(rawRepresentation: decode(parts[2]))
    #expect(key.publicKey.isValidSignature(signature, for: Data("\(parts[0]).\(parts[1])".utf8)))
    header["typ"] = "device-enrollment+jwt"
    let downgraded = try JSONSerialization.data(withJSONObject: header, options: [.sortedKeys]).base64URL
        + "." + String(parts[1])
    #expect(!key.publicKey.isValidSignature(signature, for: Data(downgraded.utf8)))
    claims.removeValue(forKey: "reuse")
    let stripped = String(parts[0]) + "." + (try JSONSerialization.data(withJSONObject: claims, options: [.sortedKeys])).base64URL
    #expect(!key.publicKey.isValidSignature(signature, for: Data(stripped.utf8)))
    #expect(throws: EnrollmentError.self) {
        try identity.enrollmentProof(idpToken: "fixture.identity.token", challenge: challenge,
                                     reuse: EnrollmentReuseIntent(expectedUserID: ""))
    }
}
