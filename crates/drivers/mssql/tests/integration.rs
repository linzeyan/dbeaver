//! What this driver does against a real SQL Server.
//!
//! Every test here is `#[ignore]`d, so `cargo test` passes with nothing
//! installed. To run them:
//!
//! ```text
//! docker run -d --name mssql-test --platform linux/amd64 \
//!   -e ACCEPT_EULA=Y -e MSSQL_SA_PASSWORD='Str0ng!Passw0rd' -e MSSQL_PID=Developer \
//!   -p 51433:1433 mcr.microsoft.com/mssql/server:2022-latest
//! cargo test -p driver-mssql -- --ignored
//! ```
//!
//! Microsoft publishes no ARM64 build of SQL Server — not for 2019, 2022 or
//! 2025 — so on Apple silicon that image runs under Rosetta emulation and needs
//! `--platform linux/amd64`. Expect thirty seconds or so before it accepts a
//! connection, and expect it to be slow. Azure SQL Edge is the usual ARM64
//! fallback and is the wrong fixture here: it is a reduced engine, and half of
//! what these tests exercise is the full `sys.*` surface and the CLR types.
//!
//! The fixture is created by these tests rather than by a seed script, so there
//! is one command to run and not two. It is built with tiberius directly and not
//! through this driver, so a fixture that came out wrong cannot be the code under
//! test agreeing with itself.
//!
//! The shared contract is not repeated here. `crates/conn/tests/contract.rs`
//! runs it against this driver like every other, so what is left in this file
//! is what is particular to SQL Server: the types that would take the process
//! down, paging a heap that has no key to order by, and cancelling a statement
//! by ending its session.

use arrow::array::{
    Array, BinaryArray, BooleanArray, Date32Array, Decimal128Array, Int16Array, Int32Array,
    StringArray, Time64MicrosecondArray, TimestampMicrosecondArray,
};
use arrow::datatypes::{DataType, TimeUnit};
use dbconn::{Computed, ConstraintKind, DbError, Driver, RelationKind, TxStep};
use driver_mssql::MsSqlSource;
use std::collections::HashSet;
use tiberius::{Client, Config};
use tokio::net::TcpStream;
use tokio::sync::OnceCell;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

const HOST: &str = "Server=tcp:localhost,51433;User Id=sa;Password=Str0ng!Passw0rd;\
                    Encrypt=true;TrustServerCertificate=true;Application Name=dbclient-tests";

fn conn_str(database: &str) -> String {
    format!("{HOST};Database={database}")
}

