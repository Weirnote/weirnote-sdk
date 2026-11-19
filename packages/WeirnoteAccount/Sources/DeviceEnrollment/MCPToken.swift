import Foundation

public struct MCPTokenMetadata: Decodable, Sendable, Identifiable, Equatable {
    public let id: String
    public let clientName: String
    public let issuedAt: String
    public let expiresAt: String
    public let revokedAt: String?
    enum CodingKeys: String, CodingKey {
        case id
        case clientName = "client_name"
        case issuedAt = "issued_at"
        case expiresAt = "expires_at"
        case revokedAt = "revoked_at"
    }
}

/// Deliberately Decodable only; raw credentials must never enter persistence.
public struct MCPTokenReceipt: Decodable, Sendable, CustomStringConvertible {
    public let metadata: MCPTokenMetadata
    public let token: String
    public var description: String { "MCPTokenReceipt(redacted)" }
    private enum CodingKeys: String, CodingKey { case token }
    public init(from decoder: Decoder) throws {
        metadata = try MCPTokenMetadata(from: decoder)
        token = try decoder.container(keyedBy: CodingKeys.self).decode(String.self, forKey: .token)
        guard token.hasPrefix("wnmcp_"), token.utf8.count == 49,
              token.dropFirst(6).utf8.allSatisfy({ (65...90).contains($0) || (97...122).contains($0) || (48...57).contains($0) || $0 == 45 || $0 == 95 }) else {
            throw AccountClientError.invalidResponse
        }
    }
}
