import AppKit
import Metal
import Observation
import SwiftUI
import simd

/// Which of the palette's two sets of values is in force, as something SwiftUI
/// can watch.
///
/// Every token in `Theme` reads `current.isLight`, and that is what makes
/// switching appearance a redraw rather than a relaunch: a view body that draws
/// in `Theme.Text.primary` has, by reading it, declared a dependency on this
/// property, so Observation re-runs that body when it changes. Giving each
/// window a new `.id` instead would also redraw, and would throw away every open
/// sheet, scroll offset and half-typed field for a change that is meant to be
/// cosmetic.
///
/// One per process, because the palette is one per process. `nonisolated(unsafe)`
/// rather than main-actor isolated: the tokens are read from the Metal renderer's
/// draw as well as from view bodies, and both of those are already on the main
/// thread — the isolation would buy nothing and would have to be spelled at
/// several hundred call sites.
@Observable
final class Appearance {
    /// What the preference offers, and what a plist can hold.
    enum Setting: String, CaseIterable, Identifiable, Hashable, Sendable {
        /// Whatever the menu bar says, and it changes when that does.
        case system
        case light
        case dark

        var id: String { rawValue }

        var label: String {
            switch self {
            case .system: "Match the system"
            case .light: "Light"
            case .dark: "Dark"
            }
        }

        /// `nil` for `system`, which is how AppKit spells "follow along".
        var nsAppearance: NSAppearance? {
            switch self {
            case .system: nil
            case .light: NSAppearance(named: .aqua)
            case .dark: NSAppearance(named: .darkAqua)
            }
        }
    }

    nonisolated(unsafe) static let current = Appearance()

    /// Dark until something says otherwise, which is what this app shipped as.
    var isLight = false

    private init() {}
}

/// One source of truth for colour, spacing, and type.
///
/// Both halves of the front-end read from here: SwiftUI for the chrome, the
/// Metal renderer for the data surface. They previously carried independent
/// palettes, which is why the grid and the panes around it did not look like
/// the same application.
///
/// Every colour has two values and resolves the one the appearance in force
/// asks for. That costs a branch per read and no call site anything: the tokens
/// are named for the role each fills, so a light theme changes what `canvas`
/// *is* rather than which views ask for it. The Metal side turned out to cost
/// nothing either — the grid reads its colours at draw time and its glyph atlas
/// is a coverage mask with no colour baked into it — so the reservation the
/// first version of this file carried, that a half-converted light mode reads as
/// a bug, is answered by converting all of it.
enum Theme {

    /// Whether the tokens below resolve to the light set.
    static var isLight: Bool { forced ?? Appearance.current.isLight }

    /// Set only by `resolving(isLight:_:)`, and never while anything is drawing.
    nonisolated(unsafe) private static var forced: Bool?

    /// Reads the palette as the appearance that is *not* in force would resolve
    /// it.
    ///
    /// The tokens are properties rather than functions of an appearance, which
    /// is what keeps five hundred call sites reading like nouns; the price is
    /// that asking "and what would this be in light?" needs somewhere to put the
    /// answer. Two callers need it — the check suite, which measures both sets
    /// against each other, and the switch itself, which has to know which
    /// spellings the palette wrote before it moved — and both run on the main
    /// thread between frames. Deliberately not the observable flag: setting that
    /// twice would tell every view on screen to redraw, twice, for a value that
    /// ends where it started.
    static func resolving<T>(isLight light: Bool, _ read: () -> T) -> T {
        let was = forced
        forced = light
        defer { forced = was }
        return read()
    }

    /// A colour expressed once and consumed by both rendering stacks.
    struct Tone {
        let r, g, b, a: Double

        init(_ hex: UInt32, alpha: Double = 1) {
            r = Double((hex >> 16) & 0xFF) / 255
            g = Double((hex >> 8) & 0xFF) / 255
            b = Double((hex >> 0) & 0xFF) / 255
            a = alpha
        }

        private init(r: Double, g: Double, b: Double, a: Double) {
            self.r = r
            self.g = g
            self.b = b
            self.a = a
        }

        func opacity(_ value: Double) -> Tone { Tone(r: r, g: g, b: b, a: value) }

        var color: Color { Color(.sRGB, red: r, green: g, blue: b, opacity: a) }

