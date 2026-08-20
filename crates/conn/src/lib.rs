//! What every driver is, from the outside.
//!
//! Phase 1 deliberately left this unwritten. With one driver a trait is invented
//! rather than derived, and an invented abstraction over a database is paid for
//! in every driver that has to pretend to fit it. It is written now because
//! there are two implementations to derive it from, and the two were chosen to
//! disagree: PostgreSQL is a server reached over a socket, SQLite is a library
//! in this process, and almost every assumption the first one made turned out to
//! be about servers rather than about databases.
//!
//! What survived the second implementation:
//!
//! **Everything is fallible in the same way.** A front end needs three things
//! from a failure — what to print, where in the statement to put the caret, and
//! whether this was a fault or a button the user pressed. Drivers keep their own
//! rich error types internally and convert at this boundary, so the FFI has one
//! error and not one per database.
//!
//! **Cancellation is asynchronous even where it is not.** PostgreSQL sends the
//! request to the server on a connection of its own, so it is something to wait
//! for; SQLite sets a flag in this process and returns. The trait takes the
//! shape the slower one needs, because a synchronous signature would have no
//! room for the round trip and every driver that talks to a server would have to
//! block a runtime thread inside it.
//!
//! **A cursor is not always a `DECLARE`.** What Phase 1 wanted from one was that
//! page two agrees with page one and that paging does not re-read. PostgreSQL
//! needs a server-side cursor for that; SQLite gets it from a statement stepped
//! forward. So the trait asks for the property and not for the mechanism.
//!
//! **A canceller is taken out in advance.** Cancelling a fetch means reaching
//! the connection at the moment the fetch has it, so it cannot be something
//! reached for through the cursor. Both drivers arrived at the same shape
//! independently, which is the strongest evidence in here that it is the right
//! one.
//!
//! Six more implementations later — MySQL, SQL Server, DuckDB, ClickHouse and,
//! deliberately, MongoDB — one thing changed: `query` and `cursor` take a
//! `statement` and not a `sql`. That is the whole of it. A document database
//! whose statements are JSON objects, an analytical database with no cursors of
//! its own, and an embedded one that runs in this thread all fit without the
//! trait growing a method, an option or an escape hatch. Where the databases
//! genuinely disagree — whether a failure carries a position, whether reading a
//! relation that is not there is a failure at all — the trait already said "or
//! not", so the disagreement had somewhere to live.

mod metadata;

pub use metadata::{
    ColumnInfo, Computed, ConstraintInfo, ConstraintKind, IndexInfo, RelationInfo, RelationKind,
    RelationshipInfo, SchemaInfo, TriggerInfo, UniqueKeyInfo,
};

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use async_trait::async_trait;
use std::fmt;

/// Field metadata marking a column the server declared NOT NULL.
///
/// Beside the validity buffer rather than in it. A driver may substitute NULL
/// for a value it cannot represent — MySQL's `'0000-00-00'` is the case this
/// exists for — so the field has to stay nullable however the column was
/// declared, and a validity buffer contradicting its own field is corrupt
/// rather than merely surprising.
///
/// Not namespaced per driver, unlike `duckdb.rendered_from`, because the reader
/// is the grid: a shared consumer keying off one driver's name would have to
/// learn a new spelling for each database that reports the same fact.
pub const DECLARED_NOT_NULL: &str = "dbclient.declared_not_null";

/// A failure, reduced to what a front end acts on.
///
/// Three fields, because there are three questions: what to show, where to put
/// the caret, and whether to show it as a fault at all. Drivers keep whatever
/// structure their database gives them and answer these on the way out.
#[derive(Debug, Clone)]
pub struct DbError {
    message: String,
    position: Option<u32>,
    cancelled: bool,
}

