import Foundation

/// A deliberately small view of the authenticated account profile.
/// Unknown feature names are discarded; an unknown sync status is unavailable.
public enum RegisteredDevicePlatform: String, Codable, Sendable { case mac, ios }

/// Additional presentation metadata. Admission continues to use `features.sync`.
/// A missing or unrecognized state is unknown, never an indication to show a paywall.
public struct SyncAccess: Decodable, Equatable, Sendable {
    public enum Enforcement: String, Decodable, Sendable { case enforced, exempt }
    public enum State: String, Sendable {
        case serverTrial = "server_trial", storeTrial = "store_trial", subscribed, expired, none
    }
    public enum TrialOffer: String, Sendable { case server, store, none }

    public struct TrialOffers: Decodable, Equatable, Sendable {
        public let mac: TrialOffer?
        public let ios: TrialOffer?

        enum CodingKeys: String, CodingKey { case mac, ios }
        public init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            mac = (try? values.decode(String.self, forKey: .mac)).flatMap(TrialOffer.init(rawValue:))
            ios = (try? values.decode(String.self, forKey: .ios)).flatMap(TrialOffer.init(rawValue:))
        }
    }

    public let enforcement: Enforcement
    public let state: State?
    public let evaluatedAt: Date
    public let trialEligible: Bool?
    public let trialOffers: TrialOffers?
    public let trialStartedAt: Date?
    public let trialEndsAt: Date?
    public let accessEndsAt: Date?
    public let renewsAt: Date?
    public let willRenew: Bool?

    enum CodingKeys: String, CodingKey {
        case enforcement, state, evaluatedAt = "evaluated_at", trialEligible = "trial_eligible"
        case trialOffers = "trial_offers", trialStartedAt = "trial_started_at"
        case trialEndsAt = "trial_ends_at", accessEndsAt = "access_ends_at"
        case renewsAt = "renews_at", willRenew = "will_renew"
    }

    public init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        enforcement = try values.decode(Enforcement.self, forKey: .enforcement)
        if let rawState = try values.decodeIfPresent(String.self, forKey: .state) {
            guard let knownState = State(rawValue: rawState) else {
                throw DecodingError.dataCorruptedError(forKey: .state, in: values,
                                                       debugDescription: "Unknown sync access state")
            }
            state = knownState
        } else {
            state = nil
        }
        guard let evaluatedAt = try Self.decodeDate(values, .evaluatedAt) else {
            throw DecodingError.dataCorruptedError(forKey: .evaluatedAt, in: values,
                                                   debugDescription: "Missing UTC evaluation date")
        }
        self.evaluatedAt = evaluatedAt
        trialEligible = try values.decodeIfPresent(Bool.self, forKey: .trialEligible)
        trialOffers = try values.decodeIfPresent(TrialOffers.self, forKey: .trialOffers)
        trialStartedAt = try Self.decodeDate(values, .trialStartedAt)
        trialEndsAt = try Self.decodeDate(values, .trialEndsAt)
        accessEndsAt = try Self.decodeDate(values, .accessEndsAt)
        renewsAt = try Self.decodeDate(values, .renewsAt)
        willRenew = try values.decodeIfPresent(Bool.self, forKey: .willRenew)
    }

    private static func decodeDate(_ values: KeyedDecodingContainer<CodingKeys>,
                                   _ key: CodingKeys) throws -> Date? {
        guard let raw = try values.decodeIfPresent(String.self, forKey: key) else { return nil }
        let pattern = #"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d{1,6})?(Z|\+00:00)$"#
        guard raw.range(of: pattern, options: .regularExpression) != nil else {
            throw DecodingError.dataCorruptedError(forKey: key, in: values, debugDescription: "Invalid UTC date")
        }
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = raw.contains(".") ? [.withInternetDateTime, .withFractionalSeconds] : [.withInternetDateTime]
        guard let date = formatter.date(from: raw) else {
            throw DecodingError.dataCorruptedError(forKey: key, in: values, debugDescription: "Invalid UTC date")
        }
        return date
    }
}

public struct AccountProfile: Decodable, Sendable {
    public enum SyncAvailability: String, Sendable {
        case available
        case subscriptionRequired = "subscription_required"
        case unavailable
    }

    public struct Device: Decodable, Sendable {
        public let deviceID: String
        public let caller: Bool

        enum CodingKeys: String, CodingKey {
            case deviceID = "device_id"
            case caller
        }
    }