        /// For the AppKit views the SwiftUI layer wraps. sRGB explicitly, so a
        /// tone means the same thing on both sides of the boundary.
        var nsColor: NSColor { NSColor(srgbRed: r, green: g, blue: b, alpha: a) }

        /// Straight, non-premultiplied alpha — which is what the grid's blend
        /// state expects.
        var simd: SIMD4<Float> { SIMD4(Float(r), Float(g), Float(b), Float(a)) }

        var mtlClear: MTLClearColor {
            MTLClearColor(red: r, green: g, blue: b, alpha: a)
        }
    }

    // MARK: - Surfaces
    //
    // Named for the depth each one is at rather than for what it happens to be
    // used for, which is what makes the second set of values possible: a light
    // theme changes what `canvas` *is* and changes nothing about which views ask
    // for it. A flat `background` / `surface` / `surfaceRaised` could not say
    // that — the first of those names a role, the other two name an order, and a
    // reader had to know the file to tell which was on top.
    //
    // The ramp inverts rather than lightens. Dark runs 0F172A → 1E293B → 27334A,
    // each step away from the floor lighter; light runs FFFFFF → F1F5F9 →
    // E2E8F0, each step darker. Either way the canvas is the furthest from the
    // text drawn on it, which is what a window that is mostly grid needs.

    enum Surface {
        /// The window's floor: the grid, and the space behind everything.
        static var canvas: Tone { isLight ? Tone(0xFFFF_FF) : Tone(0x0F17_2A) }
        /// Panels, header bands, cards, the active tab.
        static var raised: Tone { isLight ? Tone(0xF1F5_F9) : Tone(0x1E29_3B) }
        /// One step further from the floor: the inspector strip, popovers.
        static var overlay: Tone { isLight ? Tone(0xE2E8_F0) : Tone(0x2733_4A) }
    }

    enum Border {
        /// Divider lines, which are the direction the ramp runs at low alpha —
        /// white on dark, black on light — rather than a colour of their own, so
        /// they follow the surface under them instead of fighting it. Black
        /// carries further than white does at equal alpha, hence the 0.10.
        static var hairline: Tone {
            isLight ? Tone(0x0F17_2A, alpha: 0.10) : Tone(0xFFFF_FF, alpha: 0.08)
        }
        /// The edge of a control, which has to hold its own against both.
        static var control: Tone { isLight ? Tone(0xCBD5_E1) : Tone(0x4755_69) }
    }

    // MARK: - Text
    //
    // Contrast is checked rather than assumed, and against the *lightest*
    // surface each tone is drawn on rather than against `Surface.canvas`. That
    // distinction is not pedantry: it is where the previous tertiary went wrong.
    // Measured against the canvas alone it read 3.8:1 and was documented as
    // clearing the 3:1 bar — but the chrome draws it on `Surface.raised` (the
    // status bar, the filter bar, the sidebar footer) and on `Surface.overlay`
    // (the cell inspector strip, the history and outcome headers), where the
    // same tone fell to 3.1:1 and 2.7:1. A number measured against the one
    // surface a tone is rarely used on is not a check.
    //
    // So, against `Surface.overlay`: primary 12.1:1, secondary 4.9:1, tertiary
    // 3.3:1. Tertiary is still for non-essential labels only — it clears 3:1
    // everywhere it appears and 4.5:1 nowhere but the canvas — and anything a
    // user has to read rather than glance at belongs on secondary.
    //
    // The light set is measured the same way and against the same surface, which
    // there is the *darkest* one rather than the lightest — same argument upside
    // down. It comes out ahead of the dark set at every rung: primary 14.5:1,
    // secondary 6.1:1, tertiary 3.9:1, dataMuted 5.2:1, dataFaint 3.5:1. That is
    // not generosity, it is what the slate ramp gives when the text end of it is
    // the end with the room; the numbers are here so that the day one of these
    // tones is nudged, the check in `ThemeChecks` says which rung it broke.