async fn source() -> MsSqlSource {
    fixture().await;
    MsSqlSource::connect(&conn_str("dbeaver_test"))
        .await
        .expect("SQL Server unreachable; see the header of this file")
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

static FIXTURE: OnceCell<()> = OnceCell::const_new();

async fn fixture() {
    FIXTURE.get_or_init(build_fixture).await;
}

async fn raw(database: &str) -> Client<Compat<TcpStream>> {
    let config = Config::from_ado_string(&conn_str(database)).expect("connection string");
    let tcp = TcpStream::connect(config.get_addr())
        .await
        .expect("SQL Server unreachable; see the header of this file");
    tcp.set_nodelay(true).unwrap();
    Client::connect(config, tcp.compat_write())
        .await
        .expect("SQL Server refused the login")
}

async fn run(client: &mut Client<Compat<TcpStream>>, sql: &str) {
    client
        .simple_query(sql)
        .await
        .unwrap_or_else(|e| panic!("fixture statement failed: {e}\n{sql}"))
        .into_results()
        .await
        .unwrap_or_else(|e| panic!("fixture statement failed: {e}\n{sql}"));
}

/// What the database must have been built from, for the copy on the server to
/// count as this file's fixture.
///
/// The container outlives every run and `DDL` does not, so "does `sales.customer`
/// exist" answered yes for a table built by an older version of this file. A test
/// added alongside a new column then failed against a fixture that predated it,
/// and the failure named the column rather than the staleness — which reads
/// exactly like the driver losing a column. Comparing what the database was built
/// from is the only question whose answer cannot drift.
fn fixture_fingerprint() -> String {
    // FNV-1a rather than a hash crate: this is a cache key for a test fixture,
    // so the only property required of it is that different DDL gives a
    // different answer.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for statement in DDL {
        for byte in statement.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

async fn build_fixture() {
    let wanted = fixture_fingerprint();
    let mut master = raw("master").await;

    // Rebuilt rather than migrated. A migration would have to describe every
    // shape this fixture has ever had, and none of those shapes is interesting:
    // what the tests need is that the database matches the DDL beside them.
    //
    // An unstamped database is rebuilt too, and that is the case that matters
    // most: every database built before this check existed is unstamped, and
    // every one of them is stale by definition.
    if exists(&mut master).await && stamp(&mut master).await.as_deref() != Some(wanted.as_str()) {
        run(
            &mut master,
            "ALTER DATABASE dbeaver_test SET SINGLE_USER WITH ROLLBACK IMMEDIATE; \
             DROP DATABASE dbeaver_test",
        )
        .await;
    }
    run(
        &mut master,
        "IF DB_ID('dbeaver_test') IS NULL CREATE DATABASE dbeaver_test",
    )
    .await;
    drop(master);

    let mut db = raw("dbeaver_test").await;
    // A filtered index and the XML methods below both refuse to be created
    // unless these are on, and they are session state — so they are set on the
    // connection that runs the whole seed rather than repeated per statement.
    run(&mut db, "SET QUOTED_IDENTIFIER ON; SET ANSI_NULLS ON;").await;

    let already: i32 = db
        .simple_query(
            "SELECT COUNT(*) FROM sys.tables t \
             JOIN sys.schemas s ON s.schema_id = t.schema_id \
             WHERE s.name = 'sales' AND t.name = 'customer'",
        )
        .await
        .expect("counting the fixture")
        .into_row()
        .await
        .expect("counting the fixture")
        .and_then(|r| r.get(0))
        .unwrap_or(0);
    if already > 0 {
        return;
    }

    for statement in DDL {
        run(&mut db, statement).await;
    }
    // Written last, so a seed that died halfway leaves no stamp and the next run
    // rebuilds instead of trusting a half-built database.
    run(
        &mut db,
        &format!(
            "EXEC sys.sp_addextendedproperty @name = N'dbclient_fixture', \
             @value = N'{wanted}'"
        ),
    )
    .await;
}

async fn exists(master: &mut Client<Compat<TcpStream>>) -> bool {
    // Cast because `DB_ID` answers `smallint`, and a decoder asked for the wrong
    // width fails rather than widening.
    let id: Option<i32> = master
        .simple_query("SELECT CAST(DB_ID('dbeaver_test') AS int)")
        .await
        .expect("asking for the fixture database")
        .into_row()
        .await
        .expect("asking for the fixture database")
        .and_then(|row| row.get(0));
    id.is_some()
}

/// The fingerprint the database on the server was built from, or `None` when
/// nothing stamped it — which is what every database built before this check
/// existed will answer.
async fn stamp(master: &mut Client<Compat<TcpStream>>) -> Option<String> {
    master
        .simple_query(
            "SELECT CAST(value AS nvarchar(64)) FROM dbeaver_test.sys.extended_properties \
             WHERE class = 0 AND name = 'dbclient_fixture'",
        )
        .await
        .expect("reading the fixture stamp")
        .into_row()
        .await
        .expect("reading the fixture stamp")
        .and_then(|row| row.get::<&str, _>(0).map(str::to_string))
}

/// One statement per entry, because `CREATE SCHEMA` and `CREATE TRIGGER` each
/// have to be the first thing in their batch and `GO` is a tool's separator
/// rather than anything the server understands.
const DDL: &[&str] = &[
    "CREATE SCHEMA sales",
    // An empty schema, to prove `schemas` does not hide one.
    "CREATE SCHEMA archive",
    "CREATE TABLE sales.customer (
        customer_id  int IDENTITY(1,1) NOT NULL,
        ext_id       uniqueidentifier  NOT NULL DEFAULT NEWID(),
        name         nvarchar(100)     NOT NULL,
        code         varchar(16)       NULL,
        notes        nvarchar(max)     NULL,
        credit_limit decimal(18,4)     NOT NULL DEFAULT 0,
        balance      money             NULL,
        petty        smallmoney        NULL,
        tier         tinyint           NOT NULL DEFAULT 1,
        active       bit               NOT NULL DEFAULT 1,
        created_at   datetime2(7)      NOT NULL DEFAULT SYSUTCDATETIME(),
        created_tz   datetimeoffset(7) NULL,
        legacy_ts    datetime          NULL,
        coarse_ts    smalldatetime     NULL,
        born         date              NULL,
        opens_at     time(3)           NULL,
        avatar       varbinary(max)    NULL,
        doc          xml               NULL,
        row_ver      rowversion,
        display_name AS (name + N' <' + ISNULL(code, '') + N'>'),
        tier_doubled AS (tier * 2) PERSISTED,
        CONSTRAINT pk_customer PRIMARY KEY CLUSTERED (customer_id),
        CONSTRAINT uq_customer_ext UNIQUE (ext_id),
        CONSTRAINT ck_customer_tier CHECK (tier BETWEEN 1 AND 5))",
    "CREATE INDEX ix_customer_name ON sales.customer (name DESC) INCLUDE (code, tier)",
    "CREATE INDEX ix_customer_active ON sales.customer (created_at) WHERE active = 1",
    // A name that has to be quoted, and a cascading foreign key.
    "CREATE TABLE sales.[order] (
        order_id    int        NOT NULL IDENTITY(1,1) PRIMARY KEY,
        customer_id int        NOT NULL,
        placed_on   date       NOT NULL,
        total       smallmoney NULL,
        payload     xml        NULL,
        CONSTRAINT fk_order_customer FOREIGN KEY (customer_id)
            REFERENCES sales.customer (customer_id)
            ON DELETE CASCADE ON UPDATE NO ACTION)",
    // A composite key, so the two sides of a relationship have to line up.
    "CREATE TABLE sales.order_line (
        order_id int NOT NULL,
        line_no  int NOT NULL,
        sku      nvarchar(40) NOT NULL,
        qty      int NOT NULL,
        CONSTRAINT pk_order_line PRIMARY KEY (order_id, line_no),
        CONSTRAINT fk_line_order FOREIGN KEY (order_id)
            REFERENCES sales.[order] (order_id) ON DELETE CASCADE)",
    // A heap with no key of any kind: the paging requirement lives or dies here.
    "CREATE TABLE sales.event_log (
        at     datetime2(3)  NOT NULL,
        kind   varchar(32)   NOT NULL,
        detail nvarchar(400) NULL)",
    "INSERT INTO sales.event_log (at, kind, detail)
     SELECT DATEADD(second, n, '2024-01-01'), 'tick', CONCAT(N'row ', n)
     FROM (SELECT TOP (50000) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS n
           FROM sys.all_objects a CROSS JOIN sys.all_objects b) x",
    "CREATE TABLE sales.nums (
        id    int NOT NULL CONSTRAINT pk_nums PRIMARY KEY,
        label nvarchar(40) NOT NULL)",
    "INSERT INTO sales.nums (id, label)
     SELECT n, CONCAT(N'row-', n)
     FROM (SELECT TOP (500) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS n
           FROM sys.all_objects a CROSS JOIN sys.all_objects b) x",
    // The four types that used to take the process down, kept in one table so
    // the rest of the fixture stays readable. The shapes cover every branch of
    // the spatial reader — the two shorthands, rings, and a nested collection —
    // and the `sql_variant` values cover a base type from each family, because a
    // variant states its type per value and the decoder has an arm per type.
    "CREATE TABLE sales.exotic (
        id       int NOT NULL PRIMARY KEY,
        node     hierarchyid NULL,
        shape    geometry    NULL,
        place    geography   NULL,
        anything sql_variant NULL)",
    "INSERT INTO sales.exotic (id, node, shape, place, anything)
     VALUES (1, hierarchyid::Parse('/1/2/'), geometry::Point(1, 2, 0),
             geography::Point(47.6, -122.3, 4326), CAST(42 AS int))",
    "INSERT INTO sales.exotic (id, node, shape, place, anything)
     VALUES (2, NULL, NULL, NULL, CAST(N'a string' AS nvarchar(20)))",
    // A negative, dotted label — the specification's own worked example — and
    // the single-line-segment shorthand.
    "INSERT INTO sales.exotic (id, node, shape, place, anything)
     VALUES (3, hierarchyid::Parse('/1/-2.18/'),
             geometry::Parse('LINESTRING (0 0, 1 1, 2 4)'),
             geography::Parse('LINESTRING (-122.36 47.66, -122.34 47.59)'),
             CAST(12345.6789 AS decimal(18,4)))",
    // The root, which encodes to no bytes at all, and a polygon with a hole in
    // it, which is two figures under one shape.
    "INSERT INTO sales.exotic (id, node, shape, place, anything)
     VALUES (4, hierarchyid::GetRoot(),
             geometry::Parse('POLYGON ((0 0, 0 3, 3 3, 3 0, 0 0), (1 1, 1 2, 2 2, 2 1, 1 1))'),
             geography::Parse('POLYGON ((-122.4 47.6, -122.3 47.6, -122.3 47.7, -122.4 47.7, -122.4 47.6))'),
             CAST('2024-01-15T09:30:00.123' AS datetime2(3)))",
    // Shapes that contain other shapes: a collection whose members keep their
    // own type names, and a multipoint whose members do not.
    "INSERT INTO sales.exotic (id, node, shape, place, anything)
     VALUES (5, hierarchyid::Parse('/1/2/3/'),
             geometry::Parse('GEOMETRYCOLLECTION (POINT (1 2), LINESTRING (0 0, 1 1))'),
             geography::Parse('MULTIPOINT ((-122.3 47.6), (-122.2 47.5))'),
             CAST(0x0A0B AS varbinary(8)))",
    "INSERT INTO sales.exotic (id, node, shape, place, anything)
     VALUES (6, NULL, NULL, NULL, CAST(1 AS bit))",
    "INSERT INTO sales.exotic (id, node, shape, place, anything)
     VALUES (7, NULL, NULL, NULL,
             CAST('11111111-2222-3333-4444-555555555555' AS uniqueidentifier))",
    "INSERT INTO sales.exotic (id, node, shape, place, anything)
     VALUES (8, NULL, NULL, NULL, CAST('ascii only' AS varchar(16)))",
    // A null variant, which carries no base type at all and so cannot say what
    // kind of null it is.
    "INSERT INTO sales.exotic (id, node, shape, place, anything)
     VALUES (9, NULL, NULL, NULL, NULL)",
    "CREATE VIEW sales.active_customer AS
        SELECT customer_id, name, credit_limit
        FROM sales.customer
        WHERE active = 1",
    "CREATE TRIGGER sales.trg_customer_audit
     ON sales.customer AFTER INSERT, UPDATE
     AS BEGIN SET NOCOUNT ON; END",
    "CREATE TRIGGER sales.trg_order_guard
     ON sales.[order] INSTEAD OF INSERT
     AS BEGIN SET NOCOUNT ON;
        INSERT INTO sales.[order] (customer_id, placed_on, total)
        SELECT customer_id, placed_on, total FROM inserted; END",
    "ALTER TABLE sales.customer DISABLE TRIGGER trg_customer_audit",
    // Row one is ordinary, row two is not English, row three is every boundary
    // the type mapping has.
    "INSERT INTO sales.customer
        (name, code, credit_limit, balance, petty, tier, created_tz, legacy_ts,
         coarse_ts, born, opens_at, avatar, doc)
     VALUES
        (N'Ada Lovelace', 'ADA', 1234.5678, 99.9999, 12.3456, 3,
         '2024-01-01T09:00:00.1234567+09:00', '1999-12-31 23:59:59.997',
         '1999-12-31 23:59', '1815-12-10', '09:30:00.123', 0xDEADBEEF, N'<a x=\"1\"/>'),
        (N'王小明', NULL, 0.0001, -0.0001, -0.0001, 1,
         '2024-06-30T23:59:59.9999999-05:00', '2024-06-30 12:00:00.000',
         '2024-06-30 12:00', '1990-02-28', '00:00:00.000', NULL, NULL),
        (N'Boundary', 'MAX', 99999999999999.9999, 922337203685477.5807, 214748.3647, 5,
         '9999-12-31T23:59:59.9999999+14:00', '9999-12-31 23:59:59.997',
         '2079-06-06 23:59', '0001-01-01', '23:59:59.999', 0x00, NULL)",
    "INSERT INTO sales.[order] (customer_id, placed_on, total)
     VALUES (1, '2024-02-01', 10.5), (1, '2024-03-01', 20.25)",
    "INSERT INTO sales.order_line (order_id, line_no, sku, qty)
     VALUES (1, 1, N'sku-1', 2), (1, 2, N'sku-2', 3)",
];

