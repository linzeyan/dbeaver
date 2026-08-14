import AppKit
import Metal
import SwiftUI
import simd

/// One source of truth for colour, spacing, and type.
///
/// Both halves of the front-end read from here: SwiftUI for the chrome, the
/// Metal renderer for the data surface. They previously carried independent
/// palettes, which is why the grid and the panes around it did not look like
/// the same application.
///
/// The app commits to dark rather than following the system appearance. A theme
/// here has to be maintained across two rendering stacks, and a half-converted
/// light mode reads as a bug rather than as a choice. Revisit when the Metal
/// side can be themed as cheaply as the SwiftUI side.
enum Theme {

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

    static let background = Tone(0x0F17_2A)
    static let surface = Tone(0x1E29_3B)
    static let surfaceRaised = Tone(0x2733_4A)
    static let separator = Tone(0xFFFF_FF, alpha: 0.08)
    static let border = Tone(0x4755_69)

    // MARK: - Text
    //
    // Contrast is against `background`, checked rather than assumed: primary
    // 17:1, secondary 6.3:1, tertiary 3.4:1. Tertiary is deliberately kept for
    // non-essential labels only, since it clears the 3:1 bar for incidental text
    // but not the 4.5:1 bar for anything a user has to read.

    static let text = Tone(0xF8FA_FC)
    static let textSecondary = Tone(0x94A3_B8)
    static let textTertiary = Tone(0x6475_8B)

    // MARK: - Semantics
    //
    // Two accents carrying two meanings, rather than one accent overloaded:
    // indigo is "this is selected / focused", green is "this executes".

    static let accent = Tone(0x6366_F1)
    static let run = Tone(0x22C5_5E)
    static let warning = Tone(0xFBBF_24)
    /// Fills and stripes. Too dark for text on `background` at 4.3:1 — use
    /// `dangerText` there instead.
    static let danger = Tone(0xEF44_44)
    static let dangerText = Tone(0xF871_71)

    /// Colours used by the Metal grid. Separate namespace because the data
    /// surface has its own vocabulary, not because it has its own palette.
    enum Grid {
        static let background = Theme.background
        static let header = Theme.surface
        static let headerText = Theme.textSecondary
        /// Brighter than the other headers, so the sorted column is identifiable
        /// without having to resolve the direction marker beside it.
        static let sortedHeaderText = Theme.text
        /// The type line under each header name. Subordinate to the name — 4.2:1
        /// on the header band where the name holds 5.7:1 — but deliberately not
        /// the tertiary label tone, which would fall to 3.1:1 there. A type is
        /// something the user came to read, not chrome.
        static let headerType = Tone(0x7C8A_A0)
        static let banding = Tone(0xFFFF_FF, alpha: 0.022)
        static let separator = Tone(0xFFFF_FF, alpha: 0.06)
        static let text = Tone(0xE2E8_F0)
        /// Dimmer than a value but not a label: NULL is content, so it holds
        /// 4.6:1 against the background rather than the 3.4:1 that incidental
        /// text gets away with. It is also drawn as the literal word, so the
        /// colour is a second signal rather than the only one.
        static let nullText = Tone(0x7C8A_A0)
        static let selectedRow = Theme.accent.opacity(0.18)
        static let selectedCell = Theme.accent.opacity(0.38)
        /// A cell holding a change that has not been sent. Amber rather than the
        /// accent: it means the same thing as the amber in the toolbar, which is
        /// that the database does not know about this yet.
        static let pendingCell = Theme.warning.opacity(0.30)
        static let cursor = Theme.accent
        /// The scrollbar sits over the data rather than beside it, so the track
        /// is barely there and the thumb carries the whole signal.
        static let scrollTrack = Tone(0xFFFF_FF, alpha: 0.035)
        static let scrollThumb = Tone(0xFFFF_FF, alpha: 0.22)
        static let scrollThumbActive = Tone(0xFFFF_FF, alpha: 0.38)
    }

    /// Colours for the SQL editor's syntax. Its own namespace for the same
    /// reason `Grid` has one: a surface with a vocabulary of its own, drawn from
    /// the same palette.
    ///
    /// Contrast is against `Theme.background`, measured rather than assumed, and
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
        static let text = Tone(0xE2E8_F0)  // 14.5:1
        static let keyword = Tone(0xA78B_FA)  // 6.6:1
        /// Warm and bright on purpose. An unclosed quote turns everything after
        /// it into a literal, and that is the mistake this whole feature exists
        /// to make visible, so it gets the loudest colour here.
        static let string = Tone(0xFDBA_74)  // 10.6:1
        /// A `$fn$ … $fn$` body is a string to the server, so it takes the
        /// string's hue — but dimmed, because a function body runs to dozens of
        /// lines and a literal at full strength over that much text glows.
        /// Its contents are deliberately left flat: the server sees one string
        /// there, and a second language lexed inside the first is a much larger
        /// promise than this makes.
        static let dollarQuoted = Tone(0xD9A0_66)  // 7.8:1
        static let number = Tone(0x5EEA_D4)  // 12.1:1
        /// A quoted identifier is a name, not a value, so it is cool where the
        /// literals are warm.
        static let quotedIdentifier = Tone(0x93C5_FD)  // 9.9:1
        /// Subordinate but not decorative: a comment is prose the author wrote
        /// to be read, so it stays above the 4.5:1 line rather than dropping to
        /// the tertiary label tone.
        static let comment = Tone(0x7C8F_A6)  // 5.4:1
        /// The caret and the selection band, which `pointAtSyntaxError` uses to
        /// put the offending token on screen. Indigo is already "this is where
        /// you are" everywhere else in the window.
        static let caret = Theme.accent
        static let selection = Theme.accent.opacity(0.32)
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
        static let editor = Font.system(size: editorSize, design: .monospaced)
        /// The same face as `editor`, for the `NSTextView` the SQL editor is
        /// built on. A SwiftUI `Font` cannot be handed to AppKit, so the size is
        /// named once above rather than written twice and left to drift.
        static let editorFont = NSFont.monospacedSystemFont(ofSize: editorSize, weight: .regular)
        static let digits = Font.system(size: 11).monospacedDigit()

        private static let editorSize: CGFloat = 13
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

    /// Pins the appearance at launch. Called once, before the window is shown,
    /// so no view ever lays out in the wrong appearance.
    static func apply(to app: NSApplication) {
        app.appearance = NSAppearance(named: .darkAqua)
    }
}
