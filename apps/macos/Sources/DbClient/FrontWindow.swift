import AppKit

/// Which window the menu bar is acting on.
///
/// The menu bar belongs to the application and a model belongs to a window, so
/// what a menu command can hold is the *question* — which window is in front —
/// and not an answer taken once at launch, when there was only one window to
/// take it from. Every command in `AppMenu` reads its model through this, which
/// is why adding a second window changed none of their bodies.
///
/// One object rather than seventeen readers of `NSApp.keyWindow`, and not that
/// property for two reasons. It is nil while a panel is key — so ⌘R would stop
/// working because the Settings window was open, for a reason nobody could see
/// on screen — and it answers with a window rather than with the model behind
/// it, which every command would then have to look up in a list of its own.
@MainActor
final class FrontWindow {
    /// The model of the window that most recently became key.
    ///
    /// Never nil. A window is built before the menu is installed, and a window
    /// that closes hands this to whichever one takes its place — so there is no
    /// moment in the application's life when the menu has nothing to act on.
    var model: AppModel

    init(_ model: AppModel) {
        self.model = model
    }
}
