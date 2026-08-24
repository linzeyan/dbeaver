//! The statements that would recreate what the navigator is showing.
//!
//! Upstream is not an influence here, it is the specification: the phase-4 exit
//! criterion is that this output matches DBeaver's for the same object, so every
//! rule below was read out of the Java rather than chosen. Where this
//! deliberately differs, the difference is recorded on the line that differs,
//! with what upstream emits and why this does not.
//!
//! Upstream assembles a table's DDL in three layers and so does this crate,
//! because only the third of them is per-database:
//!
//! - `model/impl/sql/edit/struct/SQLTableManager.getTableDDL` decides what goes
//!   into the script and in which order — drop header, `CREATE TABLE`, then
//!   whatever could not be written inside the parentheses.
//! - `model/sql/SQLUtils.generateScript` joins those into text.
//! - `ext.postgresql`'s managers supply the text of each column, constraint and
//!   index.
//!
//! The first two live in `org.jkiss.dbeaver.model` and are shared by every
//! database upstream supports, which is why [`Script`] lives here. The third is
//! per-database and lives behind [`Renderer`], one implementation per dialect —
//! and only two of the six assemble a script at all, which is why the seam is
//! there rather than in a function that learned to branch.
//!
//! What this does *not* do is read the database twice. Everything rendered comes
//! from the `Driver` metadata calls the structure pane already makes, which is
//! also the limit on what can be rendered: a fact upstream reads from a catalog
//! column that no metadata type carries is a fact this cannot state, and the
//! honest answer to that is a refusal rather than a guess.

mod clickhouse;
mod duckdb;
mod mssql;
mod mysql;
mod postgres;
mod sqlite;

use arrow::datatypes::{Field, Schema};
use async_trait::async_trait;
use dbconn::{DbError, DbResult, Driver, RelationInfo};
use dbsql::Dialect;

/// The DDL that would recreate `relation`, in the SQL `dialect` writes.
///
/// No `schema` parameter, although upstream's equivalent call sites pass one:
/// `RelationInfo` already carries the schema it was listed under, and a separate
/// argument is one more thing that can disagree with it. The relation is
/// identified by exactly what the navigator handed the caller.
pub async fn definition(
    driver: &dyn Driver,
    dialect: &'static Dialect,
    relation: &RelationInfo,
) -> DbResult<String> {
    match for_dialect(dialect) {
        Some(renderer) => renderer.definition(driver, relation).await,
        None => Err(DbError::new(format!(
            "DDL for {} has not been written yet",
            dialect.name
        ))),
    }
}

/// The statement that would make a table shaped like `columns`, in the SQL
/// `dialect` writes.
///
/// The table's name is written as it is given, the way the transfer's own target
/// writes one: a qualified name is the caller's to spell, and quoting it here
/// would turn `public.orders` into a single identifier with a dot in it.
///
/// The one entry point in this crate that renders something the database has
/// never seen. Everything else here describes what is already there, which is
/// why everything else here reads a `Driver` and this reads a file's columns.
pub fn create_table(dialect: &'static Dialect, table: &str, columns: &Schema) -> DbResult<String> {
    match for_dialect(dialect) {
        Some(renderer) => renderer.create_table(table, columns),
        None => Err(DbError::new(format!(
            "DDL for {} has not been written yet",
            dialect.name
        ))),
    }
}

/// What a file can ask a table's column to be.
///
/// Arrow has some fifty types; this is what a file being imported can actually
/// mean by them, reduced once here rather than six times over in the renderers.
/// The reduction widens deliberately — every whole number becomes the widest
/// whole number the database has — because a column made too wide takes the
/// whole file and a column made too narrow takes most of it, which is the worse
/// of the two by far.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ColumnKind {
    Bool,
    Int,
    Float,
    /// Precision and scale as the file states them. The one kind that carries a
    /// size, because a decimal held at a different scale is a different number
    /// and no server will mention it.
    Decimal(u8, i8),
    Text,
    Date,
    Timestamp,
}

