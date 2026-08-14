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
//! The first six checks are `crates/conn/tests/contract.rs` written out again
//! against this driver. They are duplicated rather than shared because a driver
//! is registered in that file centrally, after this branch merges; when that
//! happens these can go.

use arrow::array::{
    Array, BinaryArray, BooleanArray, Date32Array, Decimal128Array, Int16Array, Int32Array,
    StringArray, Time64MicrosecondArray, TimestampMicrosecondArray,
};
use arrow::datatypes::{DataType, TimeUnit};
use dbconn::{ConstraintKind, DbError, Driver, RelationKind};
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

async fn build_fixture() {
    let mut master = raw("master").await;
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
    // The four types that would take the process down, kept in one table so the
    // rest of the fixture stays readable.
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
// The contract, checked through the trait
// ---------------------------------------------------------------------------

const READ: &str = "SELECT id FROM sales.nums ORDER BY id";
/// Broken in the middle rather than truncated, so the failure is a parse error
/// with something after it rather than an unexpected end of input.
const BROKEN: &str = "SELECT id FROM sales.nums WHERE ORDER BY id";
const MISSING: &str = "SELECT * FROM no_such_relation_anywhere";

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

#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn reads_a_result_in_batches() {
    let driver = source().await;
    let mut stream = driver.query(READ, 100).await.expect("query failed");

    // Before a single row has been read: a front end lays out a grid first and
    // asks for rows afterwards.
    assert_eq!(stream.schema().fields().len(), 1);
    // Zero is a real answer, so "not finished" cannot be zero.
    assert_eq!(stream.rows_affected(), None);

    let first = stream.next_batch().await.unwrap().expect("a first batch");
    assert_eq!(first.num_rows(), 100);
    let second = stream.next_batch().await.unwrap().expect("a second batch");
    assert_eq!(second.num_rows(), 100);

    // In order and once each, all the way to the end.
    let mut seen: Vec<i32> = ints(&first).chain(ints(&second)).collect();
    while let Some(batch) = stream.next_batch().await.unwrap() {
        seen.extend(ints(&batch));
    }
    assert_eq!(seen.len(), 500);
    assert_eq!(seen, (1..=500).collect::<Vec<i32>>());
    assert_eq!(stream.rows_affected(), Some(500));
}

#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn pages_a_cursor() {
    let driver = source().await;
    let mut cursor = driver.cursor(READ, 50).await.expect("cursor failed");
    assert_eq!(cursor.schema().fields().len(), 1);

    let mut seen = 0usize;
    for page in 1..=3 {
        let batch = cursor
            .fetch()
            .await
            .expect("fetch error")
            .unwrap_or_else(|| panic!("page {page} is missing"));
        assert_eq!(batch.num_rows(), 50);
        seen += batch.num_rows();
    }
    assert_eq!(seen, 150);

    // Closing is optional but has to work, and has to be safe to call on a
    // cursor with pages left in it.
    cursor.close().await.expect("close failed");
}

#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn cancels_an_idle_cursor_without_complaining() {
    let driver = source().await;
    let cursor = driver.cursor(READ, 10).await.expect("cursor failed");
    cursor.canceller().cancel().await.expect("cancel failed");
    driver.cancel().await.expect("session cancel failed");

    // And the session is still usable afterwards, which is the property that
    // makes `KILL`-based cancellation safe to expose: an idle connection is not
    // a target.
    driver.schemas().await.expect("the session survived");
}

#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn reports_where_a_statement_is_wrong() {
    let driver = source().await;

    let err = failure(&driver, BROKEN).await;
    lands_inside(&err, BROKEN);
    assert!(
        !err.is_cancelled(),
        "a broken statement is not a cancellation"
    );

    let missing = failure(&driver, MISSING).await;
    lands_inside(&missing, MISSING);
    assert!(!missing.is_cancelled());
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

