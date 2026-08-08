#!/usr/bin/env bash
# Creates the benchmark table used by `make bench` and the integration tests.
#
# The column set is deliberately type-diverse. A benchmark over ten integer
# columns measures almost nothing a real result set costs: conversion, variable
# width data, and null handling are the work.
#
# Assumes the container is already running (`make db-up`).

set -uo pipefail

CONTAINER="${PG_CONTAINER:-pg-bench}"
ROWS="${ROWS:-1000000}"

if ! docker exec "$CONTAINER" pg_isready -U bench -d bench >/dev/null 2>&1; then
    echo "container '$CONTAINER' is not accepting connections; run 'make db-up'" >&2
    exit 1
fi

echo "seeding $ROWS rows into bench_wide..."

docker exec -i "$CONTAINER" psql -U bench -d bench -v ON_ERROR_STOP=1 \
    -v rows="$ROWS" <<'SQL'
\timing on
-- Dropped before bench_wide, not with the rest of its own fixture below:
-- bench_child's foreign key makes bench_wide undroppable while it exists, so
-- the other order only works on a database that has never been seeded.
DROP TABLE IF EXISTS bench_child;
DROP TABLE IF EXISTS bench_wide;

CREATE TABLE bench_wide AS
SELECT
  g                                              AS id,
  -- g is int4; the product must be widened before multiplying or it overflows.
  (g::bigint * 7919)                             AS big_val,
  (random() * 1e6)::int                          AS int_val,
  (random() * 1e9)::numeric(18,4)                AS num_val,
  (random() * 1000)::real                        AS real_val,
  (random() * 1000)::double precision            AS dbl_val,
  'row-' || g                                    AS name,
  md5(g::text)                                   AS hash_hex,
  repeat('x', 40 + (g % 60))                     AS payload,
  (ARRAY['alpha','beta','gamma','delta'])[1 + g % 4] AS category,
  (g % 2 = 0)                                    AS flag,
  timestamp '2020-01-01' + (g % 2000) * interval '1 day'          AS created_at,
  (timestamp '2020-01-01' + (g % 2000) * interval '1 day')::date  AS created_on,
  make_time((g % 24), (g % 60), 0)               AS created_time,
  md5(g::text)::uuid                             AS uuid_val,
  (g % 100)::smallint                            AS small_val,
  CASE WHEN g % 17 = 0 THEN NULL ELSE 'opt-' || g END  AS nullable_text,
  CASE WHEN g % 23 = 0 THEN NULL ELSE (g * 3)::int END AS nullable_int,
  ('{"k":' || g || ',"t":"' || (ARRAY['a','b','c'])[1 + g % 3] || '"}')::jsonb AS json_val,
  decode(md5(g::text), 'hex')                    AS bytes_val
FROM generate_series(1, :rows) g;

ALTER TABLE bench_wide ADD PRIMARY KEY (id);
ANALYZE bench_wide;

-- A relation with no primary key, and so no order total enough for the browse
-- to page in. The client refuses to page it rather than paging it wrongly, and
-- that refusal needs something to refuse.
DROP TABLE IF EXISTS no_key;
CREATE TABLE no_key AS
SELECT g AS n, 'row-' || g AS label FROM generate_series(1, 250000) g;
ANALYZE no_key;

-- Structure fixtures. The Structure tab reports indexes, foreign keys, inbound
-- references, constraints and triggers, and bench_wide has only a primary key.
-- This carries one of each interesting shape — composite key, unique, partial,
-- expression, a foreign key with a non-default action, CHECK and UNIQUE
-- constraints, and both an enabled and a disabled trigger — so the pane is
-- exercised rather than assumed.
CREATE TABLE bench_child (
  order_id   int          NOT NULL,
  line_no    smallint     NOT NULL,
  parent_id  int          NOT NULL REFERENCES bench_wide(id) ON DELETE CASCADE,
  sku        text         NOT NULL,
  email      text,
  qty        int          NOT NULL DEFAULT 1,
  shipped_at timestamp,
  PRIMARY KEY (order_id, line_no),
  CONSTRAINT bench_child_qty_positive CHECK (qty > 0),
  CONSTRAINT bench_child_order_line_uniq UNIQUE (order_id, line_no, sku)
);

CREATE OR REPLACE FUNCTION bench_child_touch() RETURNS trigger AS $fn$
BEGIN
  RETURN NEW;
END;
$fn$ LANGUAGE plpgsql;

CREATE TRIGGER bench_child_before_write
  BEFORE INSERT OR UPDATE ON bench_child
  FOR EACH ROW EXECUTE FUNCTION bench_child_touch();

CREATE TRIGGER bench_child_after_delete
  AFTER DELETE ON bench_child
  FOR EACH STATEMENT EXECUTE FUNCTION bench_child_touch();
ALTER TABLE bench_child DISABLE TRIGGER bench_child_after_delete;
CREATE UNIQUE INDEX bench_child_sku_key ON bench_child (sku);
CREATE INDEX bench_child_pending_idx ON bench_child (order_id) WHERE shipped_at IS NULL;
CREATE INDEX bench_child_email_lower_idx ON bench_child (lower(email));
INSERT INTO bench_child (order_id, line_no, parent_id, sku, email, qty, shipped_at)
SELECT g, 1, g, 'sku-' || g, 'user' || g || '@example.com', 1 + g % 5,
       CASE WHEN g % 3 = 0 THEN NULL ELSE timestamp '2024-01-01' + (g % 90) * interval '1 day' END
FROM generate_series(1, 5000) g;
ANALYZE bench_child;

-- A second schema. Every metadata query takes a schema argument and nothing
-- hardcodes "public", but with one schema in the database that is a property of
-- the code nobody has watched hold. This gives the navigator a second branch,
-- and gives the browse a relation whose qualified name is not its bare name.
DROP SCHEMA IF EXISTS reporting CASCADE;
CREATE SCHEMA reporting;
CREATE TABLE reporting.daily_totals (
  day     date        PRIMARY KEY,
  orders  int         NOT NULL,
  revenue numeric(12,2) NOT NULL
);
INSERT INTO reporting.daily_totals
SELECT date '2024-01-01' + g, 10 + g % 40, (1000 + g * 13)::numeric(12,2)
FROM generate_series(0, 364) g;
ANALYZE reporting.daily_totals;

SELECT count(*) AS rows FROM bench_wide;
SELECT pg_size_pretty(pg_total_relation_size('bench_wide')) AS size;
SQL

status=$?
if [ $status -ne 0 ]; then
    echo "seed failed with status $status" >&2
    exit $status
fi
echo "seed complete"