/// The kind `field` asks for, or a refusal naming the column.
///
/// Binary is refused rather than given a column, for the reason
/// `DelimitedReader::new` refuses it: rows are sent as literal INSERTs and a
/// binary value is written into one as hex text, so a table with a binary column
/// would be a table this then fills with the wrong thing. Nested types — a JSON
/// Lines file with an object in a field — reach the same refusal, having no
/// single column to be at all.
fn kind_of(field: &Field) -> DbResult<ColumnKind> {
    use arrow::datatypes::DataType::*;
    Ok(match field.data_type() {
        Boolean => ColumnKind::Bool,
        // UInt64 among them: a value above `i64::MAX` is then refused by the
        // server, with the number in the message, rather than wrapping silently
        // into a negative one here.
        Int8 | Int16 | Int32 | Int64 | UInt8 | UInt16 | UInt32 | UInt64 => ColumnKind::Int,
        Float16 | Float32 | Float64 => ColumnKind::Float,
        Decimal128(precision, scale) | Decimal256(precision, scale) => {
            ColumnKind::Decimal(*precision, *scale)
        }
        Utf8 | LargeUtf8 | Utf8View => ColumnKind::Text,
        Date32 | Date64 => ColumnKind::Date,
        Timestamp(..) => ColumnKind::Timestamp,
        other => {
            return Err(DbError::new(format!(
                "column {} is {other}, which this cannot make a column for",
                field.name()
            )));
        }
    })
}

/// The shared half of a `CREATE TABLE`: the shape of the statement.
///
/// `word` is the whole of what differs between databases, plus whatever one of
/// them insists on after the closing bracket — ClickHouse has no table without
/// an engine. Six copies of the bracket-and-comma layout would be six places for
/// a trailing comma to be introduced in one of them.
///
/// No column is written `NOT NULL`, whatever the file's schema says. Parquet
/// records which of its columns had no nulls in it, and that is a fact about the
/// file rather than a rule about the table: a column that happens to be full
/// today would otherwise become a table that starts refusing rows part way
/// through the second import into it.
pub(crate) fn create_table_text(
    dialect: &'static Dialect,
    table: &str,
    columns: &Schema,
    word: impl Fn(ColumnKind) -> String,
    suffix: &str,
) -> DbResult<String> {
    if columns.fields().is_empty() {
        return Err(DbError::new("a table needs at least one column"));
    }
    let mut body = Vec::new();
    for field in columns.fields() {
        body.push(format!(
            "    {} {}",
            dialect.quote(field.name()),
            word(kind_of(field)?)
        ));
    }
    let mut script = Script::new();
    script.statement(&format!(
        "CREATE TABLE {table} (\n{}\n){suffix}",
        body.join(",\n")
    ));
    Ok(script.finish())
}

/// The half of DDL generation that is genuinely per-database.
///
/// One method, because that is how much the databases share. Upstream's own
/// split says the same thing: MySQL asks the server for `SHOW CREATE TABLE`,
/// SQLite keeps the statement it was created from, PostgreSQL builds one out of
/// the catalog — the *whole* of producing the text differs, and only the script
/// it goes into does not.
#[async_trait]
pub trait Renderer: Send + Sync {
    async fn definition(&self, driver: &dyn Driver, relation: &RelationInfo) -> DbResult<String>;

    /// The `CREATE TABLE` for a set of columns that came from a file.
    ///
    /// No default. A default would have to pick some database's type words, and
    /// a statement spelled for the wrong database is one that looks right and
    /// does not run — the same reason `for_dialect` refuses rather than falling
    /// back. Each renderer answers with the words its own server reads.
    fn create_table(&self, table: &str, columns: &Schema) -> DbResult<String>;
}

