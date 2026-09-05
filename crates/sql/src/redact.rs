//! What a statement may be written down as.
//!
//! The query history keeps statements across launches, in a file. Nearly every
//! statement anybody sends is a fine thing to keep — that is the whole point of
//! keeping them — and a handful are the exact opposite: `ALTER USER app
//! IDENTIFIED BY 'hunter2'` is a password, typed by hand, on its way to a plist
//! that nothing encrypts. This build's rule is that a password never reaches a
//! file, and a history that wrote that one down would be the one place it does.
//!
//! It lives here for the reason `danger.rs` gives, and the reason is stronger
//! here. A front end matching on the text would redact the word PASSWORD in a
//! comment, in a column called `password_changed_at`, and in the string literal
//! `'password'` — and would miss `$$hunter2$$`, because a regular expression
//! written for quotes does not know what a dollar-quoted body is. The lexer has
//! already told all of those apart.
//!
//! It is a heuristic and is written to fail in the safe direction: it takes out
//! more than it has to rather than less. What that costs is stated in
//! `limitations.md` — a statement that names a secret and also carries an
//! unrelated literal loses both.

use crate::dialect::Dialect;
use crate::lex::{Token, TokenKind, tokens};

/// What replaces a literal that was taken out.
///
/// Quoted, so that what is left still reads as the statement it was: somebody
/// scanning the history for "when did I last change that role" needs to
/// recognise the shape. Deliberately not valid to re-run — an ellipsis is not a
/// password, and a statement that could be sent again from here is one somebody
/// would send again without noticing what is in it.
const MASK: &str = "'…'";

/// Words that make a statement one whose literals are not worth keeping.
///
/// Matched against keyword and identifier tokens only, never against the inside
/// of a string or a comment, which is what keeps `WHERE name = 'password'` and
/// `-- reset the password` from being read as secrets.
///
/// Four rather than a long list. Each of these introduces a secret in at least
/// one dialect this build speaks — PostgreSQL and MySQL spell it PASSWORD,
/// MySQL, MariaDB and ClickHouse spell it IDENTIFIED, DuckDB has CREATE SECRET,
/// and the warehouses take CREDENTIALS — and every one of them is a word that
/// appears in ordinary statements rarely enough that over-redacting on it costs
/// almost nothing.
const NAMES_A_SECRET: [&str; 4] = ["PASSWORD", "IDENTIFIED", "SECRET", "CREDENTIALS"];

/// What `statement` may be written down as, or `None` when it may be written
/// down as it is.
///
/// `None` rather than a copy of the input, so that the ordinary statement — which
/// is every statement but a handful — costs a scan and no allocation, and so
/// that a caller can tell "nothing was taken out" from "something was".
pub fn redacted(statement: &str, dialect: &Dialect) -> Option<String> {
    let chars: Vec<char> = statement.chars().collect();
    let text = |token: &Token| -> String {
        chars[token.start as usize..token.end as usize]
            .iter()
            .collect()
    };

    let scanned = tokens(statement, dialect);
    // Every token, compared against its own text, and deliberately without a
    // test on the kind. One was written here first and turned out to change no
    // answer: a comment arrives as a single token holding the whole comment, and
    // a literal arrives with its quotes still on, so neither can ever equal a
    // bare word. `-- reset the password` and `'password'` are kept out by what
    // the lexer hands over rather than by a guard, and the tests hold both — a
    // guard that cannot fail is a line that says a rule is enforced somewhere it
    // is not.
    let names_a_secret = scanned
        .iter()
        .any(|token| NAMES_A_SECRET.contains(&text(token).to_uppercase().as_str()));

    let mut out = String::with_capacity(statement.len());
    let mut copied = 0usize;
    let mut took_something = false;
    for token in &scanned {
        if !matches!(token.kind, TokenKind::String | TokenKind::DollarQuoted) {
            continue;
        }
        if !names_a_secret && !carries_a_secret(&text(token)) {
            continue;
        }
        out.extend(chars[copied..token.start as usize].iter());
        out.push_str(MASK);
        copied = token.end as usize;
        took_something = true;
    }
    if !took_something {
        return None;
    }
    out.extend(chars[copied..].iter());
    Some(out)
}