// ---------------------------------------------------------------------------
// What is particular to this database
// ---------------------------------------------------------------------------

const READ: &str = "SELECT id FROM sales.nums ORDER BY id";
/// Broken in the middle rather than truncated, so the failure is a parse error
/// with something after it rather than an unexpected end of input.
const BROKEN: &str = "SELECT id FROM sales.nums WHERE ORDER BY id";

/// The failure `sql` produces, insisting there is one.
///
/// Either call can be the one that fails, and which it is is deliberately
/// unspecified by the trait, so a check that looked at only one of them would
/// pass on one database and hang on another.
async fn failure(driver: &dyn Driver, sql: &str) -> DbError {
    match driver.query(sql, 10).await {
        Err(e) => e,
        Ok(mut stream) => match stream.next_batch().await {
            Err(e) => e,
            Ok(_) => panic!("expected this to fail: {sql}"),
        },
    }
}

/// Every row once, in order, all the way to the end.
///
/// The shared contract reads the first two batches and stops, because that is
/// what proves batching to a front end. This drains the whole result, which is
/// the part that would catch a paging bug that only shows up after the first
/// page — and it is cheap here because the fixture is 500 rows.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn every_row_arrives_once_and_in_order() {
    let driver = source().await;
    let mut stream = driver.query(READ, 100).await.expect("query failed");

    let mut seen: Vec<i32> = Vec::new();
    while let Some(batch) = stream.next_batch().await.unwrap() {
        seen.extend(ints(&batch));
    }
    assert_eq!(seen, (1..=500).collect::<Vec<i32>>());
    // Only once the result is finished: before that the count is not known, and
    // zero is a real answer so "not finished" cannot be zero.
    assert_eq!(stream.rows_affected(), Some(500));
}

/// Cancelling something that is not running leaves the connection alive.
///
/// The property that makes `KILL`-based cancellation safe to offer at all. This
/// driver cancels by ending the session, so a Cancel aimed at an idle
/// connection would take the whole thing down — and a front end's Cancel button
/// is pressed at whatever moment the user presses it.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn an_idle_session_survives_being_cancelled() {
    let driver = source().await;
    let cursor = driver.cursor(READ, 10).await.expect("cursor failed");
    cursor.canceller().cancel().await.expect("cancel failed");
    driver.cancel().await.expect("session cancel failed");

    driver.schemas().await.expect("the session survived");
}

/// A position a front end could put a caret on: counted from one, and no further
/// than one past the end of what was sent.
fn lands_inside(err: &DbError, sql: &str) {
    let Some(position) = err.statement_position() else {
        return;
    };
    assert!(position >= 1, "positions count from one, got {position}");
    assert!(
        position as usize <= sql.chars().count() + 1,
        "position {position} is past the end of a {}-character statement",
        sql.chars().count()
    );
}

/// A schema that is not there lists no relations, rather than failing.
///
/// The shared contract asks this of a missing *relation*; a missing schema is
/// the level above, and a navigator refreshing a tree that is one edit out of
/// date reaches both.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn a_schema_that_is_not_there_lists_no_relations() {
    let driver = source().await;
    assert!(driver.relations("no_such_schema").await.unwrap().is_empty());
}

