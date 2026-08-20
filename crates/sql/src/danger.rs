//! What a statement would do, before anybody runs it.
//!
//! Read from the statement's head and nothing else. That is a limit on purpose
//! rather than a shortcut: the question this answers is "is this worth stopping
//! to confirm", and the honest way to answer it is from what the statement is.
//! A client that tried to work out how many rows a `DELETE` would touch would
//! have to run it to find out.
//!
//! It lives here because this is the crate that already reads SQL. The
//! alternative was a search over the editor's text in the front end, which finds
//! the word DROP in a comment, in a string literal and in a column called
//! `dropped_at` — this side has a lexer that has already told those apart.

use crate::dialect::Dialect;
use crate::lex::{Token, TokenKind, tokens};

/// How much a statement costs if it was not meant.
///
/// Ordered, and the order is most of the type: what a caller asks is whether
/// this is worse than what it is willing to run without a question, and `>=` is
/// that question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Danger {
    /// Reads, and statements that change only this session. Nothing to undo.
    Safe,
    /// Changes rows. Inside a transaction it can be taken back.
    Modify,
    /// Changes what a relation is, or empties one in a way a transaction may not
    /// hold: `TRUNCATE` is not transactional everywhere, and an `ALTER` can drop
    /// a column with everything in it.
    Dangerous,
    /// Destroys an object outright.
    Fatal,
}

impl Danger {
    /// The word for it, for a front end that has to say which of these it is.
    pub fn name(self) -> &'static str {
        match self {
            Danger::Safe => "safe",
            Danger::Modify => "modify",
            Danger::Dangerous => "dangerous",
            Danger::Fatal => "fatal",
        }
    }
}

/// What `statement` would do.
///
/// The dialect is here because the lexer needs one — what counts as a quoted
/// name differs — and not because the table below does.
pub fn danger(statement: &str, dialect: &Dialect) -> Danger {
    let chars: Vec<char> = statement.chars().collect();
    let word = |token: &Token| -> String {
        chars[token.start as usize..token.end as usize]
            .iter()
            .flat_map(|c| c.to_uppercase())
            .collect()
    };

    let scanned = tokens(statement, dialect);
    let mut significant = scanned.iter().filter(|t| !t.kind.is_trivia());

    let Some(head) = significant.next() else {
        // Whitespace, or a comment: there is nothing here to run, so there is
        // nothing here to ask about.
        return Danger::Safe;
    };

    // A CTE says nothing about what is done with it — `WITH … DELETE` is a
    // delete — so a `WITH` is read past and the worst thing named after it is
    // the answer.
    //
    // Every word after it is considered, not only the ones the lexer calls
    // keywords: `TRUNCATE` is a keyword in some dialects and an ordinary word in
    // others, and a rule that trusted the lexer's list would read `TRUNCATE
    // orders` as harmless in whichever of them leave it out. The price is that a
    // column named `copy` inside a CTE is reported as a modification — a
    // question nobody needed, against the alternative of a deletion nobody was
    // asked about.
    if word(head) == "WITH" {
        return significant
            .filter(|t| matches!(t.kind, TokenKind::Keyword | TokenKind::Identifier))
            .filter_map(|t| known(&word(t)))
            .max()
            .unwrap_or(Danger::Safe);
    }

    // A head this build does not recognise is not promised to be a read. Every
    // database here has statements the others do not, and the direction to be
    // wrong in is the one that asks.
    known(&word(head)).unwrap_or(Danger::Modify)
}

/// The worst thing anywhere in `script`.
///
/// A script is sent statement by statement and every one of them lands, so what
/// a caller has to ask about is the worst of them rather than the first: a
/// buffer that reads three tables and then drops one is a drop.
///
/// Split by the same reader the editor splits by, rather than on `;` here. A
/// semicolon inside a string literal or a dollar-quoted body is not a boundary,
/// and a splitter that thought it was would read `SELECT 'a; DROP TABLE t'` as
/// two statements and report a drop nobody wrote.
pub fn script_danger(script: &str, dialect: &Dialect) -> Danger {
    let chars: Vec<char> = script.chars().collect();
    crate::script::statements(script, dialect)
        .into_iter()
        .map(|span| {
            let text: String = chars[span.start as usize..span.end as usize]
                .iter()
                .collect();
            danger(&text, dialect)
        })
        .max()
        .unwrap_or(Danger::Safe)
}