/// Whether a literal holds a secret even though the statement around it names
/// none.
///
/// The case this exists for is a connection string passed as one argument:
/// `dblink_connect('host=… user=… password=…')` and PostgreSQL's `CREATE
/// SUBSCRIPTION … CONNECTION '…'` both put the password inside a literal, where
/// the statement's own words never mention one.
///
/// Keyed on the separator rather than on the word. `password=` is a setting
/// being given a value; `password` on its own is a column, a table, or the
/// answer to a security question, and redacting `WHERE name = 'password'` would
/// be taking out the one thing that row was found by.
fn carries_a_secret(literal: &str) -> bool {
    let lowered = literal.to_lowercase();
    ["password=", "password:", "pwd=", "secret=", "secret_key="]
        .iter()
        .any(|marker| lowered.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::{MYSQL, POSTGRES};

    #[test]
    fn a_password_set_by_hand_does_not_survive_into_the_history() {
        assert_eq!(
            redacted("ALTER USER app IDENTIFIED BY 'hunter2'", &MYSQL).as_deref(),
            Some("ALTER USER app IDENTIFIED BY '…'")
        );
        assert_eq!(
            redacted("CREATE USER app PASSWORD 'hunter2'", &POSTGRES).as_deref(),
            Some("CREATE USER app PASSWORD '…'")
        );
    }

    /// The form a front-end regular expression would have been written for
    /// quotes and would have walked straight past.
    #[test]
    fn a_dollar_quoted_password_is_taken_out_as_well() {
        assert_eq!(
            redacted("ALTER ROLE app PASSWORD $$hunter2$$", &POSTGRES).as_deref(),
            Some("ALTER ROLE app PASSWORD '…'")
        );
    }

    /// Every literal in the statement, not the one after the keyword. Which
    /// argument holds the secret differs by dialect and by version, and a rule
    /// that had to know would be wrong first on whichever engine changed.
    #[test]
    fn a_statement_that_names_a_secret_keeps_none_of_its_literals() {
        assert_eq!(
            redacted(
                "CREATE USER 'app'@'10.0.0.1' IDENTIFIED BY 'hunter2'",
                &MYSQL
            )
            .as_deref(),
            Some("CREATE USER '…'@'…' IDENTIFIED BY '…'")
        );
    }

    /// The password is inside the literal and the statement never says the word.
    #[test]
    fn a_connection_string_argument_is_taken_out_on_its_own_account() {
        assert_eq!(
            redacted(
                "SELECT * FROM dblink('host=db user=app password=hunter2', 'SELECT 1') AS t(n int)",
                &POSTGRES
            )
            .as_deref(),
            Some("SELECT * FROM dblink('…', 'SELECT 1') AS t(n int)")
        );
    }

    /// The three places the word appears that are not secrets, and the reason
    /// this is a lexer and not a search. Each of these would be redacted by a
    /// match over the raw text.
    #[test]
    fn the_word_somewhere_harmless_is_not_a_secret() {
        assert_eq!(
            redacted("SELECT * FROM users WHERE name = 'password'", &POSTGRES),
            None,
            "a literal that is the word, which is a row somebody is looking for"
        );
        assert_eq!(
            redacted("-- reset the password\nSELECT 'x'", &POSTGRES),
            None,
            "the word in a comment, which the server never sees"
        );
        assert_eq!(
            redacted("SELECT 'a' FROM audit", &POSTGRES),
            None,
            "and an ordinary statement keeps every literal it has"
        );
    }

    /// A column called `password` is an identifier, so it does name a secret by
    /// this rule — and the redaction it causes is the safe direction. Written
    /// down because it is the over-redaction the heuristic is known to make,
    /// rather than left to be discovered as a bug.
    #[test]
    fn a_column_of_that_name_over_redacts_and_that_is_the_direction_to_be_wrong_in() {
        assert_eq!(
            redacted("SELECT password FROM users WHERE name = 'bob'", &POSTGRES).as_deref(),
            Some("SELECT password FROM users WHERE name = '…'")
        );
    }

    /// Nothing to take out is `None` and not an empty string, which is what lets
    /// the caller keep the statement it already has.
    #[test]
    fn a_statement_with_nothing_to_hide_is_left_alone() {
        assert_eq!(redacted("SELECT 1", &POSTGRES), None);
        assert_eq!(redacted("", &POSTGRES), None);
        assert!(
            redacted("ALTER USER app IDENTIFIED BY 'p'", &MYSQL).is_some(),
            "and the one that does have something is not caught by that shortcut"
        );
    }
}