/// The renderer written for `dialect`, and `None` where none is yet.
///
/// A lookup, deliberately not a fallback: guessing that an unknown database
/// writes PostgreSQL's DDL would produce a statement that looks right and does
/// not run, which is worse than saying nothing. `dbsql::for_scheme` can afford
/// the opposite default because a wrong dialect there costs syntax colour.
pub fn for_dialect(dialect: &'static Dialect) -> Option<&'static dyn Renderer> {
    RENDERERS
        .iter()
        .find(|(known, _)| known.name == dialect.name)
        .map(|(_, renderer)| *renderer)
}

/// Every database whose DDL this build can write, in the order they arrived.
const RENDERERS: &[(&Dialect, &dyn Renderer)] = &[
    (&dbsql::POSTGRES, &postgres::POSTGRES),
    (&dbsql::SQLITE, &sqlite::SQLITE),
    (&dbsql::MYSQL, &mysql::MYSQL),
    (&dbsql::CLICKHOUSE, &clickhouse::CLICKHOUSE),
    (&dbsql::MSSQL, &mssql::MSSQL),
    (&dbsql::DUCKDB, &duckdb::DUCKDB),
];

/// A script under construction, joined the way upstream joins one.
///
/// `SQLUtils.generateScript` has two rules and they are not symmetric. A
/// statement is followed by `;` — unless it brought its own — and one newline. A
/// comment gets a blank line before it, unless one is already there, and a blank
/// line after. That asymmetry is what puts the section headings in a table's DDL
/// on their own, and reproducing it by hand at each call site is how the third
/// heading ends up spaced differently from the first two.
pub(crate) struct Script(String);

impl Script {
    pub(crate) fn new() -> Self {
        Self(String::new())
    }

    pub(crate) fn statement(&mut self, sql: &str) {
        self.0.push_str(sql);
        if !sql.trim_end().ends_with(';') {
            self.0.push(';');
        }
        self.0.push('\n');
    }

    pub(crate) fn comment(&mut self, text: &str) {
        // Upstream counts the trailing newlines and adds one if there are fewer
        // than two; since everything written here ends in a newline already,
        // that reduces to "is there a blank line". An empty script gets nothing,
        // so a DDL that opens with a comment does not open with a blank line.
        if !self.0.is_empty() && !self.0.ends_with("\n\n") {
            self.0.push('\n');
        }
        self.0.push_str("-- ");
        self.0.push_str(text);
        self.0.push_str("\n\n");
    }

    /// The finished script, without the newline the last statement left.
    ///
    /// Upstream keeps it and the editor that shows the text trims it
    /// (`SQLSourceViewer.getSourceText`). Trimming here instead means a caller
    /// that writes this to a file, a clipboard or a test assertion gets the same
    /// string as one that shows it, rather than each of them deciding.
    pub(crate) fn finish(self) -> String {
        self.0.trim_end().to_string()
    }
}

#[cfg(test)]
mod tests {
    use arrow::datatypes::{DataType, Field, Schema, TimeUnit};

    /// The seven kinds at once, and a name that has to be quoted.
    fn a_files_columns() -> Schema {
        Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("Order Date", DataType::Date32, true),
            Field::new("amount", DataType::Decimal128(12, 2), true),
            Field::new("ratio", DataType::Float64, true),
            Field::new("paid", DataType::Boolean, true),
            Field::new("note", DataType::Utf8, true),
            Field::new(
                "seen_at",
                DataType::Timestamp(TimeUnit::Microsecond, None),
                true,
            ),
        ])
    }

    /// What each database is told to make, written out in full.
    ///
    /// Six strings and not a rule, because the whole of what is being checked is
    /// the words — a mapping that reads perfectly well and spells `datetime2` for
    /// PostgreSQL is exactly the failure this catches, and it is caught by
    /// someone reading the six side by side. The schema carries all seven kinds,
    /// so every arm of every renderer appears here.
    ///
    /// Four of the six are then run against a real server or library, in this
    /// crate's own test files; ClickHouse and SQL Server have no container here
    /// and rest on these strings alone.
    #[test]
    fn a_files_columns_become_a_table_in_each_databases_own_words() {
        let columns = a_files_columns();
        let expected = [
            (
                &dbsql::POSTGRES,
                "CREATE TABLE staging.orders (
    id bigint,
    \"Order Date\" date,
    amount numeric(12, 2),
    ratio double precision,
    paid boolean,
    note text,
    seen_at timestamp
);",
            ),
            (
                &dbsql::MYSQL,
                "CREATE TABLE staging.orders (
    id BIGINT,
    `Order Date` DATE,
    amount DECIMAL(12, 2),
    ratio DOUBLE,
    paid BOOLEAN,
    note TEXT,
    seen_at DATETIME
);",
            ),
            (
                &dbsql::SQLITE,
                "CREATE TABLE staging.orders (
    id INTEGER,
    \"Order Date\" TEXT,
    amount NUMERIC(12, 2),
    ratio REAL,
    paid BOOLEAN,
    note TEXT,
    seen_at TEXT
);",
            ),
            (
                &dbsql::MSSQL,
                "CREATE TABLE staging.orders (
    id bigint,
    [Order Date] date,
    amount decimal(12, 2),
    ratio float,
    paid bit,
    note nvarchar(max),
    seen_at datetime2
);",
            ),
            (
                &dbsql::DUCKDB,
                "CREATE TABLE staging.orders (
    id BIGINT,
    \"Order Date\" DATE,
    amount DECIMAL(12, 2),
    ratio DOUBLE,
    paid BOOLEAN,
    note VARCHAR,
    seen_at TIMESTAMP
);",
            ),
            (
                &dbsql::CLICKHOUSE,
                "CREATE TABLE staging.orders (
    \"id\" Nullable(Int64),
    \"Order Date\" Nullable(Date32),
    amount Nullable(Decimal(12, 2)),
    ratio Nullable(Float64),
    paid Nullable(Bool),
    note Nullable(String),
    seen_at Nullable(DateTime64(6))
)
ENGINE = MergeTree
ORDER BY tuple();",
            ),
        ];
        for (dialect, statement) in expected {
            assert_eq!(
                super::create_table(dialect, "staging.orders", &columns).expect(dialect.name),
                statement,
                "{}",
                dialect.name
            );
        }
    }

    /// A column that cannot be imported does not get a column made for it.
    ///
    /// Binary and nested values both reach the table as literal text inside an
    /// INSERT, so a table made to hold them is a table this then fills with
    /// something else. The refusal names the column, because the file is what has
    /// to change and its name is what to look for.
    #[test]
    fn a_column_the_import_cannot_carry_is_refused_rather_than_guessed_at() {
        for data_type in [
            DataType::Binary,
            DataType::List(std::sync::Arc::new(Field::new(
                "item",
                DataType::Int64,
                true,
            ))),
        ] {
            let columns = Schema::new(vec![
                Field::new("id", DataType::Int64, true),
                Field::new("payload", data_type.clone(), true),
            ]);
            let error = super::create_table(&dbsql::POSTGRES, "t", &columns)
                .expect_err("a table was rendered for a column nothing can fill");
            assert!(error.to_string().contains("payload"), "{error}");
        }
    }

    /// An empty file is not a table with no columns.
    ///
    /// `CREATE TABLE t ()` is accepted by PostgreSQL and means something — a
    /// table nothing can be put in — so the emptiness has to be caught here
    /// rather than left for the server to be relaxed about.
    #[test]
    fn a_file_with_no_columns_is_refused() {
        let error = super::create_table(&dbsql::POSTGRES, "t", &Schema::empty())
            .expect_err("a table with no columns was rendered");
        assert!(error.to_string().contains("at least one column"), "{error}");
    }

    /// Every database the app can connect to can have its DDL written.
    ///
    /// This replaces a test that asserted the opposite — that an unwritten
    /// dialect refuses instead of being rendered as PostgreSQL — which was true
    /// until the sixth renderer landed and left it nothing to be about. The
    /// refusal in [`super::definition`] stays, because the next database to
    /// arrive will reach it before its renderer does; what this pins is that
    /// nothing already shipping is sitting on it. `dbsql::ALL` is where a
    /// database is declared, so a new entry there fails here until
    /// `RENDERERS` learns about it.
    #[test]
    fn every_dialect_the_app_speaks_has_a_renderer() {
        for dialect in dbsql::ALL {
            assert!(
                super::for_dialect(dialect).is_some(),
                "{} is a dialect this build connects with and cannot write DDL for",
                dialect.name
            );
        }
    }
}