/// Fifty thousand rows off a table with no key of any kind, in pages.
///
/// This is the requirement the whole cursor design exists for. `OFFSET`/`FETCH`
/// cannot do it: Microsoft documents stable paging as needing one transaction
/// *and* an `ORDER BY` over columns guaranteed unique, and on a heap there is no
/// such column — the `ORDER BY (SELECT NULL)` everybody writes guarantees
/// nothing, so page two may repeat rows from page one and skip others entirely.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn pages_a_heap_without_repeating_or_skipping() {
    let driver = source().await;
    let mut cursor = driver
        .cursor("SELECT detail FROM sales.event_log", 500)
        .await
        .expect("cursor failed");

    let mut seen: HashSet<String> = HashSet::with_capacity(50_000);
    let mut total = 0usize;
    while let Some(batch) = cursor.fetch().await.expect("fetch failed") {
        let column = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("detail is text");
        for i in 0..column.len() {
            total += 1;
            seen.insert(column.value(i).to_string());
        }
    }
    assert_eq!(total, 50_000, "no row was skipped");
    assert_eq!(seen.len(), 50_000, "no row arrived twice");
}

/// A cancelled statement is told apart from one that broke.
///
/// The two halves are checked in one test on purpose: the classification is a
/// conjunction, and checking only the cancelled half would pass for a driver
/// that called every failure a cancellation.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn a_cancelled_statement_is_not_a_fault_and_a_fault_is_not_a_cancellation() {
    let driver = source().await;

    let slow = async {
        let mut stream = driver.query("WAITFOR DELAY '00:00:30'", 10).await?;
        stream.next_batch().await
    };
    let stop = async {
        // Long enough for the statement to have reached the server and started
        // waiting, short enough to be well inside the thirty seconds.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        driver.cancel().await
    };
    let (outcome, cancelled) = tokio::join!(slow, stop);
    cancelled.expect("the cancel itself should be delivered");
    let err = outcome.expect_err("a killed session cannot finish its statement");
    assert!(
        err.is_cancelled(),
        "a killed statement has to read as cancelled, not as a fault: {err}"
    );

    // The other half, on a session that was never killed.
    let broken = failure(&driver, BROKEN).await;
    assert!(
        !broken.is_cancelled(),
        "a syntax error is not somebody pressing Cancel: {broken}"
    );
}

/// A cancel ends the connection statements share, and the next statement gets
/// another one.
///
/// Statements run on one connection so that a transaction can span them, and
/// this driver cancels by killing that connection: what is left is a socket that
/// will never answer again. Without replacing it, one Cancel would end not just
/// the statement but every statement after it — which is the failure this
/// arrangement could plausibly have and the one worth pinning.
///
/// What does not survive is the transaction, and that is checked too rather than
/// left to be discovered. The server rolled it back when it ended the session,
/// so the honest thing is to come back with a connection that says so.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn a_cancel_that_ends_the_session_leaves_the_next_statement_a_live_one() {
    let driver = source().await;
    driver
        .transaction(&TxStep::Begin)
        .await
        .expect("could not begin");

    let slow = async {
        let mut stream = driver.query("WAITFOR DELAY '00:00:30'", 10).await?;
        stream.next_batch().await
    };
    let stop = async {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        driver.cancel().await
    };
    let (outcome, cancelled) = tokio::join!(slow, stop);
    cancelled.expect("the cancel itself should be delivered");
    assert!(
        outcome
            .expect_err("a killed session cannot finish its statement")
            .is_cancelled()
    );

    let mut stream = driver
        .query(READ, 10)
        .await
        .expect("the statement after a cancel needs a connection that is alive");
    let batch = stream.next_batch().await.unwrap().expect("rows");
    assert_eq!(batch.num_rows(), 10);
    // Let go of before asking for anything else, and not as a tidiness: the
    // session carries one statement at a time, so a result still being held is
    // one the next statement would wait behind for as long as it was held.
    drop(stream);

    let mut open = driver.query("SELECT @@TRANCOUNT", 1).await.unwrap();
    let batch = open.next_batch().await.unwrap().expect("a count");
    assert_eq!(
        ints(&batch).next(),
        Some(0),
        "the transaction went with the session the KILL ended"
    );
}

/// The line the server named, converted into a place in the text we sent.
///
/// SQL Server reports a line number rather than a character offset, which is
/// less than the trait asks for. The non-ASCII identifier on the first line is
/// the point: the offset is counted in characters, and a driver that counted
/// bytes would put the caret four characters past where it belongs.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn reports_which_line_of_a_statement_is_wrong() {
    let driver = source().await;
    let sql = "SELECT id AS 客戶編號\nFROM sales.nums\nWHERE ORDER BY id";
    let err = failure(&driver, sql).await;
    lands_inside(&err, sql);
    let position = err
        .statement_position()
        .expect("a statement of several lines can be located to one of them");
    // Line three begins after "SELECT id AS 客戶編號" (17 characters, 25 bytes)
    // and "FROM sales.nums" (15), each followed by a newline. Counted in bytes
    // this would come out as 43, which is past the end of the third line.
    assert_eq!(position, 1 + 17 + 1 + 15 + 1);
    assert!(!err.is_cancelled());
}

