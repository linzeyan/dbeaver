//! The MySQL driver against a live server.
//!
//! Every test here is `#[ignore]`d, so `cargo test` passes with nothing
//! installed. To run them, start the server they expect:
//!
//! ```text
//! docker run -d --name mysql-test \
//!   -e MYSQL_ROOT_PASSWORD=test -e MYSQL_DATABASE=test \
//!   -p 53306:3306 mysql:8
//!
//! cargo test -p driver-mysql -- --ignored
//! ```
//!
//! The fixture is created by these tests rather than by a seed script, so
//! nothing outside this file has to be run first — and so the fixture cannot
//! drift away from what the assertions expect. It is seeded through
//! `mysql_async` directly and never through the driver, because a fixture built
//! by the code under test proves nothing about it.

use arrow::array::{
    Array, Date32Array, Decimal128Array, Decimal256Array, DurationMicrosecondArray,
    TimestampMicrosecondArray, UInt32Array, UInt64Array,
};
use arrow::datatypes::{DataType, TimeUnit};
use dbconn::{ConstraintKind, RelationKind};
use driver_mysql::MySqlSource;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use std::sync::Arc;
use tokio::sync::OnceCell;

const URL: &str = "mysql://root:test@127.0.0.1:53306/bench";
const ROOT_URL: &str = "mysql://root:test@127.0.0.1:53306/";

/// Rows in `bench_wide`. Large enough that a batch size of 500 produces ten
/// batches and an off-by-one in the batching shows up, small enough to seed in
/// under a second.
const WIDE_ROWS: u32 = 5_000;
/// Rows in `no_key`, the relation with no unique order to page by.
const NO_KEY_ROWS: u32 = 2_000;

/// A statement long enough to still be running a moment after it starts, and one
/// the server will actually abandon when told to.
///
/// Chosen by experiment rather than by taste: `SELECT SLEEP(n)` returns 1 when
/// interrupted instead of failing, and `BENCHMARK()` ignores the kill flag
/// altogether and runs to completion. A join that reads rows checks for the
/// interrupt between them, which is what makes this test about the driver rather
/// than about which builtin happens to be polite.
const SLOW: &str = "SELECT COUNT(*) FROM bench_wide a, bench_wide b, no_key c \
                    WHERE a.name <> b.hash_hex AND c.label <> a.name";

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

static FIXTURE: OnceCell<()> = OnceCell::const_new();

async fn source() -> MySqlSource {
    FIXTURE.get_or_init(seed).await;
    MySqlSource::connect(URL)
        .await
        .expect("MySQL unreachable; see the header of this file for the container")
}

/// The fixture, statement by statement.
///
/// Split rather than sent as one script because multi-statement mode is off by
/// default and turning it on for a test fixture would be turning on an injection
/// surface to save a loop.
async fn seed() {
    let opts = Opts::from_url(ROOT_URL).expect("the fixture URL should parse");
    let mut conn = Conn::new(opts)
        .await
        .expect("MySQL unreachable; see the header of this file for the container");

    for statement in statements() {
        conn.query_drop(&statement)
            .await
            .unwrap_or_else(|e| panic!("seeding failed on {statement}: {e}"));
    }
    conn.disconnect()
        .await
        .expect("closing the seed connection");
}

