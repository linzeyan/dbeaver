//! What a name typed at the caret would have to be.
//!
//! This answers the question and not the request. It says "a column of these
//! three relations, beginning with `cust`"; it does not go and find them,
//! because the names live in the metadata graph on the other side of the FFI
//! and a list of every column in a database is not something to build here on
//! every keystroke.
//!
//! Getting the *question* right is most of what makes completion feel correct.
//! An editor that offers every table in the schema after `WHERE o.` is not
//! slow, it is wrong, and no amount of ranking fixes it.

use crate::dialect::Dialect;
use crate::lex::{TokenKind, tokens};
use crate::parse::{Source, scopes};
use crate::script::{Span, spans};

/// What kind of name belongs where the caret is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// The start of a statement: a verb, and nothing else.
    Statement,
    /// A relation. `schema` is set when the user has typed `schema.` and is
    /// waiting to be told what is in it.
    Relation { schema: Option<String> },
    /// A column. `qualifier` is set when the user typed `t.`, and names the
    /// relation or alias they wrote — which may not be in scope, in which case
    /// there is nothing to offer and saying so beats offering everything.
    Column { qualifier: Option<String> },
    /// Inside a literal or a comment, where the server would not read a name
    /// and neither should this.
    Nothing,
}

/// The question the caret is asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub expect: Expect,
    /// What has been typed of the name so far, and the characters it occupies.
    /// Replacing this span is what accepting a suggestion does.
    pub prefix: String,
    pub span: Span,
    /// The relations in scope, innermost first. Empty for [`Expect::Statement`]
    /// and [`Expect::Nothing`].
    pub sources: Vec<Source>,
}

/// What could be typed at `caret`.
pub fn complete(text: &str, caret: u32, dialect: &Dialect) -> Completion {
    let all = tokens(text, dialect);

    // Inside a literal or a comment there is nothing to offer.
    //
    // A caret exactly at the end of a closed one is outside it: the user has
    // just typed the closing quote and moved on. A line comment has no closing
    // delimiter, so the end of one is still inside it — the caret is sitting in
    // the prose, which is the most common place in a script for it to be.
    let chars: Vec<char> = text.chars().collect();
    let inside = all.iter().find(|t| {
        (t.start < caret && caret < t.end)
            || (caret == t.end && t.kind == TokenKind::Comment && chars[t.end as usize - 1] != '/')
    });
    if let Some(t) = inside
        && matches!(
            t.kind,
            TokenKind::String | TokenKind::Comment | TokenKind::DollarQuoted
        )
    {
        return nothing(caret);
    }

    // The word being typed, which is the token ending exactly at the caret.
    // Ending, not containing: a caret in the middle of `orders` is completing
    // `ord`, and offering names that match `orders` there would replace text
    // the user can see.
    let word = all.iter().find(|t| {
        t.end == caret
            && matches!(
                t.kind,
                TokenKind::Identifier | TokenKind::Keyword | TokenKind::QuotedIdentifier
            )
    });
    let (prefix, span) = match word {
        Some(t) => (
            chars[t.start as usize..t.end as usize].iter().collect(),
            t.start..t.end,
        ),
        None => (String::new(), caret..caret),
    };

    // What comes before the word: a `.` makes this a qualified name, and what
    // is before the dot is the qualifier.
    let before = all
        .iter()
        .rev()
        .find(|t| t.end <= span.start && !t.kind.is_trivia());
    let qualifier = match before {
        Some(t) if t.len() == 1 && chars[t.start as usize] == '.' => all
            .iter()
            .rev()
            .find(|q| {
                q.end <= t.start
                    && matches!(q.kind, TokenKind::Identifier | TokenKind::QuotedIdentifier)
            })
            .map(|q| {
                chars[q.start as usize..q.end as usize]
                    .iter()
                    .collect::<String>()
            }),
        _ => None,
    };

    // Which statement the caret is in, and what it can see from there.
    let all_statements = spans(&all, &chars);
    let Some(stmt_span) = all_statements
        .iter()
        .find(|s| s.start <= caret && caret <= s.end)
        .or_else(|| all_statements.last())
        .cloned()
    else {
        return Completion {
            expect: Expect::Statement,
            prefix,
            span,
            sources: Vec::new(),
        };
    };
    let parsed = scopes(&all, &chars, stmt_span.clone());
    // Clamped, because the caret is often one character past the statement:
    // `statements` trims the whitespace a statement ends with, and typing
    // happens at the end.
    let anchor = caret.clamp(stmt_span.start, stmt_span.end);
    let sources: Vec<Source> = parsed.sources_at(anchor).into_iter().cloned().collect();

    // The word before the name being typed, which is what decides the kind.
    // `FROM` and its relatives want a relation; everything else in a statement
    // is an expression, where a name is a column.
    let lead = all
        .iter()
        .rev()
        .find(|t| t.end <= span.start && !t.kind.is_trivia())
        .map(|t| {
            chars[t.start as usize..t.end as usize]
                .iter()
                .map(|c| c.to_ascii_lowercase())
                .collect::<String>()
        });

    let expect = match lead.as_deref() {
        // Nothing before it in the statement: a verb is the only thing that can
        // go here.
        None => Expect::Statement,
        Some(".") => {
            // `sales.` and `o.` are the same three characters and mean
            // different things, and which it is cannot be read off the dot. It
            // is read off the clause: in a FROM list a qualified name is
            // schema-then-relation, and everywhere else it is
            // relation-then-column.
            //
            // The clause is the first thing checked and the sources are the
            // fallback, in that order. A name that is in scope settles it —
            // `SELECT o.` with `orders o` in the FROM list wants columns
            // whatever else is true — but in `FROM sales.` there is nothing in
            // scope yet to ask, and the clause is all there is.
            if in_a_relation_list(&all, &chars, span.start) {
                Expect::Relation { schema: qualifier }
            } else if qualifier
                .as_deref()
                .is_some_and(|q| sources.iter().any(|s| s.handle().eq_ignore_ascii_case(q)))
            {
                Expect::Column { qualifier }
            } else {
                Expect::Relation { schema: qualifier }
            }
        }
        Some(w) if RELATION_LEADS.contains(&w) => Expect::Relation { schema: None },
        _ if stmt_span.start == span.start => Expect::Statement,
        _ => Expect::Column { qualifier: None },
    };

    let sources = match expect {
        Expect::Statement | Expect::Nothing => Vec::new(),
        _ => sources,
    };
    Completion {
        expect,
        prefix,
        span,
        sources,
    }
}

