//! What a connection can say about the database behind it.
//!
//! One set of structs for every driver, derived from two that were written
//! independently — PostgreSQL's, which came first and set the shape, and
//! SQLite's, which did not fit it. Where they disagreed the wider shape won, and
//! the reason is recorded on the field rather than in a commit message, because
//! the next driver will reach the same fork.
//!
//! These cross the FFI as JSON. Metadata is a few thousand short rows at most,
//! so the encoding costs nothing worth measuring and the front end stays a
//! `JSONDecoder` call instead of a column reader.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct SchemaInfo {
    pub name: String,
}

/// One database on the server this connection reached.
///
/// A level above `SchemaInfo`, and only some databases have one. Most of the
/// drivers here have nothing to put in it, for one of two reasons that are worth
/// telling apart: either the engine has a single level and `schemas()` already
/// is it — MySQL's schema is its database, Cassandra's is a keyspace, BigQuery's
/// is a dataset — or the engine has two levels and the driver already reports
/// both, flattened into a qualified `SchemaInfo` name the way DuckDB reports
/// `warehouse.main`. Neither wants a second level drawn above it.
///
/// What is left is the case this exists for: PostgreSQL and SQL Server report
/// bare schema names and have a database above them that a connection cannot
/// reach. Listing them is the only way a front end can offer to open one without
/// asking somebody to edit a connection string by hand.
#[derive(Debug, Clone, Serialize)]
pub struct DatabaseInfo {
    pub name: String,

    /// Whether this is the one the connection is already on.
    ///
    /// Answered by the server rather than by comparing against the connection
    /// string, which is not the same question: a string may name no database at
    /// all and still land on one, and the name it does carry may differ from the
    /// server's own spelling of it.
    pub is_current: bool,
}

/// Whether a routine returns a value to an expression or is called as a
/// statement.
///
/// Two, and not the database's own vocabulary. PostgreSQL 11 split `pg_proc`
/// into `prokind` f/p/a/w, MySQL has FUNCTION and PROCEDURE, SQL Server has
/// FN/IF/TF/P. What a reader of a navigator needs from the distinction is
/// whether the thing can appear in a `SELECT` list or has to be `CALL`ed, and
/// that is a two-way split in all of them.
///
/// Aggregates and window functions are `Function`. They are called in an
/// expression, which is the question this answers; that they accumulate is a
/// fact about the body, and the body is what the pane shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutineKind {
    Function,
    Procedure,
}

/// One function or procedure, as much as can be said about it without reading
/// its body.
///
/// The body is deliberately not here. A schema of two hundred PL/pgSQL
/// functions is a few megabytes of source, and expanding a schema in the tree
/// would pay for all of it to draw a list of names — so the list is this and
/// the body comes from [`Driver::routine_definition`] when one is selected.
/// That is the same split `relations` and `definition` already have, for the
/// same reason.
#[derive(Debug, Clone, Serialize)]
pub struct RoutineInfo {
    pub schema: String,
    pub name: String,
    pub kind: RoutineKind,

    /// What the driver has to be given to find this routine again, spelled
    /// however that driver likes.
    ///
    /// Opaque, and the first opaque handle in this file — everything else here
    /// is addressed by the names the database itself uses. Overloading is why:
    /// PostgreSQL allows `f(int)` and `f(text)` in one schema, so a name is not
    /// an address there, and `pg_get_functiondef` takes an oid. A driver whose
    /// routines cannot be overloaded puts the name here and nothing is lost.
    ///
    /// Valid only on the connection that produced it, and only until the
    /// catalog changes under it. A caller holding one across a `DROP` gets a
    /// failure from `routine_definition`, which is the honest answer — the
    /// alternative is a body belonging to whatever took the oid next.
    pub id: String,

    /// The argument list as the database renders it: `integer, text`,
    /// `IN a int, OUT b text`. Empty for a routine that takes none.
    ///
    /// The database's own rendering rather than a list of parsed arguments,
    /// for the reason `ConstraintInfo::definition` gives: modes, defaults,
    /// `VARIADIC` and type names spelled per dialect are formatting this would
    /// have to reimplement and would get subtly wrong on the cases that
    /// matter. A pane that needs to show which overload this is needs the text,
    /// not the parts.
    pub arguments: String,

    /// What it returns, as the database renders it. `None` for a procedure
    /// that returns nothing, which is not the same as a function returning
    /// `void` — one has no return clause at all and the other declares one.
    pub returns: Option<String>,

    /// `plpgsql`, `sql`, `c`, `python`. `None` where the engine has one
    /// language and does not name it.
    pub language: Option<String>,
}