    public let userID: String
    public let identities: [String]
    public let devices: [Device]
    public let syncAvailability: SyncAvailability
    public let syncAccess: SyncAccess?
    public let experimentalMCPAvailable: Bool
    public let platforms: Set<RegisteredDevicePlatform>
    public let platformsComplete: Bool

    enum CodingKeys: String, CodingKey {
        case userID = "user_id"
        case identities
        case devices
        case features
        case syncAccess = "sync_access"
        case platforms
        case platformsComplete = "platforms_complete"
    }

    private enum FeatureKeys: String, CodingKey { case sync, experimental_mcp }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        userID = try container.decode(String.self, forKey: .userID)
        identities = try container.decode([String].self, forKey: .identities)
        devices = try container.decode([Device].self, forKey: .devices)
        let rawPlatforms = try? container.decode([String].self, forKey: .platforms)
        platforms = Set((rawPlatforms ?? []).compactMap(RegisteredDevicePlatform.init(rawValue:)))
        platformsComplete = (try? container.decode(Bool.self, forKey: .platformsComplete)) == true &&
            rawPlatforms != nil && !(rawPlatforms?.isEmpty ?? true) && !devices.isEmpty &&
            rawPlatforms!.allSatisfy { RegisteredDevicePlatform(rawValue: $0) != nil }
        let features = try container.nestedContainer(keyedBy: FeatureKeys.self, forKey: .features)
        let rawStatus = try features.decodeIfPresent(String.self, forKey: .sync)
        syncAvailability = rawStatus.flatMap(SyncAvailability.init(rawValue:)) ?? .unavailable
        syncAccess = try? container.decode(SyncAccess.self, forKey: .syncAccess)
        experimentalMCPAvailable = (try? features.decode(String.self, forKey: .experimental_mcp)) == "available"
    }
}

/// Errors expose only the bounded status or contract outcome, never response bodies or credentials.
public enum AccountClientError: Error, LocalizedError, Equatable, Sendable {
    case invalidOrigin
    case invalidResponse
    case unauthorized
    case deletionOutcomeUnknown
    case subscriptionRequired
    case serviceUnavailable
    case trialAlreadyUsed
    case storeTrialRequired
    case platformRegistrationRequired
    case accessAlreadyActive
    case trialOutcomeUnknown
    case deletionUnavailable
    case httpStatus(Int)

    public var errorDescription: String? {
        switch self {
        case .invalidOrigin: "The account API origin is invalid or insecure."
        case .invalidResponse: "The account API returned an invalid response."
        case .unauthorized: "The account session is invalid or expired."
        case .deletionOutcomeUnknown: "The deletion response was lost or the account is already inaccessible; sign out locally."
        case .subscriptionRequired: "The account does not have access to sync."
        case .serviceUnavailable: "The account service is temporarily unavailable."
        case .trialAlreadyUsed: "A sync trial has already been used for this identity."
        case .storeTrialRequired: "An App Store trial is available on this device."
        case .platformRegistrationRequired: "Register this device's platform before starting a trial."
        case .accessAlreadyActive: "Sync access is already active."
        case .trialOutcomeUnknown: "The trial request result is unknown; retry to read the original grant."
        case .deletionUnavailable: "Account deletion could not be accepted; retry later."
        case .httpStatus(let status): "The account API returned HTTP \(status)."
        }
    }
}

public enum AccountDeletionResult: Sendable, Equatable {
    /// The server committed a deletion tombstone and cleanup will continue asynchronously.
    case accepted
    /// The server finished deletion during this request.
    case completed
}

/// Client for account profile, trial start and self-service deletion endpoints.
/// `accountOrigin` is configured independently from the enrollment and sync origins.
public struct AccountClient: Sendable {
    public let accountOrigin: URL
    private let transport: URLSession

    public init(accountOrigin: URL, transport: URLSession = .shared,
                allowsInsecureHTTPForDevelopment: Bool = false) throws {
        do {
            try EnrollmentEndpointPolicy.validateOrigin(
                accountOrigin,
                allowsInsecureHTTPForDevelopment: allowsInsecureHTTPForDevelopment
            )
        } catch {
            throw AccountClientError.invalidOrigin
        }
        let encodedPath = URLComponents(url: accountOrigin, resolvingAgainstBaseURL: false)?.percentEncodedPath
        guard encodedPath == "" || encodedPath == "/",
              accountOrigin.query == nil, accountOrigin.fragment == nil,
              accountOrigin.user == nil, accountOrigin.password == nil else {
            throw AccountClientError.invalidOrigin
        }
        self.accountOrigin = accountOrigin
        self.transport = transport
    }