/// The words after which a relation is named.
///
/// Deliberately not read from the dialect table. These are the same in all six
/// — a database that spelled `FROM` differently would not be reached by a SQL
/// editor at all — and a per-dialect copy would be five chances to leave one
/// out.
const RELATION_LEADS: &[&str] = &[
    "from", "join", "into", "update", "table", "truncate", "describe", "analyze", "vacuum",
];

/// The words that end a relation list by starting something else.
///
/// `ON` and `USING` are here because a join condition is an expression written
/// in the middle of a FROM clause — `FROM a JOIN b ON a.` wants a column, and a
/// rule that only looked for the nearest `FROM` would say relation.
const EXPRESSION_LEADS: &[&str] = &[
    "select",
    "where",
    "on",
    "using",
    "having",
    "set",
    "values",
    "returning",
    "when",
    "then",
    "else",
    "and",
    "or",
    "not",
    "by",
    "case",
];

/// Whether the name being typed at `at` is part of a list of relations.
///
/// Decided by walking back to the nearest word that begins a clause, which is
/// the cheapest thing that is actually right. Commas and the join words are
/// walked over rather than treated as boundaries, because `FROM a, b, sales.`
/// is still a relation list at the third item.
fn in_a_relation_list(all: &[crate::lex::Token], chars: &[char], at: u32) -> bool {
    for token in all
        .iter()
        .rev()
        .filter(|t| t.end <= at && !t.kind.is_trivia())
    {
        let word: String = chars[token.start as usize..token.end as usize]
            .iter()
            .map(|c| c.to_ascii_lowercase())
            .collect();
        if RELATION_LEADS.contains(&word.as_str()) {
            return true;
        }
        if EXPRESSION_LEADS.contains(&word.as_str()) {
            return false;
        }
    }
    false
}

fn nothing(caret: u32) -> Completion {
    Completion {
        expect: Expect::Nothing,
        prefix: String::new(),
        span: caret..caret,
        sources: Vec::new(),
    }
}