#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn walks_the_navigator() {
    let driver = source().await;
    let (schema, relation) = ("sales", "customer");

    let schemas = driver.schemas().await.expect("schemas failed");
    assert!(schemas.iter().any(|s| s.name == schema));

    let relations = driver.relations(schema).await.expect("relations failed");
    let found = relations
        .iter()
        .find(|r| r.name == relation)
        .expect("customer should be listed under sales");
    assert_eq!(found.schema, schema, "a relation knows where it lives");

    let columns = driver.columns(schema, relation).await.expect("columns");
    assert!(!columns.is_empty());
    for (offset, column) in columns.iter().enumerate() {
        assert_eq!(
            column.position,
            offset as i32 + 1,
            "column {} is out of position",
            column.name
        );
        assert!(!column.data_type.is_empty(), "a column states its own type");
    }
    assert!(columns.iter().any(|c| c.name == "customer_id"));

    // A table is not a view, and the distinction is what the structure pane
    // hangs a section on.
    assert_eq!(driver.definition(schema, relation).await.unwrap(), None);

    driver.indexes(schema, relation).await.expect("indexes");
    driver
        .foreign_keys(schema, relation)
        .await
        .expect("foreign keys");
    driver
        .referenced_by(schema, relation)
        .await
        .expect("inbound references");
    driver
        .constraints(schema, relation)
        .await
        .expect("constraints");
    driver.triggers(schema, relation).await.expect("triggers");
}

#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn answers_for_a_relation_that_is_not_there() {
    let driver = source().await;
    let schema = "sales";
    let missing = "no_such_relation_anywhere";

    assert!(driver.columns(schema, missing).await.unwrap().is_empty());
    assert!(driver.indexes(schema, missing).await.unwrap().is_empty());
    assert!(
        driver
            .foreign_keys(schema, missing)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        driver
            .referenced_by(schema, missing)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        driver
            .constraints(schema, missing)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(driver.triggers(schema, missing).await.unwrap().is_empty());
    assert_eq!(driver.definition(schema, missing).await.unwrap(), None);
    // A schema that is not there is the same kind of answer.
    assert!(driver.relations("no_such_schema").await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// What is particular to this database
// ---------------------------------------------------------------------------

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

/// A `geography` column is refused rather than read, because reading it is fatal.
///
/// The failure asserted here is the specific one the guard produces. That
/// matters: if the guard ever stopped working, tiberius would panic inside the
/// reader task and this driver would report the task having gone away — an error
/// either way, and only one of them means the protection is still there. In a
/// release build the same regression is not an error at all, it is an abort.
#[tokio::test]
#[ignore = "requires a SQL Server"]
async fn a_column_tiberius_cannot_decode_is_refused_before_it_is_read() {
    let driver = source().await;
    for (sql, wanted) in [
        ("SELECT * FROM sales.exotic", "hierarchyid"),
        ("SELECT place FROM sales.exotic", "geography"),
        ("SELECT shape FROM sales.exotic", "geometry"),
        ("SELECT anything FROM sales.exotic", "sql_variant"),
    ] {
        let err = failure(&driver, sql).await;
        let message = err.to_string();
        assert!(
            message.contains(wanted) && message.contains("cast it to text"),
            "expected a refusal naming {wanted}, got: {message}"
        );
    }

    // The columns beside them are perfectly readable, which is what makes
    // refusing the statement rather than the table the right unit.
    let mut stream = driver
        .query("SELECT id FROM sales.exotic ORDER BY id", 10)
        .await
        .expect("the ordinary columns of that table are fine");
    let batch = stream.next_batch().await.unwrap().unwrap();
    assert_eq!(batch.num_rows(), 2);

    // And the session is still usable, which it would not be if the guard were
    // catching a panic rather than preventing one.
    driver.schemas().await.expect("the session survived");
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
    // A computed column has no default, so the expression goes where the
    // default would have been — the shared shape has nowhere else for it, and
    // showing nothing would hide the only interesting thing about the column.
    let computed = by_name("display_name").default_value;
    assert!(
        computed.as_deref().unwrap_or_default().contains("isnull"),
        "{computed:?}"
    );
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