    /// Fetches the caller's profile. Unknown feature names are ignored by decoding only `sync`.
    public func profile(token: String) async throws -> AccountProfile {
        var request = URLRequest(url: endpoint("v1/account"))
        request.httpMethod = "GET"
        request.timeoutInterval = 15
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        request.setValue("no-store", forHTTPHeaderField: "Cache-Control")
        let (data, response) = try await transport.data(for: request, delegate: RedirectRefuser())
        let http = try checkedResponse(response)
        switch http.statusCode {
        case 200:
            do { return try JSONDecoder().decode(AccountProfile.self, from: data) }
            catch { throw AccountClientError.invalidResponse }
        case 401: throw AccountClientError.unauthorized
        case 402: throw AccountClientError.subscriptionRequired
        case 500..<600: throw AccountClientError.serviceUnavailable
        default: throw AccountClientError.httpStatus(http.statusCode)
        }
    }

    /// Explicitly starts the one server-owned trial. Retrying after a lost response
    /// returns the original dates; this request sends no identity or duration.
    public func startSyncTrial(token: String) async throws -> SyncAccess {
        var request = URLRequest(url: endpoint("v1/account/sync-trial"))
        request.httpMethod = "POST"
        request.timeoutInterval = 15
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        request.setValue("no-store", forHTTPHeaderField: "Cache-Control")
        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await transport.data(for: request, delegate: RedirectRefuser())
        } catch {
            throw AccountClientError.trialOutcomeUnknown
        }
        let http = try checkedResponse(response, path: "/v1/account/sync-trial")
        switch http.statusCode {
        case 200, 201:
            struct Receipt: Decodable { let syncAccess: SyncAccess
                enum CodingKeys: String, CodingKey { case syncAccess = "sync_access" }
            }
            guard let receipt = try? JSONDecoder().decode(Receipt.self, from: data),
                  receipt.syncAccess.enforcement == .enforced,
                  receipt.syncAccess.state == .serverTrial,
                  receipt.syncAccess.trialStartedAt != nil else {
                throw AccountClientError.invalidResponse
            }
            return receipt.syncAccess
        case 401: throw AccountClientError.unauthorized
        case 409:
            struct Failure: Decodable { let code: String }
            switch (try? JSONDecoder().decode(Failure.self, from: data))?.code {
            case "trial_already_used": throw AccountClientError.trialAlreadyUsed
            case "store_trial_required": throw AccountClientError.storeTrialRequired
            case "platform_registration_required": throw AccountClientError.platformRegistrationRequired
            case "access_already_active": throw AccountClientError.accessAlreadyActive
            default: throw AccountClientError.httpStatus(409)
            }
        case 500..<600: throw AccountClientError.serviceUnavailable
        default: throw AccountClientError.httpStatus(http.statusCode)
        }
    }

    /// Deletes the authenticated account. A 202 or 204 means deletion was accepted/completed.
    @discardableResult
    public func deleteAccount(token: String) async throws -> AccountDeletionResult {
        var request = URLRequest(url: endpoint("v1/account"))
        request.httpMethod = "DELETE"
        request.timeoutInterval = 15
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        let response: URLResponse
        do {
            (_, response) = try await transport.data(for: request, delegate: RedirectRefuser())
        } catch {
            throw AccountClientError.deletionOutcomeUnknown
        }
        let http = try checkedResponse(response)
        switch http.statusCode {
        case 202: return .accepted
        case 204: return .completed
        case 401: throw AccountClientError.deletionOutcomeUnknown
        case 503: throw AccountClientError.deletionUnavailable
        case 500..<600: throw AccountClientError.serviceUnavailable
        default: throw AccountClientError.httpStatus(http.statusCode)
        }
    }

    /// Presentation metadata only. An older server may return 404.
    public func registerCurrentDevicePlatform(token: String, platform: RegisteredDevicePlatform) async throws {
        var request = URLRequest(url: endpoint("v1/devices/current/platform"))
        request.httpMethod = "PUT"
        request.timeoutInterval = 3
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("no-store", forHTTPHeaderField: "Cache-Control")
        request.httpBody = try JSONEncoder().encode(["platform": platform.rawValue])
        let (_, response) = try await transport.data(for: request, delegate: RedirectRefuser())
        let http = try checkedResponse(response, path: "/v1/devices/current/platform")
        switch http.statusCode {
        case 204: return
        case 401: throw AccountClientError.unauthorized
        default: throw AccountClientError.httpStatus(http.statusCode)
        }
    }

    /// Unbinds only the device identified by this bearer. Callers should renew
    /// first: a 401 with an expired bearer does not prove the device was removed.
    public func unbindCurrentDevice(token: String) async throws {
        var request = URLRequest(url: endpoint("v1/devices/current"))
        request.httpMethod = "DELETE"
        request.timeoutInterval = 15
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        let (_, response) = try await transport.data(for: request, delegate: RedirectRefuser())
        let http = try checkedResponse(response, path: "/v1/devices/current")
        switch http.statusCode {
        case 204: return
        case 401: throw AccountClientError.unauthorized
        case 500..<600: throw AccountClientError.serviceUnavailable
        default: throw AccountClientError.httpStatus(http.statusCode)
        }
    }

    private func endpoint(_ path: String) -> URL {
        var url = accountOrigin
        if url.path == "/" { url.deleteLastPathComponent() }
        for component in path.split(separator: "/") {
            url.appendPathComponent(String(component))
        }
        return url
    }

    private func checkedResponse(_ response: URLResponse, path: String = "/v1/account") throws -> HTTPURLResponse {
        guard let response = response as? HTTPURLResponse,
              response.url?.host?.caseInsensitiveCompare(accountOrigin.host ?? "") == .orderedSame,
              response.url?.scheme?.lowercased() == accountOrigin.scheme?.lowercased(),
              response.url?.port == accountOrigin.port,
              response.url?.path == path,
              response.url?.user == nil, response.url?.password == nil,
              response.url?.query == nil, response.url?.fragment == nil else {
            throw AccountClientError.invalidResponse
        }
        return response
    }
}

