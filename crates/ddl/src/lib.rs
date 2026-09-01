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
/// One of two entry points that render something the database has never seen.
/// Everything else here describes what is already there, which is why everything
/// else here reads a `Driver` and this reads a file's columns.
///
/// No column is written `NOT NULL`, whatever the file's schema says, and no
/// column is part of a key. Parquet records which of its columns had no nulls in
/// it, and that is a fact about the file rather than a rule about the table: a
/// column that happens to be full today would otherwise become a table that
/// starts refusing rows part way through the second import into it. The form
/// this shares a renderer with departs from exactly that, a checkbox being a
/// decision somebody took rather than a shape somebody's file happened to have.
pub fn create_table(dialect: &'static Dialect, table: &str, columns: &Schema) -> DbResult<String> {
    let columns = columns
        .fields()
        .iter()
        .map(|field| {
            Ok(NewColumn {
                name: field.name().clone(),
                kind: kind_of(field)?,
                nullable: true,
                default: None,
                primary_key: false,
            })
        })
        .collect::<DbResult<Vec<_>>>()?;
    render(dialect, |renderer| renderer.new_table(table, &columns))
}

/// The statement that would make a table somebody has described column by
/// column, in the SQL `dialect` writes.
///
/// The other of the two, and the difference from [`create_table`] is where the
/// answers came from rather than what is done with them: both end in the same
/// renderer. The name is given in two parts and quoted here, unlike the file
/// path's single pre-spelled string, because this one is typed into a form and
/// a schema called `Sales Data` has to survive reaching the server.
///
/// Rendered and handed back rather than run, like everything else in this crate.
pub fn new_table(
    dialect: &'static Dialect,
    schema: &str,
    name: &str,
    columns: &[NewColumn],
) -> DbResult<String> {
    if name.is_empty() {
        return Err(DbError::new("a table needs a name"));
    }
    // An empty schema is not an empty identifier: SQLite and DuckDB have a
    // container the front end may have nothing to call, and `.orders` is a
    // syntax error where `orders` is the table the connection would have found
    // anyway.
    let table = match schema.is_empty() {
        true => dialect.quote(name),
        false => format!("{}.{}", dialect.quote(schema), dialect.quote(name)),
    };
    render(dialect, |renderer| renderer.new_table(&table, columns))
}

