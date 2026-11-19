import AppKit
import Darwin
import DeviceEnrollment
import Foundation
import Security

@main
struct Helper {
    @MainActor
    static func main() {
        let arguments = Array(CommandLine.arguments.dropFirst())
        guard let command = arguments.first, (4...5).contains(arguments.count),
              let bundle = Bundle.main.bundleIdentifier else {
            fail(.invalidRequest)
        }
        let interactive = command == "sign-in"
        // This process uses the macOS login keychain. Disable its authorization
        // prompts before accessing a profile, including during browser sign-in.
        if command != "check-callback", command != "register-callback",
           SecKeychainSetUserInteractionAllowed(false) != errSecSuccess {
            fail(.keychainUnavailable)
        }
        if interactive { NSApplication.shared.setActivationPolicy(.accessory) }
        Task { @MainActor in
            do {
                let config = try ProfileConfiguration(origin: arguments[1], profile: arguments[2],
                                                      bundleID: bundle, allowsInsecureHTTP: arguments[3] == "loopback")
                let transport = URLSession(configuration: .ephemeral)
                let profile = try Profile(configuration: config, store: keychain(for: bundle), transport: transport,
                                          enrollmentAudience: arguments.count == 5 ? arguments[4] : nil)
                switch command {
                case "sign-in":
                    let lock = try lockProfile(config)
                    defer { close(lock) }
                    let presenter = BrowserPresenter()
                    try respond(await profile.signIn(present: presenter.present))
                case "renew":
                    try respond(await profile.renew())
                case "identity":
                    try respond(["identity": profile.identity()])
                case "sign-out":
                    let lock = try lockProfile(config)
                    defer { close(lock) }
                    try profile.signOut()
                    try respond(["result": "signed_out"])
                case "register-callback":
                    try BrowserPresenter.registerCallback(config.redirect)
                    try respond(["redirect": config.redirect.absoluteString])
                case "check-callback":
                    try BrowserPresenter.checkRegistration(config.redirect)
                    try respond(["redirect": config.redirect.absoluteString])
                #if DEBUG
                case "fixture-seed":
                    guard fixturePath(for: bundle) != nil else { throw ProfileFailure.invalidRequest }
                    try profile.seedFixture()
                    try respond(["result": "seeded"])
                #endif
                default:
                    throw ProfileFailure.invalidRequest
                }
                exit(EXIT_SUCCESS)
            } catch {
                fail(ProfileFailure.classify(error))
            }
        }
        if interactive { NSApplication.shared.run() }
        dispatchMain()
    }

    private static func keychain(for bundle: String) -> KeychainStore {
        let label = "Weirnote Python device key"
        #if DEBUG
        if let path = fixturePath(for: bundle) {
            return KeychainStore(service: bundle, label: label, filePath: path)
        }
        #endif
        return KeychainStore(service: bundle, label: label)
    }

    #if DEBUG
    private static func fixturePath(for bundle: String) -> String? {
        guard bundle.hasPrefix(ProfileConfiguration.bundleID + ".") else { return nil }
        return ProcessInfo.processInfo.environment["WEIRNOTE_ACCOUNT_FIXTURE_KEYCHAIN"]
    }
    #endif

    private static func lockProfile(_ configuration: ProfileConfiguration) throws -> Int32 {
        let file = "\(configuration.bundleID).\(configuration.storageKey).lock"
        let path = FileManager.default.temporaryDirectory.appendingPathComponent(file).path
        let descriptor = open(path, O_CREAT | O_CLOEXEC | O_NOFOLLOW | O_RDWR | O_EXLOCK | O_NONBLOCK,
                              S_IRUSR | S_IWUSR)
        guard descriptor >= 0 else {
            throw errno == EWOULDBLOCK ? ProfileFailure.profileInUse : .unavailable
        }
        return descriptor
    }

    private static func respond(_ value: some Encodable) throws {
        try FileHandle.standardOutput.write(contentsOf: JSONEncoder().encode(value))
    }

    private static func fail(_ failure: ProfileFailure) -> Never {
        try? respond(["error": failure.rawValue])
        exit(EXIT_FAILURE)
    }
}