    enum Text {
        static var primary: Tone { isLight ? Tone(0x0F17_2A) : Tone(0xF8FA_FC) }
        static var secondary: Tone { isLight ? Tone(0x4755_69) : Tone(0x94A3_B8) }
        static var tertiary: Tone { isLight ? Tone(0x6474_8B) : Tone(0x7483_9A) }
        /// Muted text on the data surface: the type line under a column header,
        /// the word NULL in a cell. Content someone reads, not chrome — it holds
        /// 4.2:1 on the header band and 4.6:1 on the grid background, where the
        /// tertiary label tone would fall to 3.1:1.
        static var dataMuted: Tone { isLight ? Tone(0x5160_7A) : Tone(0x7C8A_A0) }
        /// Dimmer than NULL: the word DEFAULT in a draft row — what the table
        /// will decide, not what the row holds.
        static var dataFaint: Tone { isLight ? Tone(0x6B7A_93) : Tone(0x6475_8B) }
    }

    // MARK: - Semantics
    //
    // Two accents carrying two meanings, rather than one accent overloaded:
    // indigo is "this is selected / focused", green is "this executes". They are
    // apart from `Semantic` because they answer a different question — where you
    // are and what will run, rather than how a thing is going.

    //
    // Every one of these is a hue at the light end of its ramp on dark and at
    // the dark end on light — indigo 500 against indigo 600, green 500 against
    // green 700 — because a tone that reads on a near-black canvas is a tone
    // that disappears on a white one.

    enum Accent {
        static var selection: Tone { isLight ? Tone(0x4F46_E5) : Tone(0x6366_F1) }
        static var execute: Tone { isLight ? Tone(0x1580_3D) : Tone(0x22C5_5E) }
    }

    enum Semantic {
        static var warning: Tone { isLight ? Tone(0xB453_09) : Tone(0xFBBF_24) }
        /// Fills and stripes. Too dark for text on `Surface.canvas` at 4.3:1 —
        /// use `dangerText` there instead. The light set inverts the pair: there
        /// the fill is the lighter of the two and the text tone is darker.
        static var danger: Tone { isLight ? Tone(0xDC26_26) : Tone(0xEF44_44) }
        static var dangerText: Tone { isLight ? Tone(0xB91C_1C) : Tone(0xF871_71) }
    }

    /// The colours a saved connection can be marked with.
    ///
    /// Seven and no more. This is not a palette to express anything with — it is a
    /// way to tell production from staging before running a statement against the
    /// wrong one, and a list of twenty tones is one where nobody remembers which is
    /// which. They are spaced around the wheel so that any two differ in hue as well
    /// as in name, and none of them is the indigo this window already spends on
    /// "this is the one you are on".
    ///
    /// Measured against `Surface.canvas`, which is what a sidebar row is drawn
    /// on: 6.5:1 for the tightest of them on dark, 4.9:1 on light, and better
    /// for the rest — well past the 3:1 a mark that is not text needs, and
    /// deliberately so, because a 3pt stripe is small enough that a ratio which
    /// passes on paper can still be hard to see. The light set is the same seven
    /// hues three rungs down their ramps; the names are what a connection stores,
    /// so an existing mark keeps its name and changes its value.
    ///
    /// Colour is never the only signal: the row it marks carries the connection's
    /// name and what it opens, and the swatch that sets it is named for a screen
    /// reader.
    enum Connection {
        static var red: Tone { isLight ? Tone(0xB91C_1C) : Tone(0xF871_71) }
        static var orange: Tone { isLight ? Tone(0xC241_0C) : Tone(0xFB92_3C) }
        static var yellow: Tone { isLight ? Tone(0xA162_07) : Tone(0xFACC_15) }
        static var green: Tone { isLight ? Tone(0x1580_3D) : Tone(0x4ADE_80) }
        static var blue: Tone { isLight ? Tone(0x1D4E_D8) : Tone(0x60A5_FA) }
        static var purple: Tone { isLight ? Tone(0x7E22_CE) : Tone(0xC084_FC) }
        static var grey: Tone { isLight ? Tone(0x4755_69) : Tone(0x94A3_B8) }
    }