/// `f` applied to the renderer for `dialect`, or the refusal that names it.
fn render(
    dialect: &'static Dialect,
    f: impl FnOnce(&'static dyn Renderer) -> DbResult<String>,
) -> DbResult<String> {
    match for_dialect(dialect) {
        Some(renderer) => f(renderer),
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

/// What a column of a new table can be asked to be.
///
/// Arrow has some fifty types; this is what a file being imported can actually
/// mean by them, reduced once here rather than six times over in the renderers.
/// The reduction widens deliberately — every whole number becomes the widest
/// whole number the database has — because a column made too wide takes the
/// whole file and a column made too narrow takes most of it, which is the worse
/// of the two by far.
///
/// The same seven are what the Create Table form offers, and that is the reason
/// this is public. A form could instead take the type as text and pass it
/// through, which is what most tools do, and it would put the front end in the
/// business of spelling SQL for a database it does not know — `nvarchar(max)` on
/// one server, `TEXT` on the next. Choosing from a closed set costs a ceiling
/// and buys a statement that is correct wherever it is sent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ColumnKind {
    Bool,
    Int,
    Float,
    /// Precision and scale as the file states them, or as the form was set to.
    /// The one kind that carries a size, because a decimal held at a different
    /// scale is a different number and no server will mention it.
    Decimal(u8, i8),
    Text,
    Date,
    Timestamp,
}

impl ColumnKind {
    /// The spelling that crosses the FFI, which is the variant in lower case.
    ///
    /// Deliberately not a database's word for the type: this names the kind and
    /// each renderer names the column, so a build that starts writing `bigint`
    /// here would be spelling PostgreSQL for everybody. Paired with
    /// [`ColumnKind::parse`], and the two are checked against each other.
    pub fn word(self) -> String {
        match self {
            ColumnKind::Bool => "bool".to_string(),
            ColumnKind::Int => "int".to_string(),
            ColumnKind::Float => "float".to_string(),
            ColumnKind::Decimal(precision, scale) => format!("decimal({precision},{scale})"),
            ColumnKind::Text => "text".to_string(),
            ColumnKind::Date => "date".to_string(),
            ColumnKind::Timestamp => "timestamp".to_string(),
        }
    }

    /// The kind `word` names, or a refusal quoting what arrived.
    ///
    /// A word this does not know is refused rather than defaulted to text: a
    /// column silently made `text` because the front end sent a spelling this
    /// build stopped reading is a table that takes every row and sorts none of
    /// them the way anybody meant.
    pub fn parse(word: &str) -> DbResult<Self> {
        Ok(match word {
            "bool" => ColumnKind::Bool,
            "int" => ColumnKind::Int,
            "float" => ColumnKind::Float,
            "text" => ColumnKind::Text,
            "date" => ColumnKind::Date,
            "timestamp" => ColumnKind::Timestamp,
            other => {
                let size = other
                    .strip_prefix("decimal(")
                    .and_then(|rest| rest.strip_suffix(')'))
                    .and_then(|rest| rest.split_once(','));
                match size {
                    Some((precision, scale)) => ColumnKind::Decimal(
                        precision.trim().parse().map_err(|_| bad_kind(other))?,
                        scale.trim().parse().map_err(|_| bad_kind(other))?,
                    ),
                    None => return Err(bad_kind(other)),
                }
            }
        })
    }
}

fn bad_kind(word: &str) -> DbError {
    DbError::new(format!("{word:?} is not a kind of column"))
}

/// One column of a table that does not exist yet.
///
/// What a hand-written form produces, and what a file's schema is reduced to
/// before it reaches a renderer — one shape rather than two, so that the
/// bracket-and-comma layout is written once. The file path fills the last three
/// fields the same way every time, which is exactly the difference between the
/// two callers: an import infers, and a form is told.
#[derive(Clone, Debug, PartialEq)]
pub struct NewColumn {
    pub name: String,
    pub kind: ColumnKind,
    pub nullable: bool,
    /// Written after `DEFAULT` exactly as given.
    ///
    /// Not quoted and not checked. A default is an expression — `0`, `now()`,
    /// `'unknown'` — and telling one of those from a literal that needs quotes
    /// means parsing the server's own grammar, which this build does not do. So
    /// what was typed is what is sent, and the statement is shown before it goes,
    /// which is where a caller reads what they wrote.
    pub default: Option<String>,
    pub primary_key: bool,
}

/// How a database spells a column that refuses NULL.
///
/// Five of the six put a modifier after the type and ClickHouse puts the answer
/// inside it: a plain `Int64` there already refuses NULL, and a column that
/// accepts one is `Nullable(Int64)`. The difference is small enough to look like
/// a detail and large enough to invert the meaning of every column in the
/// statement, so it is stated per renderer rather than assumed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum NullStyle {
    /// `NOT NULL` after the type, and nothing at all for a column that allows it.
    Suffix,
    /// The type wrapped in `Nullable(…)` when the column allows NULL.
    Wrapped,
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
/// `word`, `nulls` and `suffix` are the whole of what differs between databases
/// — the type names, where nullability is spelled, and whatever one of them
/// insists on after the closing bracket, ClickHouse having no table without an
/// engine. Six copies of the bracket-and-comma layout would be six places for a
/// trailing comma to be introduced in one of them.
///
/// The clause order inside a column is type, `DEFAULT`, then nullability, which
/// is `PostgreTableColumnManager.getSupportedModifiers` and is also what
/// [`postgres::column`] writes for a table that already exists. It reads
/// backwards — `qty bigint DEFAULT 1 NOT NULL` — and it is upstream's order, and
/// every server here takes column constraints in any order.
///
/// The primary key is a table constraint even when it is one column, rather than
/// `PRIMARY KEY` after that column's type. One shape for both cases: a key over
/// two columns can only be written this way, and having the single-column case
/// take the other branch would leave a form's commonest output on the path the
/// other tests never reach.
pub(crate) fn new_table_text(
    dialect: &'static Dialect,
    table: &str,
    columns: &[NewColumn],
    word: impl Fn(ColumnKind) -> String,
    nulls: NullStyle,
    suffix: &str,
) -> DbResult<String> {
    if columns.is_empty() {
        return Err(DbError::new("a table needs at least one column"));
    }
    let mut body = Vec::new();
    for (index, column) in columns.iter().enumerate() {
        // Exact match rather than case-insensitive, because the servers disagree
        // about that and this crate has no business deciding it: `Qty` and `qty`
        // are two columns on PostgreSQL and one on MySQL. What is caught here is
        // the pair nobody could have meant, and the rest is the server's to
        // refuse — with the statement already on screen to compare against.
        if columns[..index]
            .iter()
            .any(|other| other.name == column.name)
        {
            return Err(DbError::new(format!(
                "two columns are both called {}",
                column.name
            )));
        }
        body.push(format!(
            "    {}",
            column_declaration(dialect, column, &word, nulls)?
        ));
    }

    let key: Vec<String> = columns
        .iter()
        .filter(|column| column.primary_key)
        .map(|column| dialect.quote(&column.name))
        .collect();
    if !key.is_empty() {
        body.push(format!("    PRIMARY KEY ({})", key.join(", ")));
    }

    let mut script = Script::new();
    script.statement(&format!(
        "CREATE TABLE {table} (\n{}\n){suffix}",
        body.join(",\n")
    ));
    Ok(script.finish())
}

/// One column as a statement declares it: name, type, default, nullability.
///
/// Shared by the two statements that declare a column — the `CREATE TABLE` that
/// makes a table and the `ALTER TABLE … ADD COLUMN` that puts one into a table
/// already there — because the servers spell those two the same way. Splitting
/// them would be two places for `DEFAULT` and `NOT NULL` to end up in a
/// different order.
///
/// The clause order is type, `DEFAULT`, then nullability, which is
/// `PostgreTableColumnManager.getSupportedModifiers` and is also what
/// [`postgres::column`] writes for a column that already exists. It reads
/// backwards — `qty bigint DEFAULT 1 NOT NULL` — and it is upstream's order, and
/// every server here takes column constraints in any order.
pub(crate) fn column_declaration(
    dialect: &'static Dialect,
    column: &NewColumn,
    word: impl Fn(ColumnKind) -> String,
    nulls: NullStyle,
) -> DbResult<String> {
    if column.name.is_empty() {
        return Err(DbError::new("a column needs a name"));
    }
    if column.primary_key && column.nullable {
        return Err(DbError::new(format!(
            "{} is part of the primary key, which cannot hold a null",
            column.name
        )));
    }
    let kind = word(column.kind);
    let mut declaration = format!(
        "{} {}",
        dialect.quote(&column.name),
        match (nulls, column.nullable) {
            (NullStyle::Wrapped, true) => format!("Nullable({kind})"),
            _ => kind,
        }
    );
    if let Some(default) = &column.default {
        declaration.push_str(&format!(" DEFAULT {default}"));
    }
    if nulls == NullStyle::Suffix && !column.nullable {
        declaration.push_str(" NOT NULL");
    }
    Ok(declaration)
}

/// What to do to a column of a table that already exists.
///
/// Three verbs and not six. Changing a column's *type*, its nullability or its
/// default is the same family of statement on paper and a different one on every
/// server — PostgreSQL writes one `ALTER COLUMN` per property changed, MySQL
/// writes a single `MODIFY COLUMN` carrying the whole declaration back, SQL
/// Server splits it again — and those three are left out of this enum
/// deliberately rather than folded in. What is here is what the servers agree
/// about: a column is added, dropped, or given another name.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ColumnChange<'a> {
    /// Put a new column into the table.
    ///
    /// The same [`NewColumn`] the Create Table form fills in, because it is the
    /// same five answers. `primary_key` is refused: a key is a statement about
    /// the table rather than about one column, and a table that already has rows
    /// cannot take a new key column at all.
    Add(&'a NewColumn),
    /// Remove the column and everything in it.
    Drop { name: &'a str },
    /// Give it another name, leaving everything else about it alone.
    Rename { name: &'a str, to: &'a str },
}

/// `ALTER <noun> <table> …` for one column change, which is the shape all three
/// verbs share.
///
/// `noun` is the relation's own word — `TABLE` on most of these and
/// `FOREIGN TABLE` on one of PostgreSQL's — because every one of these
/// statements opens with it. Which relations have a noun at all is the
/// renderer's to decide before calling this: a view's columns come from its
/// query and there is no statement that alters one.
///
/// `ADD COLUMN` and not the bare `ADD` upstream's PostgreSQL manager writes.
/// The keyword is optional in every grammar here and upstream varies it on a
/// per-driver flag whose only effect is whether it appears; written the same way
/// everywhere so that a reader comparing two of these files finds one word.
pub(crate) fn column_change_text(
    dialect: &'static Dialect,
    noun: &str,
    table: &str,
    change: ColumnChange<'_>,
    word: impl Fn(ColumnKind) -> String,
    nulls: NullStyle,
) -> DbResult<String> {
    let clause = match change {
        ColumnChange::Add(column) => {
            if column.primary_key {
                return Err(DbError::new(format!(
                    "{} cannot be added as part of the primary key: a key is a rule about the \
                     whole table, and a table with rows in it has no room for another",
                    column.name
                )));
            }
            format!(
                "ADD COLUMN {}",
                column_declaration(dialect, column, word, nulls)?
            )
        }
        ColumnChange::Drop { name } => {
            if name.is_empty() {
                return Err(DbError::new("a column needs a name"));
            }
            format!("DROP COLUMN {}", dialect.quote(name))
        }
        ColumnChange::Rename { name, to } => {
            if name.is_empty() || to.is_empty() {
                return Err(DbError::new("a rename needs a name at both ends"));
            }
            format!(
                "RENAME COLUMN {} TO {}",
                dialect.quote(name),
                dialect.quote(to)
            )
        }
    };
    let mut script = Script::new();
    script.statement(&format!("ALTER {noun} {table} {clause}"));
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

    /// The `CREATE TABLE` for a set of columns, however they were arrived at.
    ///
    /// One method for both callers. A file's schema is reduced to the same
    /// columns before it gets here, so the difference between an import and a
    /// hand-filled form is which answers the [`NewColumn`]s carry and not which
    /// code writes them — and a nullability rule that held on one path and not
    /// the other would be a second `CREATE TABLE` per database to keep in step.
    ///
    /// No default. A default would have to pick some database's type words, and
    /// a statement spelled for the wrong database is one that looks right and
    /// does not run — the same reason `for_dialect` refuses rather than falling
    /// back. Each renderer answers with the words its own server reads.
    fn new_table(&self, table: &str, columns: &[NewColumn]) -> DbResult<String>;

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

    /// The statement for one change to a column of a relation that is there.
    ///
    /// No default, for the reason `table_change` has none. A refusal here is per
    /// relation as well as per database: no server alters a view's columns,
    /// those coming from the query the view is, and the front end shows the
    /// refusal where it would have shown the statement.
    fn column_change(&self, relation: &RelationInfo, change: ColumnChange<'_>) -> DbResult<String>;

    /// Whether this renderer writes any [`ColumnChange`] at all.
    ///
    /// Asked separately from `column_change` for the reason `changes_relations`
    /// is asked separately from `table_change`: the Structure tab draws its
    /// column controls before anybody has chosen a column for them to act on.
    ///
    /// Its own flag rather than a second reading of `changes_relations`, though
    /// the two answer alike today. They are not one question: upstream itself
    /// writes SQLite's `DROP TABLE` and refuses its column drop outright,
    /// recreating the table instead — so a build that read one flag for both
    /// would be asserting something upstream does not.
    fn changes_columns(&self) -> bool;

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

/// The statement that would make `change` to a column of `relation`, in the SQL
/// `dialect` writes.
///
/// Rendered and handed back rather than run, like [`table_change`]. A drop is
/// irreversible and a rename breaks everything that names the column, so both
/// are worth reading before they go.
pub fn column_change(
    dialect: &'static Dialect,
    relation: &RelationInfo,
    change: ColumnChange<'_>,
) -> DbResult<String> {
    render(dialect, |renderer| renderer.column_change(relation, change))
}

/// Whether this build writes any change to a column on `dialect`.
///
/// What the Structure tab reads to decide whether its column controls exist.
/// Which change a *particular* column can take is the narrower question, and
/// [`column_change`] answers that one where the statement would have been.
pub fn changes_columns(dialect: &'static Dialect) -> bool {
    for_dialect(dialect).is_some_and(|renderer| renderer.changes_columns())
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
    use super::{ColumnChange, ColumnKind, DatabaseChange, NewColumn, TableChange};
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

    /// A table somebody filled a form in for, written out in full.
    ///
    /// The counterpart to the test above and not a variation on it: every field
    /// a file cannot state is set here, so what these strings pin is the half of
    /// `new_table_text` the import path never reaches — `NOT NULL`, `DEFAULT`,
    /// and a key over two columns. PostgreSQL and MySQL are what M5a lights, and
    /// they differ in the delimiter and in every type word.
    #[test]
    fn a_form_becomes_a_table_in_each_databases_own_words() {
        let columns = a_filled_in_form();
        let expected = [
            (
                &dbsql::POSTGRES,
                "CREATE TABLE staging.orders (
    id bigint NOT NULL,
    \"Order Date\" date NOT NULL,
    amount numeric(12, 2) DEFAULT 0 NOT NULL,
    note text,
    seen_at timestamp DEFAULT now(),
    PRIMARY KEY (id, \"Order Date\")
);",
            ),
            (
                &dbsql::MYSQL,
                "CREATE TABLE staging.orders (
    id BIGINT NOT NULL,
    `Order Date` DATE NOT NULL,
    amount DECIMAL(12, 2) DEFAULT 0 NOT NULL,
    note TEXT,
    seen_at DATETIME DEFAULT now(),
    PRIMARY KEY (id, `Order Date`)
);",
            ),
        ];
        for (dialect, statement) in expected {
            assert_eq!(
                super::new_table(dialect, "staging", "orders", &columns).expect(dialect.name),
                statement,
                "{}",
                dialect.name
            );
        }
    }

    /// ClickHouse puts the same answer inside the type instead of after it.
    ///
    /// The reason [`super::NullStyle`] exists, and the case that would otherwise
    /// be silently inverted: a build that dropped the wrapping would make every
    /// column refuse the nulls the form said it accepts, and ClickHouse would
    /// take that statement without complaint.
    #[test]
    fn clickhouse_says_nullable_inside_the_type() {
        let statement = super::new_table(
            &dbsql::CLICKHOUSE,
            "staging",
            "orders",
            &[
                column("id", ColumnKind::Int, false),
                column("note", ColumnKind::Text, true),
            ],
        )
        .expect("ClickHouse makes a table");
        assert_eq!(
            statement,
            "CREATE TABLE staging.orders (
    \"id\" Int64,
    note Nullable(String)
)
ENGINE = MergeTree
ORDER BY tuple();"
        );
    }

    /// ClickHouse says why it will not take the key, rather than writing one the
    /// server rejects.
    ///
    /// A `PRIMARY KEY (id)` under `ORDER BY tuple()` is refused by ClickHouse
    /// itself — the key has to be a prefix of the sort order — so the choice is
    /// between refusing here with a sentence about ordering and sending a
    /// statement that can only fail. The other five write the key, which is what
    /// the second half asserts: this is one database's answer and not a hole in
    /// the feature.
    #[test]
    fn clickhouse_refuses_a_key_it_would_have_to_choose_an_order_for() {
        let keyed = [column_with("id", ColumnKind::Int, false, None, true)];
        let error = super::new_table(&dbsql::CLICKHOUSE, "staging", "orders", &keyed)
            .expect_err("ClickHouse wrote a key it cannot order by");
        assert!(error.to_string().contains("order"), "{error}");

        for dialect in [
            &dbsql::POSTGRES,
            &dbsql::MYSQL,
            &dbsql::SQLITE,
            &dbsql::MSSQL,
            &dbsql::DUCKDB,
        ] {
            let statement = super::new_table(dialect, "staging", "orders", &keyed)
                .unwrap_or_else(|e| panic!("{} refused a primary key: {e}", dialect.name));
            assert!(
                statement.contains("PRIMARY KEY"),
                "{} wrote no key: {statement}",
                dialect.name
            );
        }
    }

    /// A container the front end has no name for does not become an empty one.
    ///
    /// SQLite and DuckDB reach this with nothing to call the schema, and
    /// `CREATE TABLE .orders` is a syntax error where `CREATE TABLE orders` is
    /// the table the connection would have found anyway.
    #[test]
    fn a_table_with_no_schema_is_not_qualified_by_a_bare_dot() {
        let statement = super::new_table(
            &dbsql::SQLITE,
            "",
            "orders",
            &[column("id", ColumnKind::Int, true)],
        )
        .expect("SQLite makes a table");
        assert!(
            statement.starts_with("CREATE TABLE orders ("),
            "{statement}"
        );
    }

    /// The four answers a form can give that no statement should be written for.
    ///
    /// Each is a mistake the server would also catch, and catching them here is
    /// what puts the sentence next to the field rather than in a failed query —
    /// except the third, which no server catches at all: PostgreSQL and MySQL
    /// both make a primary key column `NOT NULL` on their own, so a form that
    /// said "nullable" and got a column that refuses nulls would have been
    /// quietly overruled.
    #[test]
    fn a_form_that_contradicts_itself_is_refused_rather_than_sent() {
        let cases: &[(&str, Vec<NewColumn>)] = &[
            ("at least one column", vec![]),
            ("needs a name", vec![column("", ColumnKind::Int, true)]),
            (
                "cannot hold a null",
                vec![column_with("id", ColumnKind::Int, true, None, true)],
            ),
            (
                "both called qty",
                vec![
                    column("qty", ColumnKind::Int, true),
                    column("qty", ColumnKind::Text, true),
                ],
            ),
        ];
        for (expected, columns) in cases {
            let error = super::new_table(&dbsql::POSTGRES, "staging", "orders", columns)
                .expect_err("a statement was written for {expected}");
            assert!(
                error.to_string().contains(expected),
                "wanted {expected:?}, got {error}"
            );
        }

        // And the name of the table itself, which is the one the front end asks
        // about before there are any columns to ask about.
        let error = super::new_table(&dbsql::POSTGRES, "staging", "", &[])
            .expect_err("a table with no name was rendered");
        assert!(
            error.to_string().contains("a table needs a name"),
            "{error}"
        );
    }

    /// The word for a kind survives the trip to the front end and back.
    ///
    /// The seam that has no compiler on it: the front end sends a string and
    /// this reads one, so a spelling changed on either side is a column silently
    /// refused — or, for the decimal, a size silently lost. Every variant is
    /// round-tripped, including one whose scale is negative, which is legal in
    /// Arrow and is what a file of round thousands infers.
    #[test]
    fn every_kind_is_spelled_the_same_in_both_directions() {
        for kind in [
            ColumnKind::Bool,
            ColumnKind::Int,
            ColumnKind::Float,
            ColumnKind::Text,
            ColumnKind::Date,
            ColumnKind::Timestamp,
            ColumnKind::Decimal(12, 2),
            ColumnKind::Decimal(38, -3),
        ] {
            let word = kind.word();
            assert_eq!(
                ColumnKind::parse(&word).unwrap_or_else(|e| panic!("{word}: {e}")),
                kind,
                "{word} did not come back as the kind that wrote it"
            );
        }

        // A word this build does not know is refused rather than defaulted to
        // text, which is the failure that would look like a working feature.
        for word in ["", "varchar(64)", "decimal", "decimal(x,2)", "Int"] {
            let error = ColumnKind::parse(word)
                .expect_err("a kind was invented for a word this build does not write");
            assert!(error.to_string().contains(word), "{error}");
        }
    }

    /// Five columns with every field a file cannot state.
    fn a_filled_in_form() -> Vec<NewColumn> {
        vec![
            column_with("id", ColumnKind::Int, false, None, true),
            column_with("Order Date", ColumnKind::Date, false, None, true),
            column_with(
                "amount",
                ColumnKind::Decimal(12, 2),
                false,
                Some("0"),
                false,
            ),
            column("note", ColumnKind::Text, true),
            column_with("seen_at", ColumnKind::Timestamp, true, Some("now()"), false),
        ]
    }

    fn column(name: &str, kind: ColumnKind, nullable: bool) -> NewColumn {
        column_with(name, kind, nullable, None, false)
    }

    fn column_with(
        name: &str,
        kind: ColumnKind,
        nullable: bool,
        default: Option<&str>,
        primary_key: bool,
    ) -> NewColumn {
        NewColumn {
            name: name.to_string(),
            kind,
            nullable,
            default: default.map(str::to_string),
            primary_key,
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

    /// Each column change written out in full, for each database that writes it.
    ///
    /// Strings and not a rule, for the reason the two tests above are written
    /// that way. What differs between these three is smaller than it looks and
    /// more dangerous for being small: one noun, one optional keyword, and — on
    /// MySQL — a whole clause upstream writes that this deliberately does not.
    #[test]
    fn a_column_change_is_spelled_the_way_the_server_being_changed_reads_it() {
        let orders = relation("staging", "orders", RelationKind::Table);
        let note = column("note", ColumnKind::Text, true);
        let cases: &[(&Dialect, ColumnChange, &str)] = &[
            (
                &dbsql::POSTGRES,
                ColumnChange::Add(&note),
                "ALTER TABLE staging.orders ADD COLUMN note text;",
            ),
            (
                &dbsql::POSTGRES,
                ColumnChange::Drop { name: "note" },
                "ALTER TABLE staging.orders DROP COLUMN note;",
            ),
            (
                &dbsql::POSTGRES,
                ColumnChange::Rename {
                    name: "note",
                    to: "Comment",
                },
                "ALTER TABLE staging.orders RENAME COLUMN note TO \"Comment\";",
            ),
            (
                &dbsql::MYSQL,
                ColumnChange::Add(&note),
                "ALTER TABLE staging.orders ADD COLUMN note TEXT;",
            ),
            (
                &dbsql::MYSQL,
                ColumnChange::Drop { name: "note" },
                "ALTER TABLE staging.orders DROP COLUMN note;",
            ),
            // `RENAME COLUMN` and not upstream's `CHANGE note note TEXT`, which
            // restates a declaration this build cannot restate in full.
            (
                &dbsql::MYSQL,
                ColumnChange::Rename {
                    name: "note",
                    to: "Comment",
                },
                "ALTER TABLE staging.orders RENAME COLUMN note TO `Comment`;",
            ),
            (
                &dbsql::SQLITE,
                ColumnChange::Add(&note),
                "ALTER TABLE staging.orders ADD COLUMN note TEXT;",
            ),
            // Written where upstream throws and recreates the table instead.
            (
                &dbsql::SQLITE,
                ColumnChange::Drop { name: "note" },
                "ALTER TABLE staging.orders DROP COLUMN note;",
            ),
            (
                &dbsql::SQLITE,
                ColumnChange::Rename {
                    name: "note",
                    to: "Comment",
                },
                "ALTER TABLE staging.orders RENAME COLUMN note TO \"Comment\";",
            ),
        ];
        for (dialect, change, expected) in cases {
            let statement = super::column_change(dialect, &orders, *change)
                .unwrap_or_else(|e| panic!("{} refused {change:?}: {e}", dialect.name));
            assert_eq!(
                statement, *expected,
                "{} wrote the wrong statement for {change:?}",
                dialect.name
            );
        }

        // A column added with everything a form can say about it, which is the
        // one arm that shares its text with `CREATE TABLE`.
        let stamped = super::column_change(
            &dbsql::POSTGRES,
            &orders,
            ColumnChange::Add(&column_with(
                "seen_at",
                ColumnKind::Timestamp,
                false,
                Some("now()"),
                false,
            )),
        )
        .expect("PostgreSQL adds a column");
        assert_eq!(
            stamped,
            "ALTER TABLE staging.orders ADD COLUMN seen_at timestamp DEFAULT now() NOT NULL;"
        );

        // PostgreSQL's own noun, which is the half of this statement that is not
        // shared: a foreign table is altered as a foreign table.
        let foreign = super::column_change(
            &dbsql::POSTGRES,
            &relation("staging", "remote", RelationKind::ForeignTable),
            ColumnChange::Drop { name: "note" },
        )
        .expect("PostgreSQL drops a foreign table's column");
        assert_eq!(
            foreign,
            "ALTER FOREIGN TABLE staging.remote DROP COLUMN note;"
        );
    }

    /// A view's columns are its query's, and no server alters one.
    ///
    /// Refused per relation rather than per database, which is the distinction
    /// `table_change` already draws: the front end draws its controls from
    /// `changes_columns` and reads this refusal where the statement would have
    /// been. All three that write these statements have to agree, because a
    /// build where one of them wrote `ALTER VIEW … DROP COLUMN` would be one
    /// that composed a statement no server has.
    #[test]
    fn no_database_alters_the_columns_of_a_view() {
        let view = relation("staging", "summary", RelationKind::View);
        for dialect in [&dbsql::POSTGRES, &dbsql::MYSQL, &dbsql::SQLITE] {
            let error = super::column_change(dialect, &view, ColumnChange::Drop { name: "note" })
                .unwrap_err();
            assert!(
                !error.to_string().contains("yet"),
                "{}: a view is not a later release, it is a view: {error}",
                dialect.name
            );
        }
    }

    /// The three answers no column change should be written for.
    #[test]
    fn a_column_change_that_contradicts_itself_is_refused_rather_than_sent() {
        let orders = relation("staging", "orders", RelationKind::Table);
        let keyed = column_with("id", ColumnKind::Int, false, None, true);
        let unnamed = column("", ColumnKind::Text, true);
        let cases: &[(&str, ColumnChange)] = &[
            // A key is a rule about the whole table, and a table with rows in it
            // has no room for another — so the checkbox the Create Table form
            // offers is not offered here, and the core says so if it arrives.
            ("primary key", ColumnChange::Add(&keyed)),
            ("a column needs a name", ColumnChange::Add(&unnamed)),
            ("a column needs a name", ColumnChange::Drop { name: "" }),
            (
                "both ends",
                ColumnChange::Rename {
                    name: "note",
                    to: "",
                },
            ),
            (
                "both ends",
                ColumnChange::Rename {
                    name: "",
                    to: "note",
                },
            ),
        ];
        for (expected, change) in cases {
            let error = super::column_change(&dbsql::POSTGRES, &orders, *change)
                .expect_err("a statement was written for {change:?}");
            assert!(
                error.to_string().contains(expected),
                "{change:?}: wanted {expected:?}, got {error}"
            );
        }
    }

    /// `changes_columns` and `column_change` say the same thing.
    ///
    /// The third of these, and its own test rather than a branch inside the
    /// others: the three capabilities are deliberately independent, and a check
    /// that asserted them together would be the drift it exists to catch.
    ///
    /// Asked with a drop from an ordinary table, which is the change every
    /// renderer that writes any of the three writes — the narrower refusals are
    /// about a particular relation, and `changes_columns` is not.
    #[test]
    fn a_renderer_that_claims_column_changes_writes_one() {
        let table = relation("s", "t", RelationKind::Table);
        for dialect in dbsql::ALL {
            let Some(renderer) = super::for_dialect(dialect) else {
                continue;
            };
            let written = renderer
                .column_change(&table, ColumnChange::Drop { name: "c" })
                .is_ok();
            assert_eq!(
                renderer.changes_columns(),
                written,
                "{} says it {} change a column and {} write the statement",
                dialect.name,
                if renderer.changes_columns() {
                    "can"
                } else {
                    "cannot"
                },
                if written { "does" } else { "does not" }
            );
            assert_eq!(
                super::changes_columns(dialect),
                written,
                "{} answers differently through the crate's own entry point",
                dialect.name
            );
        }

        // The three that are not lit say so by name and say "yet", which is the
        // distinction every refusal in this crate draws: these servers all have
        // the statements, and nobody has read the Java for them.
        for dialect in [&dbsql::CLICKHOUSE, &dbsql::MSSQL, &dbsql::DUCKDB] {
            let error = super::column_change(dialect, &table, ColumnChange::Drop { name: "c" })
                .expect_err("a statement was written for an unlit database");
            assert!(error.to_string().contains("yet"), "{error}");
        }
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
