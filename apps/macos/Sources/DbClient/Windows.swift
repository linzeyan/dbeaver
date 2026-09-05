import AppKit
import SwiftUI

/// An open connection a picker can offer, and what it calls it.
///
/// Here rather than beside either picker that uses it, because the rule it
/// carries is a window rule. A pair rather than the session alone: the name a
/// connection carries stopped being enough the moment there was a second window.
/// Two windows are most often the same saved connections opened twice — that is
/// what a second window is *for* — so the labels collide, and a picker offering
/// "prod" twice with nothing between them is a choice nobody can make.
@MainActor
struct ConnectionChoice: Identifiable {
    let session: Session
    let label: String

    var id: UUID { session.id }

    /// A connection in the window doing the asking, called what its tab is
    /// called.
    static func inThisWindow(_ session: Session) -> ConnectionChoice {
        ConnectionChoice(session: session, label: session.connectionLabel)
    }

    /// A connection in one of the other windows, named for where it is.
    ///
    /// Not numbered. AppKit numbers nothing on screen — the Window menu lists
    /// titles — so "Window 2" would be a number this application never shows
    /// anywhere else. What the person needs to know is that the work will land
    /// somewhere they are not looking, and that is what this says.
    static func inAnotherWindow(_ session: Session) -> ConnectionChoice {
        ConnectionChoice(session: session, label: "\(session.connectionLabel) — another window")
    }
}

/// One window: the AppKit window, the model behind it, and the delegate that
/// answers for both.
///
/// The delegate is here rather than on the application because what ⌘W asks
/// about belongs to one window. `AppLifecycle` used to be both, which worked
/// exactly as long as there was only one window to be.
@MainActor
final class WindowController: NSObject, NSWindowDelegate {
    /// Below this the grid shows one column and the filter bar wraps; there is no
    /// useful layout smaller, so the window is not allowed to reach it.
    ///
    /// Named rather than written into `init`, because `--short-window` opens the
    /// window at exactly this size and reading it back off the window does not
    /// answer: `NSHostingView` writes the root view's own minimum into
    /// `contentMinSize` once it is installed, and what `minSize` reports
    /// afterwards is SwiftUI's number rather than this one.
    static let minimumSize = NSSize(width: 940, height: 580)

    let window: NSWindow
    let model: AppModel

    /// The list this window is in. Unowned because it outlives every window —
    /// it is held by the top-level `let` in main.swift, and a window that closes
    /// is removed from it rather than the other way round.
    private unowned let list: WindowList

