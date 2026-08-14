//! What the scanner has to get right, in every dialect that has an opinion.
//!
//! Most of these came from the Swift splitter this crate replaces, where they
//! were earned one bug at a time against real scripts. They are kept because
//! the rules are not obvious and a rewrite is exactly when they get lost.
//!
//! The dialect-specific ones below are the argument for the table in
//! `dialect.rs`: each is a case where two databases read the same characters
//! differently, and reading it the other database's way leaves the scanner one
//! quote out of step for the rest of the buffer.

use dbsql::{CLICKHOUSE, DUCKDB, Dialect, MSSQL, MYSQL, Origin, POSTGRES, SQLITE, TokenKind};

/// The statements `script` splits into, as text.
fn split(script: &str, dialect: &Dialect) -> Vec<String> {
    let chars: Vec<char> = script.chars().collect();
    dbsql::statements(script, dialect)
        .into_iter()
        .map(|s| chars[s.start as usize..s.end as usize].iter().collect())
        .collect()
}

/// The painted tokens, as `kind:text` pairs. Trivia and plain identifiers are
/// left out, which is what the editor does with them.
fn painted(script: &str, dialect: &Dialect) -> Vec<String> {
    let chars: Vec<char> = script.chars().collect();
    dbsql::tokens(script, dialect)
        .into_iter()
        .filter(|t| {
            !t.kind.is_trivia()
                && !matches!(
                    t.kind,
                    TokenKind::Identifier | TokenKind::Other | TokenKind::Terminator
                )
        })
        .map(|t| {
            let text: String = chars[t.start as usize..t.end as usize].iter().collect();
            format!("{:?}:{}", t.kind, text)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Splitting
// ---------------------------------------------------------------------------

#[test]
fn a_semicolon_at_the_top_level_separates_statements() {
    assert_eq!(
        split("SELECT 1; SELECT 2", &POSTGRES),
        ["SELECT 1", "SELECT 2"]
    );
    // A trailing separator does not make an empty statement after it.
    assert_eq!(split("SELECT 1;", &POSTGRES), ["SELECT 1"]);
    assert_eq!(split("   \n\t ", &POSTGRES), Vec::<String>::new());
}

#[test]
fn a_semicolon_inside_a_literal_separates_nothing() {
    for (script, dialect) in [
        ("SELECT 'a;b'", &POSTGRES),
        (r#"SELECT "a;b""#, &POSTGRES),
        ("SELECT `a;b`", &MYSQL),
        ("SELECT [a;b]", &MSSQL),
        ("SELECT -- a;b\n 1", &POSTGRES),
        ("SELECT /* a;b */ 1", &POSTGRES),
        ("SELECT # a;b\n 1", &MYSQL),
    ] {
        assert_eq!(split(script, dialect).len(), 1, "split: {script}");
    }
}

#[test]
fn a_doubled_quote_is_one_character_and_not_a_close() {
    assert_eq!(split("SELECT 'it''s; fine'", &POSTGRES).len(), 1);
    assert_eq!(split(r#"SELECT "a""b; c""#, &POSTGRES).len(), 1);
    assert_eq!(split("SELECT [a]]b; c]", &MSSQL).len(), 1);
}

#[test]
fn a_dollar_quoted_body_separates_nothing() {
    let script = "CREATE FUNCTION f() RETURNS int AS $$ BEGIN; RETURN 1; END; $$ LANGUAGE plpgsql; \
                  SELECT 1";
    assert_eq!(split(script, &POSTGRES).len(), 2);
    // A tagged body, and one holding what looks like a shorter tag.
    assert_eq!(
        split("SELECT $tag$ a;$b$ c $tag$; SELECT 2", &POSTGRES).len(),
        2
    );
}

#[test]
fn a_dollar_that_opens_nothing_is_not_a_body() {
    // `$1` is a placeholder: a tag may not begin with a digit. Reading it as an
    // opening tag would swallow the rest of the script.
    assert_eq!(split("SELECT $1; SELECT $2", &POSTGRES).len(), 2);
    // `$` continues an identifier, so `a$b$c` is one name to the server.
    assert_eq!(split("SELECT a$b$c; SELECT 2", &POSTGRES).len(), 2);
}

#[test]
fn a_chunk_holding_only_comments_is_not_a_statement() {
    assert_eq!(split("SELECT 1;\n-- done", &POSTGRES), ["SELECT 1"]);
    assert_eq!(split("-- nothing here", &POSTGRES), Vec::<String>::new());
}

#[test]
fn a_leading_comment_belongs_to_the_statement_below_it() {
    // Which is how scripts are written, and what puts a caret parked on the
    // comment in the statement it describes.
    assert_eq!(
        split("-- the wide rows\nSELECT 1;", &POSTGRES),
        ["-- the wide rows\nSELECT 1"]
    );
}

#[test]
fn an_unterminated_quote_swallows_the_rest_rather_than_recovering() {
    // The user is mid-keystroke. Treating the semicolon inside the half-written
    // literal as a boundary would run half a string as though it were a
    // statement.
    assert_eq!(split("SELECT 'oops; SELECT 2", &POSTGRES).len(), 1);
    assert_eq!(split("SELECT $$ oops; SELECT 2", &POSTGRES).len(), 1);
    assert_eq!(split("SELECT /* oops; SELECT 2", &POSTGRES).len(), 1);
}

// ---------------------------------------------------------------------------
// Which statement a caret is in
// ---------------------------------------------------------------------------

#[test]
fn the_caret_picks_the_statement_it_sits_in() {
    let script = "SELECT 1;\nSELECT 2;\nSELECT 3";
    let chars: Vec<char> = script.chars().collect();
    let text = |caret: u32| {
        dbsql::target(script, caret..caret, &POSTGRES).map(|t| {
            let s: String = chars[t.span.start as usize..t.span.end as usize]
                .iter()
                .collect();
            (s, t.origin)
        })
    };

    assert_eq!(
        text(0),
        Some((
            "SELECT 1".to_string(),
            Origin::Statement { index: 1, of: 3 }
        ))
    );
    // In the whitespace after a statement it is still that statement, which is
    // where the caret visually is.
    assert_eq!(text(9).unwrap().0, "SELECT 1");
    assert_eq!(text(12).unwrap().0, "SELECT 2");
    assert_eq!(text(script.chars().count() as u32).unwrap().0, "SELECT 3");
}

#[test]
fn a_buffer_of_one_statement_reports_itself_as_the_whole_thing() {
    let t = dbsql::target("SELECT 1", 0..0, &POSTGRES).unwrap();
    assert_eq!(t.origin, Origin::Whole);
}

#[test]
fn a_selection_is_taken_as_written() {
    // Somebody who highlighted three lines meant those three lines. The
    // surrounding whitespace goes, because a server error position counts from
    // the start of what it was handed.
    let script = "SELECT 1; SELECT 2";
    let t = dbsql::target(script, 8..16, &POSTGRES).unwrap();
    assert_eq!(t.origin, Origin::Selection);
    let chars: Vec<char> = script.chars().collect();
    let text: String = chars[t.span.start as usize..t.span.end as usize]
        .iter()
        .collect();
    assert_eq!(text, "; SELECT");
}

#[test]
fn a_position_from_the_server_lands_where_the_statement_started() {
    // 1-based, from the start of what was sent — not of the buffer. Applying it
    // to the buffer looks right every time the statement that failed happened
    // to be the first.
    assert_eq!(dbsql::error_offset(1, &(10..20)), Some(10));
    assert_eq!(dbsql::error_offset(11, &(10..20)), Some(20));
    // One past the end is what an unexpected end of input points at.
    assert_eq!(dbsql::error_offset(12, &(10..20)), None);
    assert_eq!(dbsql::error_offset(0, &(10..20)), None);
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

#[test]
fn every_character_is_covered_exactly_once() {
    let script = "SELECT 'a', \"b\", $$c$$, 1.5e-3, -- x\n/* y */ t.col FROM s.t WHERE x = $1;";
    for dialect in dbsql::ALL {
        let mut at = 0u32;
        for token in dbsql::tokens(script, dialect) {
            assert_eq!(token.start, at, "a gap or an overlap in {}", dialect.name);
            assert!(
                token.end > token.start,
                "an empty token in {}",
                dialect.name
            );
            at = token.end;
        }
        assert_eq!(
            at,
            script.chars().count() as u32,
            "short in {}",
            dialect.name
        );
    }
}

#[test]
fn the_words_an_editor_paints_are_the_ones_that_are_keywords() {
    assert_eq!(
        painted("SELECT id FROM t", &POSTGRES),
        ["Keyword:SELECT", "Keyword:FROM"]
    );
    // Case does not matter, and a name that merely contains a keyword is not
    // one.
    assert_eq!(painted("select selected", &POSTGRES), ["Keyword:select"]);
}

#[test]
fn a_number_is_told_from_a_name_that_starts_the_same_way() {
    assert_eq!(
        painted("SELECT 1, 1.5, .5, 1e3, 1.5e-3, 0xff, 1_000", &POSTGRES),
        [
            "Keyword:SELECT",
            "Number:1",
            "Number:1.5",
            "Number:.5",
            "Number:1e3",
            "Number:1.5e-3",
            "Number:0xff",
            "Number:1_000",
        ]
    );
    // `col1` is a name, `t.x` is a name and a dot and a name, and `1e` is the
    // number 1 followed by a column called `e`.
    assert_eq!(painted("SELECT col1, t.x", &POSTGRES), ["Keyword:SELECT"]);
    assert_eq!(
        painted("SELECT 1e", &POSTGRES),
        ["Keyword:SELECT", "Number:1"]
    );
}

#[test]
fn a_literal_carries_its_own_prefix() {
    // The `E` is part of the string, not a one-letter column called `e`.
    assert_eq!(
        painted(r"SELECT E'a\'b'", &POSTGRES),
        ["Keyword:SELECT", r"String:E'a\'b'"]
    );
    assert_eq!(
        painted("SELECT N'x'", &MSSQL),
        ["Keyword:SELECT", "String:N'x'"]
    );
    // But only when the prefix is the whole word.
    assert_eq!(
        painted("SELECT someE'x'", &POSTGRES),
        ["Keyword:SELECT", "String:'x'"]
    );
}

// ---------------------------------------------------------------------------
// Where the dialects disagree
// ---------------------------------------------------------------------------

#[test]
fn a_double_quote_opens_an_identifier_or_a_string_depending_on_who_is_asked() {
    // MySQL without ANSI_QUOTES — which is the default — reads this as a
    // string. Guessing "identifier" would mis-scan the most ordinary MySQL
    // there is.
    assert_eq!(
        painted(r#"SELECT "a""#, &POSTGRES),
        ["Keyword:SELECT", r#"QuotedIdentifier:"a""#]
    );
    assert_eq!(
        painted(r#"SELECT "a""#, &MYSQL),
        ["Keyword:SELECT", r#"String:"a""#]
    );
}

#[test]
fn a_backslash_escapes_a_quote_where_the_database_says_it_does() {
    // MySQL: `'a\'b'` is one string. PostgreSQL with standard_conforming_strings
    // on — the default since 9.1 — reads `'a\'` as a complete string ending in
    // a backslash, so what follows is outside it.
    assert_eq!(split(r"SELECT 'a\'; SELECT 2", &MYSQL).len(), 1);
    assert_eq!(split(r"SELECT 'a\'; SELECT 2", &POSTGRES).len(), 2);
    assert_eq!(split(r"SELECT 'a\'; SELECT 2", &CLICKHOUSE).len(), 1);
}

#[test]
fn block_comments_nest_where_the_database_nests_them() {
    // A scanner that stops at the first `*/` where the server does not leaves
    // the tail of a commented-out block being read as SQL — which is exactly
    // what MySQL does here, correctly, and PostgreSQL does not.
    let script = "SELECT 1; /* a /* b */ SELECT 2; */ SELECT 3";
    assert_eq!(split(script, &POSTGRES).len(), 2);
    assert_eq!(split(script, &MYSQL).len(), 3);
    assert!(split(script, &MYSQL)[1].ends_with("SELECT 2"));
}

#[test]
fn a_hash_starts_a_comment_only_where_it_does() {
    assert_eq!(split("SELECT 1 # note; SELECT 2", &MYSQL).len(), 1);
    assert_eq!(split("SELECT 1 # note; SELECT 2", &POSTGRES).len(), 2);
}

#[test]
fn a_dollar_body_is_a_string_only_where_the_database_has_them() {
    // In MySQL `$$` is two identifier characters, so the semicolons inside are
    // real boundaries.
    assert_eq!(split("SELECT $$ a; b $$", &POSTGRES).len(), 1);
    assert_eq!(split("SELECT $$ a; b $$", &MYSQL).len(), 2);
    assert_eq!(split("SELECT $$ a; b $$", &DUCKDB).len(), 1);
}

#[test]
fn each_database_paints_the_words_that_are_its_own() {
    // Eight words is DuckDB's whole delta from PostgreSQL's vocabulary, and
    // this is one of them.
    assert!(painted("SELECT * FROM t QUALIFY x", &DUCKDB).contains(&"Keyword:QUALIFY".to_string()));
    assert!(
        !painted("SELECT * FROM t QUALIFY x", &POSTGRES).contains(&"Keyword:QUALIFY".to_string())
    );

    assert!(painted("SELECT a PREWHERE b", &CLICKHOUSE).contains(&"Keyword:PREWHERE".to_string()));
    assert!(painted("SELECT TOP 10 a", &MSSQL).contains(&"Keyword:TOP".to_string()));
    assert!(painted("SELECT a FROM t LIMIT 1", &MYSQL).contains(&"Keyword:LIMIT".to_string()));

    // And the common vocabulary is common: SELECT is a keyword in all six.
    for dialect in dbsql::ALL {
        assert!(
            painted("SELECT 1", dialect).contains(&"Keyword:SELECT".to_string()),
            "{} does not paint SELECT",
            dialect.name
        );
    }
}

#[test]
fn a_placeholder_is_not_painted_as_a_name() {
    assert_eq!(
        painted("SELECT $1", &POSTGRES),
        ["Keyword:SELECT", "Parameter:$1"]
    );
    assert_eq!(
        painted("SELECT ?", &MYSQL),
        ["Keyword:SELECT", "Parameter:?"]
    );
    assert_eq!(
        painted("SELECT @x", &MSSQL),
        ["Keyword:SELECT", "Parameter:@x"]
    );
    assert_eq!(
        painted("SELECT :x", &SQLITE),
        ["Keyword:SELECT", "Parameter::x"]
    );
    // `::` is a cast, not a placeholder, in the dialects that have both.
    assert_eq!(
        painted("SELECT x::int", &DUCKDB),
        ["Keyword:SELECT", "Keyword:int"]
    );
}

// ---------------------------------------------------------------------------
// Positions
// ---------------------------------------------------------------------------

#[test]
fn offsets_are_counted_in_characters_and_not_in_bytes() {
    // The difference is invisible until somebody names a table in a language
    // that is not English, and then every caret after it is wrong.
    let script = "SELECT 'ëmoji 🎉', x";
    let tokens = dbsql::tokens(script, &POSTGRES);
    let last = tokens.last().unwrap();
    assert_eq!(last.end, script.chars().count() as u32);
    // And the identifier at the end really is the one character `x`.
    assert_eq!(last.kind, TokenKind::Identifier);
    assert_eq!(last.len(), 1);
}