    /// Colours used by the Metal grid. Separate namespace because the data
    /// surface has its own vocabulary, not because it has its own palette.
    enum Grid {
        static var background: Tone { Theme.Surface.canvas }
        static var header: Tone { Theme.Surface.raised }
        static var headerText: Tone { Theme.Text.secondary }
        /// Brighter than the other headers, so the sorted column is identifiable
        /// without having to resolve the direction marker beside it. Darker, on
        /// light — "further from the surface" is the property, not "brighter".
        static var sortedHeaderText: Tone { Theme.Text.primary }
        /// The type line under each header name. Subordinate to the name — 4.2:1
        /// on the header band where the name holds 5.7:1 — but deliberately not
        /// the tertiary label tone, which would fall to 3.1:1 there. A type is
        /// something the user came to read, not chrome.
        static var headerType: Tone { Theme.Text.dataMuted }
        /// Row banding and the column rule: the ramp's direction at a low alpha,
        /// like `Border.hairline` and for the same reason. Black at 0.022 over
        /// white is a step nobody can see, so the light values are a shade
        /// stronger than their dark counterparts rather than the same number.
        static var banding: Tone {
            isLight ? Tone(0x0F17_2A, alpha: 0.030) : Tone(0xFFFF_FF, alpha: 0.022)
        }
        static var separator: Tone {
            isLight ? Tone(0x0F17_2A, alpha: 0.080) : Tone(0xFFFF_FF, alpha: 0.060)
        }
        static var text: Tone { isLight ? Tone(0x1E29_3B) : Tone(0xE2E8_F0) }
        /// Dimmer than a value but not a label: NULL is content, so it holds
        /// 4.6:1 against the background rather than the 3.4:1 that incidental
        /// text gets away with. It is also drawn as the literal word, so the
        /// colour is a second signal rather than the only one.
        static var nullText: Tone { Theme.Text.dataMuted }
        static var selectedRow: Tone { Theme.Accent.selection.opacity(0.18) }
        static var selectedCell: Tone { Theme.Accent.selection.opacity(0.38) }
        /// A cell holding a change that has not been sent. Amber rather than the
        /// accent: it means the same thing as the amber in the toolbar, which is
        /// that the database does not know about this yet.
        static var pendingCell: Tone { Theme.Semantic.warning.opacity(0.30) }
        /// A row marked to be deleted. Red rather than the amber a changed cell
        /// gets: both are unsent, but one of them takes the row away, and that
        /// is worth being able to tell apart at a glance across a long result.
        static var deletedRow: Tone { Theme.Semantic.danger.opacity(0.26) }
        /// A row that is not in the database yet. Green, the third of the three
        /// signals a grid full of unsent work needs — added, changed, going —
        /// and the same green the Run button uses for the thing that has not
        /// happened yet.
        static var draftRow: Tone { Theme.Accent.execute.opacity(0.20) }
        /// A draft column nobody has typed into, drawn as the word DEFAULT.
        /// Dimmer than a value and dimmer than NULL, because unlike either of
        /// them it is not what the row will hold — it is what the table will
        /// decide.
        static var defaultText: Tone { Theme.Text.dataFaint }
        static var cursor: Tone { Theme.Accent.selection }
        /// The scrollbar sits over the data rather than beside it, so the track
        /// is barely there and the thumb carries the whole signal.
        static var scrollTrack: Tone {
            isLight ? Tone(0x0F17_2A, alpha: 0.040) : Tone(0xFFFF_FF, alpha: 0.035)
        }
        static var scrollThumb: Tone {
            isLight ? Tone(0x0F17_2A, alpha: 0.18) : Tone(0xFFFF_FF, alpha: 0.22)
        }
        static var scrollThumbActive: Tone {
            isLight ? Tone(0x0F17_2A, alpha: 0.32) : Tone(0xFFFF_FF, alpha: 0.38)
        }
    }

