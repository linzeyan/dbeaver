//! Breaking a buffer into the statements somebody runs one at a time.
//!
//! Splitting on `;` is wrong in ways that bite on the first real script. A
//! semicolon inside a string literal, a quoted identifier, a comment or a
//! dollar-quoted body is not a boundary, and every PL/pgSQL function body ever
//! written contains one. So this reads the token stream and only the
//! terminators that reach it separate anything.

use crate::dialect::Dialect;
use crate::lex::{TokenKind, tokens};

/// A half-open range of characters, which is what every offset here is counted
/// in. See `lex` for why characters and not bytes.
pub type Span = std::ops::Range<u32>;

/// Which part of the buffer a run is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The only statement in the buffer.
    Whole,
    /// One statement of several, both counted from 1.
    Statement { index: usize, of: usize },
    /// Text somebody highlighted.
    Selection,
}

/// What running the buffer will send, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The exact text to send. What goes to the server has to be exactly this
    /// slice: a server error position is counted from the start of the string
    /// it was handed, so trimming anything afterwards moves every position with
    /// it.
    pub span: Span,
    pub origin: Origin,
}

/// Every statement in `text`, in order, with its terminating semicolon and the
/// blank space around it removed.
///
/// A chunk holding only comments and whitespace is not a statement — there is
/// nothing there to run — so a trailing `-- done` after the last `;` produces
/// no entry. Leading comments, on the other hand, stay inside the statement
/// below them. That is how scripts are written, the server treats them as
/// whitespace, and it is what puts a caret parked on `-- fetch the wide rows`
/// in the statement that comment describes. The cost is that a comment trailing
/// a semicolon on the same line attaches to the statement after it rather than
/// the one before; no rule gets both, and leading comments are the ones that
/// occur.
pub fn statements(text: &str, dialect: &Dialect) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    let mut found = Vec::new();
    let mut start = 0u32;
    let mut has_code = false;

    for token in tokens(text, dialect) {
        match token.kind {
            TokenKind::Terminator => {
                if has_code {
                    found.push(trimmed(&chars, start..token.start));
                }
                has_code = false;
                start = token.end;
            }
            kind if kind.is_trivia() => {}
            _ => has_code = true,
        }
    }
    if has_code {
        found.push(trimmed(&chars, start..chars.len() as u32));
    }
    found
}

/// The statement a run means, given where the caret or the selection is.
///
/// A selection is taken as written: somebody who highlighted three lines meant
/// those three lines, and second-guessing that is how a client runs something
/// the user did not ask for. Everything else is the statement the caret sits
/// in. `None` for a buffer with nothing in it to run.
pub fn target(text: &str, selection: Span, dialect: &Dialect) -> Option<Target> {
    let chars: Vec<char> = text.chars().collect();
    let end = chars.len() as u32;

    if selection.start != selection.end {
        let clamped = selection.start.min(end)..selection.end.min(end);
        let span = trimmed(&chars, clamped);
        return (span.start != span.end).then_some(Target {
            span,
            origin: Origin::Selection,
        });
    }

    let all = statements(text, dialect);
    if all.is_empty() {
        return None;
    }
    // The last statement that starts at or before the caret. Inside a statement
    // that is the statement; in the blank space or the trailing comment after
    // one it is the one just above, which is where the caret still visually is.
    // Only a caret in the buffer's leading whitespace matches nothing, and there
    // the first statement is what was meant.
    let index = all
        .iter()
        .rposition(|s| s.start <= selection.start)
        .unwrap_or(0);
    let origin = if all.len() == 1 {
        Origin::Whole
    } else {
        Origin::Statement {
            index: index + 1,
            of: all.len(),
        }
    };
    Some(Target {
        span: all[index].clone(),
        origin,
    })
}

/// Where a server's error position lands in the buffer.
///
/// A position is counted from 1, in characters, and from the start of the
/// string the server was handed — which is the statement, not the buffer.
/// Applying it to the buffer directly points confidently at a character in the
/// wrong statement, and looks right every time the one that failed happened to
/// be the first. `None` when the number could not have come from `sent`.
pub fn error_offset(position: u32, sent: &Span) -> Option<u32> {
    if position < 1 {
        return None;
    }
    let offset = sent.start + position - 1;
    // One past the last character is a real answer — it is what an unexpected
    // end of input points at — but anything beyond it is not.
    (offset <= sent.end).then_some(offset)
}

fn trimmed(chars: &[char], span: Span) -> Span {
    let mut lower = span.start as usize;
    let mut upper = (span.end as usize).min(chars.len());
    while lower < upper && chars[lower].is_whitespace() {
        lower += 1;
    }
    while upper > lower && chars[upper - 1].is_whitespace() {
        upper -= 1;
    }
    lower as u32..upper as u32
}