impl DbError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            position: None,
            cancelled: false,
        }
    }

    /// Where in the statement the database says the trouble is: 1-based, counted
    /// in characters, into the text that was sent.
    ///
    /// Characters and not bytes, and from one and not zero, because the two
    /// databases already disagreed about both and a front end cannot ask which
    /// it is holding. SQLite reports a byte offset from zero and converts here;
    /// PostgreSQL reports this contract natively. The difference is invisible
    /// until somebody names a table in a language that is not English.
    pub fn at_position(mut self, position: Option<u32>) -> Self {
        self.position = position;
        self
    }

    /// Marks a failure as something somebody asked for.
    ///
    /// Kept apart from the message because it is a fact about the failure rather
    /// than a way of describing it: "canceling statement due to user request" in
    /// an error banner reads as a fault, when it is the button they just pressed
    /// working.
    pub fn as_cancelled(mut self, cancelled: bool) -> Self {
        self.cancelled = cancelled;
        self
    }

    pub fn statement_position(&self) -> Option<u32> {
        self.position
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DbError {}

pub type DbResult<T> = Result<T, DbError>;

/// What actually answered, once the connection was made.
///
/// Asked rather than assumed, because a scheme names a wire protocol and not a
/// product: `postgres://` reaches CockroachDB and GreptimeDB as readily as
/// PostgreSQL, and `mysql://` reaches TiDB and MariaDB. A client that printed the
/// scheme's label would be showing somebody the name of the driver that opened
/// the connection while calling it the name of their database.
///
/// Two fields and no more. Charset and identifier case are facts each driver
/// already acts on where they matter, and collecting them here would be a second
/// copy for a front end to disagree with. What a front end has no other way to
/// learn is which product is on the other end, and which version of it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ServerInfo {
    /// What the product calls itself: "PostgreSQL", "TiDB", "SQLite".
    pub product: String,
    /// The version as the server states it, or empty where it states none.
    ///
    /// The server's own spelling, unparsed. The databases here spell it three
    /// ways — `17.0`, `v1.1.3`, `8.0.11-TiDB-v7.5.0` — and a client that
    /// normalized them would be deciding, for a version it has never seen, which
    /// half of the string was the number.
    pub version: String,
}

impl ServerInfo {
    /// The product and version a driver names itself.
    pub fn new(product: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            product: product.into(),
            version: version.into(),
        }
    }

    /// The product and version out of a banner whose first word is the product's
    /// own name.
    ///
    /// The shape PostgreSQL answers `SELECT version()` in — "PostgreSQL 17.0
    /// (Debian 17.0-1) on aarch64…" — and the shape the products speaking its
    /// protocol answer in too, with their own name in front. Reading the first
    /// word is therefore how a connection opened as `postgres://` learns that it
    /// is not on PostgreSQL, which is the whole reason for asking.
    ///
    /// The version is the first word starting with a digit rather than the second
    /// word, because the second word is not always one: CockroachDB answers
    /// "CockroachDB CCL v23.1.11 (aarch64, built …)", and a rule that took the
    /// word after the name would report its licence as its version.
    pub fn from_banner(banner: &str) -> Self {
        let mut words = banner.split_whitespace();
        let product = words.next().unwrap_or_default();
        let version = words
            .find(|word| {
                word.strip_prefix('v')
                    .unwrap_or(word)
                    .starts_with(|c: char| c.is_ascii_digit())
            })
            // The leading `v` and any trailing comma go, and nothing else does:
            // both are punctuation around the number rather than part of it, and
            // a banner is prose.
            .unwrap_or_default()
            .trim_end_matches(',')
            .trim_start_matches('v');
        Self::new(product, version)
    }
}

/// One session against one database.
///
/// A session, not a connection: an implementation is free to hold several, and
/// both of the first two do. What the caller is promised is that a result being
/// read does not make the navigator wait behind it — PostgreSQL keeps a pool for
/// that, SQLite opens a file handle per reader, and neither arrangement is
/// visible from here.
#[async_trait]
pub trait Driver: Send + Sync {
    // ---- Identity -------------------------------------------------------

    /// What is actually at the other end of this connection.
    ///
    /// A round trip rather than a fact read off the connection string: the
    /// string names a protocol and the answer names a product. Priced as a
    /// statement, because it is one — whoever opens the connection asks once.
    ///
    /// No default, for the reason `transactional` has none. A driver that cannot
    /// ask has to say what it is instead, out loud, where the reason it cannot
    /// can be read beside it.
    async fn server_info(&self) -> DbResult<ServerInfo>;

    // ---- Metadata -------------------------------------------------------

    /// The navigator root. A database with no schema layer of its own answers
    /// with the one container it has rather than with an empty list, so that the
    /// navigator has the same shape everywhere.
    async fn schemas(&self) -> DbResult<Vec<SchemaInfo>>;

    async fn relations(&self, schema: &str) -> DbResult<Vec<RelationInfo>>;

