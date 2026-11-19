import CryptoKit
import DeviceEnrollment
import Foundation
import Security

enum ProfileFailure: String, Error {
    case invalidRequest = "invalid_request"
    case notSignedIn = "not_signed_in"
    case profileChanged = "profile_changed"
    case profileInUse = "profile_in_use"
    case accountMismatch = "account_mismatch"
    case callbackUnregistered = "callback_unregistered"
    case presentationUnavailable = "presentation_unavailable"
    case cancelled
    case callbackRejected = "callback_rejected"
    case keychainUnavailable = "keychain_unavailable"
    case deviceRevoked = "device_revoked"
    case unavailable
    case rejected

    static func classify(_ error: Error) -> Self {
        switch error {
        case let failure as Self: return failure
        case BrowserLoginError.cancelled, BrowserLoginError.authorizationDenied: return .cancelled
        case BrowserLoginError.stateMismatch, BrowserLoginError.callbackMismatch,
             BrowserLoginError.invalidCallback: return .callbackRejected
        case EnrollmentError.keychain: return .keychainUnavailable
        case EnrollmentError.server(.deviceUnknown): return .deviceRevoked
        case EnrollmentError.server(.unavailable), BrowserLoginError.serviceUnavailable,
             is URLError: return .unavailable
        default: return .rejected
        }
    }
}

struct ProfileConfiguration: Sendable {
    static let bundleID = "com.waterfallelephant.Weirnote.PythonAccount"

    let origin: URL
    let profile: String
    let bundleID: String
    let allowsInsecureHTTP: Bool

    init(origin: String, profile: String, bundleID: String, allowsInsecureHTTP: Bool) throws {
        guard var components = URLComponents(string: origin),
              let scheme = components.scheme?.lowercased(),
              let host = components.percentEncodedHost?.lowercased(), !host.isEmpty,
              components.port.map({ (1...65535).contains($0) }) != false,
              profile.range(of: #"^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$"#, options: .regularExpression) != nil,
              bundleID == Self.bundleID || bundleID.hasPrefix(Self.bundleID + ".")
        else { throw ProfileFailure.invalidRequest }
        components.scheme = scheme
        components.percentEncodedHost = host
        if components.path == "/" { components.path = "" }
        if (scheme == "https" && components.port == 443) || (scheme == "http" && components.port == 80) {
            components.port = nil
        }
        guard let url = components.url else { throw ProfileFailure.invalidRequest }
        do {
            try EnrollmentEndpointPolicy.validateOrigin(url, allowsInsecureHTTPForDevelopment: allowsInsecureHTTP)
        } catch {
            throw ProfileFailure.invalidRequest
        }
        self.origin = url
        self.profile = profile
        self.bundleID = bundleID
        self.allowsInsecureHTTP = allowsInsecureHTTP
    }

    var redirect: URL { URL(string: "\(bundleID.lowercased()).login:/oauth/callback")! }

    var storageKey: String {
        let digest = SHA256.hash(data: Data("\(origin.absoluteString)\n\(profile)".utf8))
        return digest.map { String(format: "%02x", $0) }.joined()
    }
}

protocol ProfileStore: Sendable {
    func read(_ account: String) throws -> Data?
    func insert(_ data: Data, account: String) throws -> Bool
    func delete(_ account: String) throws
}

extension KeychainStore: ProfileStore {}

struct Profile: Sendable {
    let configuration: ProfileConfiguration
    private let store: any ProfileStore
    private let enrollment: EnrollmentClient
    private let browser: BrowserLoginClient

    struct Identity: Encodable {
        let userID: String
        let deviceID: String

        enum CodingKeys: String, CodingKey {
            case userID = "user_id"
            case deviceID = "device_id"
        }
    }

    private struct Binding: Codable {
        let key: Data
        let userID: String

        func deviceIdentity() throws -> DeviceIdentity {
            try DeviceIdentity(key: P256.Signing.PrivateKey(rawRepresentation: key))
        }
    }

    init(configuration: ProfileConfiguration, store: any ProfileStore,
         transport: URLSession, enrollmentAudience: String? = nil) throws {
        self.configuration = configuration
        self.store = store
        let origin = configuration.origin
        if let enrollmentAudience {
            guard URLComponents(string: enrollmentAudience)?.scheme == "https" else {
                throw ProfileFailure.invalidRequest
            }
        }
        enrollment = try EnrollmentClient(idpURL: origin, apiURL: origin, transport: transport,
                                          enrollmentAudience: enrollmentAudience,
                                          allowsInsecureHTTPForDevelopment: configuration.allowsInsecureHTTP)
        browser = try BrowserLoginClient(configuration: BrowserLoginConfiguration(
            idpURL: origin, clientID: configuration.bundleID,
            redirectURI: configuration.redirect, flow: .appleWeb), session: transport)
    }

    func signIn(present: @MainActor (URL, URL) async throws -> URL) async throws -> APISession {
        if let bytes = try store.read(configuration.storageKey) {
            return try await renew(bytes)
        }
        let preparation = try await enrollment.appleChallenge()
        let start = try await browser.beginAuthorization(nonce: preparation.requestNonce)
        let callback: URL
        do {
            callback = try await present(start.authorizationURL, configuration.redirect)
        } catch {
            await start.transaction.cancel()
            throw error
        }
        let credential = try await start.transaction.exchangeAppleCredential(callbackURL: callback)
        let key = P256.Signing.PrivateKey()
        let session = try await enrollment.completeAppleEnrollment(
            challenge: preparation.challenge, identityToken: credential.identityToken,
            authorizationCode: credential.authorizationCode, identity: DeviceIdentity(key: key))
        try Self.validate(session)
        try remember(key: key, userID: session.userID)
        return session
    }

    private func remember(key: P256.Signing.PrivateKey, userID: String) throws {
        let binding = Binding(key: key.rawRepresentation, userID: userID)
        guard try store.insert(JSONEncoder().encode(binding), account: configuration.storageKey) else {
            throw ProfileFailure.profileInUse
        }
    }

    #if DEBUG
    func seedFixture() throws {
        try remember(key: P256.Signing.PrivateKey(), userID: "fixture-user")
    }
    #endif

    func identity() throws -> Identity? {
        guard let bytes = try store.read(configuration.storageKey) else { return nil }
        let binding = try JSONDecoder().decode(Binding.self, from: bytes)
        guard !binding.userID.isEmpty else { throw ProfileFailure.rejected }
        return Identity(userID: binding.userID, deviceID: try binding.deviceIdentity().deviceID)
    }

    func renew() async throws -> APISession {
        guard let bytes = try store.read(configuration.storageKey) else {
            throw ProfileFailure.notSignedIn
        }
        return try await renew(bytes)
    }

    private func renew(_ bytes: Data) async throws -> APISession {
        let binding = try JSONDecoder().decode(Binding.self, from: bytes)
        let session = try await enrollment.renew(identity: binding.deviceIdentity())
        try Self.validate(session)
        guard session.userID == binding.userID else { throw ProfileFailure.accountMismatch }
        guard try store.read(configuration.storageKey) == bytes else { throw ProfileFailure.profileChanged }
        return session
    }

    func signOut() throws {
        try store.delete(configuration.storageKey)
    }

    private static func validate(_ session: APISession) throws {
        guard !session.accessToken.isEmpty, !session.userID.isEmpty, !session.deviceID.isEmpty,
              session.expiresIn > 0 else { throw EnrollmentError.invalidResponse }
    }
}