/// One sequence in a schema.
///
/// Everything a sequence is, in one struct, and no second call for a body: a
/// sequence has no source. That is the whole of why it is not shaped like
/// [`RoutineInfo`] — there is nothing here that is expensive enough to defer,
/// so the navigator's one read answers every question the Structure pane asks.
///
/// The numbers are strings. A sequence on PostgreSQL is `bigint` and on another
/// engine may be wider, `last_value` is unsigned on some and signed on others,
/// and a client that parsed them into one Rust integer would be choosing which
/// databases it can describe. Nothing here does arithmetic on them; they are
/// read.
#[derive(Debug, Clone, Serialize)]
pub struct SequenceInfo {
    pub schema: String,
    pub name: String,

    /// The last value written, or `None` where the server would not say.
    ///
    /// Two causes and the driver cannot tell them apart, so nothing above it
    /// may claim to either: nothing has been taken from the sequence yet, or
    /// this login may see that it exists without being allowed to read it —
    /// `USAGE` on the schema and `SELECT` on the sequence are separate grants.
    /// `None` rather than zero, because zero is a value a sequence can hold.
    pub last_value: Option<String>,

    /// The step, which may be negative: a descending sequence is an ordinary
    /// thing and the pane has to be able to say so.
    pub increment: String,

    pub min_value: String,
    pub max_value: String,

    /// Whether it wraps at the end instead of failing.
    pub cycles: bool,

    /// How many values are handed out per trip to the catalog. Worth showing
    /// because it explains the gaps somebody is looking at: a cache of 50 means
    /// the numbers in a table jump by 50 whenever a session ends.
    pub cache: Option<String>,
}

/// What kind of relation a navigator entry is.
///
/// A closed set rather than the database's own word for it. A free string would
/// let `BASE TABLE`, `table` and `TABLE` all reach the front end for the same
/// thing, and the sidebar would need to know each database's spelling to choose
/// an icon. The cost is that a driver with a kind not listed here has to add it,
/// which is the point: it forces a decision instead of inventing a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RelationKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
    PartitionedTable,
    /// A relation whose rows come from an extension rather than from storage:
    /// SQLite's virtual tables, and whatever else arrives with a module behind
    /// it.
    Virtual,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationInfo {
    pub schema: String,
    pub name: String,
    pub kind: RelationKind,
    /// The planner's estimate, and `None` where nothing has measured it.
    ///
    /// Optional because of SQLite, which has no estimate at all until `ANALYZE`
    /// has run — and because PostgreSQL has the same hole and was hiding it.
    /// `reltuples` is -1 for a relation that has never been analyzed, and the
    /// first version of this clamped that to 0, so a sidebar reported a full
    /// table as empty. Declining to answer is not the same as answering zero,
    /// and only one of them is true.
    pub estimated_rows: Option<i64>,
}

/// How a computed column's value is kept.
///
/// Named for the fact rather than for one database's keyword: SQL Server writes
/// `PERSISTED`, while MySQL, SQLite and PostgreSQL write `STORED` for the same
/// arrangement, and the column that is not stored is the one every one of them
/// evaluates on read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Computed {
    /// Evaluated on every read; the table stores nothing for it.
    Virtual,
    /// Evaluated on write and stored with the row, so an index can be built on
    /// it and a constraint can be declared over it.
    Stored,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    /// The type as the database states it: `numeric(18,4)`, `character
    /// varying(64)`, `INTEGER`. Not the Arrow type the values arrive as — a
    /// structure pane showing `Utf8` where the table says `VARCHAR(64)` is
    /// describing this client rather than the database.
    pub data_type: String,
    pub nullable: bool,
    /// One-based, as PostgreSQL's `attnum` is. Drivers whose catalog counts from
    /// zero convert, so that the same column is not first in one database and
    /// zeroth in another.
    pub position: i32,
    pub is_primary_key: bool,
    /// The default applied when a statement names no value — or, where
    /// `computed` says so, the expression the column is computed from.
    pub default_value: Option<String>,
    /// Which of those two the field above is holding: `None` for a default,
    /// `Some` for a computation.
    ///
    /// One field carried both until a renderer had to write the column back.
    /// SQL Server accepts `qty AS ([a]+[b]) PERSISTED` and refuses `qty int
    /// DEFAULT ([a]+[b])`, so a renderer that cannot tell a computation from a
    /// default emits a script that reads plausibly and does not run — wrong in
    /// the way that is hardest to notice. The expression stays where it was
    /// because a structure pane showing nothing for a computed column hides the
    /// only interesting thing about it; this says what it is.
    pub computed: Option<Computed>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexInfo {
    pub name: String,
    /// Key expressions in index order. Expressions rather than plain names,
    /// because an index on `lower(email)` is not an index on `email` and
    /// printing it as one would be a lie about what the planner can use.
    pub columns: Vec<String>,
    pub is_unique: bool,
    pub is_primary: bool,
    /// Access method: btree, hash, gin, gist, brin. `btree` where the database
    /// has only one.
    pub method: String,
    /// WHERE clause of a partial index, if any.
    pub predicate: Option<String>,
}

