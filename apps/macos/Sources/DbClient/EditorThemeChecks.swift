import AppKit
import Foundation
import SwiftUI

/// Executable checks for the editor's colour theme, run by
/// `--verify-editor-theme`.
///
/// The theme is an indirection — token to user colour to drawn colour — and
/// every failure mode of an indirection is a value quietly not travelling: a
/// default that differs from the palette it claims to be, an override that
/// does not survive the store, a menu reading Default over customised
/// colours, a misspelt hex drawn as black. Each of those is pinned here,
/// against a scratch store for the reason `PreferencesChecks` uses one.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so
/// a test target would have to reproduce that link.
@MainActor
enum EditorThemeChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        defer { ScratchDefaults.release() }
        checkTheHexCodecRoundTrips()
        checkTheDefaultThemeIsThePaletteItself()
        checkASetColourSurvivesTheStore()
        checkAnyEditedColourReadsAsCustom()
        checkResetRestoresEveryDefault()
        checkAnUnparseableColourFallsBackToTheDefault()
        checkAHandEditedSpellingIsFoldedToCanonical()
        if failures == 0 {
            fputs("editor-theme: all checks passed\n", stderr)
        } else {
            fputs("editor-theme: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    /// Every slot by name, for the checks that visit each one. Named so a
    /// failure says which slot rather than "one of eleven".
    private static let slots:
        [(
            name: String, keyPath: ReferenceWritableKeyPath<Preferences, String>,
            shipped: Theme.Tone
        )] = [
            ("background", \.editorBackgroundColor, EditorTheme.defaults.background),
            ("text", \.editorTextColor, EditorTheme.defaults.text),
            ("keyword", \.editorKeywordColor, EditorTheme.defaults.keyword),
            ("string", \.editorStringColor, EditorTheme.defaults.string),
            ("dollar-quoted", \.editorDollarQuotedColor, EditorTheme.defaults.dollarQuoted),
            ("number", \.editorNumberColor, EditorTheme.defaults.number),
            (
                "quoted identifier", \.editorQuotedIdentifierColor,
                EditorTheme.defaults.quotedIdentifier
            ),
            ("comment", \.editorCommentColor, EditorTheme.defaults.comment),
            ("caret", \.editorCaretColor, EditorTheme.defaults.caret),
            ("selection", \.editorSelectionColor, EditorTheme.defaults.selection),
            ("statement", \.editorStatementColor, EditorTheme.defaults.statement)
        ]

    // MARK: - The codec

    /// The two spellings read and the one spelling written, exactly.
    ///
    /// The codec is the boundary between the plist and the palette, so its
    /// edges are pinned by value: a byte off in either direction is a colour
    /// the user did not pick, and "canonical" is what lets two strings answer
    /// "is this still the default?".
    private static func checkTheHexCodecRoundTrips() {
        expect(Theme.Tone(hex: "#112233")?.hex, "#112233", "six digits come back as themselves")
        expect(Theme.Tone(hex: "#6366F152")?.hex, "#6366F152", "eight digits keep their alpha")
        expect(Theme.Tone(hex: "112233")?.hex, "#112233", "the # is optional on the way in")
        expect(
            Theme.Tone(hex: "#a78bfa")?.hex, "#A78BFA",
            "lower case reads, and comes back canonical")
        expect(
            Theme.Tone(hex: "#AABBCCFF")?.hex, "#AABBCC",
            "a spelled-out full alpha collapses to the six-digit form")
        expect(Theme.Tone(hex: "#12345") == nil, true, "five digits are refused, not padded")
        expect(Theme.Tone(hex: "#GGHHII") == nil, true, "and so are letters past F")
        expect(
            Theme.Tone(hex: "+112233") == nil, true,
            "and the leading + the integer parser would take")
        expect(
            Theme.Tone(Color(.sRGB, red: 1, green: 0.5, blue: 0, opacity: 1))?.hex, "#FF8000",
            "a colour well's answer lands on the nearest byte")
    }

    // MARK: - The theme

    /// A fresh install draws exactly what the build has always drawn.
    ///
    /// Not merely "the defaults round-trip": the resolved tones are compared
    /// to `Theme.Editor`'s own values, because the one way the Default theme
    /// can be wrong is by being a near-copy of the palette — quantised, or
    /// paired against the wrong slot — and near is invisible until a designer
    /// asks why the selection band changed.
    private static func checkTheDefaultThemeIsThePaletteItself() {
        let fresh = scratch()
        expect(fresh.editorThemeIsCustom, false, "a fresh install is the Default theme")
        for slot in slots {
            expect(
                fresh[keyPath: slot.keyPath], slot.shipped.hex,
                "the \(slot.name) colour starts as the palette's own spelling")
        }
        expect(
            fresh.editorTheme == EditorTheme.defaults, true,
            "and the resolved theme is the palette's tones exactly — Default changes nothing")
    }

    /// An override has to outlive the window, or the colour wells are switches
    /// that reset every launch.
    private static func checkASetColourSurvivesTheStore() {
        let store = ScratchDefaults.store("verify-editor-theme")
        let first = Preferences(store: store)
        first.editorKeywordColor = "#112233"

        // A second reader over the same store, which is what the next launch is.
        let second = Preferences(store: store)
        expect(second.editorKeywordColor, "#112233", "the keyword colour was kept")
        expect(
            second.editorTheme.keyword == Theme.Tone(hex: "#112233"), true,
            "and is what the editor now draws keywords in")
        expect(
            second.editorTheme.string == EditorTheme.defaults.string, true,
            "while an untouched slot stays the palette's")
    }

    /// Touching any one well makes the theme Custom — and only touching does.
    ///
    /// Every slot is tried, because the menu's fact is an || over eleven
    /// comparisons and a slot left out of it would customise invisibly.
    private static func checkAnyEditedColourReadsAsCustom() {
        for slot in slots {
            let fresh = scratch()
            fresh[keyPath: slot.keyPath] = "#123456"
            expect(
                fresh.editorThemeIsCustom, true,
                "changing the \(slot.name) colour alone makes the theme Custom")
        }
        // Re-stating a default is not a customisation: the menu describes the
        // colours, not the history of the wells.
        let fresh = scratch()
        fresh.editorKeywordColor = EditorTheme.defaults.keyword.hex
        expect(fresh.editorThemeIsCustom, false, "writing the default back is still Default")
    }

    /// Reset takes all eleven slots back, on this launch and the next.
    private static func checkResetRestoresEveryDefault() {
        let store = ScratchDefaults.store("verify-editor-theme-reset")
        let preferences = Preferences(store: store)
        for slot in slots { preferences[keyPath: slot.keyPath] = "#123456" }
        expect(preferences.editorThemeIsCustom, true, "eleven changed colours are a Custom theme")

        preferences.resetEditorTheme()
        for slot in slots {
            expect(
                preferences[keyPath: slot.keyPath], slot.shipped.hex,
                "the \(slot.name) colour is the palette's again")
        }
        expect(preferences.editorThemeIsCustom, false, "and the menu reads Default again")
        expect(
            Preferences(store: store).editorThemeIsCustom, false,
            "on the next launch as well — the reset reached the store")
    }

    /// A value the wells could never have written reads as the default.
    ///
    /// The plist is a file somebody can edit, and most colour APIs make black
    /// of a string they cannot read — an editor drawn all-black over one
    /// misspelt digit, with the well showing the black it will keep writing.
    /// The keys are spelled out here because they are a contract with the
    /// disk; a renamed key would silently orphan every kept colour.
    private static func checkAnUnparseableColourFallsBackToTheDefault() {
        let store = ScratchDefaults.store("verify-editor-theme-hex")
        store.set("chartreuse", forKey: "dev.dbclient.editorKeywordColor")
        store.set("#12345", forKey: "dev.dbclient.editorStringColor")
        store.set("#GGHHII", forKey: "dev.dbclient.editorNumberColor")

        let preferences = Preferences(store: store)
        expect(
            preferences.editorTheme.keyword == EditorTheme.defaults.keyword, true,
            "a colour name is not a colour — keywords stay the palette's, not black")
        expect(
            preferences.editorTheme.string == EditorTheme.defaults.string, true,
            "five digits are not a colour either")
        expect(
            preferences.editorTheme.number == EditorTheme.defaults.number, true,
            "nor digits past F")
        expect(
            preferences.editorKeywordColor, EditorTheme.defaults.keyword.hex,
            "and the property holds the default's spelling, ready for the well")
        expect(
            preferences.editorThemeIsCustom, false,
            "so a broken plist is the Default theme, not a Custom one")
    }

    /// `#a78bfa` typed into the plist by hand is the default keyword colour,
    /// not a Custom theme: spellings are folded to canonical before anything
    /// compares them.
    private static func checkAHandEditedSpellingIsFoldedToCanonical() {
        let store = ScratchDefaults.store("verify-editor-theme-case")
        store.set("#a78bfa", forKey: "dev.dbclient.editorKeywordColor")
        let preferences = Preferences(store: store)
        expect(preferences.editorKeywordColor, "#A78BFA", "the spelling reads back canonical")
        expect(preferences.editorThemeIsCustom, false, "and the theme still reads Default")
    }

    // MARK: - Harness

    /// A preferences store nothing else can see; see `PreferencesChecks`.
    private static func scratch() -> Preferences {
        Preferences(store: ScratchDefaults.store("verify-editor-theme"))
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("editor-theme FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}
