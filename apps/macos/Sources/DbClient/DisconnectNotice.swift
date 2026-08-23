import AppKit
import UserNotifications

/// The one notification this application posts: a connection went red while
/// the window was in the background.
///
/// The *when* does not live here. `recordHealth` owns the rule — only on the
/// transition to red, only off the front, only while the setting says so — and
/// hands this nothing but a connection's name, which is what lets the rule be
/// check-pinned while the delivery stays swappable: the checks put a closure
/// in front of this and count what came through.
///
/// The delivery has two shapes because the binary runs two ways. Bundled —
/// `make run`, `make screenshot`, anything launched from `dist/DbClient.app` —
/// it goes through `UNUserNotificationCenter` like any other application's.
/// Unbundled — the `--verify-*` suites, the probes, `make run-console` — there
/// is no bundle for the notification centre to register under, and merely
/// asking for `UNUserNotificationCenter.current()` raises an Objective-C
/// exception before it can refuse. So the bare binary says its one line to
/// stderr instead, which is the console that kind of run is watched on.
@MainActor
enum DisconnectNotice {
    static func deliver(about label: String) {
        guard Bundle.main.bundlePath.hasSuffix(".app") else {
            fputs("disconnected: \(label)\n", stderr)
            return
        }
        let center = UNUserNotificationCenter.current()
        // Asked at the first delivery rather than at launch, so the permission
        // panel appears over an application that has just tried to tell the
        // user something — the one moment the question explains itself. Every
        // later call answers from the recorded choice without a panel.
        center.requestAuthorization(options: [.alert, .sound]) { granted, _ in
            guard granted else { return }
            let content = UNMutableNotificationContent()
            content.title = "Connection dropped"
            content.body = "\(label) stopped answering. Reconnect is in its status bar."
            center.add(
                UNNotificationRequest(
                    identifier: UUID().uuidString, content: content, trigger: nil))
        }
    }
}