/// One UNIQUE constraint, in the form that can name a row.
///
/// Separate from [`IndexInfo`] although every database here backs a UNIQUE
/// constraint with a unique index, and the difference is the whole reason this
/// type exists. `IndexInfo::columns` holds key *expressions* — `lower(email)`,
/// `id DESC`, a prefix — because a structure pane states what the planner can
/// use. Deciding which row an `UPDATE` names is a different question: the
/// columns have to be looked up in [`ColumnInfo`] to find out whether they can
/// be null, and a name that does not match one is not a key, it is a string that
/// happened to look like a name.
///
/// So a driver reports here only what it can state as columns, and leaves out
/// what it cannot:
///
/// * a key over an expression, which no `WHERE column = value` can reproduce;
/// * a partial or filtered one, which is unique over some rows rather than over
///   the table, so two rows outside the predicate can share its values.
///
/// Leaving them out is what makes the omission safe: the caller sees a relation
/// with fewer keys, which costs an edit somebody has to make in SQL, where
/// including them would name a row that is not the row on screen.
///
/// The primary key is not here. Every driver already reports it on
/// `ColumnInfo::is_primary_key`, and a second answer to the same question is one
/// that can disagree with the first.
#[derive(Debug, Clone, Serialize)]
pub struct UniqueKeyInfo {
    /// The constraint's own name, which is what a refusal has to say out loud:
    /// "this table cannot be edited" is not actionable, "uq_orders_email is over
    /// a column that can be null" is.
    pub name: String,
    /// The columns it is over, in key order, spelled exactly as
    /// `ColumnInfo::name` spells them.
    pub columns: Vec<String>,
}

/// One foreign key, seen from whichever relation was asked about.
///
/// The same constraint is a table's own key when looked at from the referencing
/// side and an inbound reference when looked at from the referenced side, so the
/// fields are named for the vantage point rather than for one direction. Reusing
/// "referenced_table" for both would name the field after the wrong end half the
/// time.
#[derive(Debug, Clone, Serialize)]
pub struct RelationshipInfo {
    /// The declared name where the database keeps one. SQLite does not, even
    /// where the user wrote one, so its driver builds a name from the table that
    /// declared the key and the key's position.
    pub name: String,
    /// Columns on the relation that was asked about.
    pub local_columns: Vec<String>,
    pub other_schema: String,
    pub other_table: String,
    pub other_columns: Vec<String>,
    pub on_update: String,
    pub on_delete: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConstraintKind {
    Check,
    Unique,
    Exclude,
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConstraintInfo {
    pub name: String,
    pub kind: ConstraintKind,
    /// The database's own rendering of the constraint. Reproducing it from
    /// catalog columns would mean reimplementing expression formatting, and
    /// getting it subtly wrong on the cases that matter.
    pub definition: String,
}

/// A trigger, in as much detail as the database records.
///
/// The field that had to change shape, and the clearest case of two catalogs
/// disagreeing about what a thing is. PostgreSQL keeps the timing, the events,
/// the level and the function in columns of `pg_trigger`, so all four can be
/// stated. SQLite keeps the statement the trigger was created from and nothing
/// else, so none of them can be — and picking `AFTER` out of that text is
/// guessing at something the reader can see for themselves.
///
/// So the descriptors are optional and the definition is carried beside them.
/// Both drivers fill `definition`, so a structure pane always has something to
/// show; the descriptors are what a database can add when it knows them.
#[derive(Debug, Clone, Serialize)]
pub struct TriggerInfo {
    pub name: String,
    /// BEFORE, AFTER, or INSTEAD OF.
    pub timing: Option<String>,
    /// INSERT / UPDATE / DELETE / TRUNCATE, in that order.
    pub events: Vec<String>,
    /// ROW or STATEMENT.
    pub level: Option<String>,
    /// The function the trigger calls, where it calls one rather than carrying a
    /// body of its own.
    pub function: Option<String>,
    /// A disabled trigger listed as though it fires is worse than not listing it:
    /// it makes the reader expect behaviour that will not happen. `true` where
    /// the database has no way to disable one.
    pub enabled: bool,
    /// The statement it was created from.
    pub definition: Option<String>,
}
