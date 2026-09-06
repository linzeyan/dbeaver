import AppKit
import Foundation

/// Executable checks for the palette's two sets of values, run by
/// `--verify-theme`.
///
/// A theme is a promise made in numbers, and the numbers are the part nobody
/// re-reads: a tone nudged to look better on one surface is a tone that may have
/// stopped being legible on another, and nothing on screen says so. So the
/// promises `Theme` states in prose are asserted here — every text tone clears
/// its bar against every surface it is drawn on, in both appearances; the ramp
/// runs the direction each appearance claims; no token has one value where it
/// should have two.
///
/// Contrast is computed the way WCAG defines it, from the sRGB values the tones
/// already carry, which is why this can be a check at all rather than a note in
/// a design file.
///
/// Behind a flag on the binary for the reason `EditorThemeChecks` gives.
@MainActor
enum ThemeChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        defer { ScratchDefaults.release() }
        checkTheRampRunsBothWays()
        checkAnIconButtonAnswersAPressMoreStronglyThanAHover()
        checkTextClearsItsBarOnEverySurfaceItIsDrawnOn()
        checkTheFaintToneIsFainterThanTheMutedOne()
        checkAConnectionMarkIsVisibleInBothAppearances()
        checkEverySyntaxColourIsReadableOnTheEditorsBackground()
        checkNoTokenWasLeftWithOneValue()
        checkASettingNamesTheAppearanceItAsksFor()
        checkAnUntouchedEditorSlotFollowsTheAppearanceAndAChosenOneDoesNot()
        if failures == 0 {
            fputs("theme: all checks passed\n", stderr)
        } else {
            fputs("theme: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Contrast

    /// Relative luminance, WCAG 2.1 §relative luminance.
    ///
    /// Alpha is ignored, which is correct for every tone this asks about: they
    /// are opaque. The translucent ones — banding, the selection fills, the
    /// scrollbar — are marks over content rather than text on a surface, and a
    /// ratio computed as though they were opaque would be a number about nothing.
    private static func luminance(_ tone: Theme.Tone) -> Double {
        func channel(_ value: Double) -> Double {
            value <= 0.03928 ? value / 12.92 : pow((value + 0.055) / 1.055, 2.4)
        }
        return 0.2126 * channel(tone.r) + 0.7152 * channel(tone.g) + 0.0722 * channel(tone.b)
    }

    private static func contrast(_ one: Theme.Tone, _ other: Theme.Tone) -> Double {
        let (high, low) = (
            max(luminance(one), luminance(other)), min(luminance(one), luminance(other))
        )
        return (high + 0.05) / (low + 0.05)
    }

    /// The three surfaces by name, as the appearance in force resolves them.
    private static var surfaces: [(name: String, tone: Theme.Tone)] {
        [
            ("canvas", Theme.Surface.canvas),
            ("raised", Theme.Surface.raised),
            ("overlay", Theme.Surface.overlay)
        ]
    }

    /// Runs a body under each appearance, naming which one failed.
    private static func inBothAppearances(_ body: (String) -> Void) {
        for light in [false, true] {
            Theme.resolving(isLight: light) { body(light ? "light" : "dark") }
        }
    }

    // MARK: - The checks

    /// The canvas is the surface furthest from the text on it, either way up.
    ///
    /// This is the whole of what "the ramp inverts rather than lightens" means,
    /// and it is the thing a second set of values gets wrong first: a light
    /// theme assembled by picking pleasant greys, rather than by inverting the
    /// order, ends up with panels lighter than the window they sit on and a grid
    /// that reads as a hole.
    private static func checkTheRampRunsBothWays() {
        Theme.resolving(isLight: false) {
            let canvas = luminance(Theme.Surface.canvas)
            let raised = luminance(Theme.Surface.raised)
            let overlay = luminance(Theme.Surface.overlay)
            expect(canvas < raised, true, "dark: the canvas is darker than a panel on it")
            expect(raised < overlay, true, "dark: a panel is darker than the strip above it")
        }
        Theme.resolving(isLight: true) {
            let canvas = luminance(Theme.Surface.canvas)
            let raised = luminance(Theme.Surface.raised)
            let overlay = luminance(Theme.Surface.overlay)
            expect(canvas > raised, true, "light: the canvas is lighter than a panel on it")
            expect(raised > overlay, true, "light: a panel is lighter than the strip above it")
        }
    }

    /// A press reads as heavier than a hover, both ways up.
    ///
    /// A bare icon button has nothing else to say it with. There is no label to
    /// change and the foreground belongs to the call site, so the fill carries
    /// the whole of the state — which means the two live states have to differ,
    /// and they have to differ in the same *direction* under both sets of
    /// values. "Pressed is brighter" is right on dark and backwards on light,
    /// which is the mistake `checkTheRampRunsBothWays` exists to catch one level
    /// up; this is the same mistake made in a component instead of in the ramp.
    private static func checkAnIconButtonAnswersAPressMoreStronglyThanAHover() {
        expect(
            IconButtonStyle.fill(hovering: false, pressed: false, enabled: true) == nil, true,
            "an icon button nobody is pointing at draws no fill at all")
        for pressed in [false, true] {
            for hovering in [false, true] {
                expect(
                    IconButtonStyle.fill(hovering: hovering, pressed: pressed, enabled: false)
                        == nil, true,
                    "a disabled icon button answers nothing (hovering: \(hovering),"
                        + " pressed: \(pressed))")
            }
        }
        inBothAppearances { appearance in
            guard
                let hover = IconButtonStyle.fill(hovering: true, pressed: false, enabled: true),
                let press = IconButtonStyle.fill(hovering: true, pressed: true, enabled: true)
            else {
                failures += 1
                fputs("theme FAIL: \(appearance): a live icon button drew nothing\n", stderr)
                return
            }
            expect(
                luminance(press) != luminance(hover), true,
                "\(appearance): a press differs from a hover, or pressing says nothing new")
            let canvas = luminance(Theme.Surface.canvas)
            expect(
                abs(luminance(press) - canvas) > abs(luminance(hover) - canvas), true,
                "\(appearance): a press sits further from the canvas than a hover does")
        }
    }

    /// Every text tone against every surface, against the bar its own role sets.
    ///
    /// Against all three rather than against the one it is mostly drawn on,
    /// which is the mistake this file exists to make impossible to repeat: the
    /// tertiary tone was documented at 3.8:1 for years on a measurement taken
    /// against the canvas, while the chrome drew it on the two lighter surfaces
    /// at 3.1:1 and 2.7:1.
    private static func checkTextClearsItsBarOnEverySurfaceItIsDrawnOn() {
        // The bars are the roles, not the current values: primary is read at
        // length, secondary is read, tertiary is glanced at. A tone that clears
        // its bar by a mile is fine; one that clears the value it happens to
        // have today is not a check.
        let bars: [(name: String, tone: () -> Theme.Tone, floor: Double)] = [
            ("primary", { Theme.Text.primary }, 7),
            ("secondary", { Theme.Text.secondary }, 4.5),
            ("tertiary", { Theme.Text.tertiary }, 3),
            ("dataMuted", { Theme.Text.dataMuted }, 3.5)
        ]
        inBothAppearances { appearance in
            for bar in bars {
                for surface in surfaces {
                    let ratio = contrast(bar.tone(), surface.tone)
                    expect(
                        ratio >= bar.floor, true,
                        "\(appearance): \(bar.name) on \(surface.name) is "
                            + "\(rounded(ratio)):1, and owes \(bar.floor):1")
                }
            }
        }
    }

    /// DEFAULT is dimmer than NULL, which is dimmer than a value.
    ///
    /// The three are drawn in the same cell of the same grid and mean three
    /// different things — what the row holds, what it holds nothing of, and what
    /// the table will decide — so the order matters more than any of the three
    /// numbers. Against the canvas, which is what the grid draws them on.
    private static func checkTheFaintToneIsFainterThanTheMutedOne() {
        inBothAppearances { appearance in
            let value = contrast(Theme.Grid.text, Theme.Surface.canvas)
            let null = contrast(Theme.Text.dataMuted, Theme.Surface.canvas)
            let unset = contrast(Theme.Text.dataFaint, Theme.Surface.canvas)
            expect(value > null, true, "\(appearance): a value stands out more than NULL")
            expect(null > unset, true, "\(appearance): NULL stands out more than DEFAULT")
            expect(
                unset >= 3, true,
                "\(appearance): DEFAULT is at \(rounded(unset)):1, under the 3:1 a word "
                    + "somebody has to read owes even when it is the dimmest one")
        }
    }

    /// All seven connection marks clear the bar a mark that is not text owes.
    ///
    /// 3:1 is the floor for a non-text signal, and this asks for more, because
    /// the mark is a 3pt stripe: a ratio that passes on paper at that size is
    /// still one somebody has to hunt for. The point of the mark is telling
    /// production from staging at a glance.
    private static func checkAConnectionMarkIsVisibleInBothAppearances() {
        let marks: [(name: String, tone: () -> Theme.Tone)] = [
            ("red", { Theme.Connection.red }),
            ("orange", { Theme.Connection.orange }),
            ("yellow", { Theme.Connection.yellow }),
            ("green", { Theme.Connection.green }),
            ("blue", { Theme.Connection.blue }),
            ("purple", { Theme.Connection.purple }),
            ("grey", { Theme.Connection.grey })
        ]
        inBothAppearances { appearance in
            for mark in marks {
                let ratio = contrast(mark.tone(), Theme.Surface.canvas)
                expect(
                    ratio >= 4, true,
                    "\(appearance): the \(mark.name) connection mark is \(rounded(ratio)):1 "
                        + "on the sidebar")
            }
        }
        // And no two of them are the same colour, which is the other half of
        // what "seven marks" promises: a list where two entries look alike is a
        // list with six useful entries and a trap.
        inBothAppearances { appearance in
            let hexes = Set(marks.map { $0.tone().hex })
            expect(hexes.count, marks.count, "\(appearance): the seven marks are seven colours")
        }
    }

    /// Every syntax colour is read, so every one of them owes 4.5:1.
    ///
    /// Against the editor's own background rather than the canvas, because the
    /// editor's background is a preference and the syntax colours are measured
    /// against whatever it currently is — which for the default theme is the
    /// canvas, and is checked here as the palette resolves it.
    private static func checkEverySyntaxColourIsReadableOnTheEditorsBackground() {
        let slots: [(name: String, tone: () -> Theme.Tone)] = [
            ("text", { Theme.Editor.text }),
            ("keyword", { Theme.Editor.keyword }),
            ("string", { Theme.Editor.string }),
            ("dollarQuoted", { Theme.Editor.dollarQuoted }),
            ("number", { Theme.Editor.number }),
            ("quotedIdentifier", { Theme.Editor.quotedIdentifier }),
            ("comment", { Theme.Editor.comment })
        ]
        inBothAppearances { appearance in
            for slot in slots {
                let ratio = contrast(slot.tone(), EditorTheme.defaults.background)
                expect(
                    ratio >= 4.5, true,
                    "\(appearance): the editor's \(slot.name) is \(rounded(ratio)):1, and "
                        + "every token in the editor is text somebody is reading")
            }
        }
    }

    /// No token resolves to the same value in both appearances.
    ///
    /// This is the check for the failure mode a second palette has that a first
    /// one cannot: a token added later with one value, which looks right in
    /// whichever appearance its author was in and is invisible in the other. The
    /// list is written out by hand so that adding a token means answering this
    /// question about it.
    private static func checkNoTokenWasLeftWithOneValue() {
        let tokens: [(name: String, tone: () -> Theme.Tone)] = [
            ("Surface.canvas", { Theme.Surface.canvas }),
            ("Surface.raised", { Theme.Surface.raised }),
            ("Surface.overlay", { Theme.Surface.overlay }),
            ("Border.hairline", { Theme.Border.hairline }),
            ("Border.control", { Theme.Border.control }),
            ("Text.primary", { Theme.Text.primary }),
            ("Text.secondary", { Theme.Text.secondary }),
            ("Text.tertiary", { Theme.Text.tertiary }),
            ("Text.dataMuted", { Theme.Text.dataMuted }),
            ("Text.dataFaint", { Theme.Text.dataFaint }),
            ("Accent.selection", { Theme.Accent.selection }),
            ("Accent.execute", { Theme.Accent.execute }),
            ("Semantic.warning", { Theme.Semantic.warning }),
            ("Semantic.danger", { Theme.Semantic.danger }),
            ("Semantic.dangerText", { Theme.Semantic.dangerText }),
            ("Grid.banding", { Theme.Grid.banding }),
            ("Grid.separator", { Theme.Grid.separator }),
            ("Grid.text", { Theme.Grid.text }),
            ("Grid.scrollTrack", { Theme.Grid.scrollTrack }),
            ("Grid.scrollThumb", { Theme.Grid.scrollThumb }),
            ("Grid.scrollThumbActive", { Theme.Grid.scrollThumbActive }),
            ("Editor.text", { Theme.Editor.text }),
            ("Editor.keyword", { Theme.Editor.keyword }),
            ("Editor.string", { Theme.Editor.string }),
            ("Editor.dollarQuoted", { Theme.Editor.dollarQuoted }),
            ("Editor.number", { Theme.Editor.number }),
            ("Editor.quotedIdentifier", { Theme.Editor.quotedIdentifier }),
            ("Editor.comment", { Theme.Editor.comment }),
            ("Connection.red", { Theme.Connection.red }),
            ("Connection.orange", { Theme.Connection.orange }),
            ("Connection.yellow", { Theme.Connection.yellow }),
            ("Connection.green", { Theme.Connection.green }),
            ("Connection.blue", { Theme.Connection.blue }),
            ("Connection.purple", { Theme.Connection.purple }),
            ("Connection.grey", { Theme.Connection.grey })
        ]
        for token in tokens {
            let dark = Theme.resolving(isLight: false) { token.tone().hex }
            let light = Theme.resolving(isLight: true) { token.tone().hex }
            expect(dark != light, true, "\(token.name) is \(dark) in both appearances")
        }
    }

    /// The preference asks AppKit for the appearance it is named for.
    ///
    /// `system` is `nil` rather than a third appearance, which is the whole of
    /// how "follow along" is spelled; getting that wrong pins the window to
    /// whatever it launched in and the bug is invisible until sunset.
    private static func checkASettingNamesTheAppearanceItAsksFor() {
        expect(Appearance.Setting.system.nsAppearance == nil, true, "system asks for nothing")
        expect(Appearance.Setting.light.nsAppearance?.name, .aqua, "light asks for aqua")
        expect(Appearance.Setting.dark.nsAppearance?.name, .darkAqua, "dark asks for darkAqua")
        // And the reading back, which is what the controller resolves the
        // palette from — the preference cannot answer this under `system`.
        expect(
            Theme.isLight(NSAppearance(named: .aqua)!), true, "aqua reads as the light palette")
        expect(
            Theme.isLight(NSAppearance(named: .darkAqua)!), false,
            "darkAqua reads as the dark palette")
        // A preference nothing recognises is the system's, not a crash and not
        // black-on-black: a plist edited by hand, or written by a later version
        // offering a fourth appearance, still opens a window.
        expect(Appearance.Setting(rawValue: "sepia") ?? .system, .system, "an unknown name")
    }

    /// A slot the user never touched follows the appearance; one they chose
    /// stays theirs.
    ///
    /// The two halves are the whole design: eight editor colours nobody picked
    /// were chosen to be read on a near-black canvas and have no business
    /// staying there when the canvas turns white, and the one colour somebody
    /// did pick is theirs in both appearances. Told apart by the spelling stored
    /// for each slot, which is why the switch reads the palette it is leaving
    /// before it moves.
    private static func checkAnUntouchedEditorSlotFollowsTheAppearanceAndAChosenOneDoesNot() {
        let preferences = Preferences(store: ScratchDefaults.store("theme"))
        // Somebody's own keyword colour, which is nothing the palette would
        // have written in either appearance.
        preferences.editorKeywordColor = "#FF00FF"
        let before = Theme.resolving(isLight: false) { EditorTheme.defaults }
        expect(
            preferences.editorCommentColor, before.comment.hex,
            "the slot nobody touched starts at the dark palette's spelling")

        Theme.resolving(isLight: true) { preferences.followEditorPalette(from: before) }

        let light = Theme.resolving(isLight: true) { EditorTheme.defaults }
        expect(
            preferences.editorCommentColor, light.comment.hex,
            "the untouched comment colour moved to the light palette")
        expect(
            preferences.editorBackgroundColor, light.background.hex,
            "so did the background, which is the one that would be white on white")
        expect(preferences.editorKeywordColor, "#FF00FF", "the chosen keyword colour stayed")
        // And the pane still says somebody customised it, because somebody did.
        let custom = Theme.resolving(isLight: true) { preferences.editorThemeIsCustom }
        expect(custom, true, "one chosen colour is still a custom theme")
    }

    // MARK: - Harness

    private static func rounded(_ value: Double) -> String {
        String(format: "%.1f", value)
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("theme FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
