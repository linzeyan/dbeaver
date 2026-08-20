//! Reading SQL well enough to edit it.
//!
//! Nothing here talks to a database. It takes the text in an editor and answers
//! the questions an editor asks about it: which of these characters is a
//! keyword, where does this statement end, which statement is the caret in.
//!
//! It lives in the core rather than the front end because there will be two
//! front ends, and a scanner is exactly the kind of thing that is written twice
//! and then disagrees with itself. The macOS build had a Swift one first; this
//! replaces it, and the rules it earned are kept as the tests in `tests/`.

mod complete;
mod danger;
mod dialect;
mod format;
mod keywords;
mod lex;
mod parse;
mod script;

pub use complete::{Completion, Expect, complete};
pub use danger::{Danger, danger};
pub use dialect::{
    ALL, CLICKHOUSE, DUCKDB, Dialect, DoubleQuote, MSSQL, MYSQL, POSTGRES, Parameter, RowLimit,
    SQLITE, for_scheme, of_scheme,
};
pub use format::format;
pub use lex::{Token, TokenKind, tokens};
pub use parse::{Scope, Source, Statement, statement};
pub use script::{Origin, Scan, Span, Target, error_offset, scan, statements, target};