    async fn columns(&self, schema: &str, relation: &str) -> DbResult<Vec<ColumnInfo>>;

    /// The statement a view is defined by; `None` for a relation that has none.
    ///
    /// How faithful that statement is differs by database and cannot be made
    /// uniform: SQLite returns the text it was given, PostgreSQL renders the
    /// query back from its parse tree and so returns a normalized body without
    /// the `CREATE VIEW` around it. Both are the definition as that database
    /// holds it, which is the most this can promise.
    async fn definition(&self, schema: &str, relation: &str) -> DbResult<Option<String>>;

    async fn indexes(&self, schema: &str, relation: &str) -> DbResult<Vec<IndexInfo>>;

    /// UNIQUE constraints this relation declares, other than its primary key.
    ///
    /// The list a row can be identified by when there is no primary key, which
    /// is the only thing it is for — so a driver reports a constraint here only
    /// if it can name the columns the way `columns` names them, and omits the
    /// ones it cannot state that way. `UniqueKeyInfo` says which those are and
    /// why leaving them out is the safe direction.
    ///
    /// No default, for the reason `transactional` has none: empty means two
    /// different things — a database with no unique constraints of its own, and
    /// a catalog this driver cannot read them out of — and both deserve a
    /// sentence where the driver is, rather than silence inherited from here.
    async fn unique_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<UniqueKeyInfo>>;

