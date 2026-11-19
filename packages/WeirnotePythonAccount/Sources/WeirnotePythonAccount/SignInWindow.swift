import AppKit
import SwiftUI

@MainActor
enum SignInWindow {
    static func make(on screen: NSScreen, styleMask: NSWindow.StyleMask = [.titled]) -> NSWindow? {
        let icon = Bundle.main.url(forResource: "Weirnote", withExtension: "icns")
            .flatMap { NSImage(contentsOf: $0) }
        let view = NSHostingView(rootView: SignInContent(icon: icon))
        let size = view.fittingSize
        let bounds = screen.visibleFrame
        let frame = NSWindow.frameRect(forContentRect: NSRect(origin: .zero, size: size), styleMask: styleMask)
        guard size.width > 0, size.height > 0, bounds.width >= frame.width, bounds.height >= frame.height else {
            return nil
        }
        let window = NSWindow(contentRect: NSRect(origin: .zero, size: size), styleMask: styleMask,
                              backing: .buffered, defer: false, screen: screen)
        window.title = "Sign in to Weirnote"
        window.isReleasedWhenClosed = false
        window.contentView = view
        window.setFrameOrigin(NSPoint(x: bounds.midX - frame.width / 2, y: bounds.midY - frame.height / 2))
        return window
    }
}

private struct SignInContent: View {
    let icon: NSImage?

    var body: some View {
        VStack(spacing: WeirnoteMetric.space4) {
            if let icon {
                Image(nsImage: icon)
                    .resizable()
                    .scaledToFit()
                    .frame(width: WeirnoteMetric.space16, height: WeirnoteMetric.space16)
                    .accessibilityHidden(true)
            }
            VStack(spacing: WeirnoteMetric.space2) {
                Text("Continue signing in using your browser")
                    .font(.headline)
                Text("Weirnote keeps a device key in Keychain so your scripts can reconnect.")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
            .multilineTextAlignment(.center)
        }
        .padding(WeirnoteMetric.space8)
        .frame(width: 384)
        .fixedSize(horizontal: false, vertical: true)
    }
}