    /// Colours for the SQL editor's syntax. Its own namespace for the same
    /// reason `Grid` has one: a surface with a vocabulary of its own, drawn from
    /// the same palette.
    ///
    /// Contrast is against `Theme.Surface.canvas`, measured rather than assumed, and
    /// the numbers below are the ratios. All of them clear 4.5:1, because every
    /// one of these is text somebody is reading rather than a label beside it.
    ///
    /// The ordering is deliberate. Plain identifiers — the table and column
    /// names a reader is actually hunting for — stay brightest at 14.5:1 and
    /// take no colour at all; keywords are the most frequent thing on screen and
    /// sit lowest, so that colouring them separates the sentence without
    /// shouting over its nouns.
    enum Editor {
        /// Anything with no token of its own: identifiers, operators,
        /// punctuation. The grid's data tone, since both are content.
        static var text: Tone { isLight ? Tone(0x1E29_3B) : Tone(0xE2E8_F0) }  // 14.5:1 / 14.6:1
        static var keyword: Tone { isLight ? Tone(0x7C3A_ED) : Tone(0xA78B_FA) }  // 6.6:1 / 5.7:1
        /// Warm and bright on purpose. An unclosed quote turns everything after
        /// it into a literal, and that is the mistake this whole feature exists
        /// to make visible, so it gets the loudest colour here.
        static var string: Tone { isLight ? Tone(0xB453_09) : Tone(0xFDBA_74) }  // 10.6:1 / 5.0:1
        /// A `$fn$ … $fn$` body is a string to the server, so it takes the
        /// string's hue — but dimmed, because a function body runs to dozens of
        /// lines and a literal at full strength over that much text glows.
        /// Its contents are deliberately left flat: the server sees one string
        /// there, and a second language lexed inside the first is a much larger
        /// promise than this makes.
        static var dollarQuoted: Tone { isLight ? Tone(0x8A65_34) : Tone(0xD9A0_66) }  // 7.8:1 / 5.3:1
        static var number: Tone { isLight ? Tone(0x0F76_6E) : Tone(0x5EEA_D4) }  // 12.1:1 / 5.5:1
        /// A quoted identifier is a name, not a value, so it is cool where the
        /// literals are warm.
        static var quotedIdentifier: Tone { isLight ? Tone(0x1D4E_D8) : Tone(0x93C5_FD) }  // 9.9:1 / 6.7:1
        /// Subordinate but not decorative: a comment is prose the author wrote
        /// to be read, so it stays above the 4.5:1 line rather than dropping to
        /// the tertiary label tone.
        static var comment: Tone { isLight ? Tone(0x6474_8B) : Tone(0x7C8F_A6) }  // 5.4:1 / 4.8:1
        /// The caret and the selection band, which `pointAtSyntaxError` uses to
        /// put the offending token on screen. Indigo is already "this is where
        /// you are" everywhere else in the window.
        static var caret: Tone { Theme.Accent.selection }
        static var selection: Tone { Theme.Accent.selection.opacity(0.32) }
        /// The band behind a matched pair of parentheses. The palette's "lifted off
        /// the page" tone rather than a colour of its own: this mark says where the
        /// partner is, and a hue would compete with the token colours it sits under.
        static var bracketMatch: Tone { Theme.Surface.overlay }
        /// The band behind the statement ⌘R would run, when the buffer holds
        /// several. The same "lifted off the page" family as `bracketMatch`,
        /// one step below it: this mark covers whole lines of tokens, and at
        /// `Surface.overlay`'s strength it would read as a selection.
        static var statement: Tone { Theme.Surface.raised }
    }

    // MARK: - Spacing
    //
    // A 4pt rhythm at dashboard density. A database client trades whitespace for
    // rows on screen; the scale is tight on purpose and consistent so that it
    // reads as dense rather than cramped.

    enum Space {
        static let xs: CGFloat = 4
        static let sm: CGFloat = 8
        static let md: CGFloat = 12
        static let lg: CGFloat = 16
        static let xl: CGFloat = 24
    }

    enum Radius {
        static let control: CGFloat = 5
        static let card: CGFloat = 7

        /// The corner itself, rather than only how far it turns.
        ///
        /// `.continuous` is the curve every rounded surface macOS draws for
        /// itself uses — a sheet, a text field, a button — and it is the half of
        /// a corner a radius does not state. A circular arc meets its edges at
        /// an angle the eye can find, so beside anything AppKit drew, a corner
        /// left on the default reads as the sharper one.
        ///
        /// Handed out as shapes rather than left to each call site, because the
        /// call sites are where it went wrong: twenty of the twenty-five rounded
        /// rectangles in this tree said `cornerRadius:` and nothing else, which
        /// is a default nobody chose and nobody could see until they were
        /// counted. Stating it once removes the chance to forget it.
        static var controlShape: RoundedRectangle {
            RoundedRectangle(cornerRadius: control, style: .continuous)
        }