/// The types this database is actually used for, read back as themselves.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn the_types_this_database_is_used_for_arrive_as_themselves() {
    let driver = source().await;
    let mut stream = driver
        .query(
            "SELECT customer_id, ext_id, name, code, credit_limit, balance, petty, tier, \
                    active, created_at, created_tz, legacy_ts, coarse_ts, born, opens_at, \
                    avatar, doc, row_ver, display_name \
             FROM sales.customer ORDER BY customer_id",
            10,
        )
        .await
        .expect("query failed");

    let schema = stream.schema();
    let field = |name: &str| schema.field_with_name(name).unwrap().data_type().clone();
    assert_eq!(field("customer_id"), DataType::Int32);
    // A uniqueidentifier renders as the text a server-side CAST would produce.
    assert_eq!(field("ext_id"), DataType::Utf8);
    assert_eq!(field("name"), DataType::Utf8);
    assert_eq!(field("code"), DataType::Utf8);
    // The declared precision, which tiberius does not carry and the server was
    // asked for separately.
    assert_eq!(field("credit_limit"), DataType::Decimal128(18, 4));
    assert_eq!(field("balance"), DataType::Decimal128(19, 4));
    assert_eq!(field("petty"), DataType::Decimal128(19, 4));
    // tinyint is unsigned in SQL Server; Int16 holds it without loss and is a
    // type the front end's Arrow reader knows.
    assert_eq!(field("tier"), DataType::Int16);
    assert_eq!(field("active"), DataType::Boolean);
    assert_eq!(
        field("created_at"),
        DataType::Timestamp(TimeUnit::Microsecond, None)
    );
    assert_eq!(
        field("created_tz"),
        DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
    );
    assert_eq!(
        field("legacy_ts"),
        DataType::Timestamp(TimeUnit::Microsecond, None)
    );
    assert_eq!(
        field("coarse_ts"),
        DataType::Timestamp(TimeUnit::Microsecond, None)
    );
    assert_eq!(field("born"), DataType::Date32);
    assert_eq!(field("opens_at"), DataType::Time64(TimeUnit::Microsecond));
    assert_eq!(field("avatar"), DataType::Binary);
    assert_eq!(field("doc"), DataType::Utf8);
    // rowversion is binary(8) and a row-change counter. Its catalog name is
    // `timestamp`, which it is not, and rendering it as one would invent a time
    // nobody recorded.
    assert_eq!(field("row_ver"), DataType::Binary);
    assert_eq!(field("display_name"), DataType::Utf8);

    let batch = stream.next_batch().await.unwrap().expect("three rows");
    assert_eq!(batch.num_rows(), 3);
    let column = |name: &str| batch.column(schema.index_of(name).unwrap()).clone();

    let ids = column("customer_id");
    let ids = ids.as_any().downcast_ref::<Int32Array>().unwrap();
    assert_eq!(ids.values(), &[1, 2, 3]);

    let names = column("name");
    let names = names.as_any().downcast_ref::<StringArray>().unwrap();
    assert_eq!(names.value(1), "王小明");

    // A NULL in a nullable column is a NULL and not an empty string.
    let codes = column("code");
    let codes = codes.as_any().downcast_ref::<StringArray>().unwrap();
    assert!(codes.is_null(1));

    let limits = column("credit_limit");
    let limits = limits.as_any().downcast_ref::<Decimal128Array>().unwrap();
    assert_eq!(limits.value(0), 12_345_678);
    // decimal(18,4) at full magnitude, which is what proves the i128 path:
    // `rust_decimal` has 96 bits of mantissa and could not hold this.
    assert_eq!(limits.value(2), 999_999_999_999_999_999);

    let balance = column("balance");
    let balance = balance.as_any().downcast_ref::<Decimal128Array>().unwrap();
    assert_eq!(balance.value(0), 999_999, "99.9999 at scale 4");
    assert_eq!(balance.value(1), -1, "-0.0001 at scale 4");
    // money's documented maximum, one ten-thousandth out. tiberius decoded it
    // through an f64 before this driver saw a byte, and no arithmetic here can
    // put back a bit that was dropped in the decoder.
    assert_eq!(
        balance.value(2),
        9_223_372_036_854_775_808,
        "the stored value is ...5807; this is what survives tiberius' f64"
    );

    // smallmoney's whole range fits inside the f64, so it is exact everywhere.
    let petty = column("petty");
    let petty = petty.as_any().downcast_ref::<Decimal128Array>().unwrap();
    assert_eq!(petty.value(2), 2_147_483_647, "214748.3647 at scale 4");

    let tiers = column("tier");
    let tiers = tiers.as_any().downcast_ref::<Int16Array>().unwrap();
    assert_eq!(tiers.values(), &[3, 1, 5]);

    let active = column("active");
    let active = active.as_any().downcast_ref::<BooleanArray>().unwrap();
    assert!(active.value(0));

    // 1815-12-10 and 0001-01-01, the earliest date the type has. The second is
    // what pins the epoch constant, which differs from the PostgreSQL driver's
    // by one.
    let born = column("born");
    let born = born.as_any().downcast_ref::<Date32Array>().unwrap();
    assert_eq!(born.value(0), -56_270);
    assert_eq!(born.value(2), -719_162);

    // 09:30:00.123 and 23:59:59.999, both at scale 3 and both exact.
    let opens = column("opens_at");
    let opens = opens
        .as_any()
        .downcast_ref::<Time64MicrosecondArray>()
        .unwrap();
    assert_eq!(opens.value(0), 34_200_123_000);
    assert_eq!(opens.value(2), 86_399_999_000);

    // 1999-12-31 23:59:59.997 as `datetime` stores it: 25 919 999 ticks of a
    // three-hundredth of a second, whose true value is .996667 seconds.
    let legacy = column("legacy_ts");
    let legacy = legacy
        .as_any()
        .downcast_ref::<TimestampMicrosecondArray>()
        .unwrap();
    assert_eq!(legacy.value(0), 946_684_799_996_667);

    // 2024-01-01 09:00:00.1234567 +09:00 is midnight UTC, to the microsecond
    // Arrow can hold; the seventh digit is truncated and the offset is dropped.
    let tz = column("created_tz");
    let tz = tz
        .as_any()
        .downcast_ref::<TimestampMicrosecondArray>()
        .unwrap();
    assert_eq!(tz.value(0), 1_704_067_200_123_456);
    // From the other side of the world: the last instant of June at -05:00 is
    // just before five in the morning, UTC, on the first of July. The wall-clock
    // reading the user stored is not recoverable from the column — Arrow has one
    // timezone for the whole of it — which is the loss this mapping accepts.
    assert_eq!(tz.value(1), 1_719_809_999_999_999);

    let avatar = column("avatar");
    let avatar = avatar.as_any().downcast_ref::<BinaryArray>().unwrap();
    assert_eq!(avatar.value(0), &[0xDE, 0xAD, 0xBE, 0xEF]);
    assert!(avatar.is_null(1));

    let doc = column("doc");
    let doc = doc.as_any().downcast_ref::<StringArray>().unwrap();
    assert_eq!(doc.value(0), "<a x=\"1\"/>");

    // A computed column is read like any other.
    let display = column("display_name");
    let display = display.as_any().downcast_ref::<StringArray>().unwrap();
    assert_eq!(display.value(1), "王小明 <>");

    // A GUID reads back as the text a server-side cast would produce: SQL Server
    // stores the first three groups little-endian and RFC 4122 is big-endian, so
    // a client that did not swap them would print a different identifier.
    let ext = column("ext_id");
    let ext = ext.as_any().downcast_ref::<StringArray>().unwrap();
    let mut stated = driver
        .query(
            "SELECT LOWER(CAST(ext_id AS varchar(36))) FROM sales.customer ORDER BY customer_id",
            10,
        )
        .await
        .unwrap();
    let stated = stated.next_batch().await.unwrap().unwrap();
    let stated = stated
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(ext.value(0), stated.value(0));
}

/// A batch the server will not describe still runs, and its decimals fall back.
///
/// Building a temp table and selecting from it is ordinary SQL Server work, and
/// the server cannot describe such a batch before it runs — it answers with
/// error 11525 instead. Refusing everything undescribable would break far more
/// than the guard protects, so it is allowed through. What is lost with the
/// description is the declared precision of a decimal column, which tiberius
/// does not carry either, so the column arrives in the normalized layout with
/// every value rescaled into it.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn a_batch_the_server_will_not_describe_is_still_run() {
    let driver = source().await;
    let mut stream = driver
        .query(
            "CREATE TABLE #t (a int, b decimal(9,2)); \
             INSERT INTO #t VALUES (1, 2.50), (2, -3.75); \
             SELECT a, b FROM #t ORDER BY a;",
            10,
        )
        .await
        .expect("an undescribable batch is still a batch");

    let schema = stream.schema();
    assert_eq!(schema.field(0).data_type(), &DataType::Int32);
    assert_eq!(
        schema.field(1).data_type(),
        &DataType::Decimal128(38, 10),
        "with no description there is no declared scale to use"
    );

    let batch = stream.next_batch().await.unwrap().expect("two rows");
    let b = batch.column(1);
    let b = b.as_any().downcast_ref::<Decimal128Array>().unwrap();
    // 2.50 and -3.75, rescaled from the wire's scale of 2 into the column's 10.
    assert_eq!(b.value(0), 25_000_000_000);
    assert_eq!(b.value(1), -37_500_000_000);
}

