//! Where the six databases disagree about what the text means.
//!
//! One table, six rows. The exit criterion this exists for is that dialect
//! differences are table-driven and not branched through the lexer, and the
//! test of that is mechanical: the lexer below reads fields of `Dialect` and
//! never names a database. Adding a seventh is adding a row.
//!
//! The fields are only the differences that change where a token *ends*. A
//! dialect that spells `SUBSTRING` differently is a parser's problem; a dialect
//! where `"` opens a string rather than an identifier is a lexer's, because
//! getting it wrong leaves the scanner one quote out of step for the rest of
//! the buffer and every statement boundary after it lands in the wrong place.

use crate::keywords;

/// What a double quote opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoubleQuote {
    /// A quoted identifier, which is what the standard says.
    Identifier,
    /// A string. MySQL's default, and the reason `"a;b"` is not a statement
    /// boundary there for a different reason than everywhere else.
    String,
}

/// What a database understands, in the ways that decide where a token ends.
#[derive(Debug, Clone, Copy)]
pub struct Dialect {
    /// The scheme this is reached by, which is also how a caller names it.
    pub name: &'static str,
    pub double_quote: DoubleQuote,
    /// `` `name` `` is an identifier.
    pub backtick_identifiers: bool,
    /// `[name]` is an identifier. SQL Server's own spelling, and SQLite accepts
    /// it too for the sake of scripts written against SQL Server.
    pub bracket_identifiers: bool,
    /// A backslash escapes the character after it inside an ordinary `'…'`.
    ///
    /// True for MySQL and ClickHouse. False for PostgreSQL, where
    /// `standard_conforming_strings` has been on by default since 9.1, so
    /// `'a\'` is a complete string ending in a backslash — reading it the other
    /// way leaves the scanner one quote out of step for the rest of the buffer.
    pub backslash_escapes: bool,
    /// `E'…'` is a string in which a backslash escapes, whatever
    /// `backslash_escapes` says about a plain one. PostgreSQL only.
    pub escape_string_prefix: bool,
    /// Letters that may sit immediately before an opening quote and belong to
    /// the literal: `N'…'` for SQL Server's national character strings, `x'…'`
    /// and `b'…'` for MySQL's hex and bit literals.
    ///
    /// Lower case; matching folds. A prefix is only a prefix when nothing
    /// identifier-shaped precedes it, so `someN'x'` is still an identifier
    /// followed by a string.
    pub string_prefixes: &'static [char],
    /// `# comment to end of line`.
    pub hash_line_comments: bool,
    /// `/* /* */ */` is one comment rather than one and a half.
    ///
    /// PostgreSQL, SQL Server and DuckDB nest; MySQL and SQLite do not. A
    /// scanner that stops at the first `*/` where the server does not leaves
    /// the tail of a commented-out block being read as SQL.
    pub nested_block_comments: bool,
    /// `$tag$ … $tag$` is a string. PostgreSQL, and DuckDB after it.
    pub dollar_quoting: bool,
    /// What a placeholder looks like, for the highlighter to leave alone rather
    /// than paint as an identifier.
    pub parameters: &'static [Parameter],
    /// What this dialect calls a keyword beyond [`keywords::COMMON`].
    pub(crate) extra_keywords: &'static [&'static str],
}

/// One spelling of a parameter placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parameter {
    /// `?`, positional and anonymous.
    Question,
    /// `$1`, positional and numbered.
    DollarNumber,
    /// `@name`.
    AtName,
    /// `:name`.
    ColonName,
}

impl Dialect {
    /// Whether `word` — already folded to lower case — is a keyword here.
    pub(crate) fn is_keyword(&self, word: &str) -> bool {
        keywords::contains(keywords::COMMON, word) || keywords::contains(self.extra_keywords, word)
    }
}

/// The row every other row is a difference from.
///
/// Private, and spread with `..` below, so that a field added here has to be
/// answered for by every dialect rather than silently defaulting — the compiler
/// will not catch a missing answer, but a reader comparing two rows will.
const BASE: Dialect = Dialect {
    name: "",
    double_quote: DoubleQuote::Identifier,
    backtick_identifiers: false,
    bracket_identifiers: false,
    backslash_escapes: false,
    escape_string_prefix: false,
    string_prefixes: &[],
    hash_line_comments: false,
    nested_block_comments: false,
    dollar_quoting: false,
    parameters: &[Parameter::Question],
    extra_keywords: &[],
};