    /// Foreign keys this relation declares.
    async fn foreign_keys(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>>;

    /// Foreign keys other relations declare against this one.
    async fn referenced_by(&self, schema: &str, relation: &str) -> DbResult<Vec<RelationshipInfo>>;

    async fn constraints(&self, schema: &str, relation: &str) -> DbResult<Vec<ConstraintInfo>>;

    async fn triggers(&self, schema: &str, relation: &str) -> DbResult<Vec<TriggerInfo>>;

    // ---- Results --------------------------------------------------------

    /// Run `statement` and return a stream over its results, in batches of at
    /// most `batch_rows` rows.
    ///
    /// `statement` is text this database understands, and not necessarily SQL.
    /// The parameter was called `sql` while PostgreSQL and SQLite were the only
    /// implementations, and MongoDB is what showed the word to be wrong: it
    /// takes a command document, `{"find": "orders", "filter": {…}}`, which is
    /// what its own protocol carries. Nothing about the shape of this call had
    /// to change to accommodate that, which is the useful result — a statement
    /// really is just text the database understands, and only the name said
    /// otherwise.
    ///
    /// When this resolves relative to the statement running is deliberately not
    /// specified, because the two implementations already differ and neither is
    /// wrong: PostgreSQL waits out the whole command because the server buffers
    /// its output, SQLite waits for the first row because that is when a column's
    /// type is settled. What is specified is that an execution failure may
    /// surface from `next_batch` rather than from here, so a caller that stops at
    /// a successful `query` has not established that the statement worked.
    async fn query(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn ResultStream>>;

    /// The statement that reads a relation's rows, as this database spells it.
    ///
    /// Text and not rows, because a browse is a statement like any other: it goes
    /// back through `query` or `cursor`, inside whatever transaction the session
    /// is in, under the same Cancel button — and it can be shown to the person
    /// who is about to run it.
    ///
    /// Here rather than in the front end, which is where it was until a front end
    /// that had only ever met PostgreSQL wrote `SELECT * FROM "bench"."orders"`
    /// for every database it could open. MySQL rejects that as a syntax error
    /// unless `ANSI_QUOTES` is set, and MongoDB rejects it because it is not a
    /// command document. Quoting is dialect-specific and a statement is not
    /// always SQL, so both answers belong to the driver.
    ///
    /// No I/O: everything this needs is in `what`, so a caller can build the
    /// statement and decide not to run it.
    fn browse(&self, what: &Browse<'_>) -> String;

    /// Read `statement` forward, a page at a time.
    ///
    /// Two properties, which is all this asks for — the mechanism is the
    /// driver's. Reading page *n* must not re-read the pages before it, so page
    /// *n* costs what page one costs. And the pages must agree with each other:
    /// a write landing between two of them must not make a row appear twice or
    /// not at all. Neither is achievable with `LIMIT`/`OFFSET`, which is what
    /// this exists instead of.
    async fn cursor(&self, statement: &str, batch_rows: usize) -> DbResult<Box<dyn Cursor>>;

    /// Ask the database to abandon whatever this session is running.
    ///
    /// May be called while another call is in flight on the same session, and
    /// has to be: everything else here blocks, so a cancel that waited its turn
    /// would arrive after the statement it exists to interrupt.
    ///
    /// Best-effort. Success means the request was delivered, not that anything
    /// stopped — the statement may have finished first, or there may have been
    /// nothing running. What actually happened shows up where the statement is,
    /// as a failure whose `is_cancelled` is true.
    ///
    /// Does not reach a cursor. A cursor is handed out to be held and outlives
    /// the call that made it, so it carries its own canceller.
    async fn cancel(&self) -> DbResult<()>;

    // ---- Transactions ---------------------------------------------------

    /// Whether statements on this session can be wrapped in a transaction.
    ///
    /// No default. A driver that cannot has to say so out loud, because the
    /// front end hides Commit and Rollback rather than offering buttons that
    /// fail — and because "false" here means two quite different things that
    /// both deserve a sentence in the implementation: a database with no
    /// transactions, and a driver whose statements do not yet share one
    /// connection to hold one.
    ///
    /// A transaction is a property of a connection, so this is really a question
    /// about the arrangement inside the driver rather than about the database.
    /// It does not cover cursors: a cursor runs on a connection of its own and
    /// is outside whatever the session has open.
    fn transactional(&self) -> bool;

    /// Take one step of transaction control, on the connection statements run
    /// on.
    ///
    /// That connection and no other. A `BEGIN` sent down a pooled connection
    /// opens a transaction on a connection the next statement will not be given,
    /// and nothing afterwards can commit it.
    ///
    /// A step this database does not have is refused rather than skipped. SQL
    /// Server releases savepoints implicitly and DuckDB has none at all, and a
    /// client that quietly did nothing would leave somebody believing there is a
    /// point they can come back to.
    async fn transaction(&self, step: &TxStep) -> DbResult<()>;
}

/// The first value of the first row `statement` produces, as text.
///
/// Here rather than in each driver that wants one: asking a database what it is
/// is a scalar query in every database that can answer at all, and fifteen copies
/// of "run it, take the first batch, read cell (0, 0)" would be fifteen places
/// for one of them to read the wrong column. It takes the trait object rather
/// than a generic so that there is one instantiation of it, and so a driver can
/// pass `self`.
///
/// Formatted rather than downcast, because the answer's type is the server's
/// choice and not this caller's: SQL Server hands a property back as a variant
/// and ClickHouse hands a version back as a string, so a downcast to one of them
/// would fail on the next database to be asked. `ArrayFormatter` renders whatever
/// arrived.
pub async fn scalar_text(driver: &dyn Driver, statement: &str) -> DbResult<String> {
    let mut stream = driver.query(statement, 1).await?;
    while let Some(batch) = stream.next_batch().await? {
        // An empty batch is not the end of a result: a driver may settle the
        // schema before it has a row, and the value is in the batch after it.
        if batch.num_columns() == 0 || batch.num_rows() == 0 {
            continue;
        }
        let options = FormatOptions::default();
        let formatter = ArrayFormatter::try_new(batch.column(0).as_ref(), &options)
            .map_err(|e| DbError::new(e.to_string()))?;
        return Ok(formatter.value(0).to_string());
    }
    Err(DbError::new(format!(
        "the server answered nothing to `{statement}`"
    )))
}

/// What a browse is asking for.
///
/// A struct rather than five parameters, because three of them are optional and
/// a call site with three `None`s in a row is a place to put one in the wrong
/// order. The text fields are the user's own — typed into the filter bar in
/// whatever language this database reads — and reach the statement unaltered.
pub struct Browse<'a> {
    pub schema: &'a str,
    pub relation: &'a str,
    /// The filter as it was typed: a `WHERE` clause without the word, a MongoDB
    /// filter document, a key pattern.
    pub filter: Option<&'a str>,
    /// The ordering as it was typed, without the words `ORDER BY`.
    pub order: Option<&'a str>,
    /// Columns to order by after `order`, and the reason a browse looks the same
    /// twice: without a total order the rows arrive in whatever order the plan
    /// produced, which is stable within one result and arbitrary between two.
    /// The caller supplies them because it is the caller that knows which
    /// columns the catalog called a key — and which of them the user has already
    /// named.
    pub keys: &'a [String],
    /// A row ceiling, for a caller that wants a statement to put in an editor
    /// rather than one to page through. The Content tab passes `None`: its
    /// bound is the cursor.
    pub limit: Option<u32>,
}

impl Browse<'_> {
    /// This browse as SQL, in `dialect`'s spelling.
    ///
    /// Here rather than six times over, because the six drivers that speak SQL
    /// write the same statement and differ only in how a name is quoted and
    /// where a row ceiling goes — both of which `dbsql` already answers. The
    /// trait does not require SQL of anybody; this is for the implementations
    /// that want it, and MongoDB's `browse` never calls it.
    pub fn sql(&self, dialect: &dbsql::Dialect) -> String {
        let mut name = String::new();
        // A database with no schema layer answers with an empty one rather than
        // with a name, and `.orders` is not a relation anywhere.
        if !self.schema.is_empty() {
            name.push_str(&dialect.quote(self.schema));
            name.push('.');
        }
        name.push_str(&dialect.quote(self.relation));
        self.sql_named(dialect, &name)
    }

    /// The same, for a driver that writes the relation's name itself.
    ///
    /// DuckDB is why this is separate: its schemas are reported as
    /// `database.schema` because its namespace has a level the trait does not,
    /// and that name is already SQL. Quoting it as one identifier would name a
    /// schema with a dot in it, which is a different schema or none.
    pub fn sql_named(&self, dialect: &dbsql::Dialect, name: &str) -> String {
        let mut sql = String::from("SELECT ");
        if let (Some(rows), dbsql::RowLimit::Top) = (self.limit, dialect.row_limit) {
            sql.push_str(&format!("TOP ({rows}) "));
        }
        sql.push_str("* FROM ");
        sql.push_str(name);

        if let Some(filter) = self.filter.map(str::trim).filter(|f| !f.is_empty()) {
            // As typed. The filter bar takes an expression in the database's own
            // language, and a client that rewrote it would be parsing SQL in
            // order to hand it back.
            sql.push_str(" WHERE ");
            sql.push_str(filter);
        }

        let mut terms = Vec::new();
        if let Some(order) = self.order.map(str::trim).filter(|o| !o.is_empty()) {
            terms.push(order.to_string());
        }
        terms.extend(self.keys.iter().map(|key| dialect.quote(key)));
        if !terms.is_empty() {
            sql.push_str(" ORDER BY ");
            sql.push_str(&terms.join(", "));
        }

        if let (Some(rows), dbsql::RowLimit::Limit) = (self.limit, dialect.row_limit) {
            sql.push_str(&format!(" LIMIT {rows}"));
        }
        sql
    }
}

/// One step of transaction control.
///
/// An enum rather than six methods on the trait. Seven drivers implement this,
/// and a method apiece would be six more places for one of them to be quietly
/// forgotten — a compiler insists on a method existing, never on it doing
/// anything. One `match` per driver puts every answer, including "this database
/// cannot do that", where a reader can check them together.
///
/// The names in the three savepoint steps are generated by whoever holds the
/// session, and are always letters, digits and underscores. Drivers write them
/// into a statement, which is only safe because of that: a savepoint name is an
/// identifier and no database takes one as a parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxStep {
    Begin,
    Commit,
    Rollback,
    /// A point to come back to, inside the transaction already open.
    Savepoint(String),
    /// Undo back to a savepoint, leaving the transaction open.
    RollbackTo(String),
    /// Forget a savepoint, keeping everything done since.
    Release(String),
}