fn statements() -> Vec<String> {
    let mut out: Vec<String> = [
        "DROP DATABASE IF EXISTS bench2",
        "DROP DATABASE IF EXISTS bench",
        "CREATE DATABASE bench",
        "USE bench",
        // The default is 1000, and the generators below want more.
        "SET SESSION cte_max_recursion_depth = 4000000",
        // The throughput table: a type mix a benchmark over ten INTs would not
        // measure, and the relation the batching and ordering checks read.
        "CREATE TABLE bench_wide (
           id            INT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
           big_val       BIGINT       NOT NULL,
           int_val       INT          NOT NULL,
           num_val       DECIMAL(18,4) NOT NULL,
           real_val      FLOAT        NOT NULL,
           dbl_val       DOUBLE       NOT NULL,
           name          VARCHAR(64)  NOT NULL,
           hash_hex      CHAR(32)     NOT NULL,
           category      ENUM('alpha','beta','gamma','delta') NOT NULL,
           flag          TINYINT(1)   NOT NULL,
           created_at    DATETIME(6)  NOT NULL,
           updated_at    TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
           created_on    DATE         NOT NULL,
           elapsed       TIME(6)      NOT NULL,
           small_val     SMALLINT     NOT NULL,
           nullable_text VARCHAR(64)      NULL,
           json_val      JSON         NOT NULL,
           bytes_val     VARBINARY(64) NOT NULL
         ) ENGINE=InnoDB",
        // Every branch of the type mapping, one row per interesting value
        // rather than a million rows of the same one.
        "CREATE TABLE bench_types (
           id            INT PRIMARY KEY,
           t_tinyint     TINYINT,        t_tinyint_u    TINYINT UNSIGNED,
           t_smallint    SMALLINT,       t_smallint_u   SMALLINT UNSIGNED,
           t_mediumint   MEDIUMINT,      t_mediumint_u  MEDIUMINT UNSIGNED,
           t_int         INT,            t_int_u        INT UNSIGNED,
           t_bigint      BIGINT,         t_bigint_u     BIGINT UNSIGNED,
           t_bool        BOOLEAN,        t_tinyint1     TINYINT(1),
           t_float       FLOAT,          t_double       DOUBLE,
           t_dec_small   DECIMAL(10,2),
           t_dec_128     DECIMAL(38,10),
           t_dec_wide    DECIMAL(65,30),
           t_dec_zero    DECIMAL(12,0),
           t_dec_u       DECIMAL(9,3) UNSIGNED,
           t_date        DATE,
           t_datetime    DATETIME,       t_datetime6    DATETIME(6),
           t_timestamp   TIMESTAMP NULL, t_timestamp6   TIMESTAMP(6) NULL,
           t_time        TIME,           t_time6        TIME(6),
           t_year        YEAR,
           t_bit1        BIT(1),         t_bit17        BIT(17),   t_bit64 BIT(64),
           t_char        CHAR(10),       t_binary       BINARY(8),
           t_varchar     VARCHAR(100),   t_varbinary    VARBINARY(100),
           t_enum        ENUM('red','green','blue'),
           t_set         SET('read','write','execute'),
           t_tinytext    TINYTEXT,       t_text     TEXT,
           t_mediumtext  MEDIUMTEXT,     t_longtext LONGTEXT,
           t_tinyblob    TINYBLOB,       t_blob     BLOB,
           t_mediumblob  MEDIUMBLOB,     t_longblob LONGBLOB,
           t_json        JSON,
           t_geom        GEOMETRY SRID 4326,
           t_gen_virtual BIGINT AS (t_int * 2) VIRTUAL,
           t_gen_stored  BIGINT AS (t_int + 1) STORED
         ) ENGINE=InnoDB",
        // The extremes: every unsigned maximum, both signed limits, the widest
        // decimal MySQL has, and a leap day.
        "INSERT INTO bench_types
           (id, t_tinyint,t_tinyint_u, t_smallint,t_smallint_u, t_mediumint,t_mediumint_u,
            t_int,t_int_u, t_bigint,t_bigint_u, t_bool,t_tinyint1, t_float,t_double,
            t_dec_small,t_dec_128,t_dec_wide,t_dec_zero,t_dec_u,
            t_date,t_datetime,t_datetime6,t_timestamp,t_timestamp6,t_time,t_time6,t_year,
            t_bit1,t_bit17,t_bit64, t_char,t_binary,t_varchar,t_varbinary,
            t_enum,t_set, t_tinytext,t_text,t_mediumtext,t_longtext,
            t_tinyblob,t_blob,t_mediumblob,t_longblob, t_json,t_geom)
         VALUES
           (1, -128,255, -32768,65535, -8388608,16777215,
            -2147483648,4294967295, -9223372036854775808,18446744073709551615,
            TRUE, 7, 1.5, 2.25,
            -12345678.90,
            -1234567890123456789012345678.0123456789,
            -12345678901234567890123456789012345.123456789012345678901234567890,
            123456789012, 123456.789,
            '2024-02-29','2024-02-29 13:45:56','2024-02-29 13:45:56.123456',
            '2024-02-29 13:45:56','2024-02-29 13:45:56.123456',
            '13:45:56','13:45:56.123456', 2024,
            b'1', b'10101010101010101',
            b'1111111111111111111111111111111111111111111111111111111111111111',
            'chars',UNHEX('0001020304050607'),'varchar value',UNHEX('DEADBEEF'),
            'green','read,execute',
            'tiny','text','medium','long',
            UNHEX('00'),UNHEX('0102'),UNHEX('030405'),UNHEX('0607'),
            JSON_OBJECT('a',1,'b',JSON_ARRAY(1,2,3)),
            ST_GeomFromText('POINT(30 10)', 4326))",
        // A NULL in every builder.
        "INSERT INTO bench_types (id) VALUES (2)",
        // `TIME` as the signed interval it is: negative, and past a day. The row
        // that fails if TIME is ever mapped to Time64.
        "INSERT INTO bench_types (id, t_time, t_time6)
         VALUES (3, '-838:59:59', '838:59:58.999999')",
        // The ends of the temporal ranges, including TIMESTAMP's 2038 ceiling.
        "INSERT INTO bench_types (id, t_date, t_datetime, t_timestamp, t_year) VALUES
           (4, '1000-01-01', '1000-01-01 00:00:00', '1970-01-01 00:00:01', 1901),
           (5, '9999-12-31', '9999-12-31 23:59:59', '2038-01-19 03:14:07', 2155)",
        // Zero dates. Legal values with no Arrow representation, and the mode is
        // relaxed for these two statements only so that every read below still
        // runs against a default-configured server.
        "SET SESSION sql_mode = 'NO_ENGINE_SUBSTITUTION'",
        "INSERT INTO bench_types (id, t_date, t_datetime, t_year) VALUES
           (6, '0000-00-00', '0000-00-00 00:00:00', 0000),
           (7, '2010-00-01', '2010-00-01 00:00:00', 0000)",
        "SET SESSION sql_mode = DEFAULT",
        // No primary key and no unique index: the relation LIMIT/OFFSET cannot
        // page and a cursor can.
        "CREATE TABLE no_key (n INT NOT NULL, label VARCHAR(32) NOT NULL) ENGINE=InnoDB",
        // Every index and constraint shape the catalog queries have to render.
        "CREATE TABLE bench_child (
           order_id   INT          NOT NULL,
           line_no    SMALLINT     NOT NULL,
           parent_id  INT UNSIGNED NOT NULL,
           sku        VARCHAR(64)  NOT NULL,
           email      VARCHAR(128)     NULL,
           qty        INT          NOT NULL DEFAULT 1,
           note       TEXT             NULL,
           shipped_at DATETIME         NULL,
           PRIMARY KEY (order_id, line_no),
           UNIQUE KEY bench_child_sku_key (sku),
           KEY bench_child_email_prefix (email(16)),
           KEY bench_child_qty_desc (qty DESC),
           KEY bench_child_email_lower ((LOWER(email))),
           FULLTEXT KEY bench_child_note_ft (note),
           CONSTRAINT bench_child_qty_positive CHECK (qty > 0),
           CONSTRAINT bench_child_line_sane   CHECK (line_no < 1000) NOT ENFORCED,
           CONSTRAINT bench_child_parent_fk FOREIGN KEY (parent_id)
             REFERENCES bench_wide (id) ON DELETE CASCADE ON UPDATE RESTRICT
         ) ENGINE=InnoDB",
        // Two on the same timing and event, so ACTION_ORDER is not always 1.
        "CREATE TRIGGER bench_child_before_ins BEFORE INSERT ON bench_child
           FOR EACH ROW SET NEW.qty = GREATEST(NEW.qty, 1)",
        "CREATE TRIGGER bench_child_before_ins_2 BEFORE INSERT ON bench_child
           FOR EACH ROW SET NEW.sku = TRIM(NEW.sku)",
        "CREATE TRIGGER bench_child_after_upd AFTER UPDATE ON bench_child
           FOR EACH ROW SET @bench_touched = NEW.order_id",
        "CREATE TRIGGER bench_child_before_del BEFORE DELETE ON bench_child
           FOR EACH ROW SET @bench_deleted = OLD.order_id",
        "CREATE VIEW bench_open_lines AS
         SELECT c.order_id, c.line_no, c.sku, c.qty, w.category, w.created_on AS ordered_on
         FROM bench_child c JOIN bench_wide w ON w.id = c.parent_id
         WHERE c.shipped_at IS NULL",
        // `partitioned` in CREATE_OPTIONS is the only thing that tells this from
        // an ordinary table; TABLE_TYPE says BASE TABLE for both.
        "CREATE TABLE bench_parts (
           id INT NOT NULL, created_on DATE NOT NULL, PRIMARY KEY (id)
         ) ENGINE=InnoDB PARTITION BY HASH(id) PARTITIONS 4",
        // A name that is not ASCII, for the failure message and for the
        // character counting a position would need if MySQL reported one.
        "CREATE TABLE `價格表` (
           `編號` INT NOT NULL PRIMARY KEY,
           `名稱` VARCHAR(64) NOT NULL
         ) ENGINE=InnoDB",
        // Somewhere to write, so that a statement with no result set has a
        // count worth reporting.
        "CREATE TABLE scratch (n INT NOT NULL) ENGINE=InnoDB",
        // A second database: `schemas()` is not scoped to the connection's
        // default one, no metadata query hardcodes it, and a foreign key can
        // cross the boundary — which PostgreSQL cannot express at all.
        "CREATE DATABASE bench2",
        "CREATE TABLE bench2.daily_totals (
           day     DATE          NOT NULL PRIMARY KEY,
           orders  INT           NOT NULL,
           revenue DECIMAL(12,2) NOT NULL
         ) ENGINE=InnoDB",
        "CREATE TABLE bench2.cross_ref (
           id        INT PRIMARY KEY,
           parent_id INT UNSIGNED NULL,
           CONSTRAINT cross_ref_fk FOREIGN KEY (parent_id)
             REFERENCES bench.bench_wide (id) ON DELETE SET NULL
         ) ENGINE=InnoDB",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    out.push(format!(
        "INSERT INTO bench_wide
           (big_val,int_val,num_val,real_val,dbl_val,name,hash_hex,category,flag,
            created_at,created_on,elapsed,small_val,nullable_text,json_val,bytes_val)
         WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n < {WIDE_ROWS})
         SELECT n * 7919, n MOD 1000000, (n MOD 1000000) / 7,
                n MOD 1000, n MOD 1000,
                CONCAT('row-', n), MD5(n),
                ELT(1 + n MOD 4, 'alpha','beta','gamma','delta'), n MOD 2,
                TIMESTAMPADD(DAY, n MOD 2000, '2020-01-01 00:00:00.123456'),
                DATE(TIMESTAMPADD(DAY, n MOD 2000, '2020-01-01')),
                SEC_TO_TIME(n MOD 86400),
                n MOD 100,
                IF(n MOD 17 = 0, NULL, CONCAT('opt-', n)),
                JSON_OBJECT('k', n),
                UNHEX(MD5(n))
         FROM seq"
    ));
    out.push(format!(
        "INSERT INTO no_key (n, label)
         WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n < {NO_KEY_ROWS})
         SELECT n, CONCAT('row-', n) FROM seq"
    ));
    out.push(
        "INSERT INTO bench_child (order_id,line_no,parent_id,sku,email,qty,note,shipped_at)
         WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n < 500)
         SELECT n, 1, n, CONCAT('sku-', n), CONCAT('user', n, '@example.com'),
                1 + n MOD 5, CONCAT('note for order ', n),
                IF(n MOD 3 = 0, NULL, TIMESTAMPADD(DAY, n MOD 90, '2024-01-01'))
         FROM seq"
            .to_string(),
    );
    // So `estimated_rows` has something behind it rather than nothing.
    out.push("ANALYZE TABLE bench_wide, bench_child, no_key, bench_types, bench_parts".to_string());
    out
}