private final class RedirectRefuser: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    func urlSession(_ session: URLSession, task: URLSessionTask,
                    willPerformHTTPRedirection response: HTTPURLResponse,
                    newRequest request: URLRequest,
                    completionHandler: @escaping (URLRequest?) -> Void) {
        completionHandler(nil)
    }
}

extension AccountClient {
    public func mcpTokens(token: String) async throws -> [MCPTokenMetadata] {
        struct List: Decodable { let tokens: [MCPTokenMetadata] }
        let data = try await accountRequest(token: token, method: "GET", path: "/v1/account/mcp-tokens", success: 200)
        do { return try JSONDecoder().decode(List.self, from: data).tokens }
        catch { throw AccountClientError.invalidResponse }
    }

    public func createMCPToken(token: String, clientName: String, expiresInDays: Int = 30) async throws -> MCPTokenReceipt {
        guard !clientName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              clientName.unicodeScalars.count <= 80,
              !clientName.unicodeScalars.contains(where: { $0.value < 32 || $0.value == 127 }),
              (1...90).contains(expiresInDays) else { throw AccountClientError.invalidResponse }
        struct Body: Encodable { let client_name: String; let expires_in_days: Int }
        let body = try JSONEncoder().encode(Body(client_name: clientName, expires_in_days: expiresInDays))
        let data = try await accountRequest(token: token, method: "POST", path: "/v1/account/mcp-tokens", body: body, success: 201)
        do { return try JSONDecoder().decode(MCPTokenReceipt.self, from: data) }
        catch { throw AccountClientError.invalidResponse }
    }

    public func revokeMCPToken(token: String, id: String) async throws {
        guard id.count == 36, UUID(uuidString: id) != nil else { throw AccountClientError.invalidResponse }
        _ = try await accountRequest(token: token, method: "DELETE", path: "/v1/account/mcp-tokens/\(id)", success: 204)
    }

    func accountRequest(token: String, method: String, path: String, body: Data? = nil, success: Int) async throws -> Data {
        var request = URLRequest(url: endpoint(path))
        request.httpMethod = method
        request.timeoutInterval = 15
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        request.setValue("no-store", forHTTPHeaderField: "Cache-Control")
        if let body {
            request.httpBody = body
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        let (data, response) = try await transport.data(for: request, delegate: RedirectRefuser())
        let http = try checkedResponse(response, path: path)
        switch http.statusCode {
        case success: return data
        case 401: throw AccountClientError.unauthorized
        case 500..<600: throw AccountClientError.serviceUnavailable
        default: throw AccountClientError.httpStatus(http.statusCode)
        }
    }
}
