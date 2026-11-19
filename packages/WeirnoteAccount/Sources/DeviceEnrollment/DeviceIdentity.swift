import CryptoKit
import Foundation
import Security

public enum EnrollmentServerError: Error, Equatable, Sendable, LocalizedError {
    case challengeExpiredOrUsed
    case invalidProof
    case appleAuthorizationRejected
    case deviceUnknown
    case accountDeleting
    case deviceBoundToOtherUser
    case deviceReuseRejected
    case deletionInProgress
    case unavailable(statusCode: Int)
    case unauthorized
    case httpStatus(Int)

    static func map(statusCode: Int, detail: String) -> Self {
        let normalized = detail.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        if statusCode == 503 { return .unavailable(statusCode: statusCode) }
        if statusCode == 401 {
            if normalized.contains("challenge expired or already used") { return .challengeExpiredOrUsed }
            if normalized == "device_unknown" || normalized == "unknown device binding" { return .deviceUnknown }
            if normalized == "account_deleting" { return .accountDeleting }
            if normalized == "invalid_proof" || normalized.contains("invalid device proof") { return .invalidProof }
            if normalized.contains("invalid or expired api token") { return .unauthorized }
        }
        if statusCode == 409 {
            if normalized.contains("device already enrolled to another user") { return .deviceBoundToOtherUser }
            if normalized.contains("account deletion is still in progress") { return .deletionInProgress }
        }
        if (500..<600).contains(statusCode) { return .unavailable(statusCode: statusCode) }
        return .httpStatus(statusCode)
    }

    public var errorDescription: String? {
        switch self {
        case .challengeExpiredOrUsed: return "The enrollment challenge expired or was already used."
        case .invalidProof: return "The server rejected the device or identity proof."
        case .appleAuthorizationRejected: return "Apple authorization expired or was rejected. Please sign in again."
        case .deviceUnknown: return "This device is not enrolled or its binding is unavailable."
        case .accountDeleting: return "This account is being deleted."
        case .deviceBoundToOtherUser: return "This device is already enrolled to another account."
        case .deviceReuseRejected: return "The existing device binding cannot be reused."
        case .deletionInProgress: return "Account deletion is still in progress."
        case .unavailable: return "The enrollment service is unavailable."
        case .unauthorized: return "The account session was rejected."
        case .httpStatus(let status): return "The server returned HTTP \(status)."
        }
    }
}

public enum EnrollmentError: Error, LocalizedError {
    case keychain(OSStatus)
    case invalidResponse
    case http(Int, String)
    case server(EnrollmentServerError)
    case notLoggedIn
    case invalidURL

    public var errorDescription: String? {
        switch self {
        case .keychain(let status): return "Keychain: \(SecCopyErrorMessageString(status, nil) as String? ?? String(status))"
        case .invalidResponse: return "The server returned an invalid response."
        case .http(let code, _): return "HTTP \(code)."
        case .server(let error): return error.localizedDescription
        case .notLoggedIn: return "No API token for this device/server. Run login first."
        case .invalidURL: return "Expected an absolute HTTP(S) server URL without credentials, query, or fragment."
        }
    }
}

extension Data {
    public var base64URL: String {
        base64EncodedString().replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: "=", with: "")
    }
}

/// A device-signed requirement, not a request-body flag. Its distinct proof
/// type makes older servers reject reuse instead of treating it as new enrollment.
public struct EnrollmentReuseIntent: Equatable, Sendable {
    public let expectedUserID: String

    public init(expectedUserID: String) { self.expectedUserID = expectedUserID }
}

/// Stores only this simulator's items; never enumerates or changes unrelated Keychain data.
public struct KeychainStore: Sendable {
    private let service: String
    private let label: String?
    #if os(macOS)
    private let filePath: String?
    #endif

    public init(service: String = "com.waterfallelephant.dummy-device", label: String? = nil) {
        self.service = service
        self.label = label
        #if os(macOS)
        self.filePath = nil
        #endif
    }

    #if os(macOS)
    public init(service: String, label: String, filePath: String) {
        self.service = service
        self.label = label
        self.filePath = filePath
    }
    #endif

    public func delete(_ account: String) throws {
        let status = SecItemDelete(try query(account) as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw EnrollmentError.keychain(status) }
    }