/// Browsing a table with a `geography`, a `hierarchyid` and a `sql_variant` in
/// it returns rows.
///
/// This is the test that matters. Before the patched client in
/// `third_party/tiberius`, this statement did not fail — it aborted the process,
/// because tiberius walked into `todo!()` while parsing the column metadata and
/// this workspace builds release with `panic = "abort"`. There was no error to
/// catch and no message to show. `SELECT *` is deliberate: it is what a person
/// double-clicking a table in a navigator sends, and it names none of the types
/// it is about to hit.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn a_table_with_the_clr_types_in_it_can_simply_be_browsed() {
    let driver = source().await;
    let batches = drain(&driver, "SELECT * FROM sales.exotic ORDER BY id").await;
    let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 9, "every row of the fixture came back");

    // Every one of the four is a text column, which is what makes them
    // sortable, filterable and copyable like any other.
    let schema = batches[0].schema();
    for column in ["node", "shape", "place", "anything"] {
        let field = schema.field_with_name(column).expect("column is present");
        assert_eq!(
            field.data_type(),
            &DataType::Utf8,
            "{column} should arrive as text"
        );
    }

    // And the session is still usable afterwards, which it would not be if the
    // decoder had lost its place in the token stream.
    driver.schemas().await.expect("the session survived");
}

/// What the three CLR types show is what SQL Server itself calls them.
///
/// Asserted against the server's own `.ToString()` rather than against strings
/// written here, because that is the claim being made: a cell in the grid says
/// the same thing as the same value does in SSMS, in a hand-written query, and
/// in Microsoft's documentation. A literal expectation would only prove this
/// decoder agrees with whoever typed the literal.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn the_clr_types_read_back_as_the_text_the_server_gives_them() {
    let driver = source().await;

    // The server's answer, through columns that are already nvarchar, so
    // nothing under test is involved in producing it.
    let expected = drain(
        &driver,
        "SELECT node.ToString(), shape.ToString(), place.ToString() \
         FROM sales.exotic ORDER BY id",
    )
    .await;
    let ours = drain(
        &driver,
        "SELECT node, shape, place FROM sales.exotic ORDER BY id",
    )
    .await;

    for (column, name) in [(0, "hierarchyid"), (1, "geometry"), (2, "geography")] {
        let expected = text_column(&expected, column);
        let ours = text_column(&ours, column);
        assert_eq!(
            ours, expected,
            "{name} does not read back as the server's own text"
        );
    }

    // Spelled out for the two forms somebody is most likely to meet, so that a
    // change to both sides at once still has to explain itself.
    let node = text_column(&ours, 0);
    assert_eq!(node[0].as_deref(), Some("/1/2/"));
    // The specification's own worked example: a negative label, a dotted level,
    // and a level whose value carries antiambiguity bits.
    assert_eq!(node[2].as_deref(), Some("/1/-2.18/"));
    // The root is encoded as no bytes at all.
    assert_eq!(node[3].as_deref(), Some("/"));
    // Stored latitude first, written longitude first.
    assert_eq!(
        text_column(&ours, 2)[0].as_deref(),
        Some("POINT (-122.3 47.6)")
    );
    // A null CLR value is a null cell and not the word "null".
    assert_eq!(node[1], None);
}

/// A `sql_variant` shows the value that is actually in it, whatever type that
/// is.
///
/// The column has no one type — row 1 holds an `int` and row 8 a `varchar` — so
/// the decoder reads the base type out of each value's own header. Getting that
/// wrong shows a plausible number for a completely different value, which is why
/// each base type is pinned rather than sampled.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn a_sql_variant_shows_whichever_type_the_value_actually_is() {
    let driver = source().await;
    let rows = drain(&driver, "SELECT anything FROM sales.exotic ORDER BY id").await;
    let values = text_column(&rows, 0);

    assert_eq!(values[0].as_deref(), Some("42"), "int");
    assert_eq!(values[1].as_deref(), Some("a string"), "nvarchar");
    assert_eq!(values[2].as_deref(), Some("12345.6789"), "decimal(18,4)");
    assert_eq!(
        values[3].as_deref(),
        Some("2024-01-15 09:30:00.123"),
        "datetime2(3)"
    );
    assert_eq!(values[4].as_deref(), Some("0x0A0B"), "varbinary");
    assert_eq!(values[5].as_deref(), Some("1"), "bit");
    assert_eq!(
        values[6].as_deref(),
        Some("11111111-2222-3333-4444-555555555555"),
        "uniqueidentifier"
    );
    // A codepage string rather than UTF-16, decoded through the collation the
    // value carries with it.
    assert_eq!(values[7].as_deref(), Some("ascii only"), "varchar");
    // A null variant states no base type at all, so there is nothing that could
    // have been rendered instead of a null.
    assert_eq!(values[8], None, "null");
}

/// Every batch a statement produces, read to the end.
async fn drain(driver: &MsSqlSource, sql: &str) -> Vec<arrow::array::RecordBatch> {
    let mut stream = driver
        .query(sql, 100)
        .await
        .unwrap_or_else(|e| panic!("{sql}\nfailed: {e}"));
    let mut batches = Vec::new();
    while let Some(batch) = stream
        .next_batch()
        .await
        .unwrap_or_else(|e| panic!("{sql}\nfailed: {e}"))
    {
        batches.push(batch);
    }
    batches
}

