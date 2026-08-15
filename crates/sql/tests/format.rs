//! What the formatter must not do, which is most of what matters about it.
//!
//! Laying a statement out differently is the easy half and nobody is hurt when
//! it is ugly. The half these tests hold is that the statement still says what
//! it said: an identifier that quietly gains a space, or a function body that
//! quietly gains a newline, is a defect the user finds by running it.
//!
//! Each case here is one the candidate this crate rejected gets wrong, so they
//! also record why `lax-sql` is the dependency rather than `sqlformat`.

use dbsql::format;

#[test]
fn a_backtick_identifier_is_not_taken_apart() {
    // `a b` is one name in MySQL, and a formatter free to insert whitespace
    // between tokens would make it two.
    let out = format("SELECT `select`, `a b` FROM `orders` WHERE `qty` > 1");
    assert!(out.contains("`select`"), "got {out}");
    assert!(out.contains("`a b`"), "got {out}");
    assert!(out.contains("`orders`"), "got {out}");
}

#[test]
fn a_bracketed_identifier_keeps_its_brackets_tight() {
    // `[ order ]` is not `[order]`: SQL Server takes what is between the
    // brackets literally, so the spaces would be part of the name. The other
    // candidate produces exactly that under its default dialect.
    let out = format("SELECT [order], [a b] FROM [dbo].[t] WHERE [x] = 1");
    assert!(out.contains("[order]"), "got {out}");
    assert!(out.contains("[a b]"), "got {out}");
    assert!(!out.contains("[ "), "an identifier gained a space: {out}");
}

#[test]
fn a_dollar_quoted_body_comes_back_byte_for_byte() {
    // Inside `$$ … $$` whitespace is the value, not the layout. Reflowing it
    // rewrites the function the server will store.
    let body = " SELECT 1; SELECT 2; ";
    let out = format(&format!(
        "CREATE FUNCTION f() RETURNS int AS $${body}$$ LANGUAGE sql"
    ));
    let seen = out
        .split("$$")
        .nth(1)
        .expect("the formatted text must still have a dollar quoted region");
    assert_eq!(seen, body, "the body was rewritten: {out}");
}

#[test]
fn a_multi_clause_select_puts_each_clause_on_its_own_line() {
    // The half that is actually formatting: a statement typed on one line comes
    // back readable, one clause per line.
    let out = format("SELECT a, b FROM t WHERE x = 1 GROUP BY a HAVING count(*) > 1 ORDER BY b");
    let starts: Vec<&str> = out.lines().map(|line| line.trim_start()).collect();
    for clause in ["SELECT", "FROM", "WHERE", "GROUP BY", "HAVING", "ORDER BY"] {
        assert!(
            starts.iter().any(|line| line.starts_with(clause)),
            "no line starts with {clause}: {out}"
        );
    }
}

#[test]
fn text_that_is_not_sql_is_handed_back_rather_than_mangled() {
    // The editor's text is the user's. Words that parse as nothing are laid out
    // as the words they are — not emptied, not guessed at, not rearranged.
    let input = "this is not sql at all";
    assert_eq!(format(input).trim_end(), input);
}

#[test]
fn the_result_always_ends_in_exactly_one_newline() {
    // Worth pinning rather than discovering: the output is a formatted file, so
    // it is newline terminated even when the input was not. An editor replacing
    // its buffer with this gets one trailing line, and gets the same one again
    // however many times the user asks — which is what makes Format repeatable.
    let once = format("SELECT 1");
    assert!(once.ends_with('\n'), "got {once:?}");
    assert!(!once.ends_with("\n\n"), "got {once:?}");
    assert_eq!(format(&once), once, "formatting twice moved it");
}

#[test]
fn a_comment_asking_to_be_left_alone_is_obeyed() {
    // The escape hatch has to be a word somebody writes on purpose. This also
    // pins the directive against a value like `/*`, which would match the start
    // of any block comment and turn formatting off for statements nobody
    // excluded.
    let input = "-- sql-format-ignore-file\nSELECT a,b FROM t WHERE x=1";
    assert_eq!(format(input), input);

    let ordinary = "/* an ordinary note */\nSELECT a,b FROM t WHERE x=1";
    assert_ne!(format(ordinary), ordinary, "an ordinary comment stopped it");
}