/// A result being read forward in batches.
#[async_trait]
pub trait ResultStream: Send {
    /// The columns the rows arrive in, known before the first batch.
    fn schema(&self) -> SchemaRef;

    /// Rows the statement affected, or `None` until the result has been read to
    /// the end.
    ///
    /// Negative space matters here: zero is a real answer — an `UPDATE` that
    /// matched nothing — so "not known yet" has to be something else. What the
    /// number counts differs with the statement: rows changed for one that
    /// writes, rows produced for one that reads.
    fn rows_affected(&self) -> Option<u64>;

    /// Next batch, or `None` once the result is fully consumed.
    async fn next_batch(&mut self) -> DbResult<Option<RecordBatch>>;
}

/// A result read a page at a time, with a stable position.
#[async_trait]
pub trait Cursor: Send {
    fn schema(&self) -> SchemaRef;

    /// Next page, or `None` once the cursor has reached the end.
    async fn fetch(&mut self) -> DbResult<Option<RecordBatch>>;

    /// A handle for stopping this cursor's fetch from another thread.
    ///
    /// Taken out in advance rather than reached for at cancel time, because by
    /// then the cursor itself is borrowed by the fetch that is to be stopped —
    /// which is the whole situation.
    fn canceller(&self) -> Box<dyn CursorCancel>;