/// Where a word sits, or `None` for one this table says nothing about.
///
/// One table rather than one per dialect. What it holds are the words every SQL
/// database shares, and writing a table per dialect before there is a database
/// that disagrees would be guessing at differences instead of recording them.
fn known(word: &str) -> Option<Danger> {
    Some(match word {
        "DROP" => Danger::Fatal,
        "TRUNCATE" | "ALTER" | "RENAME" => Danger::Dangerous,
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "REPLACE" | "UPSERT" | "COPY" | "LOAD"
        | "IMPORT" | "CREATE" | "GRANT" | "REVOKE" | "CALL" | "EXEC" | "EXECUTE" | "COMMENT"
        | "VACUUM" | "ANALYZE" | "ANALYSE" | "OPTIMIZE" | "REINDEX" | "REFRESH" => Danger::Modify,
        // Transaction control is here rather than under `Modify` because none of
        // it writes anything of its own: what a `COMMIT` makes permanent was
        // written by the statements this same list already classified.
        "SELECT" | "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC" | "VALUES" | "TABLE" | "PRAGMA"
        | "USE" | "SET" | "RESET" | "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT"
        | "RELEASE" | "END" | "WITH" => Danger::Safe,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{Danger, danger, script_danger};
    use crate::dialect::{MYSQL, POSTGRES};

    #[test]
    fn a_read_is_never_a_reason_to_stop_somebody() {
        assert_eq!(danger("SELECT * FROM orders", &POSTGRES), Danger::Safe);
        assert_eq!(danger("EXPLAIN SELECT 1", &POSTGRES), Danger::Safe);
        assert_eq!(
            danger("  -- nothing to run here\n", &POSTGRES),
            Danger::Safe
        );
        assert_eq!(danger("", &POSTGRES), Danger::Safe);
    }

    /// The word in a column name, in a string and in a comment is not the
    /// statement's head — which is the whole reason this is not a search.
    #[test]
    fn the_word_drop_inside_a_read_leaves_it_a_read() {
        assert_eq!(
            danger("SELECT dropped_at FROM boxes", &POSTGRES),
            Danger::Safe
        );
        assert_eq!(
            danger("SELECT 'DROP TABLE orders'", &POSTGRES),
            Danger::Safe
        );
        assert_eq!(
            danger("/* DROP TABLE orders */ SELECT 1", &POSTGRES),
            Danger::Safe
        );
    }

    #[test]
    fn a_statement_is_named_by_what_it_does() {
        assert_eq!(danger("DELETE FROM orders", &POSTGRES), Danger::Modify);
        assert_eq!(
            danger("truncate table orders", &POSTGRES),
            Danger::Dangerous
        );
        assert_eq!(
            danger("ALTER TABLE orders DROP COLUMN note", &POSTGRES),
            Danger::Dangerous
        );
        assert_eq!(danger("DROP TABLE orders", &POSTGRES), Danger::Fatal);
    }

    /// A CTE in front of a delete does not make it a read, and this is the case
    /// a rule that read only the first word would get wrong.
    #[test]
    fn a_cte_is_read_past_to_whatever_it_feeds() {
        assert_eq!(
            danger(
                "WITH old AS (SELECT id FROM orders) SELECT * FROM old",
                &POSTGRES
            ),
            Danger::Safe
        );
        assert_eq!(
            danger(
                "WITH old AS (SELECT id FROM orders) DELETE FROM lines WHERE id IN (SELECT id FROM old)",
                &POSTGRES
            ),
            Danger::Modify
        );
    }

    /// A head this build has never seen is not promised to be harmless.
    #[test]
    fn an_unrecognised_head_is_asked_about_rather_than_waved_through() {
        assert_eq!(danger("FLUSH PRIVILEGES", &MYSQL), Danger::Modify);
    }

    /// A script is answered by its worst statement, not its first. Reading the
    /// first is the mistake that lets a buffer beginning `SELECT 1;` carry a
    /// drop past a question.
    #[test]
    fn a_script_is_named_by_the_worst_thing_in_it() {
        assert_eq!(script_danger("SELECT 1; SELECT 2", &POSTGRES), Danger::Safe);
        assert_eq!(
            script_danger("SELECT 1;\nDROP TABLE orders;\nSELECT 2", &POSTGRES),
            Danger::Fatal
        );
        assert_eq!(script_danger("", &POSTGRES), Danger::Safe);
    }

    /// The semicolon inside the string is not a boundary, so there is one
    /// statement here and it is a read.
    #[test]
    fn a_semicolon_in_a_string_does_not_make_a_second_statement() {
        assert_eq!(
            script_danger("SELECT 'a; DROP TABLE orders'", &POSTGRES),
            Danger::Safe
        );
    }
}
