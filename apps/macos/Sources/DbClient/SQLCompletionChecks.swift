import Foundation

/// Executable checks for `SQLCompletion`, run by `--verify-completion`.
///
/// What to offer at a caret is the core's business and is tested in
/// `crates/sql` and `crates/catalog`. Restating any of it here would be a second
/// copy of a rule, which is a rule that will disagree with the first one the day
/// it is corrected.
///
/// What is checked here is the seam and this side's own rules: that a payload
/// decodes into the fields the popup reads, that a kind this build has never
/// heard of does not empty the list, that the span to replace survives the
/// crossing from Unicode scalars to UTF-16 units, and that the editor asks for
/// offers when a name is being typed and stays quiet the rest of the time.
///
/// Behind a flag on the binary for the reason `SQLScriptChecks` gives: the
/// package declares one executable target and it links the Rust staticlib, so a
/// test target would have to reproduce that link.
enum SQLCompletionChecks {
    private static var failures = 0

    static func run() -> Bool {
        failures = 0
        checkAnswersDecodeIntoWhatThePopupReads()
        checkAnUnknownKindDoesNotEmptyTheList()
        checkTheSpanToReplaceSurvivesTheCrossing()
        checkOffersAreAskedForWhileANameIsBeingTyped()
        if failures == 0 {
            fputs("completion: all checks passed\n", stderr)
        } else {
            fputs("completion: \(failures) check(s) failed\n", stderr)
        }
        return failures == 0
    }

    // MARK: - Cases

    /// The wire shape `db_complete_json` documents arrives as the fields the
    /// list is drawn from.
    ///
    /// `label` and `insert` are the pair worth being sure of: they differ
    /// exactly when a name needs quoting, and a front end that drew one and
    /// inserted the other would produce SQL that finds nothing — while looking
    /// right on screen.
    private static func checkAnswersDecodeIntoWhatThePopupReads() {
        let answer = decoded(
            """
            {"start":14,"end":21,"offers":[
              {"label":"Order Lines","insert":"\\"Order Lines\\"","kind":"relation",
               "detail":"table in public"},
              {"label":"id","insert":"id","kind":"column","detail":"integer · orders"}]}
            """)
        expect(answer?.replacing, 14..<21, "the span to replace arrives as a range")
        expect(answer?.offers.count, 2, "both offers arrive")
        expect(answer?.offers.first?.label, "Order Lines", "shown as the catalog holds it")
        expect(
            answer?.offers.first?.insert, "\"Order Lines\"",
            "and inserted as this database will read it")
        expect(answer?.offers.first?.kind, .relation, "the kind arrives as the kind it is")
        expect(answer?.offers.last?.detail, "integer · orders", "and the second line with it")

        // A caret with nothing typed yet replaces nothing, which is a range and
        // not a missing field.
        expect(decoded(#"{"start":7,"end":7,"offers":[]}"#)?.replacing, 7..<7, "an empty span")
        // Two numbers arriving the wrong way round would trap `Range` on
        // construction, in the middle of a keystroke, taking the window with it.
        expect(decoded(#"{"start":9,"end":4,"offers":[]}"#)?.replacing, 9..<9, "a reversed pair")
    }

    /// A kind the core grew and this build has not heard of is shown, not
    /// dropped.
    ///
    /// The alternative is a decode failure, which loses the whole list — every
    /// column of the relation gone because one entry said a word this build did
    /// not recognise.
    private static func checkAnUnknownKindDoesNotEmptyTheList() {
        let answer = decoded(
            #"{"start":0,"end":0,"offers":[{"label":"x","insert":"x","kind":"procedure","detail":""}]}"#
        )
        expect(answer?.offers.count, 1, "the offer survives a kind this build does not know")
        expect(answer?.offers.first?.kind, .unknown, "and is shown as the unknown it is")
    }

    /// The span the core names is the span the editor replaces.
    ///
    /// The core counts Unicode scalars and `NSTextView` edits in UTF-16 units.
    /// They agree on every character in the Basic Multilingual Plane and
    /// disagree on every emoji, so this is invisible until a buffer holds one —
    /// and then accepting a suggestion eats a character the user can see.
    private static func checkTheSpanToReplaceSurvivesTheCrossing() {
        let plain = "SELECT * FROM bench_w"
        expect(
            SQLCompletion.utf16Range(of: 14..<21, in: plain), NSRange(location: 14, length: 7),
            "a plain buffer counts the same in both units")

        let wide = "SELECT '🇹🇼', bench_w"
        // The flag is two scalars and four UTF-16 units — two regional
        // indicators, each outside the Basic Multilingual Plane — so the word
        // after it starts two places further along in AppKit's count than in the
        // core's.
        expect(
            SQLCompletion.utf16Range(of: 13..<20, in: wide), NSRange(location: 15, length: 7),
            "a multi-scalar character moves the span it precedes")
        expect(
            wide.utf16Slice(SQLCompletion.utf16Range(of: 13..<20, in: wide)), "bench_w",
            "and the range still names the word being typed")
        expect(
            SQLCompletion.utf16Range(of: 0..<999, in: plain), nil,
            "a span the buffer cannot hold names nothing rather than trapping")
    }

    /// The editor asks while a name is being typed, and not otherwise.
    ///
    /// This is the whole of what separates a completion that helps from one that
    /// is in the way. A list that opens on every keystroke covers the buffer
    /// while somebody types `3` into a WHERE clause; one that never opens by
    /// itself is a feature nobody finds.
    private static func checkOffersAreAskedForWhileANameIsBeingTyped() {
        func asks(_ marked: String) -> Bool {
            let caret = marked.unicodeScalars.firstIndex(of: "▮").map {
                marked.unicodeScalars.distance(from: marked.unicodeScalars.startIndex, to: $0)
            }
            let text = marked.replacingOccurrences(of: "▮", with: "")
            return SQLCompletion.wantsOffers(before: caret ?? 0, in: text)
        }

        expect(asks("SELECT ord▮"), true, "a name being typed")
        expect(asks("SELECT * FROM public.▮"), true, "the dot that qualifies one")
        expect(asks("SELECT \"▮"), true, "the quote that opens one that needs quoting")
        expect(asks("SELECT bench_▮"), true, "an underscore is part of a name")
        expect(asks("SELECT 1▮"), true, "a digit can be, and 1 alone offers nothing anyway")

        expect(asks("SELECT * FROM ▮"), false, "a space, where ⌥Esc is how a user asks")
        expect(asks("▮"), false, "an empty buffer")
        expect(asks("SELECT (▮"), false, "punctuation")
        expect(asks("SELECT x = ▮"), false, "an operator")
    }

    // MARK: - Harness

    private static func decoded(_ json: String) -> SQLCompletion.Answer? {
        guard let data = json.data(using: .utf8) else { return nil }
        return try? JSONDecoder().decode(SQLCompletion.Answer.self, from: data)
    }

    private static func expect<T: Equatable>(_ got: T, _ want: T, _ what: String) {
        guard got != want else { return }
        failures += 1
        fputs("completion FAIL: \(what)\n  want: \(want)\n  got:  \(got)\n", stderr)
    }
}

extension String {
    /// The text a UTF-16 range names, for a check that has just converted one.
    fileprivate func utf16Slice(_ range: NSRange?) -> String? {
        guard let range, let indices = Range(range, in: self) else { return nil }
        return String(self[indices])
    }
}
