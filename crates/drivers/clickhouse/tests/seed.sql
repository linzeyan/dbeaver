-- The fixture the integration tests read, applied by them at start-up.
--
-- Rows are extremes rather than ones and twos: a mapping that drops the top bit
-- of a UInt64, or reads a Date as the small integer it arrives as, is invisible
-- against `1` and unmistakable against `18446744073709551615`.
--
-- Statements are separated by a line containing only `;;`, because the test
-- posts them one at a time — ClickHouse's HTTP interface takes one statement per
-- request, and splitting on `;` would cut the INSERTs apart at every value.

CREATE DATABASE IF NOT EXISTS bench
;;
DROP TABLE IF EXISTS bench.types_all
;;
-- One column per row of the type table in the spec, so a change in the server's
-- ArrowStream output shows up as a failing assertion rather than as a support
-- ticket.
CREATE TABLE bench.types_all
(
    id              UInt32,

    i8              Int8,
    i16             Int16,
    i32             Int32,
    i64             Int64,
    i128            Int128,
    i256            Int256,
    u8              UInt8,
    u16             UInt16,
    u32             UInt32,
    u64             UInt64,
    u128            UInt128,
    u256            UInt256,

    f32             Float32,
    f64             Float64,

    d32             Decimal32(4),
    d64             Decimal64(8),
    d128            Decimal128(18),
    d256            Decimal256(40),

    dt_date         Date,
    dt_date32       Date32,
    dt_datetime     DateTime,
    dt_datetime_tz  DateTime('Asia/Taipei'),
    dt_dt64_3       DateTime64(3),
    dt_dt64_9_tz    DateTime64(9, 'Asia/Taipei'),

    e8              Enum8('draft' = -1, 'live' = 0, 'archived' = 1),
    e16             Enum16('alpha' = 1000, 'beta' = 2000),

    lc              LowCardinality(String),
    lc_nullable     LowCardinality(Nullable(String)),

    n_i32           Nullable(Int32),
    n_str           Nullable(String),

    arr             Array(Int32),
    arr_nested      Array(Array(String)),
    arr_lc          Array(LowCardinality(String)),
    tup             Tuple(Int32, String),
    tup_named       Tuple(qty Int32, unit String),
    map_ss          Map(String, String),
    map_si          Map(String, Array(Int64)),

    fs              FixedString(8),
    s               String,

    uid             UUID,
    ip4             IPv4,
    ip6             IPv6,

    b               Bool,
    iv              IntervalDay
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(dt_date)
PRIMARY KEY (id)
ORDER BY (id, i32)
SETTINGS index_granularity = 8192
;;
INSERT INTO bench.types_all VALUES
(
    1,
    -128, -32768, -2147483648, -9223372036854775808,
    -170141183460469231731687303715884105728,
    -57896044618658097711785492504343953926634992332820282019728792003956564819968,
    255, 65535, 4294967295,
    18446744073709551615,
    340282366920938463463374607431768211455,
    115792089237316195423570985008687907853269984665640564039457584007913129639935,
    -3.4028235e38, 1.7976931348623157e308,
    -99999.9999, -9999999999.99999999, -999999999999999999.999999999999999999,
    -0.0000000000000000000000000000000000000001,
    '1970-01-01', '1900-01-01', '1970-01-01 00:00:00', '1970-01-01 08:00:00',
    '1970-01-01 00:00:00.001', '1970-01-01 08:00:00.000000001',
    'draft', 'alpha',
    'red', NULL,
    NULL, NULL,
    [], [[]], [],
    (1, 'a'), (1, 'kg'),
    {'k':'v'}, {'k':[1,2,3]},
    'fixed\0\0\0', '',
    '00000000-0000-0000-0000-000000000000',
    '0.0.0.0', '::',
    false, 0
),
(
    2,
    127, 32767, 2147483647, 9223372036854775807,
    170141183460469231731687303715884105727,
    57896044618658097711785492504343953926634992332820282019728792003956564819967,
    0, 0, 0, 0, 0, 0,
    3.4028235e38, -1.7976931348623157e308,
    99999.9999, 9999999999.99999999, 999999999999999999.999999999999999999,
    0.0000000000000000000000000000000000000001,
    '2149-06-06', '2299-12-31', '2106-02-07 06:28:15', '2106-02-07 06:28:15',
    '2299-12-31 23:59:59.999', '2262-04-11 23:47:16.854775807',
    'archived', 'beta',
    'blue', 'green',
    -1, 'ünïcödé ✓ 漢字',
    [1, -1, 2147483647], [['a','b'],['c']], ['x','x','y'],
    (2147483647, 'z'), (7, 'kg'),
    {'a':'1','b':'2'}, {'a':[1],'b':[2,3]},
    'abcdefgh', 'ordinary',
    'ffffffff-ffff-ffff-ffff-ffffffffffff',
    '255.255.255.255', 'ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff',
    true, 365
),
(
    3,
    0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1,
    0, 0, 0, 0, 0, 0,
    '2024-01-15', '2024-01-15', '2024-01-15 12:34:56', '2024-01-15 12:34:56',
    '2024-01-15 12:34:56.789', '2024-01-15 12:34:56.123456789',
    'live', 'alpha',
    'red', NULL,
    42, 'plain',
    [7], [['q']], ['x'],
    (0, ''), (0, ''),
    {}, {},
    'ok      ', 'plain',
    '01890a5d-ac96-774b-bcce-b302099a8057',
    '10.0.0.1', '2001:db8::1',
    true, 1
)
;;
-- A String column holding bytes that are not UTF-8, kept out of `types_all` so
-- the rest of the type coverage stays readable when this one is being difficult.
DROP TABLE IF EXISTS bench.dirty_strings
;;
CREATE TABLE bench.dirty_strings (id UInt32, s String) ENGINE = MergeTree ORDER BY id
;;
INSERT INTO bench.dirty_strings VALUES (1, unhex('fffe')), (2, 'clean'), (3, unhex('000102ff'))
;;
DROP TABLE IF EXISTS bench.meta_rich
;;
CREATE TABLE bench.meta_rich
(
    id          UInt64,
    ts          DateTime,
    payload     String,
    tag         LowCardinality(String) DEFAULT 'none',
    derived     UInt64 MATERIALIZED id * 2,
    alias_col   UInt64 ALIAS id + 1,
    eph         UInt8 EPHEMERAL 0,
    zipped      String CODEC(ZSTD(3)),

    CONSTRAINT payload_not_empty CHECK length(payload) > 0,

    INDEX idx_payload_bloom payload TYPE bloom_filter(0.01) GRANULARITY 4,
    INDEX idx_ts_minmax     ts      TYPE minmax             GRANULARITY 1,
    INDEX idx_tag_set       tag     TYPE set(100)           GRANULARITY 2,
    INDEX idx_expr          lower(payload) TYPE ngrambf_v1(3, 256, 2, 0) GRANULARITY 1
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(ts)
PRIMARY KEY (id)
ORDER BY (id, ts)
SAMPLE BY id
COMMENT 'Everything the Structure tab has to show'
;;
INSERT INTO bench.meta_rich (id, ts, payload, tag, zipped)
SELECT number, now() - number, concat('p', toString(number)),
       ['a','b','c'][number % 3 + 1], repeat('z', 10)
FROM numbers(1000)
;;
DROP VIEW IF EXISTS bench.plain_view
;;
CREATE VIEW bench.plain_view AS
    SELECT id, ts, tag FROM bench.meta_rich WHERE id % 2 = 0
;;
DROP TABLE IF EXISTS bench.mv_target
;;
CREATE TABLE bench.mv_target (tag LowCardinality(String), n UInt64)
    ENGINE = SummingMergeTree ORDER BY tag
;;
DROP VIEW IF EXISTS bench.mat_view
;;
CREATE MATERIALIZED VIEW bench.mat_view TO bench.mv_target AS
    SELECT tag, count() AS n FROM bench.meta_rich GROUP BY tag
;;
-- An engine that tracks no row count, so `estimated_rows: None` is exercised
-- rather than assumed.
DROP TABLE IF EXISTS bench.no_stats
;;
CREATE TABLE bench.no_stats (a Int32) ENGINE = Log
;;
INSERT INTO bench.no_stats VALUES (1), (2), (3)
;;
-- The paging fixture, at the same scale as the PostgreSQL `bench_wide` so the
-- phase-0 throughput numbers stay comparable, and under the same name so the
-- shared contract test can read it with the statement it uses everywhere else.
DROP TABLE IF EXISTS bench.bench_wide
;;
CREATE TABLE bench.bench_wide
(
    id      UInt64,
    grp     LowCardinality(String),
    v_i32   Int32,
    v_i64   Int64,
    v_f64   Float64,
    v_dec   Decimal64(4),
    v_ts    DateTime64(3),
    v_str   String,
    v_uuid  UUID,
    v_arr   Array(Int32)
)
ENGINE = MergeTree ORDER BY id
;;
INSERT INTO bench.bench_wide
SELECT number,
       ['alpha','beta','gamma','delta'][number % 4 + 1],
       toInt32(number % 2147483647),
       toInt64(number),
       number / 7,
       toDecimal64(number, 4) / 100,
       toDateTime64('2024-01-01 00:00:00.000', 3) + number,
       concat('row-', toString(number)),
       generateUUIDv4(),
       range(number % 5)
FROM numbers(1000000)
;;