/// Every value of a text column in a set of batches, in order.
fn text_column(batches: &[arrow::array::RecordBatch], column: usize) -> Vec<Option<String>> {
    batches
        .iter()
        .flat_map(|batch| {
            let a = batch.column(column);
            let a = a
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("a text column");
            (0..a.len())
                .map(|i| a.is_valid(i).then(|| a.value(i).to_string()))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn a_statement_that_writes_reports_the_rows_it_changed() {
    let driver = source().await;
    // Left as it was: the number is what the statement matched, and the fixture
    // has to survive the test.
    let mut stream = driver
        .query("UPDATE sales.nums SET label = label WHERE id <= 7", 10)
        .await
        .expect("update failed");
    assert!(stream.schema().fields().is_empty(), "no result set");
    assert_eq!(stream.next_batch().await.unwrap(), None);
    assert_eq!(
        stream.rows_affected(),
        Some(7),
        "rows changed, not the zero rows returned"
    );
}

#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn lists_every_schema_including_an_empty_one() {
    let driver = source().await;
    let names: Vec<String> = driver
        .schemas()
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(names.contains(&"sales".to_string()));
    // A schema somebody created a moment ago and has not put anything in yet.
    // Upstream hides this one, by listing only schemas that contain something.
    assert!(names.contains(&"archive".to_string()));
    assert!(names.contains(&"dbo".to_string()));
    // The fixed database-role schemas are noise in a navigator.
    assert!(!names.contains(&"db_owner".to_string()));
    assert!(!names.contains(&"sys".to_string()));
    assert!(!names.contains(&"INFORMATION_SCHEMA".to_string()));
}

#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn describes_a_table_and_the_view_over_it() {
    let driver = source().await;
    let relations = driver.relations("sales").await.unwrap();
    let table = relations.iter().find(|r| r.name == "customer").unwrap();
    assert_eq!(table.kind, RelationKind::Table);
    // Documented as approximate with no condition under which it is exact, so
    // this only checks it is in the right neighbourhood.
    let log = relations.iter().find(|r| r.name == "event_log").unwrap();
    assert_eq!(log.estimated_rows, Some(50_000));

    let view = relations
        .iter()
        .find(|r| r.name == "active_customer")
        .unwrap();
    assert_eq!(view.kind, RelationKind::View);
    // A view is not a table with zero rows: nothing counted it, and saying zero
    // would report a full view as empty.
    assert_eq!(view.estimated_rows, None);

    // The text as it was typed, `CREATE VIEW` header and all — SQLite's
    // behaviour rather than PostgreSQL's, which re-renders from a parse tree.
    let definition = driver
        .definition("sales", "active_customer")
        .await
        .unwrap()
        .expect("a view has a definition");
    assert!(definition.contains("CREATE VIEW"), "{definition}");
    assert!(definition.contains("WHERE active = 1"), "{definition}");
}

#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn states_a_column_the_way_the_database_states_it() {
    let driver = source().await;
    let columns = driver.columns("sales", "customer").await.unwrap();
    let by_name = |name: &str| {
        columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no column {name}"))
            .clone()
    };

    // max_length is in bytes, so an nvarchar(100) reports 200 in the catalog.
    assert_eq!(by_name("name").data_type, "nvarchar(100)");
    assert_eq!(by_name("code").data_type, "varchar(16)");
    assert_eq!(by_name("notes").data_type, "nvarchar(max)");
    assert_eq!(by_name("credit_limit").data_type, "decimal(18,4)");
    assert_eq!(by_name("created_at").data_type, "datetime2(7)");
    assert_eq!(by_name("created_tz").data_type, "datetimeoffset(7)");
    assert_eq!(by_name("opens_at").data_type, "time(3)");
    assert_eq!(by_name("avatar").data_type, "varbinary(max)");
    assert_eq!(by_name("balance").data_type, "money");
    assert_eq!(by_name("tier").data_type, "tinyint");
    // rowversion is spelled `timestamp` by the catalog, which is what the
    // structure pane should show even though the Arrow column is binary.
    assert_eq!(by_name("row_ver").data_type, "timestamp");

    assert!(by_name("customer_id").is_primary_key);
    assert!(!by_name("name").is_primary_key);
    assert!(!by_name("name").nullable);
    assert!(by_name("code").nullable);
    assert_eq!(by_name("tier").default_value.as_deref(), Some("((1))"));
    assert_eq!(by_name("tier").computed, None);
    // A computed column has no default, so the expression goes where the
    // default would have been — the shared shape has nowhere else for it, and
    // showing nothing would hide the only interesting thing about the column.
    // What says which of the two is there is the flag beside it, and without it
    // the DDL for this column is a default the server would refuse.
    let display_name = by_name("display_name");
    assert!(
        display_name
            .default_value
            .as_deref()
            .unwrap_or_default()
            .contains("isnull"),
        "{:?}",
        display_name.default_value
    );
    assert_eq!(display_name.computed, Some(Computed::Virtual));
    // And `sys.computed_columns.is_persisted` is the whole of the difference
    // between this one and the last: the server stores this column's value with
    // the row, which is the `PERSISTED` its DDL has to say.
    let tier_doubled = by_name("tier_doubled");
    assert_eq!(
        tier_doubled.default_value.as_deref(),
        Some("([tier]*(2))"),
        "{tier_doubled:?}"
    );
    assert_eq!(tier_doubled.computed, Some(Computed::Stored));
}

#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn reports_indexes_by_the_keys_they_can_be_seeked_on() {
    let driver = source().await;
    let indexes = driver.indexes("sales", "customer").await.unwrap();
    let by_name = |name: &str| {
        indexes
            .iter()
            .find(|i| i.name == name)
            .unwrap_or_else(|| panic!("no index {name}"))
            .clone()
    };

    let pk = by_name("pk_customer");
    assert!(pk.is_primary && pk.is_unique);
    assert_eq!(pk.columns, vec!["customer_id"]);
    // The server's own word, rather than a table of our own that would have to
    // keep up with a new index kind.
    assert_eq!(pk.method, "CLUSTERED");
    assert_eq!(
        indexes.first().unwrap().name,
        "pk_customer",
        "primary first"
    );

    let named = by_name("ix_customer_name");
    // Descending is part of what the key is: an index on (name DESC) does not
    // serve an ascending scan.
    assert_eq!(named.columns, vec!["name DESC"]);
    // `code` and `tier` are INCLUDE columns. They are payload, not keys, and
    // listing them here would say the planner can seek on them.
    assert!(!named.is_unique);
    assert_eq!(named.method, "NONCLUSTERED");

    let filtered = by_name("ix_customer_active");
    assert_eq!(filtered.predicate.as_deref(), Some("([active]=(1))"));

    // A composite primary key, in key order rather than in column order.
    let lines = driver.indexes("sales", "order_line").await.unwrap();
    assert_eq!(lines[0].columns, vec!["order_id", "line_no"]);
}

