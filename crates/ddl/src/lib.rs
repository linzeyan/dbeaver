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

/// The statement that would make `change` to a relation that is already there,
/// in the SQL `dialect` writes.
///
/// Rendered and handed back rather than run. Everything in this crate composes
/// SQL and nothing in it executes any, which is what lets the front end show the
/// statement before it goes — and these three are the ones where showing it
/// matters most, two of them being irreversible.
pub fn table_change(
    dialect: &'static Dialect,
    relation: &RelationInfo,
    change: TableChange<'_>,
) -> DbResult<String> {
    match for_dialect(dialect) {
        Some(renderer) => renderer.table_change(relation, change),
        None => Err(DbError::new(format!(
            "DDL for {} has not been written yet",
            dialect.name
        ))),
    }
}

/// What to do to a relation that already exists.
///
/// Three verbs rather than one `alter` with a payload, because they are not
/// variations on each other. Two destroy something and the third does not; the
/// one in the middle needs an argument the others have no use for; and upstream
/// keeps them in three different places — `addObjectDeleteActions` on the shared
/// table manager, `addObjectRenameActions` per database, and truncate not an
/// editor action at all but a per-database tool.
///
/// Deliberately not extended to cover a relation's columns or indexes. Those are
/// `ALTER` statements whose text differs per database in ways these do not, and
/// folding them in here would make one enum stand for two different amounts of
/// per-dialect work.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TableChange<'a> {
    /// Remove the relation and everything in it.
    Drop,
    /// Remove every row and leave the relation standing.
    Truncate,
    /// Give it another name, in the schema it is already in.
    ///
    /// Moving between schemas is a different statement on several of these
    /// databases and is not offered: `ALTER TABLE … SET SCHEMA` on PostgreSQL,
    /// a qualified `RENAME TABLE` on MySQL, and nothing at all on SQLite.
    Rename { to: &'a str },
}