// ---------------------------------------------------------------------------
// Reading a result
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_result_arrives_in_batches_of_the_size_that_was_asked_for() {
    let source = source().await;
    let mut stream = source
        .query("SELECT id FROM bench_wide ORDER BY id", 500)
        .await
        .expect("the query should run");

    // Before a row has been read: a grid is laid out first and filled
    // afterwards, which is what preparing the statement buys.
    assert_eq!(stream.schema().fields().len(), 1);
    // Zero is a real answer for a statement that changed nothing, so "not
    // finished yet" cannot be zero.
    assert_eq!(stream.rows_affected(), None);

    let mut seen: Vec<u32> = Vec::with_capacity(WIDE_ROWS as usize);
    let mut batches = 0;
    while let Some(batch) = stream.next_batch().await.expect("reading a batch") {
        batches += 1;
        assert!(
            batch.num_rows() <= 500,
            "a batch of {} exceeds what was asked for",
            batch.num_rows()
        );
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .expect("an INT UNSIGNED primary key is a UInt32");
        seen.extend(ids.values());
    }

    assert_eq!(batches, (WIDE_ROWS / 500) as usize);
    assert_eq!(seen.len(), WIDE_ROWS as usize, "every row, once");
    // Order and duplicates in one check: a batching bug that repeated a row or
    // dropped one shows up here rather than in the count alone.
    assert!(
        seen.iter().enumerate().all(|(i, id)| *id == i as u32 + 1),
        "the rows did not arrive in the order they were asked for"
    );
    assert_eq!(stream.rows_affected(), Some(WIDE_ROWS as u64));
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_statement_that_produces_no_rows_still_says_what_it_changed() {
    let source = source().await;
    source
        .query("DELETE FROM scratch", 10)
        .await
        .expect("clearing")
        .next_batch()
        .await
        .expect("draining");

    let mut stream = source
        .query("INSERT INTO scratch VALUES (1), (2), (3)", 10)
        .await
        .expect("the insert should run");
    // No result set means no columns, which is the honest schema for it rather
    // than a made-up one.
    assert_eq!(stream.schema().fields().len(), 0);
    assert_eq!(stream.next_batch().await.expect("draining"), None);
    assert_eq!(stream.rows_affected(), Some(3));
}

// ---------------------------------------------------------------------------
// Paging
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_cursor_pages_a_relation_that_has_no_key_to_page_by() {
    let source = source().await;
    let mut cursor = source
        .cursor("SELECT n FROM no_key", 250)
        .await
        .expect("the cursor should open");
    assert_eq!(cursor.schema().fields().len(), 1);

    let mut seen: Vec<i32> = Vec::with_capacity(NO_KEY_ROWS as usize);
    while let Some(page) = cursor.fetch().await.expect("fetching a page") {
        let ns = page
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int32Array>()
            .expect("an INT is an Int32");
        seen.extend(ns.values());
    }

    // No repeats and no skips, on a table with no unique order — which is the
    // property LIMIT/OFFSET cannot have, because without an ORDER BY the server
    // is free to return page two's rows in page one.
    assert_eq!(seen.len(), NO_KEY_ROWS as usize);
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), NO_KEY_ROWS as usize, "a row appeared twice");
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_cursor_can_be_closed_with_pages_still_in_it() {
    let source = source().await;
    let mut cursor = source
        .cursor("SELECT n FROM no_key", 10)
        .await
        .expect("the cursor should open");
    cursor
        .fetch()
        .await
        .expect("the first page")
        .expect("a page");

    // The ordinary case: somebody closed a table browser after looking at the
    // top of it. Closing must not wait for the rest of the result, and must not
    // leave the producer parked on a send nobody will take.
    cursor.close().await.expect("closing");
    assert_eq!(cursor.fetch().await.expect("after closing"), None);
    cursor.close().await.expect("closing twice");
}