    init(model: AppModel, in list: WindowList) {
        self.model = model
        self.list = list
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1600, height: 1000),
            styleMask: [.titled, .closable, .resizable, .miniaturizable, .fullSizeContentView],
            backing: .buffered,
            defer: false)
        super.init()

        // Transparent, so the unified titlebar and toolbar take the background set
        // on the line below rather than the system's own material. Opaque, that
        // strip is a neutral near-black running the full width of the window —
        // above the sidebar and the detail column alike — while every other
        // surface under it is the palette's blue-tinted background, and the seam
        // reads as two applications stacked. `.fullSizeContentView` is already in
        // the style mask and the toolbar still lays out beneath it, so nothing
        // moves; what changes is the fill.
        window.titlebarAppearsTransparent = true
        window.toolbarStyle = .unified
        window.backgroundColor = NSColor(Theme.Surface.canvas.color)
        window.minSize = Self.minimumSize
        // Until a connection lands the window has no relation to name, and
        // `navigationTitle` has not run. A titleless window reads as one that
        // failed to finish launching.
        window.title = "DbClient"
        window.delegate = self
        window.contentView = NSHostingView(rootView: MainView(model: model))
        // Here rather than in the model's own init, because only a window that is
        // about to run a run loop has any business owning a repeating timer: the
        // `--verify-*` suites build models by the dozen and exit.
        model.startKeepAliveTimer()
    }

    /// Puts the window on screen, offset from the one before it.
    ///
    /// The first is centred and the rest are staggered down and to the right, the
    /// way every document application on the platform does it. Two windows drawn
    /// at one point are one window as far as anybody looking at the screen can
    /// tell, and seeing both is the entire reason for a second one.
    ///
    /// Not `NSWindow.cascadeTopLeft`, which is the API for this and does nothing
    /// here: this window opens at 1600×1000 and the screen it opens on is often
    /// smaller, so AppKit constrains it to the visible frame and then has nowhere
    /// to move it to. Measured on a 1512pt display, where both windows came back
    /// at exactly (0, 0). Shrinking by the step is what buys the room.
    func show(after previous: WindowController?) {
        guard let previous, let screen = previous.window.screen ?? NSScreen.main else {
            window.center()
            window.makeKeyAndOrderFront(nil)
            return
        }
        let visible = screen.visibleFrame
        let step: CGFloat = 24
        var frame = previous.window.frame
        // Room for a step on both sides before anything is moved. Shrinking by
        // one step would leave the window flush against the edge it moved away
        // from, which on a screen-filling window is a stagger you cannot see.
        frame.size.width = min(frame.width, visible.width - 2 * step)
        frame.size.height = min(frame.height, visible.height - 2 * step)
        // Staggered by the top left corner rather than by the origin. AppKit's
        // origin is the *bottom* left, so offsetting it moves a window that is
        // also getting shorter twice as far down as it looks — and a window
        // already sitting on the bottom edge is pushed straight off the screen.
        frame.origin.x = previous.window.frame.minX + step
        frame.origin.y = previous.window.frame.maxY - step - frame.height
        // Back to the top left when the stagger would push it off, which is what
        // a cascade does rather than piling windows up against the edge.
        if frame.maxX > visible.maxX || frame.minY < visible.minY {
            frame.origin = CGPoint(x: visible.minX, y: visible.maxY - frame.height)
        }
        window.setFrame(frame, display: false)
        window.makeKeyAndOrderFront(nil)
    }

    // MARK: - Delegate

    /// ⌘W and the close button, guarded per window: what closing this one throws
    /// away is this window's work, and a question naming another window's would
    /// be a question about something still on screen.
    func windowShouldClose(_ sender: NSWindow) -> Bool {
        guard list.mayClose(self) else { return false }
        return true
    }

    func windowWillClose(_ notification: Notification) {
        list.closed(self)
    }

    /// The menu bar acts on whichever window was most recently key; see
    /// `FrontWindow`.
    func windowDidBecomeKey(_ notification: Notification) {
        list.front.model = model
    }
}

/// Every window this application has open, and the two questions that have to be
/// answered for all of them at once: what quitting would throw away, and what the
/// next launch puts back.
///
/// A class rather than a global array because both of those answers are folds
/// over the list, and a fold over a global is a fold nothing can hand a different
/// list to.
@MainActor
final class WindowList {
    private(set) var windows: [WindowController] = []

    /// Which window the menu bar acts on.
    let front: FrontWindow

    /// The stores every window shares.
    ///
    /// Shared objects and not merely a shared `UserDefaults`: the history a
    /// statement is recorded in, the favorites list and the settings are the
    /// person's rather than the window's, and two windows holding two
    /// `QueryHistory` objects over one defaults domain would each be writing over
    /// what the other had just recorded.
    private let history: QueryHistory
    private let favorites: QueryFavorites
    private let preferences: Preferences

    /// Where what was open is read from and written back to, or nil for a process
    /// that keeps none.
    ///
    /// Injected, and nil for a capture: a screenshot that restored the
    /// developer's own last session would put their tabs in the picture, and
    /// would then write its own single window over theirs on the way out.
    private let restore: SessionRestoreStore?

    /// Whether the question ⌘Q asks has already been asked and answered.
    ///
    /// ⌘W on the last window arrives twice — once as that window closing, and
    /// again as the termination closing it causes — and without this the person
    /// who has just said "Discard and Quit" is asked the same thing a second
    /// time, over a window that has already gone.
    private var askedOnClose = false

    init(
        first model: AppModel, history: QueryHistory, favorites: QueryFavorites,
        preferences: Preferences, restore: SessionRestoreStore?
    ) {
        self.history = history
        self.favorites = favorites
        self.preferences = preferences
        self.restore = restore
        front = FrontWindow(model)
        adopt(model)
    }