/// `DROP <word> <name>`, which is the one of the three that is shared.
///
/// `SQLTableManager.addObjectDeleteActions` writes every database's `DROP` and
/// only the noun after it is per-dialect, so the shape lives here and the word
/// is the renderer's. The other two do not share: `addObjectRenameActions`
/// throws by default and each manager that supports one writes its own.
///
/// No `CASCADE`. Upstream appends it only when `OPTION_DELETE_CASCADE` is set,
/// which is a checkbox that defaults off — and the default is the one to keep,
/// since a cascade takes objects that are not on the screen the button was
/// pressed on. A drop the server refuses for having dependents is an answer
/// somebody can act on; one that quietly took four other tables is not.
pub(crate) fn drop_text(word: &str, name: &str) -> String {
    let mut script = Script::new();
    script.statement(&format!("DROP {word} {name}"));
    script.finish()
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

    /// The statement for one change to a relation that is already there.
    ///
    /// No default, for the reason `create_table` has none, and one more: a
    /// default that wrote `DROP TABLE` for everybody would be right often enough
    /// to look correct and wrong exactly where it costs the most — a
    /// materialized view, a foreign table, a database that spells rename with
    /// `sp_rename`. Each renderer answers for its own database or refuses by
    /// name.
    ///
    /// A refusal here is per relation as well as per database: SQLite renames a
    /// table and cannot rename a view, and no server truncates a view, because a
    /// view has no rows of its own to remove. The front end shows the refusal
    /// where it would have shown the statement, which is the same place and the
    /// same gesture — see the sheet this feeds.
    fn table_change(&self, relation: &RelationInfo, change: TableChange<'_>) -> DbResult<String>;

    /// Whether this renderer writes any [`TableChange`] at all.
    ///
    /// Asked separately from `table_change` because a menu is built for a
    /// connection and not for a row: the front end needs to know whether to draw
    /// the items before anybody has chosen a relation for them to act on. A
    /// renderer that has not been written must answer `false` so that the items
    /// are absent rather than present and always refusing.
    ///
    /// It can disagree with `table_change`, which is what
    /// `a_renderer_that_claims_changes_writes_one` exists to stop: every
    /// renderer answering `true` there has to return a statement for an ordinary
    /// table's drop, and every one answering `false` has to refuse it.
    fn changes_relations(&self) -> bool;

    /// The statement that makes or removes a whole database.
    ///
    /// No default, and here the reason is the noun rather than the verb: the
    /// object these two act on is called a database on one server and a schema
    /// on the next, and MySQL's `CREATE SCHEMA` and PostgreSQL's `CREATE SCHEMA`
    /// make different things. A renderer that inherited either word would be
    /// making the wrong object on half of these servers.
    fn database_change(&self, change: DatabaseChange<'_>) -> DbResult<String>;

    /// Whether this renderer writes either [`DatabaseChange`].
    ///
    /// Separate from `changes_relations` and not implied by it. SQLite is the
    /// case that proves it: it drops and renames a table, and it has no
    /// statement for making a database at all — a database there is a file, and
    /// a file is made by opening a path rather than by sending SQL.
    fn changes_databases(&self) -> bool;
}

/// Making or removing a whole database.
///
/// Two, not three. A rename is missing because it is missing upstream too:
/// `MySQLDatabaseManager.renameObject` throws outright, and PostgreSQL's
/// `ALTER DATABASE … RENAME TO` only works from a connection to some *other*
/// database — which is exactly the connection a window pointed at this one does
/// not have. A verb that worked on one engine and threw on the other is not a
/// verb this enum should carry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DatabaseChange<'a> {
    /// Make one, empty, with the server's own defaults for everything else.
    ///
    /// No owner, template, encoding or tablespace, although
    /// `PostgreDatabaseManager` appends all four. Each is optional there and
    /// defaults to null, so the statement below is the one upstream writes for a
    /// database created without touching its form — and every one of the four
    /// names an object this build does not read.
    Create { name: &'a str },
    /// Remove one and everything in it.
    Drop { name: &'a str },
}

/// The statement that would make or remove a database, in the SQL `dialect`
/// writes.
///
/// Rendered and handed back rather than run, like [`table_change`]. The drop is
/// the most destructive statement this crate composes — it takes every relation
/// in the database with it — so showing it first matters more here than
/// anywhere else.
pub fn database_change(dialect: &'static Dialect, change: DatabaseChange<'_>) -> DbResult<String> {
    match for_dialect(dialect) {
        Some(renderer) => renderer.database_change(change),
        None => Err(DbError::new(format!(
            "DDL for {} has not been written yet",
            dialect.name
        ))),
    }
}

/// Whether this build makes or removes a database on `dialect`.
///
/// What the front end reads to decide whether the New Database item and the
/// database row's Drop item exist at all.
pub fn changes_databases(dialect: &'static Dialect) -> bool {
    for_dialect(dialect).is_some_and(|renderer| renderer.changes_databases())
}

/// Whether this build writes any change to a relation on `dialect`.
///
/// What the front end reads to decide whether the Drop, Truncate and Rename
/// items exist. Which of the three a *particular* relation can take is a
/// narrower question with its own answer — a view cannot be truncated anywhere —
/// and that one is answered by [`table_change`] where the statement would have
/// been, so that the refusal is read in the place the statement is shown.
pub fn changes_relations(dialect: &'static Dialect) -> bool {
    for_dialect(dialect).is_some_and(|renderer| renderer.changes_relations())
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
    use super::{DatabaseChange, TableChange};
    use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
    use dbconn::{RelationInfo, RelationKind};
    use dbsql::Dialect;

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

    fn relation(schema: &str, name: &str, kind: RelationKind) -> RelationInfo {
        RelationInfo {
            schema: schema.to_string(),
            name: name.to_string(),
            kind,
            estimated_rows: None,
        }
    }

    /// Each change written out in full, for each database that writes it.
    ///
    /// Strings and not a rule, for the reason
    /// `a_files_columns_become_a_table_in_each_databases_own_words` is written
    /// that way: the whole of what is being checked is the words, and a rename
    /// that reads perfectly well and spells MySQL's form for PostgreSQL is
    /// exactly the failure this catches.
    ///
    /// These are also the statements nobody gets a second try at. Two of the
    /// three destroy something, so a test that asserted only "some statement
    /// came back" would be no test at all — what matters is that the noun, the
    /// name and the verb are the ones the server will read.
    #[test]
    fn a_change_is_spelled_the_way_the_server_being_changed_reads_it() {
        let orders = relation("staging", "orders", RelationKind::Table);
        let cases: &[(&Dialect, &RelationInfo, TableChange, &str)] = &[
            (
                &dbsql::POSTGRES,
                &orders,
                TableChange::Drop,
                "DROP TABLE staging.orders;",
            ),
            (
                &dbsql::POSTGRES,
                &orders,
                TableChange::Truncate,
                "TRUNCATE TABLE staging.orders;",
            ),
            (
                &dbsql::POSTGRES,
                &orders,
                TableChange::Rename { to: "orders_old" },
                "ALTER TABLE staging.orders RENAME TO orders_old;",
            ),
            (
                &dbsql::MYSQL,
                &orders,
                TableChange::Drop,
                "DROP TABLE staging.orders;",
            ),
            (
                &dbsql::MYSQL,
                &orders,
                TableChange::Truncate,
                "TRUNCATE TABLE staging.orders;",
            ),
            // Both ends named, which is MySQL's own form and is what keeps the
            // table in the database it is in.
            (
                &dbsql::MYSQL,
                &orders,
                TableChange::Rename { to: "orders_old" },
                "RENAME TABLE staging.orders TO staging.orders_old;",
            ),
            (
                &dbsql::SQLITE,
                &orders,
                TableChange::Drop,
                "DROP TABLE staging.orders;",
            ),
            (
                &dbsql::SQLITE,
                &orders,
                TableChange::Rename { to: "orders_old" },
                "ALTER TABLE staging.orders RENAME TO orders_old;",
            ),
        ];
        for (dialect, relation, change, expected) in cases {
            let statement = super::table_change(dialect, relation, *change)
                .unwrap_or_else(|e| panic!("{} refused {change:?}: {e}", dialect.name));
            assert_eq!(
                statement, *expected,
                "{} wrote the wrong statement for {change:?}",
                dialect.name
            );
        }
    }

    /// PostgreSQL says which kind of relation it is dropping, and so must this.
    ///
    /// The one place this crate knowingly departs from upstream in a way that
    /// changes the statement. `SQLTableManager.getDropTableType` reduces to
    /// `isView(table) ? "VIEW" : "TABLE"`, so DBeaver emits `DROP VIEW` for a
    /// materialized view and PostgreSQL answers "…is not a view. Use DROP
    /// MATERIALIZED VIEW". A statement that cannot run is not a specification to
    /// match, and the noun PostgreSQL's own rename path already uses is the one
    /// written here.
    #[test]
    fn postgres_names_the_kind_of_relation_it_is_dropping() {
        let cases = [
            (RelationKind::View, "DROP VIEW staging.summary;"),
            (
                RelationKind::MaterializedView,
                "DROP MATERIALIZED VIEW staging.summary;",
            ),
            (
                RelationKind::ForeignTable,
                "DROP FOREIGN TABLE staging.summary;",
            ),
            // A partition is a table to every statement here; only `CREATE`
            // cares that it was made with a partition clause.
            (
                RelationKind::PartitionedTable,
                "DROP TABLE staging.summary;",
            ),
        ];
        for (kind, expected) in cases {
            let statement = super::table_change(
                &dbsql::POSTGRES,
                &relation("staging", "summary", kind),
                TableChange::Drop,
            )
            .unwrap_or_else(|e| panic!("PostgreSQL refused to drop a {kind:?}: {e}"));
            assert_eq!(statement, expected, "the noun for a {kind:?} is wrong");
        }
    }

    /// A name the server would not read bare is quoted, in each database's own
    /// delimiter.
    ///
    /// The one thing about these statements that is not visible in the ones
    /// above, and the one that turns a drop into a syntax error or — worse — a
    /// drop of something else. The capital letters are the case that matters on
    /// PostgreSQL, where an unquoted `Daily Totals` is not merely unreadable but
    /// a different identifier from the one in the catalog.
    #[test]
    fn a_name_the_server_could_not_read_bare_is_quoted() {
        let awkward = relation("staging", "Daily Totals", RelationKind::Table);
        assert_eq!(
            super::table_change(&dbsql::POSTGRES, &awkward, TableChange::Drop).expect("rendered"),
            "DROP TABLE staging.\"Daily Totals\";"
        );
        assert_eq!(
            super::table_change(&dbsql::MYSQL, &awkward, TableChange::Drop).expect("rendered"),
            "DROP TABLE staging.`Daily Totals`;"
        );
        // And the new name too, which is the half a renderer can forget: the old
        // name arrives from the catalog and the new one was typed by hand.
        assert_eq!(
            super::table_change(
                &dbsql::POSTGRES,
                &relation("staging", "orders", RelationKind::Table),
                TableChange::Rename { to: "Orders 2026" }
            )
            .expect("rendered"),
            "ALTER TABLE staging.orders RENAME TO \"Orders 2026\";"
        );
    }

    /// `changes_relations` and `table_change` say the same thing.
    ///
    /// The two can disagree, and each direction is its own silent failure. A
    /// renderer claiming changes it cannot write gives the navigator three menu
    /// items that open a sheet only to refuse; one that writes them and says
    /// otherwise is never asked, and the items are simply missing with nothing
    /// anywhere to say why.
    ///
    /// Asked with a drop of an ordinary table, which is the change every
    /// renderer that writes any of the three writes — the narrower refusals are
    /// about a particular relation, and `changes_relations` is not.
    #[test]
    fn a_renderer_that_claims_changes_writes_one() {
        let table = relation("s", "t", RelationKind::Table);
        for dialect in dbsql::ALL {
            let Some(renderer) = super::for_dialect(dialect) else {
                continue;
            };
            let written = renderer.table_change(&table, TableChange::Drop).is_ok();
            assert_eq!(
                renderer.changes_relations(),
                written,
                "{} says it {} change a relation and {} write the statement",
                dialect.name,
                if renderer.changes_relations() {
                    "can"
                } else {
                    "cannot"
                },
                if written { "does" } else { "does not" }
            );
            assert_eq!(
                super::changes_relations(dialect),
                written,
                "{} answers differently through the crate's own entry point",
                dialect.name
            );
        }
    }

    /// A database is made and removed in each server's own noun.
    ///
    /// The noun is the whole of the risk here. PostgreSQL's `CREATE SCHEMA`
    /// makes a namespace inside the database this connection is already on, and
    /// MySQL's makes a database — so a renderer that borrowed the other's word
    /// would run, succeed, and make the wrong object.
    #[test]
    fn a_database_is_made_and_removed_in_each_servers_own_noun() {
        let cases: &[(&Dialect, DatabaseChange, &str)] = &[
            (
                &dbsql::POSTGRES,
                DatabaseChange::Create { name: "reporting" },
                "CREATE DATABASE reporting;",
            ),
            (
                &dbsql::POSTGRES,
                DatabaseChange::Drop { name: "reporting" },
                "DROP DATABASE reporting;",
            ),
            // `SCHEMA`, which is the word upstream's MySQL manager writes and
            // which MySQL reads as `DATABASE`.
            (
                &dbsql::MYSQL,
                DatabaseChange::Create { name: "reporting" },
                "CREATE SCHEMA reporting;",
            ),
            (
                &dbsql::MYSQL,
                DatabaseChange::Drop { name: "reporting" },
                "DROP SCHEMA reporting;",
            ),
        ];
        for (dialect, change, want) in cases {
            let written = super::database_change(dialect, *change)
                .unwrap_or_else(|e| panic!("{} refused {change:?}: {e}", dialect.name));
            assert_eq!(written.trim_end(), *want, "{} wrote it wrong", dialect.name);
        }

        // A name the server could not read bare is quoted, as everywhere else
        // in this crate — and a name holding the closing delimiter is the case
        // upstream's hand-written backticks get wrong.
        let awkward = super::database_change(
            &dbsql::MYSQL,
            DatabaseChange::Create {
                name: "wei`rd order",
            },
        )
        .expect("MySQL makes a database");
        assert_eq!(awkward.trim_end(), "CREATE SCHEMA `wei``rd order`;");
    }

    /// `changes_databases` and `database_change` say the same thing.
    ///
    /// The companion to `a_renderer_that_claims_changes_writes_one`, and its own
    /// test rather than a second loop inside that one: the two capabilities are
    /// deliberately independent, and a check that asserted them together would
    /// be the drift it exists to catch.
    #[test]
    fn a_renderer_that_claims_database_changes_writes_one() {
        for dialect in dbsql::ALL {
            let Some(renderer) = super::for_dialect(dialect) else {
                continue;
            };
            let written = renderer
                .database_change(DatabaseChange::Create { name: "d" })
                .is_ok();
            assert_eq!(
                renderer.changes_databases(),
                written,
                "{} says it {} make a database and {} write the statement",
                dialect.name,
                if renderer.changes_databases() {
                    "can"
                } else {
                    "cannot"
                },
                if written { "does" } else { "does not" }
            );
            assert_eq!(
                super::changes_databases(dialect),
                written,
                "{} answers differently through the crate's own entry point",
                dialect.name
            );
        }

        // SQLite is the case that keeps the two capabilities apart: it changes
        // relations and cannot make a database. A build where both came from one
        // flag would have to be wrong about one of them.
        assert!(super::changes_relations(&dbsql::SQLITE));
        assert!(!super::changes_databases(&dbsql::SQLITE));
    }

    /// SQLite says a database is a file rather than promising one later.
    ///
    /// The distinction every refusal in this crate draws, and the one that is
    /// easiest to lose: "not written yet" invites somebody to wait for a release
    /// that will never contain it, because there is no `CREATE DATABASE` in
    /// SQLite to write.
    #[test]
    fn sqlite_says_a_database_is_a_file_rather_than_promising_one_later() {
        let refusal = super::database_change(&dbsql::SQLITE, DatabaseChange::Create { name: "d" })
            .expect_err("SQLite has no CREATE DATABASE");
        let said = refusal.to_string();
        assert!(said.contains("file"), "got {said}");
        assert!(
            !said.contains("yet"),
            "a refusal that will never change: {said}"
        );

        // And the three that are waiting for somebody to write them say so.
        for dialect in [&dbsql::CLICKHOUSE, &dbsql::MSSQL, &dbsql::DUCKDB] {
            let said = super::database_change(dialect, DatabaseChange::Create { name: "d" })
                .expect_err("not written yet")
                .to_string();
            assert!(said.contains("yet"), "{}: {said}", dialect.name);
        }
    }

    /// What each database refuses, and why the refusal says so.
    ///
    /// Every one of these is a statement that would otherwise be written,
    /// accepted by the sheet, sent, and refused by the server — with a message
    /// about syntax rather than about the thing that was actually wrong. The
    /// refusals here are the ones that can say what to do instead.
    #[test]
    fn a_change_the_database_cannot_make_is_refused_by_name() {
        let view = relation("staging", "summary", RelationKind::View);
        let table = relation("main", "orders", RelationKind::Table);

        for dialect in [&dbsql::POSTGRES, &dbsql::MYSQL] {
            let error = super::table_change(dialect, &view, TableChange::Truncate)
                .expect_err("a view was truncated");
            assert!(
                error.to_string().contains("rows of its own"),
                "{}: {error}",
                dialect.name
            );
        }

        let error = super::table_change(&dbsql::SQLITE, &table, TableChange::Truncate)
            .expect_err("SQLite truncated a table");
        assert!(
            error.to_string().contains("DELETE FROM"),
            "the refusal should name what SQLite has instead: {error}"
        );

        let error = super::table_change(&dbsql::SQLITE, &view, TableChange::Rename { to: "s2" })
            .expect_err("SQLite renamed a view");
        assert!(
            error.to_string().contains("cannot rename a view"),
            "{error}"
        );

        // The three whose renderers have not been written. A refusal naming the
        // database, rather than a statement composed from another one's rules.
        for dialect in [&dbsql::CLICKHOUSE, &dbsql::MSSQL, &dbsql::DUCKDB] {
            let error = super::table_change(dialect, &table, TableChange::Drop)
                .expect_err("a renderer that was never written answered");
            assert!(
                error.to_string().contains("has not been written"),
                "{}: {error}",
                dialect.name
            );
        }
    }
}