pub const POSTGRES: Dialect = Dialect {
    name: "postgres",
    escape_string_prefix: true,
    // `B'1010'` and `X'1f'` are bit-string literals; `U&'…'` is a Unicode
    // escape string, whose `&` this does not cover and which is rare enough to
    // be worth less than the complication.
    string_prefixes: &['b', 'x'],
    nested_block_comments: true,
    dollar_quoting: true,
    parameters: &[Parameter::DollarNumber],
    extra_keywords: &[],
    ..BASE
};

pub const MYSQL: Dialect = Dialect {
    name: "mysql",
    // Not a preference: without `ANSI_QUOTES` in `sql_mode`, which is off by
    // default, MySQL reads `"abc"` as a string. A client that guessed
    // "identifier" would mis-scan the most ordinary MySQL there is.
    double_quote: DoubleQuote::String,
    backtick_identifiers: true,
    backslash_escapes: true,
    string_prefixes: &['x', 'b', 'n'],
    hash_line_comments: true,
    extra_keywords: keywords::MYSQL,
    ..BASE
};

pub const MSSQL: Dialect = Dialect {
    name: "sqlserver",
    bracket_identifiers: true,
    string_prefixes: &['n'],
    nested_block_comments: true,
    parameters: &[Parameter::AtName],
    extra_keywords: keywords::MSSQL,
    ..BASE
};

pub const SQLITE: Dialect = Dialect {
    name: "sqlite",
    // SQLite accepts all three, deliberately, so that scripts written for MySQL
    // or SQL Server run unaltered. A client that accepted fewer would refuse
    // text the database takes.
    backtick_identifiers: true,
    bracket_identifiers: true,
    parameters: &[Parameter::Question, Parameter::ColonName, Parameter::AtName],
    extra_keywords: keywords::SQLITE,
    ..BASE
};

pub const DUCKDB: Dialect = Dialect {
    name: "duckdb",
    string_prefixes: &['b', 'x'],
    nested_block_comments: true,
    dollar_quoting: true,
    parameters: &[
        Parameter::Question,
        Parameter::DollarNumber,
        Parameter::ColonName,
    ],
    extra_keywords: keywords::DUCKDB,
    ..BASE
};

pub const CLICKHOUSE: Dialect = Dialect {
    name: "clickhouse",
    backtick_identifiers: true,
    backslash_escapes: true,
    hash_line_comments: true,
    parameters: &[Parameter::Question],
    extra_keywords: keywords::CLICKHOUSE,
    ..BASE
};

/// Every dialect this build knows, in the order a list of them should read.
pub const ALL: &[&Dialect] = &[&POSTGRES, &MYSQL, &MSSQL, &SQLITE, &DUCKDB, &CLICKHOUSE];

/// The dialect a connection scheme is written in.
///
/// PostgreSQL for anything unrecognised, which is a deliberate choice rather
/// than a fallback with nothing behind it: the alternative is refusing to paint
/// an editor at all, and PostgreSQL's grammar is the one the SQL standard is
/// closest to. A wrong guess costs colour, not correctness — the statement is
/// sent as typed either way.
pub fn for_scheme(scheme: &str) -> &'static Dialect {
    match scheme {
        "mysql" => &MYSQL,
        "sqlserver" => &MSSQL,
        "sqlite" => &SQLITE,
        "duckdb" => &DUCKDB,
        "clickhouse" | "clickhouses" => &CLICKHOUSE,
        _ => &POSTGRES,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every dialect is reachable by the scheme it names.
    ///
    /// The failure this guards against is the quiet one: a dialect added to
    /// `ALL` and not to `for_scheme` is a table row nothing reads, and the
    /// editor silently paints that database as PostgreSQL.
    #[test]
    fn every_dialect_is_reachable_by_its_own_name() {
        for dialect in ALL {
            assert_eq!(
                for_scheme(dialect.name).name,
                dialect.name,
                "{} is in the table but not reachable",
                dialect.name
            );
        }
    }

    #[test]
    fn an_unknown_scheme_is_read_as_postgresql() {
        assert_eq!(for_scheme("mongodb").name, "postgres");
        assert_eq!(for_scheme("").name, "postgres");
    }
}
