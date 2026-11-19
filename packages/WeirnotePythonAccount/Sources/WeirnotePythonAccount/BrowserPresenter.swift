import AppKit
import AuthenticationServices
import CoreServices

@MainActor
final class BrowserPresenter: NSObject, ASWebAuthenticationPresentationContextProviding {
    private var window: NSWindow?
    private var session: ASWebAuthenticationSession?
    private var continuation: CheckedContinuation<URL, any Error>?

    static func checkRegistration(_ redirect: URL) throws {
        let owner = NSWorkspace.shared.urlForApplication(toOpen: redirect)?.resolvingSymlinksInPath()
        let bundle = Bundle.main.bundleURL.resolvingSymlinksInPath()
        guard owner?.path == bundle.path else { throw ProfileFailure.callbackUnregistered }
    }

    static func registerCallback(_ redirect: URL) throws {
        let bundle = Bundle.main.bundleURL
        guard LSRegisterURL(bundle as CFURL, true) == noErr else {
            throw ProfileFailure.callbackUnregistered
        }
        try checkRegistration(redirect)
    }

    func present(_ authorizationURL: URL, _ redirect: URL) async throws -> URL {
        try Self.registerCallback(redirect)
        guard let window = SignInWindow.make(on: try screen()) else {
            throw ProfileFailure.presentationUnavailable
        }
        self.window = window
        NSApplication.shared.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
        defer {
            window.orderOut(nil)
            self.window = nil
            self.session = nil
        }

        return try await withCheckedThrowingContinuation { continuation in
            self.continuation = continuation
            let completion: ASWebAuthenticationSession.CompletionHandler = { [weak self] callback, error in
                let cancelled = (error as? ASWebAuthenticationSessionError)?.code == .canceledLogin
                Task { @MainActor [weak self] in
                    guard let self else { return }
                    if let callback {
                        self.finish(.success(callback))
                    } else {
                        self.finish(.failure(cancelled ? ProfileFailure.cancelled : .presentationUnavailable))
                    }
                }
            }
            let session: ASWebAuthenticationSession
            if #available(macOS 14.4, *) {
                session = ASWebAuthenticationSession(url: authorizationURL,
                    callback: .customScheme(redirect.scheme!), completionHandler: completion)
            } else {
                session = ASWebAuthenticationSession(url: authorizationURL,
                    callbackURLScheme: redirect.scheme!, completionHandler: completion)
            }
            self.session = session
            session.presentationContextProvider = self
            if !session.start() { finish(.failure(ProfileFailure.presentationUnavailable)) }
        }
    }

    func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        window!
    }

    private func finish(_ result: Result<URL, any Error>) {
        let completion = continuation
        continuation = nil
        completion?.resume(with: result)
    }

    private func screen() throws -> NSScreen {
        if let name = ProcessInfo.processInfo.environment["WEIRNOTE_TEST_DISPLAY"] {
            let matching = NSScreen.screens.filter { $0.localizedName == name }
            guard matching.count == 1 else { throw ProfileFailure.presentationUnavailable }
            return matching[0]
        }
        guard let screen = NSScreen.main else { throw ProfileFailure.presentationUnavailable }
        return screen
    }
}