    /// Opens a window on a model somebody else built — main.swift's first, which
    /// carries the launch flags and whatever was restored into it.
    @discardableResult
    func adopt(_ model: AppModel) -> WindowController {
        let controller = WindowController(model: model, in: self)
        // What a picker in this window can reach past its own tabs: the
        // connections every other window has open. Read when the picker draws
        // rather than taken now, because the windows this walks did not all
        // exist when this one was built — and the one being built is left out by
        // identity, so a window is never offered its own tabs twice.
        // Weakly on both sides. The closure is a property of the model it asks
        // about, so a strong capture of either would be a window that never gets
        // released — and the model of a window that has closed outlives it for
        // as long as `FrontWindow` is still pointing at it.
        model.otherWindowChoices = { [weak self, weak model] in
            guard let self, let model else { return [] }
            return windows.filter { $0.model !== model }
                .flatMap { $0.model.idleSessions.map(ConnectionChoice.inAnotherWindow) }
        }
        controller.show(after: windows.last)
        windows.append(controller)
        front.model = model
        return controller
    }

    /// File ▸ New Window. Opens on nothing, like the first window of a launch
    /// with nothing to restore: a second window is a second place to work, not a
    /// second copy of the first — copying its tabs would open connections nobody
    /// asked for, which is the rule restore is built around.
    @discardableResult
    func openWindow(restoring tabs: RestoredWindow? = nil) -> WindowController {
        adopt(
            AppModel(
                history: history, favorites: favorites, preferences: preferences,
                restoring: tabs))
    }

    /// What ⌘Q would throw away, across every window.
    var unsavedWork: UnsavedWork? {
        UnsavedWork.acrossWindows(windows.compactMap(\.model.unsavedWork))
    }

    /// Whether one window may close, asking about that window's work alone.
    func mayClose(_ controller: WindowController) -> Bool {
        // The last window closing ends the process, so the question is the quit
        // question: what is at stake is everything, and it is called Quit because
        // that is what the key does here.
        let last = windows.count == 1
        let work = last ? unsavedWork : controller.model.unsavedWork
        guard Self.mayDiscard(work, last ? .quitting : .closing) else { return false }
        // Only the last one: closing a window with others behind it does not end
        // the process, so the quit question has not been put.
        //
        // Written down here rather than in `mayTerminate`, and this is the whole
        // reason that split exists: the termination this close causes arrives
        // *after* `windowWillClose` has taken the window out of the list, so by
        // then there is nothing left to write and ⌘W would quietly empty the
        // file every time it was the way out.
        if last {
            askedOnClose = true
            remember()
        }
        return true
    }

    /// Whether the process may end, and — where it may — what to write down.
    func mayTerminate() -> Bool {
        // Already asked, and already written down by the close that caused this.
        if askedOnClose { return true }
        guard Self.mayDiscard(unsavedWork, .quitting) else { return false }
        // After the question and only on the way out, so a quit somebody
        // cancelled leaves what was written last time alone rather than replacing
        // it with the windows they decided not to leave.
        remember()
        return true
    }

    /// Takes a closed window out of the list, and hands the menu bar to whichever
    /// one is left.
    func closed(_ controller: WindowController) {
        windows.removeAll { $0 === controller }
        if let next = windows.last { front.model = next.model }
    }

    /// Writes down what every window has open.
    func remember() {
        restore?.remember(
            windows.map(\.model.rememberedWindow), restoring: preferences.restoresSession)
    }

    /// Puts the question, and answers whether the work may go.
    ///
    /// One dialog for every way out, worded by `UnsavedWork` so that what it says
    /// can be checked without anybody at the keyboard — see `--verify-quitting`.
    /// A modal alert rather than a window's own error banner for the reason
    /// `AppModel.confirmDeletion` gives: a strip that can be ignored is not a
    /// question, and this one has to be answered before the work goes.
    private static func mayDiscard(_ work: UnsavedWork?, _ departure: UnsavedWork.Departure)
        -> Bool
    {
        guard let work else { return true }
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = work.question(departure)
        alert.informativeText = work.detail
        // Leaving leads because it is what the keystroke asked for, and it says
        // what it costs rather than only where it goes. Cancel takes the escape
        // key, so dismissing the dialog without reading it keeps the work.
        alert.addButton(withTitle: departure.confirmation)
        let cancel = alert.addButton(withTitle: "Cancel")
        cancel.keyEquivalent = "\u{1b}"
        return alert.runModal() == .alertFirstButtonReturn
    }
}