    private func query(_ account: String, adding: Bool = false) throws -> [String: Any] {
        var query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service, kSecAttrAccount as String: account,
            kSecAttrSynchronizable as String: false,
        ]
        #if os(macOS)
        if let filePath {
            var opened: SecKeychain?
            let status = SecKeychainOpen(filePath, &opened)
            guard status == errSecSuccess, let keychain = opened else {
                throw EnrollmentError.keychain(status)
            }
            if adding {
                query[kSecUseKeychain as String] = keychain
            } else {
                query[kSecMatchSearchList as String] = [keychain]
            }
        }
        #endif
        return query
    }

    public func read(_ account: String) throws -> Data? {
        var query = try query(account)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess else { throw EnrollmentError.keychain(status) }
        guard let data = result as? Data else { throw EnrollmentError.invalidResponse }
        return data
    }

    /// Returns false on a concurrent insert so callers can reload the winning identity.
    public func insert(_ data: Data, account: String) throws -> Bool {
        var query = try query(account, adding: true)
        query[kSecValueData as String] = data
        query[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        if let label { query[kSecAttrLabel as String] = label }
        let status = SecItemAdd(query as CFDictionary, nil)
        if status == errSecDuplicateItem { return false }
        guard status == errSecSuccess else { throw EnrollmentError.keychain(status) }
        return true
    }

    public func save(_ data: Data, account: String) throws {
        if try insert(data, account: account) { return }
        let status = SecItemUpdate(try query(account) as CFDictionary, [kSecValueData as String: data] as CFDictionary)
        guard status == errSecSuccess else { throw EnrollmentError.keychain(status) }
    }
}

/// Portable Apple implementation: software P-256 key, persisted in Keychain.
/// This intentionally does not claim Secure Enclave or device attestation guarantees.
public struct DeviceIdentity: Sendable {
    private let key: P256.Signing.PrivateKey

    public init(key: P256.Signing.PrivateKey) { self.key = key }

    public static func loadOrCreate(profile: String, store: KeychainStore = KeychainStore()) throws -> Self {
        let account = "identity:\(profile)"
        if let stored = try store.read(account) {
            return try Self(key: P256.Signing.PrivateKey(rawRepresentation: stored))
        }
        let key = P256.Signing.PrivateKey()
        if try store.insert(key.rawRepresentation, account: account) { return Self(key: key) }
        guard let stored = try store.read(account) else { throw EnrollmentError.invalidResponse }
        return try Self(key: P256.Signing.PrivateKey(rawRepresentation: stored))
    }

    public var publicJWK: [String: String] {
        let bytes = key.publicKey.x963Representation
        return ["kty": "EC", "crv": "P-256", "x": Data(bytes[1..<33]).base64URL,
                "y": Data(bytes[33..<65]).base64URL]
    }

    /// RFC 7638 thumbprint over exactly crv, kty, x, y in lexical order.
    public var deviceID: String {
        let jwk = publicJWK
        let canonical = "{\"crv\":\"P-256\",\"kty\":\"EC\",\"x\":\"\(jwk["x"]!)\",\"y\":\"\(jwk["y"]!)\"}"
        return Data(SHA256.hash(data: Data(canonical.utf8))).base64URL
    }

    public func enrollmentProof(idpToken: String, challenge: EnrollmentChallenge,
                                reuse: EnrollmentReuseIntent? = nil) throws -> String {
        var claims: [String: Any] = ["idp_token": idpToken, "nonce": challenge.nonce,
            "aud": challenge.audience, "iat": challenge.issuedAt, "exp": challenge.expiresAt]
        if let reuse {
            guard !reuse.expectedUserID.isEmpty, reuse.expectedUserID.utf8.count <= 256,
                  !reuse.expectedUserID.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains) else {
                throw EnrollmentError.invalidResponse
            }
            claims["reuse"] = ["expected_user_id": reuse.expectedUserID, "require_existing_binding": true]
        }
        return try signedProof(type: reuse == nil ? "device-enrollment+jwt" : "device-enrollment-reuse+jwt", claims: claims)
    }

    public func renewalProof(challenge: EnrollmentChallenge) throws -> String {
        try signedProof(type: "device-renewal+jwt", claims: ["nonce": challenge.nonce,
            "aud": challenge.audience, "iat": challenge.issuedAt, "exp": challenge.expiresAt])
    }

    private func signedProof(type: String, claims: [String: Any]) throws -> String {
        let header: [String: Any] = ["alg": "ES256", "typ": type, "jwk": publicJWK]
        let input = try JSONSerialization.data(withJSONObject: header, options: [.sortedKeys]).base64URL
            + "." + JSONSerialization.data(withJSONObject: claims, options: [.sortedKeys]).base64URL
        // JWS uses fixed-width r||s (64 bytes), not ASN.1 DER ECDSA signatures.
        let signature = try key.signature(for: Data(input.utf8)).rawRepresentation.base64URL
        return input + "." + signature
    }
}
