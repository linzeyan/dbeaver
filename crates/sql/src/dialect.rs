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
    /// The delimiters to write around a name that needs them.
    ///
    /// Preference, not capability: SQLite takes all three and is given the
    /// standard one, so that a script written here reads as SQL rather than as
    /// SQLite. SQL Server is given brackets because `"…"` there depends on
    /// `QUOTED_IDENTIFIER` being on, and a client that emits a name whose
    /// meaning depends on a session setting is emitting a guess.
    pub identifier_quotes: (&'static str, &'static str),
    /// How a statement asks for at most so many rows.
    pub row_limit: RowLimit,
    /// How this database spells an insert of one row of nothing but its own
    /// defaults, written after `INSERT INTO <table>`.
    ///
    /// A dialect fact for the reason `row_limit` is one: the standard's `DEFAULT
    /// VALUES` is what PostgreSQL, SQL Server, SQLite and DuckDB take, and MySQL
    /// takes an empty pair of column and value lists instead. `None` says the
    /// database has no spelling at all, which is a different answer from not
    /// having been filled in — ClickHouse's `INSERT` takes values or a `SELECT`
    /// and has no form meaning "a row of every default". A caller that guessed
    /// there would hand somebody a statement that does not run.
    pub default_row: Option<&'static str>,
    /// What this dialect calls a keyword beyond [`keywords::COMMON`].
    pub(crate) extra_keywords: &'static [&'static str],
}

/// Where a row ceiling goes in a statement.
///
/// A dialect fact rather than a suffix every caller appends, because SQL Server
/// puts it in front of the columns and everything else puts it at the end. A
/// client that appended `LIMIT 1000` to a T-SQL statement would be handing the
/// user something that does not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowLimit {
    /// `… LIMIT 1000`, at the end.
    Limit,
    /// `SELECT TOP (1000) …`, before the columns.
    Top,
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

    /// `name` written so that this database reads it as the name it is.
    ///
    /// Quoted only when it has to be, because quoting everything is correct and
    /// unreadable — an editor that completes `SELECT "id" FROM "orders"` is
    /// technically right and nobody wants it. It has to be when the name holds
    /// something an unquoted identifier cannot, when it starts with a digit, or
    /// when it is a keyword and would be read as one.
    ///
    /// Case is why the second rule is not "is it lower case": PostgreSQL folds
    /// an unquoted name down and SQL Server does not fold at all, so `Orders`
    /// unquoted finds `orders` on one and `Orders` on the other. Quoting a name
    /// that is not already lower case keeps it meaning what the catalog says it
    /// means on every one of them.
    pub fn quote(&self, name: &str) -> String {
        let plain = !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name
                .chars()
                .all(|c| c == '_' || c.is_ascii_lowercase() || c.is_ascii_digit())
            && !self.is_keyword(name);
        if plain {
            return name.to_string();
        }
        let (open, close) = self.identifier_quotes;
        // A delimiter inside the name is doubled, which is the same rule the
        // lexer reads one by.
        let inner = name.replace(close, &format!("{close}{close}"));
        format!("{open}{inner}{close}")
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
    identifier_quotes: ("\"", "\""),
    row_limit: RowLimit::Limit,
    default_row: Some("DEFAULT VALUES"),
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
    identifier_quotes: ("`", "`"),
    // MySQL rejects `DEFAULT VALUES` outright; the empty lists are its own way of
    // saying the same thing, and they are only valid as a pair.
    default_row: Some("() VALUES ()"),
    extra_keywords: keywords::MYSQL,
    ..BASE
};

pub const MSSQL: Dialect = Dialect {
    name: "sqlserver",
    bracket_identifiers: true,
    string_prefixes: &['n'],
    nested_block_comments: true,
    parameters: &[Parameter::AtName],
    identifier_quotes: ("[", "]"),
    row_limit: RowLimit::Top,
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
    // ClickHouse has no spelling for it. `INSERT INTO t` must be followed by
    // `VALUES`, a `FORMAT`, or a `SELECT`, and none of those can be empty — so a
    // row of pure defaults is a thing this database cannot be asked for, rather
    // than a thing this table has not been told how to ask for.
    default_row: None,
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
///
/// Callers for whom a wrong guess costs more than colour want [`of_scheme`].
pub fn for_scheme(scheme: &str) -> &'static Dialect {
    of_scheme(scheme).unwrap_or(&POSTGRES)
}

/// The dialect a connection scheme is written in, or `None` where this build
/// does not know that database's SQL.
///
/// The same question [`for_scheme`] answers, without the guess. MongoDB is the
/// case that made this necessary: it has no dialect here, `for_scheme` hands it
/// PostgreSQL's, and anything downstream that generates SQL rather than painting
/// it then produces a PostgreSQL statement about a collection — which is the
/// exact failure `dbddl::for_dialect` refuses to make one level lower.
pub fn of_scheme(scheme: &str) -> Option<&'static Dialect> {
    match scheme {
        "postgres" | "postgresql" => Some(&POSTGRES),
        "mysql" => Some(&MYSQL),
        "sqlserver" => Some(&MSSQL),
        "sqlite" => Some(&SQLITE),
        "duckdb" => Some(&DUCKDB),
        "clickhouse" | "clickhouses" => Some(&CLICKHOUSE),
        _ => None,
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
            assert_eq!(
                of_scheme(dialect.name).map(|d| d.name),
                Some(dialect.name),
                "{} is reachable only through the guess",
                dialect.name
            );
        }
    }

    #[test]
    fn an_unknown_scheme_is_read_as_postgresql() {
        assert_eq!(for_scheme("mongodb").name, "postgres");
        assert_eq!(for_scheme("").name, "postgres");
    }

    /// The same two schemes, asked the question that does not guess.
    ///
    /// This is the answer anything generating SQL has to use: painting MongoDB's
    /// editor with PostgreSQL's keywords costs colour, and writing a PostgreSQL
    /// `CREATE TABLE` for one of its collections costs a statement that cannot
    /// run.
    #[test]
    fn a_database_with_no_dialect_here_is_not_given_one() {
        assert!(of_scheme("mongodb").is_none());
        assert!(of_scheme("").is_none());
    }
}
