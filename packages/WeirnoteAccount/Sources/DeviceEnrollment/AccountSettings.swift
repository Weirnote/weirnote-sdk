import Foundation

/// Unknown object members are retained when callers change a known field.
public indirect enum SettingsJSON: Codable, Equatable, Sendable {
    case object([String: SettingsJSON]), array([SettingsJSON]), string(String), number(Decimal), bool(Bool), null
    public init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let v = try? c.decode(Bool.self) { self = .bool(v) }
        else if let v = try? c.decode(String.self) { self = .string(v) }
        else if let v = try? c.decode([String: SettingsJSON].self) { self = .object(v) }
        else if let v = try? c.decode([SettingsJSON].self) { self = .array(v) }
        else { self = .number(try c.decode(Decimal.self)) }
    }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .object(let v): try c.encode(v)
        case .array(let v): try c.encode(v)
        case .string(let v): try c.encode(v)
        case .number(let v):
            guard !v.isNaN, AccountSettingsNumbers.supports(NSDecimalNumber(decimal: v).stringValue) else {
                throw AccountClientError.invalidResponse
            }
            try c.encode(v)
        case .bool(let v): try c.encode(v)
        case .null: try c.encodeNil()
        }
    }
}

public struct AccountSettingsEdit: Codable, Equatable, Sendable {
    public let version: Int
    public let editedAtMS: Int64
    public let editID: String
    public let settings: [String: SettingsJSON]
    enum CodingKeys: String, CodingKey { case version, editedAtMS = "edited_at_ms", editID = "edit_id", settings }
    public init(editedAtMS: Int64, editID: String = UUID().uuidString.lowercased(), settings: [String: SettingsJSON]) {
        version = 1; self.editedAtMS = editedAtMS; self.editID = editID; self.settings = settings
    }
    public func validate() throws {
        guard version == 1, (0...253402300799999).contains(editedAtMS), editID.count == 36,
              editID == editID.lowercased(), UUID(uuidString: editID) != nil else { throw AccountClientError.invalidResponse }
    }
    public func isNewer(than other: Self) -> Bool {
        editedAtMS != other.editedAtMS ? editedAtMS > other.editedAtMS : editID > other.editID
    }
}

public struct AccountSettingsEnvelope: Codable, Sendable {
    public let version: Int
    public let edit: AccountSettingsEdit?
    public init(edit: AccountSettingsEdit?) { version = 1; self.edit = edit }
    public func validate() throws {
        guard version == 1 else { throw AccountClientError.invalidResponse }
        try edit?.validate()
    }
}

extension AccountClient {
    public func settings(token: String, edit: AccountSettingsEdit? = nil) async throws -> AccountSettingsEnvelope {
        try edit?.validate()
        let body = try edit.map { try JSONEncoder().encode($0) }
        guard (body?.count ?? 0) <= 65536 else { throw AccountClientError.invalidResponse }
        let data = try await accountRequest(token: token, method: edit == nil ? "GET" : "PUT",
                                        path: "/v1/account/settings", body: body, success: 200)
        guard data.count <= 524288 else { throw AccountClientError.invalidResponse }
        do {
            try AccountSettingsNumbers.validateJSON(data)
            let result = try JSONDecoder().decode(AccountSettingsEnvelope.self, from: data)
            try result.validate()
            return result
        } catch { throw AccountClientError.invalidResponse }
    }
}

/// Portable exact subset of Foundation Decimal: up to 38 significant digits,
/// normalized base-ten exponent -128...127. Reject before Decimal can round.
public enum AccountSettingsNumbers {
    public static func supports(_ text: String) -> Bool {
        let parts = text.lowercased().split(separator: "e", omittingEmptySubsequences: false)
        guard parts.count <= 2 else { return false }
        let mantissa = parts[0].hasPrefix("-") ? parts[0].dropFirst() : parts[0][...]
        let pieces = mantissa.split(separator: ".", omittingEmptySubsequences: false)
        guard pieces.count <= 2 else { return false }
        var digits = pieces.joined()
        guard !digits.isEmpty, digits.allSatisfy({ $0 >= "0" && $0 <= "9" }) else { return false }
        while digits.first == "0" { digits.removeFirst() }
        if digits.isEmpty { return true }
        guard let power = parts.count == 2 ? Int(parts[1]) : 0 else { return false }
        let fraction = pieces.count == 2 ? pieces[1].count : 0
        let (initialExponent, overflow) = power.subtractingReportingOverflow(fraction)
        guard !overflow else { return false }
        var exponent = initialExponent
        while digits.last == "0" {
            digits.removeLast()
            guard exponent < Int.max else { return false }
            exponent += 1
        }
        return digits.count <= 38 && (-128...127).contains(exponent)
    }

    public static func validateJSON(_ data: Data) throws {
        let bytes = Array(data)
        var i = 0
        while i < bytes.count {
            if bytes[i] == 34 {
                i += 1
                while i < bytes.count {
                    if bytes[i] == 92 { i += 2; continue }
                    if bytes[i] == 34 { i += 1; break }
                    i += 1
                }
            } else if bytes[i] == 45 || (48...57).contains(bytes[i]) {
                let start = i
                while i < bytes.count, [45,43,46,69,101].contains(bytes[i]) || (48...57).contains(bytes[i]) { i += 1 }
                guard supports(String(decoding: bytes[start..<i], as: UTF8.self)) else {
                    throw AccountClientError.invalidResponse
                }
            } else { i += 1 }
        }
    }
}
