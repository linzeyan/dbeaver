//! Laying SQL out again without changing what it says.
//!
//! The formatter is `lax-sql`, chosen over the far more widely used `sqlformat`
//! on one property: it keeps backtick, `[bracketed]` and `$tag$dollar quoted$tag$`
//! regions as single opaque tokens, so it cannot reach inside one. `sqlformat`
//! can, and does — it reflows the body of a `$$ … $$` function, where whitespace
//! is the value rather than the layout, and under its default dialect it turns
//! the T-SQL identifier `[order]` into `[ order ]`, which is a different
//! identifier. A formatter that rewrites a function body is worse than no
//! formatter, because the damage is invisible until the function next runs.
//!
//! That property is also why nothing here takes a `Dialect`. The crate is
//! dialect agnostic by construction, so a dialect argument would be a parameter
//! nothing reads.

use std::path::Path;

use dprint_core::configuration::NewLineKind;
use lax_sql::configuration::{ClauseStyle, Configuration, KeywordCase};

/// Keywords are left exactly as they were typed, and clauses pack to the line
/// width rather than taking one line per item.
///
/// Both are choices about surprise. Upper-casing keywords rewrites text the user
/// did not ask to have rewritten, and `Expanded` — the one-item-per-line look
/// upstream produces — puts `JOIN` alone on a line with its table indented
/// below, which is longer and harder to read than `join customers c on …`.
fn settings() -> Configuration {
    Configuration {
        line_width: 80,
        indent_width: 2,
        use_tabs: false,
        new_line_kind: NewLineKind::LineFeed,
        keyword_case: KeywordCase::Preserve,
        clause_style: ClauseStyle::Fill,
        // Directives a comment can carry to turn formatting off. Nothing in the
        // product surfaces them yet; they are named for this project rather than
        // left at the crate's `dprint-ignore` defaults, which name a tool that is
        // not shipped here. The value has to be a word somebody would only write
        // on purpose — a punctuation string like `/*` would match the opening of
        // an ordinary block comment and silently stop formatting altogether.
        ignore_node_comment_text: "sql-format-ignore".to_string(),
        ignore_file_comment_text: "sql-format-ignore-file".to_string(),
    }
}

/// The statement laid out again, or the text untouched where it cannot be.
///
/// Every failure returns the input. This runs on what somebody is holding in an
/// editor, so the worst outcome is not an ugly result but a lost one: a
/// `Result` here would make every caller invent a policy, and every sensible
/// policy is this one.
pub fn format(text: &str) -> String {
    // The path is what a dprint plugin formats by; this crate ignores it.
    match lax_sql::format_text(Path::new("statement.sql"), text, &settings()) {
        Ok(Some(formatted)) => formatted,
        Ok(None) | Err(_) => text.to_string(),
    }
}