    /// Close the cursor and release whatever it was holding.
    ///
    /// Optional: dropping it does the same. This exists for a front end that
    /// wants to close at a moment of its choosing rather than whenever the last
    /// reference goes away — which on macOS can be the main thread.
    async fn close(&mut self) -> DbResult<()>;
}

/// Stops the fetch one cursor is running.
#[async_trait]
pub trait CursorCancel: Send + Sync {
    /// Delivered is not interrupted, as with `Driver::cancel`: a fetch that had
    /// already finished leaves nothing to stop and this still succeeds.
    async fn cancel(&self) -> DbResult<()>;
}

#[cfg(test)]
mod tests {
    use super::{Browse, ServerInfo};

    fn browse<'a>(
        filter: Option<&'a str>,
        order: Option<&'a str>,
        keys: &'a [String],
    ) -> Browse<'a> {
        Browse {
            schema: "bench",
            relation: "orders",
            filter,
            order,
            keys,
            limit: None,
        }
    }

    /// A banner is read for the product as well as the version, which is the
    /// whole reason a connection asks instead of trusting its own scheme.
    #[test]
    fn a_postgres_wire_server_is_named_by_its_own_banner() {
        assert_eq!(
            ServerInfo::from_banner("PostgreSQL 17.0 (Debian 17.0-1) on aarch64"),
            ServerInfo::new("PostgreSQL", "17.0")
        );
        // Not "CCL". The second word of this one is a licence, and a rule that
        // took the word after the name would report it as the version.
        assert_eq!(
            ServerInfo::from_banner("CockroachDB CCL v23.1.11 (aarch64, built 2023/11/13)"),
            ServerInfo::new("CockroachDB", "23.1.11")
        );
    }

    /// A banner with no number in it keeps its name and admits to no version,
    /// rather than reporting a word as one.
    #[test]
    fn a_banner_with_no_version_in_it_states_none() {
        assert_eq!(
            ServerInfo::from_banner("Ferrous"),
            ServerInfo::new("Ferrous", "")
        );
        assert_eq!(ServerInfo::from_banner(""), ServerInfo::new("", ""));
    }

    /// The defect this whole call exists for: a front end wrote PostgreSQL's
    /// quoting for every database, and MySQL reads it as a string.
    #[test]
    fn mysql_is_not_given_the_quoting_that_makes_it_read_a_string() {
        let sql = browse(None, None, &[]).sql(&dbsql::MYSQL);
        assert!(!sql.contains('"'), "{sql}");
    }

    /// A row ceiling is a dialect fact, not a suffix: T-SQL has no LIMIT.
    #[test]
    fn sql_server_bounds_a_result_before_the_columns() {
        let mut what = browse(None, None, &[]);
        what.limit = Some(1000);
        let sql = what.sql(&dbsql::MSSQL);
        assert_eq!(sql, "SELECT TOP (1000) * FROM bench.orders");
    }

    /// The filter and the order are the user's own words and reach the statement
    /// as typed; the key columns are this side's and are quoted.
    #[test]
    fn the_users_order_comes_first_and_the_key_makes_it_total() {
        let keys = ["id".to_string()];
        let sql = browse(Some("qty > 10"), Some("label desc"), &keys).sql(&dbsql::POSTGRES);
        assert_eq!(
            sql,
            "SELECT * FROM bench.orders WHERE qty > 10 ORDER BY label desc, id"
        );
    }

    /// A name that needs quoting gets it, and a database with no schema layer
    /// does not get a leading dot.
    #[test]
    fn a_name_that_is_a_keyword_is_still_the_name_it_is() {
        let what = Browse {
            schema: "",
            relation: "order",
            filter: None,
            order: None,
            keys: &[],
            limit: None,
        };
        assert_eq!(what.sql(&dbsql::POSTGRES), r#"SELECT * FROM "order""#);
    }
}