/// Unique keys are reported as the columns the server keeps unique — not the
/// payload beside them, and not the ones it keeps unique only sometimes.
///
/// SQL Server is where the two lists differ most, so the shapes that differ get
/// a table of their own rather than a place in the shared fixture: `ux_probe_ref`
/// is unique over `ref` and carries `payload` as an `INCLUDE`, which the server
/// stores in the index without enforcing anything about — putting it in the key
/// would add a condition to every `WHERE` that has no business being there. And
/// `ux_probe_code` is unique over the rows its filter admits and silent about
/// the rest, so two rows with no code can both exist and neither is named by
/// `code = …`.
///
/// Created and dropped here because the fixture above is built once and skipped
/// whenever the database already has it, so a shape added to it would not reach
/// a server that has been up since before this check existed.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn reports_unique_keys_without_their_payload_or_their_filter() {
    let driver = source().await;
    let mut db = raw("dbeaver_test").await;
    // The session settings a filtered index refuses to be created without, as
    // the fixture sets them for the same reason.
    run(&mut db, "SET QUOTED_IDENTIFIER ON; SET ANSI_NULLS ON;").await;
    run(&mut db, "DROP TABLE IF EXISTS sales.unique_probe").await;
    run(
        &mut db,
        "CREATE TABLE sales.unique_probe (
             id      int          NOT NULL CONSTRAINT pk_probe PRIMARY KEY,
             ext     int          NOT NULL CONSTRAINT uq_probe_ext UNIQUE,
             [ref]   nvarchar(40) NOT NULL,
             code    nvarchar(40)     NULL,
             payload nvarchar(40)     NULL)",
    )
    .await;
    run(
        &mut db,
        "CREATE UNIQUE INDEX ux_probe_ref ON sales.unique_probe ([ref]) INCLUDE (payload)",
    )
    .await;
    run(
        &mut db,
        "CREATE UNIQUE INDEX ux_probe_code ON sales.unique_probe (code) WHERE code IS NOT NULL",
    )
    .await;

    let keys = driver.unique_keys("sales", "unique_probe").await.unwrap();
    let named: Vec<(&str, &[String])> = keys
        .iter()
        .map(|k| (k.name.as_str(), k.columns.as_slice()))
        .collect();
    assert_eq!(
        named,
        [
            ("uq_probe_ext", ["ext"].map(String::from).as_slice()),
            ("ux_probe_ref", ["ref"].map(String::from).as_slice()),
        ],
        "got {named:?}"
    );

    // The primary key is `ColumnInfo::is_primary_key`'s to report; a heap with
    // no key of any kind answers with an empty list rather than failing.
    assert!(
        driver
            .unique_keys("sales", "order_line")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        driver
            .unique_keys("sales", "event_log")
            .await
            .unwrap()
            .is_empty()
    );

    run(&mut db, "DROP TABLE sales.unique_probe").await;
}

#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn reports_a_foreign_key_from_both_ends() {
    let driver = source().await;

    let outbound = driver.foreign_keys("sales", "order").await.unwrap();
    let fk = outbound
        .iter()
        .find(|k| k.name == "fk_order_customer")
        .expect("the order table declares one");
    assert_eq!(fk.local_columns, vec!["customer_id"]);
    assert_eq!(fk.other_schema, "sales");
    assert_eq!(fk.other_table, "customer");
    assert_eq!(fk.other_columns, vec!["customer_id"]);
    assert_eq!(fk.on_delete, "CASCADE");
    assert_eq!(fk.on_update, "NO ACTION");

    // The same constraint seen from the other side, with every field named for
    // the relation that was asked about.
    let inbound = driver.referenced_by("sales", "customer").await.unwrap();
    let seen = inbound
        .iter()
        .find(|k| k.name == "fk_order_customer")
        .expect("customer is referenced by order");
    assert_eq!(seen.local_columns, vec!["customer_id"]);
    assert_eq!(seen.other_table, "order");
    assert_eq!(seen.other_columns, vec!["customer_id"]);

    // A table nobody points at.
    assert!(
        driver
            .referenced_by("sales", "event_log")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        driver
            .foreign_keys("sales", "event_log")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn reports_check_and_unique_constraints_and_leaves_the_keys_alone() {
    let driver = source().await;
    let constraints = driver.constraints("sales", "customer").await.unwrap();

    let check = constraints
        .iter()
        .find(|c| c.name == "ck_customer_tier")
        .expect("the check constraint");
    assert_eq!(check.kind, ConstraintKind::Check);
    // The server's own rendering. Rebuilding it from catalog columns would mean
    // reimplementing expression formatting.
    assert!(check.definition.contains("tier"), "{}", check.definition);

    let unique = constraints
        .iter()
        .find(|c| c.name == "uq_customer_ext")
        .expect("the unique constraint");
    assert_eq!(unique.kind, ConstraintKind::Unique);
    assert_eq!(unique.definition, "UNIQUE (ext_id)");

    // The primary key has its own section and is not repeated here.
    assert!(!constraints.iter().any(|c| c.name == "pk_customer"));
    // The foreign key likewise.
    assert!(
        driver
            .constraints("sales", "order")
            .await
            .unwrap()
            .iter()
            .all(|c| c.name != "fk_order_customer")
    );
    // A table with none of them.
    assert!(
        driver
            .constraints("sales", "event_log")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn reports_a_trigger_and_whether_it_will_fire() {
    let driver = source().await;
    let triggers = driver.triggers("sales", "customer").await.unwrap();
    let audit = triggers
        .iter()
        .find(|t| t.name == "trg_customer_audit")
        .expect("the audit trigger");
    // SQL Server has no BEFORE trigger.
    assert_eq!(audit.timing.as_deref(), Some("AFTER"));
    let mut events = audit.events.clone();
    events.sort();
    assert_eq!(events, vec!["INSERT", "UPDATE"]);
    // And no row-level DML trigger: `inserted` and `deleted` are set-valued and
    // the body runs once per statement however many rows it touched.
    assert_eq!(audit.level.as_deref(), Some("STATEMENT"));
    // A trigger's body is inline here, so there is no function to name.
    assert_eq!(audit.function, None);
    // Disabled by the fixture. Listing a disabled trigger as though it fires is
    // worse than not listing it.
    assert!(!audit.enabled);
    assert!(
        audit
            .definition
            .as_deref()
            .unwrap_or_default()
            .contains("CREATE TRIGGER")
    );

    let guard = driver.triggers("sales", "order").await.unwrap();
    assert_eq!(guard[0].timing.as_deref(), Some("INSTEAD OF"));
    assert_eq!(guard[0].events, vec!["INSERT"]);
    assert!(guard[0].enabled);

    // A table with no trigger, and one whose only triggers would be the
    // constraint machinery if we did not exclude what the server ships.
    assert!(
        driver
            .triggers("sales", "event_log")
            .await
            .unwrap()
            .is_empty()
    );
}

/// The instance has more databases than this connection can reach, and says so.
///
/// A client that showed only the one database as though it were the server would
/// be hiding a real limitation. The list is offered so a front end can open
/// another connection, not so it can expand a node through this one:
/// cross-database three-part names work on a box-product server and do not work
/// on Azure SQL Database.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn names_the_databases_this_connection_cannot_reach() {
    let driver = source().await;
    assert_eq!(driver.database(), "dbeaver_test");

    let databases = driver.databases().await.unwrap();
    let names: Vec<&str> = databases.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"dbeaver_test"));
    assert!(names.contains(&"master"));
    let this = databases.iter().find(|d| d.name == "dbeaver_test").unwrap();
    assert_eq!(this.state, "ONLINE");
    assert!(this.collation.is_some());

    // And the schemas this connection answers with are this database's, not the
    // instance's: `master` has a `dbo` too, and there is no way to tell them
    // apart from a bare schema name.
    let schemas = driver.schemas().await.unwrap();
    assert!(schemas.iter().any(|s| s.name == "sales"));
}

/// Rows of the first result set, as `i32`.
fn ints(batch: &arrow::array::RecordBatch) -> impl Iterator<Item = i32> + '_ {
    let column = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int32Array>()
        .expect("id is a 32-bit integer");
    (0..column.len()).map(|i| column.value(i))
}
