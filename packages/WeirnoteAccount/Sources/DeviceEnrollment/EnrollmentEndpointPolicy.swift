import Foundation

/// Validates a credential-bearing HTTP origin. Insecure HTTP is limited to
/// explicitly enabled development endpoints on loopback or Tailscale DNS.
public enum EnrollmentEndpointPolicy {
    public static func validateOrigin(_ url: URL, allowsInsecureHTTPForDevelopment: Bool = false) throws {
        guard let scheme = url.scheme?.lowercased(), ["https", "http"].contains(scheme),
              let host = url.host?.lowercased(), !host.isEmpty,
              url.user == nil, url.password == nil, url.query == nil, url.fragment == nil,
              url.path.isEmpty || url.path == "/" else { throw EnrollmentError.invalidURL }
        guard scheme == "http" else { return }
        guard allowsInsecureHTTPForDevelopment, (isLoopback(host) || host.hasSuffix(".ts.net")) else {
            throw EnrollmentError.invalidURL
        }
    }

    private static func isLoopback(_ host: String) -> Bool {
        if host == "localhost" || host == "::1" || host == "[::1]" { return true }
        let octets = host.split(separator: ".")
        guard octets.count == 4, octets[0] == "127" else { return false }
        return octets.allSatisfy { part in
            guard let value = UInt8(part) else { return false }
            return String(value) == part
        }
    }
}
