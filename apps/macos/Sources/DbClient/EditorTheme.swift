import AppKit
import SwiftUI

/// The SQL editor's colours as one value: what `Theme.Editor` decides by
/// default, and whatever the user overrode in Settings.
///
/// This is the indirection M1.8 is about. The editor used to read
/// `Theme.Editor` directly, which made its palette a compile-time fact; these
/// slots put one layer between a token and the colour it is drawn in, so a
/// value can come from somewhere else — today a colour well per slot, later
/// whole themes — without the editor learning where. The editor takes this
/// value rather than the `Preferences` it is resolved from, for the reason it
/// takes `EditorTyping.Rules`: the layer stays checkable as plain data, and
/// the pane's dependencies stay visible to SwiftUI.
struct EditorTheme: Equatable {
    /// What the whole pane is filled with, behind every token.
    var background: Theme.Tone
    /// Anything with no token of its own: identifiers, operators, punctuation.
    var text: Theme.Tone
    var keyword: Theme.Tone
    var string: Theme.Tone
    /// A `$fn$ … $fn$` body — string-hued but dimmed; see `Theme.Editor`.
    var dollarQuoted: Theme.Tone
    var number: Theme.Tone
    var quotedIdentifier: Theme.Tone
    var comment: Theme.Tone
    var caret: Theme.Tone
    var selection: Theme.Tone
    /// The band behind the statement ⌘R would run.
    var statement: Theme.Tone

    /// The palette as shipped: `Theme.Editor`, gathered. The one statement of
    /// which slot pairs with which default — `Preferences` registers these
    /// tones' hex and resolves untouched slots back to these very values, so
    /// "Default" is not a copy that can drift from the palette but the palette
    /// itself.
    ///
    /// Computed rather than stored, because the palette has two sets of values
    /// now: read under a light appearance it is the light defaults, and a stored
    /// `let` would have frozen whichever set the process launched in.
    static var defaults: EditorTheme {
        EditorTheme(
            background: Theme.Surface.canvas,
            text: Theme.Editor.text,
            keyword: Theme.Editor.keyword,
            string: Theme.Editor.string,
            dollarQuoted: Theme.Editor.dollarQuoted,
            number: Theme.Editor.number,
            quotedIdentifier: Theme.Editor.quotedIdentifier,
            comment: Theme.Editor.comment,
            caret: Theme.Editor.caret,
            selection: Theme.Editor.selection,
            statement: Theme.Editor.statement)
    }
}

/// Tones compare exactly, not perceptually. Only two things ever produce one —
/// `Theme`'s own values and the hex codec below — and both are deterministic,
/// so equality answers the one question asked of it, "is this slot still the
/// default?", without a tolerance that would need justifying.
extension Theme.Tone: Equatable {
    static func == (lhs: Theme.Tone, rhs: Theme.Tone) -> Bool {
        lhs.r == rhs.r && lhs.g == rhs.g && lhs.b == rhs.b && lhs.a == rhs.a
    }
}

extension Theme.Tone {
    /// Reads `#RRGGBB` or `#RRGGBBAA`, with or without the `#`.
    ///
    /// Refuses rather than guesses on anything else: the caller's fallback is
    /// the palette's default, and a half-parsed colour would be a third value
    /// nobody chose. The digit check is not redundant with the integer parse —
    /// `UInt64(_:radix:)` accepts a leading `+`, which is not a colour.
    init?(hex: String) {
        var digits = Substring(hex)
        if digits.first == "#" { digits = digits.dropFirst() }
        guard digits.count == 6 || digits.count == 8,
            digits.allSatisfy(\.isHexDigit),
            let value = UInt64(digits, radix: 16)
        else { return nil }
        if digits.count == 8 {
            self.init(UInt32(value >> 8), alpha: Double(value & 0xFF) / 255)
        } else {
            self.init(UInt32(value))
        }
    }

    /// The spelling `Preferences` keeps: six digits, eight only where the tone
    /// is translucent — the selection band is — and upper case throughout. One
    /// canonical form, so "is this still the default?" can be asked of two
    /// strings.
    var hex: String {
        func byte(_ value: Double) -> Int { Int((value * 255).rounded()) }
        return a == 1
            ? String(format: "#%02X%02X%02X", byte(r), byte(g), byte(b))
            : String(format: "#%02X%02X%02X%02X", byte(r), byte(g), byte(b), byte(a))
    }

    /// The tone a colour well hands back, in the sRGB the rest of `Tone`
    /// speaks. `nil` for a colour with no sRGB reading — a pattern, a catalog
    /// colour — which the caller keeps its old value over.
    init?(_ color: Color) {
        guard let srgb = NSColor(color).usingColorSpace(.sRGB) else { return nil }
        func byte(_ value: CGFloat) -> UInt32 { UInt32((min(max(value, 0), 1) * 255).rounded()) }
        self.init(
            byte(srgb.redComponent) << 16 | byte(srgb.greenComponent) << 8
                | byte(srgb.blueComponent),
            alpha: Double(min(max(srgb.alphaComponent, 0), 1)))
    }
}
