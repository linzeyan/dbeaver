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

mod dialect;
mod keywords;
mod lex;
mod script;

pub use dialect::{
    ALL, CLICKHOUSE, DUCKDB, Dialect, DoubleQuote, MSSQL, MYSQL, POSTGRES, Parameter, SQLITE,
    for_scheme,
};
pub use lex::{Token, TokenKind, tokens};
pub use script::{Origin, Span, Target, error_offset, statements, target};