        static var cardShape: RoundedRectangle {
            RoundedRectangle(cornerRadius: card, style: .continuous)
        }
    }

    // MARK: - Type
    //
    // The system face, not a bundled one. SF is what the design reference's
    // Inter is imitating, and shipping a webfont into a native Mac app trades
    // platform consistency for nothing.

    enum Typography {
        static let micro = Font.system(size: 10)
        static let caption = Font.system(size: 11)
        static let captionEmphasis = Font.system(size: 11, weight: .semibold)
        static let body = Font.system(size: 12)
        static let bodyEmphasis = Font.system(size: 12, weight: .medium)
        static let title = Font.system(size: 13, weight: .semibold)
        static let mono = Font.system(size: 12, design: .monospaced)
        static let monoSmall = Font.system(size: 11, design: .monospaced)
        // The editor's font is not here: its size became a preference, so the
        // one statement of it lives in `Preferences.registered` and the editor
        // builds the font from what the preference says.
        static let digits = Font.system(size: 11).monospacedDigit()
    }

    // MARK: - Motion

    enum Motion {
        static let quick: Double = 0.12
        static let standard: Double = 0.18

        /// Returns `nil` when the system asks for reduced motion, so callers get
        /// an instant change by passing it to `withAnimation`/`.animation()`
        /// rather than each site remembering to branch.
        static func ease(_ reduceMotion: Bool, _ duration: Double = standard) -> Animation? {
            reduceMotion ? nil : .easeOut(duration: duration)
        }
    }

    /// Which set of values an appearance asks for.
    ///
    /// Asked of what the app is drawing in rather than of the preference,
    /// because under `system` the preference does not know: only AppKit does,
    /// and only AppKit is told when the menu bar flips at sunset.
    static func isLight(_ appearance: NSAppearance) -> Bool {
        appearance.bestMatch(from: [.aqua, .darkAqua]) != .darkAqua
    }
}

/// Keeps the palette on whatever the preference and the system between them say.
///
/// The two sources are watched the way `MCPCoordinator` watches its own: the
/// defaults notification for the preference, KVO for the system, and one
/// idempotent `apply` behind both — so a change arriving twice costs a
/// comparison rather than a second redraw.
@MainActor
final class AppearanceController {
    static let shared = AppearanceController()

    private var preferences: Preferences?
    private var app: NSApplication?
    private var overriding: Appearance.Setting?
    private var pinned: Appearance.Setting?
    private var systemObserver: NSKeyValueObservation?
    private var defaultsObserver: NSObjectProtocol?

    private init() {}

    /// Applies the preference and keeps applying it. Called before any window is
    /// built, so nothing lays out in the wrong appearance and flashes on the
    /// first frame.
    /// `overriding` is `--appearance`, which belongs to this launch rather than
    /// to the person using the app: a screenshot needs both appearances and
    /// neither of them is a setting anybody chose. It is never written back.
    func follow(
        preferences: Preferences, app: NSApplication, overriding: Appearance.Setting? = nil
    ) {
        self.preferences = preferences
        self.app = app
        self.overriding = overriding
        defaultsObserver = NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification, object: nil, queue: .main
        ) { _ in
            // On the main queue by the observer's own terms; stated rather than
            // hopped so the apply is synchronous with the change.
            MainActor.assumeIsolated { AppearanceController.shared.apply() }
        }
        // The system half. `app.appearance = nil` is what "follow along" means,
        // and nothing else in this process is told when the thing being followed
        // moves.
        systemObserver = app.observe(\.effectiveAppearance, options: [.new]) { _, _ in
            MainActor.assumeIsolated { AppearanceController.shared.apply() }
        }
        apply()
    }

    private func apply() {
        guard let preferences, let app else { return }
        let setting = overriding ?? preferences.appearance
        if setting != pinned {
            pinned = setting
            app.appearance = setting.nsAppearance
        }
        let light = Theme.isLight(app.effectiveAppearance)
        guard light != Appearance.current.isLight else { return }
        // Read before the switch, because "was this slot still the palette's?"
        // is a question about the palette that is being left behind.
        let previous = EditorTheme.defaults
        Appearance.current.isLight = light
        preferences.followEditorPalette(from: previous)
    }
}