// ---------------------------------------------------------------------------
// Stopping
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_cancelled_statement_is_told_apart_from_a_broken_one() {
    let source = Arc::new(source().await);
    let running = {
        let source = Arc::clone(&source);
        tokio::spawn(async move {
            let mut stream = source
                .query(SLOW, 100)
                .await
                .expect("the join should start");
            stream.next_batch().await
        })
    };
    // Long enough for the statement to be executing rather than parsing;
    // cancelling before it starts would be testing the wrong moment.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    source.cancel().await.expect("delivering the cancel");

    let err = running
        .await
        .expect("the reader task")
        .expect_err("a cancelled statement fails");
    assert!(
        err.is_cancelled(),
        "a cancelled statement has to say so, or a front end shows the button \
         they pressed as a fault: {err}"
    );
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_cursor_carries_its_own_stop() {
    let source = source().await;
    let mut cursor = source
        .cursor(SLOW, 100)
        .await
        .expect("the cursor should open");
    // Taken out in advance, because by cancel time the cursor is borrowed by the
    // fetch that is to be stopped.
    let canceller = cursor.canceller();
    let fetching = tokio::spawn(async move { cursor.fetch().await });

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    canceller.cancel().await.expect("delivering the cancel");

    let err = fetching
        .await
        .expect("the fetch task")
        .expect_err("a cancelled fetch fails");
    assert!(err.is_cancelled(), "{err}");
    // The session's own cancel could not have done this: it names the session's
    // connections, and a cursor runs on one of its own.
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn stopping_something_that_is_not_running_is_not_a_failure() {
    let source = source().await;
    // A Cancel button pressed at a quiet moment must not report an error for
    // being pressed at a quiet moment.
    source.cancel().await.expect("an idle session");

    let cursor = source
        .cursor("SELECT n FROM no_key", 10)
        .await
        .expect("the cursor should open");
    cursor.canceller().cancel().await.expect("an idle cursor");
    // And once its connection has gone entirely, which the server answers with
    // ER_NO_SUCH_THREAD rather than with silence.
    let canceller = cursor.canceller();
    drop(cursor);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    canceller.cancel().await.expect("a cursor that has closed");
}

// ---------------------------------------------------------------------------
// Failing
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_broken_statement_says_what_is_wrong_and_declines_to_say_where() {
    let source = source().await;
    let broken = "SELECT id FROM bench_wide WHERE ORDER BY id";
    let err = source
        .query(broken, 10)
        .await
        .err()
        .expect("this does not parse");

    assert!(
        !err.to_string().is_empty(),
        "a failure has to say something"
    );
    assert!(!err.is_cancelled(), "a broken statement is not a cancel");
    // MySQL sends no offset at all. What it sends instead is the tail of the
    // statement it stopped at, and turning that back into a position means
    // searching the text for a fragment that is truncated at 80 characters and
    // not escaped. A caret in a plausible wrong place is worse than none, so
    // this asserts the absence rather than papering over it.
    assert_eq!(
        err.statement_position(),
        None,
        "no position should be invented from the message text"
    );

    // The same, with an identifier that is not ASCII: if a position were ever
    // reconstructed from the message, this is the statement where counting
    // bytes instead of characters would put the caret in the middle of a
    // character.
    let broken = "SELECT `編號`, FROM `價格表` WHERE `編號` = 1";
    let err = source
        .query(broken, 10)
        .await
        .err()
        .expect("this does not parse");
    assert_eq!(err.statement_position(), None);
    assert!(
        err.to_string().contains('價'),
        "the identifier should survive into the message: {err}"
    );
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_missing_relation_fails_the_statement_but_not_the_navigator() {
    let source = source().await;
    let err = source
        .query("SELECT * FROM no_such_relation_anywhere", 10)
        .await
        .err()
        .expect("SQL will not plan over a name it cannot resolve");
    assert!(!err.is_cancelled());

    // A navigator works from a tree that can be one refresh out of date, so the
    // same name has to be an ordinary empty answer here.
    let missing = "no_such_relation_anywhere";
    assert!(source.columns("bench", missing).await.unwrap().is_empty());
    assert!(source.indexes("bench", missing).await.unwrap().is_empty());
    assert!(
        source
            .foreign_keys("bench", missing)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        source
            .referenced_by("bench", missing)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        source
            .constraints("bench", missing)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(source.triggers("bench", missing).await.unwrap().is_empty());
    assert_eq!(source.definition("bench", missing).await.unwrap(), None);
    // And so does a schema that is not there.
    assert!(source.relations("no_such_schema").await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// The navigator
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn schemas_are_the_server_s_databases_and_not_the_connection_s_own() {
    let source = source().await;
    let names: Vec<String> = source
        .schemas()
        .await
        .expect("schemas")
        .into_iter()
        .map(|s| s.name)
        .collect();

    // MySQL's databases are siblings on one server rather than islands, so
    // listing all of them is right — a cross-database query is ordinary here.
    assert!(names.contains(&"bench".to_string()));
    assert!(names.contains(&"bench2".to_string()));
    // The server's own four belong in a sidebar no more than a catalog does.
    for kept_back in ["information_schema", "performance_schema", "mysql", "sys"] {
        assert!(
            !names.contains(&kept_back.to_string()),
            "{kept_back} should not be listed"
        );
    }
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_relation_says_what_kind_it_is() {
    let source = source().await;
    let relations = source.relations("bench").await.expect("relations");
    let kind = |name: &str| {
        relations
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name} should be listed"))
            .kind
    };

    assert_eq!(kind("bench_wide"), RelationKind::Table);
    assert_eq!(kind("bench_open_lines"), RelationKind::View);
    // TABLE_TYPE says BASE TABLE for this one too; the word `partitioned` in
    // CREATE_OPTIONS is the whole difference.
    assert_eq!(kind("bench_parts"), RelationKind::PartitionedTable);

    let wide = relations.iter().find(|r| r.name == "bench_wide").unwrap();
    assert_eq!(wide.schema, "bench", "a relation knows where it lives");
    // Analyzed above, so there is an estimate; it is documented as being off by
    // up to half on InnoDB, which is why nothing here checks it exactly.
    assert!(
        wide.estimated_rows.is_some_and(|n| n > 0),
        "an analyzed table should have an estimate"
    );
    // A view has no rows to estimate, and declining to answer is not the same
    // as answering zero.
    let view = relations
        .iter()
        .find(|r| r.name == "bench_open_lines")
        .unwrap();
    assert_eq!(view.estimated_rows, None);
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_column_reports_the_type_the_table_declares() {
    let source = source().await;
    let columns = source
        .columns("bench", "bench_wide")
        .await
        .expect("columns");

    for (offset, column) in columns.iter().enumerate() {
        assert_eq!(column.position, offset as i32 + 1, "{}", column.name);
    }

    let column = |name: &str| columns.iter().find(|c| c.name == name).unwrap();
    // COLUMN_TYPE rather than DATA_TYPE: a structure pane showing `int` where
    // the table says `int unsigned` is describing something else.
    assert_eq!(column("id").data_type, "int unsigned");
    assert!(column("id").is_primary_key);
    assert!(!column("id").nullable);
    assert_eq!(column("num_val").data_type, "decimal(18,4)");
    assert_eq!(column("name").data_type, "varchar(64)");
    assert_eq!(
        column("category").data_type,
        "enum('alpha','beta','gamma','delta')"
    );
    assert!(column("nullable_text").nullable);
    assert!(!column("nullable_text").is_primary_key);
    // The server's own text, verbatim, rather than one this side re-quoted.
    assert_eq!(
        column("updated_at").default_value.as_deref(),
        Some("CURRENT_TIMESTAMP(3)")
    );
    assert_eq!(column("name").default_value, None);
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_view_reports_its_body_and_a_table_reports_nothing() {
    let source = source().await;
    let body = source
        .definition("bench", "bench_open_lines")
        .await
        .expect("definition")
        .expect("a view has one");
    assert!(body.to_lowercase().contains("select"), "{body}");
    assert!(body.contains("bench_child"), "{body}");

    // Absent rather than empty for a table, which is the distinction a
    // structure pane hangs a section on.
    assert_eq!(
        source.definition("bench", "bench_wide").await.unwrap(),
        None
    );
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn an_index_says_what_the_planner_can_actually_use() {
    let source = source().await;
    let indexes = source
        .indexes("bench", "bench_child")
        .await
        .expect("indexes");
    let index = |name: &str| {
        indexes
            .iter()
            .find(|i| i.name == name)
            .unwrap_or_else(|| panic!("{name} should be listed"))
    };

    // Primary key first, and composite in declaration order.
    assert_eq!(indexes[0].name, "PRIMARY");
    assert!(indexes[0].is_primary && indexes[0].is_unique);
    assert_eq!(indexes[0].columns, ["`order_id`", "`line_no`"]);

    assert!(index("bench_child_sku_key").is_unique);
    assert!(!index("bench_child_sku_key").is_primary);

    // An index on the first sixteen characters is not an index on the column,
    // and printing it as one misstates what a query can use it for.
    assert_eq!(index("bench_child_email_prefix").columns, ["`email`(16)"]);
    assert_eq!(index("bench_child_qty_desc").columns, ["`qty` DESC"]);
    // A functional key part reports a NULL column and the expression instead —
    // the case a join against the column list silently drops.
    assert_eq!(index("bench_child_email_lower").columns.len(), 1);
    assert!(
        index("bench_child_email_lower").columns[0].contains("lower"),
        "{:?}",
        index("bench_child_email_lower").columns
    );

    assert_eq!(index("bench_child_note_ft").method, "FULLTEXT");
    assert_eq!(index("PRIMARY").method, "BTREE");
    // MySQL has no partial index, so this field is empty on every one of them.
    assert!(indexes.iter().all(|i| i.predicate.is_none()));

    // A table with a primary key and nothing else still answers.
    let plain = source
        .indexes("bench2", "daily_totals")
        .await
        .expect("indexes");
    assert_eq!(plain.len(), 1);
    assert!(plain[0].is_primary);
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_constraint_the_server_does_not_enforce_says_so() {
    let source = source().await;
    let constraints = source
        .constraints("bench", "bench_child")
        .await
        .expect("constraints");
    let found = |name: &str| {
        constraints
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} should be listed"))
    };

    let positive = found("bench_child_qty_positive");
    assert_eq!(positive.kind, ConstraintKind::Check);
    assert!(positive.definition.starts_with("CHECK ("), "{positive:?}");
    assert!(positive.definition.contains("qty"), "{positive:?}");
    assert!(!positive.definition.contains("NOT ENFORCED"));

    // A check that does not fire, listed as though it did, is the same lie a
    // disabled trigger would be.
    let lax = found("bench_child_line_sane");
    assert!(
        lax.definition.contains("NOT ENFORCED"),
        "an unenforced check has to say so: {lax:?}"
    );

    let unique = found("bench_child_sku_key");
    assert_eq!(unique.kind, ConstraintKind::Unique);
    // Built from the index of the same name, because MySQL has no
    // `pg_get_constraintdef` to ask.
    assert_eq!(unique.definition, "UNIQUE (`sku`)");

    // Primary and foreign keys have their own sections and are not repeated
    // here.
    assert!(constraints.iter().all(|c| c.name != "PRIMARY"));
    assert!(
        constraints
            .iter()
            .all(|c| c.name != "bench_child_parent_fk")
    );

    // A table with neither.
    assert!(
        source
            .constraints("bench", "bench_parts")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_foreign_key_reads_the_same_from_both_ends() {
    let source = source().await;

    let outbound = source
        .foreign_keys("bench", "bench_child")
        .await
        .expect("foreign keys");
    assert_eq!(outbound.len(), 1);
    let key = &outbound[0];
    assert_eq!(key.name, "bench_child_parent_fk");
    assert_eq!(key.local_columns, ["parent_id"]);
    assert_eq!(key.other_schema, "bench");
    assert_eq!(key.other_table, "bench_wide");
    assert_eq!(key.other_columns, ["id"]);
    // Already the DDL spelling in the catalog, so there is no translation for
    // this side to get wrong.
    assert_eq!(key.on_delete, "CASCADE");
    assert_eq!(key.on_update, "RESTRICT");

    let inbound = source
        .referenced_by("bench", "bench_wide")
        .await
        .expect("inbound references");
    let from_child = inbound
        .iter()
        .find(|r| r.other_table == "bench_child")
        .expect("bench_child references bench_wide");
    // Named for the vantage point: `local` is bench_wide's column here and
    // bench_child's in the outbound direction.
    assert_eq!(from_child.local_columns, ["id"]);
    assert_eq!(from_child.other_columns, ["parent_id"]);
    assert_eq!(from_child.other_schema, "bench");

    // A reference from another database, which MySQL has and PostgreSQL cannot
    // express. The referencing side's schema is read rather than assumed to be
    // the one that was asked about.
    let from_other_db = inbound
        .iter()
        .find(|r| r.other_table == "cross_ref")
        .expect("bench2.cross_ref references bench.bench_wide");
    assert_eq!(from_other_db.other_schema, "bench2");
    assert_eq!(from_other_db.on_delete, "SET NULL");

    // A table at neither end of one.
    assert!(
        source
            .foreign_keys("bench", "bench_parts")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        source
            .referenced_by("bench", "bench_parts")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_trigger_carries_its_body_where_there_is_no_routine_to_name() {
    let source = source().await;
    let triggers = source
        .triggers("bench", "bench_child")
        .await
        .expect("triggers");
    assert_eq!(triggers.len(), 4);

    let found = |name: &str| {
        triggers
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} should be listed"))
    };
    let before = found("bench_child_before_ins");
    assert_eq!(before.timing.as_deref(), Some("BEFORE"));
    // Exactly one, always: MySQL has no `BEFORE INSERT OR UPDATE` trigger, so
    // where PostgreSQL reports a set this reports a single element.
    assert_eq!(before.events, ["INSERT"]);
    assert_eq!(before.level.as_deref(), Some("ROW"));
    // There is no separate routine to name — the body is inline, and it is
    // carried in `definition` rather than stuffed into a field a structure pane
    // renders as a function name.
    assert_eq!(before.function, None);
    assert!(
        before
            .definition
            .as_deref()
            .is_some_and(|d| d.contains("GREATEST")),
        "{before:?}"
    );
    // MySQL has no disabled trigger; everything in the catalog fires.
    assert!(triggers.iter().all(|t| t.enabled));

    assert_eq!(
        found("bench_child_after_upd").timing.as_deref(),
        Some("AFTER")
    );
    assert_eq!(found("bench_child_before_del").events, ["DELETE"]);

    assert!(
        source
            .triggers("bench", "bench_parts")
            .await
            .unwrap()
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Reads `bench_types` once and answers about its Arrow schema and its values.
async fn types(source: &MySqlSource) -> (arrow::datatypes::SchemaRef, arrow::array::RecordBatch) {
    let mut stream = source
        .query("SELECT * FROM bench_types ORDER BY id", 100)
        .await
        .expect("reading every type");
    let schema = stream.schema();
    let batch = stream
        .next_batch()
        .await
        .expect("a batch")
        .expect("bench_types has rows");
    (schema, batch)
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn every_column_arrives_as_the_arrow_type_it_was_mapped_to() {
    let source = source().await;
    let (schema, _) = types(&source).await;
    let of = |name: &str| {
        schema
            .field_with_name(name)
            .unwrap_or_else(|_| panic!("{name} should be in the result"))
            .data_type()
            .clone()
    };

    assert_eq!(of("t_tinyint"), DataType::Int8);
    assert_eq!(of("t_tinyint_u"), DataType::UInt8);
    assert_eq!(of("t_smallint"), DataType::Int16);
    assert_eq!(of("t_smallint_u"), DataType::UInt16);
    // There is no UInt24, so MEDIUMINT's unsigned form widens a step.
    assert_eq!(of("t_mediumint"), DataType::Int32);
    assert_eq!(of("t_mediumint_u"), DataType::UInt32);
    assert_eq!(of("t_int"), DataType::Int32);
    assert_eq!(of("t_int_u"), DataType::UInt32);
    assert_eq!(of("t_bigint"), DataType::Int64);
    assert_eq!(of("t_bigint_u"), DataType::UInt64);
    // MySQL has no boolean; BOOL is a synonym for TINYINT(1), and a TINYINT(1)
    // holding 7 is indistinguishable from one holding a truth value.
    assert_eq!(of("t_bool"), DataType::Int8);
    assert_eq!(of("t_tinyint1"), DataType::Int8);
    assert_eq!(of("t_float"), DataType::Float32);
    assert_eq!(of("t_double"), DataType::Float64);

    assert_eq!(of("t_dec_small"), DataType::Decimal128(10, 2));
    assert_eq!(of("t_dec_128"), DataType::Decimal128(38, 10));
    // The whole reason for Decimal256: 65 digits do not fit 128 bits.
    assert_eq!(of("t_dec_wide"), DataType::Decimal256(65, 30));
    assert_eq!(of("t_dec_zero"), DataType::Decimal128(12, 0));
    assert_eq!(of("t_dec_u"), DataType::Decimal128(9, 3));

    assert_eq!(of("t_date"), DataType::Date32);
    assert_eq!(
        of("t_datetime"),
        DataType::Timestamp(TimeUnit::Microsecond, None)
    );
    assert_eq!(
        of("t_timestamp"),
        DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
    );
    assert_eq!(of("t_time"), DataType::Duration(TimeUnit::Microsecond));
    // A year is not a day.
    assert_eq!(of("t_year"), DataType::Int16);

    assert_eq!(of("t_bit1"), DataType::Boolean);
    assert_eq!(of("t_bit17"), DataType::UInt64);
    assert_eq!(of("t_bit64"), DataType::UInt64);

    // Character set 63 is the only thing separating each of these pairs.
    assert_eq!(of("t_char"), DataType::Utf8);
    assert_eq!(of("t_binary"), DataType::Binary);
    assert_eq!(of("t_varchar"), DataType::Utf8);
    assert_eq!(of("t_varbinary"), DataType::Binary);
    assert_eq!(of("t_longtext"), DataType::Utf8);
    assert_eq!(of("t_longblob"), DataType::Binary);
    // And the flag word is the only thing separating these from a CHAR.
    assert_eq!(of("t_enum"), DataType::Utf8);
    assert_eq!(of("t_set"), DataType::Utf8);

    assert_eq!(of("t_json"), DataType::Utf8);
    assert_eq!(of("t_geom"), DataType::Binary);
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn the_values_at_the_extremes_arrive_intact() {
    let source = source().await;
    let (schema, batch) = types(&source).await;
    let column = |name: &str| batch.column(schema.index_of(name).unwrap()).clone();

    // The case Int64 would have wrapped negative, and the reason unsigned
    // columns widen rather than reusing the signed type of the same width.
    let bigint_u = column("t_bigint_u");
    let bigint_u = bigint_u.as_any().downcast_ref::<UInt64Array>().unwrap();
    assert_eq!(bigint_u.value(0), u64::MAX);

    // The mask, most significant byte first.
    let bit17 = column("t_bit17");
    let bit17 = bit17.as_any().downcast_ref::<UInt64Array>().unwrap();
    assert_eq!(bit17.value(0), 0b1_0101_0101_0101_0101);

    // A decimal that a 28-digit fixed-width type would have rounded away, and
    // the reason nothing here goes through one.
    let wide = column("t_dec_wide");
    let wide = wide.as_any().downcast_ref::<Decimal256Array>().unwrap();
    assert_eq!(
        wide.value_as_string(0),
        "-12345678901234567890123456789012345.123456789012345678901234567890"
    );

    let small = column("t_dec_small");
    let small = small.as_any().downcast_ref::<Decimal128Array>().unwrap();
    assert_eq!(small.value_as_string(0), "-12345678.90");

    // A leap day, which an off-by-one in the epoch constant would move.
    let date = column("t_date");
    let date = date.as_any().downcast_ref::<Date32Array>().unwrap();
    assert_eq!(date.value(0), 19782); // 2024-02-29

    // The wall-clock reading, kept whole: 2024-02-29 13:45:56.123456.
    let dt = column("t_datetime6");
    let dt = dt
        .as_any()
        .downcast_ref::<TimestampMicrosecondArray>()
        .unwrap();
    assert_eq!(dt.value(0), 19782 * 86_400_000_000 + 49_556_123_456);

    // Row two is a NULL in every builder.
    for field in schema.fields().iter().filter(|f| f.name() != "id") {
        let values = column(field.name());
        assert!(
            values.is_null(1),
            "{} should be NULL in the all-null row",
            field.name()
        );
    }
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_time_keeps_its_sign_and_its_days() {
    let source = source().await;
    let (schema, batch) = types(&source).await;
    let times = batch.column(schema.index_of("t_time").unwrap()).clone();
    let times = times
        .as_any()
        .downcast_ref::<DurationMicrosecondArray>()
        .unwrap();

    // Row three: `'-838:59:59'`, which is what a TIMEDIFF can return and what
    // Arrow's Time64 could represent neither the sign nor the extent of.
    assert_eq!(times.value(2), -(838 * 3600 + 59 * 60 + 59) * 1_000_000);

    let long = batch.column(schema.index_of("t_time6").unwrap()).clone();
    let long = long
        .as_any()
        .downcast_ref::<DurationMicrosecondArray>()
        .unwrap();
    assert_eq!(
        long.value(2),
        (838 * 3600 + 59 * 60 + 58) * 1_000_000 + 999_999
    );
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_zero_date_arrives_as_null_because_there_is_nowhere_else_to_put_it() {
    let source = source().await;
    let (schema, batch) = types(&source).await;

    // Rows six and seven: `'0000-00-00'` and `'2010-00-01'`. Both are legal
    // values a MySQL column can hold and neither is a point on any calendar.
    // Failing instead would make one legal row take the whole column down.
    for name in ["t_date", "t_datetime"] {
        let values = batch.column(schema.index_of(name).unwrap()).clone();
        assert!(values.is_null(5), "{name} row 6");
        assert!(values.is_null(6), "{name} row 7");
        // And the rows that do have dates still have them.
        assert!(!values.is_null(0), "{name} row 1");
    }
}

#[tokio::test]
#[ignore = "requires a MySQL server"]
async fn a_timestamp_is_read_in_the_zone_its_type_claims() {
    let source = source().await;
    // Every connection sets its session zone to UTC, which is what makes the
    // `UTC` on the Arrow type true rather than decorative — the server converts
    // a TIMESTAMP into the session zone before it reaches the wire.
    let mut stream = source
        .query("SELECT @@session.time_zone AS tz", 1)
        .await
        .expect("reading the session zone");
    let batch = stream.next_batch().await.unwrap().unwrap();
    let zone = batch
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .unwrap();
    assert_eq!(zone.value(0), "+00:00");

    let (schema, batch) = types(&source).await;
    let stamps = batch
        .column(schema.index_of("t_timestamp").unwrap())
        .clone();
    let stamps = stamps
        .as_any()
        .downcast_ref::<TimestampMicrosecondArray>()
        .unwrap();
    // Row five is TIMESTAMP's documented ceiling, 2038-01-19 03:14:07 UTC.
    assert_eq!(stamps.value(4), 2_147_483_647_000_000);
}
