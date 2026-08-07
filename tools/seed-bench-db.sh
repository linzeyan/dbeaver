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

SELECT count(*) AS rows FROM bench_wide;
SELECT pg_size_pretty(pg_total_relation_size('bench_wide')) AS size;
SQL

status=$?
if [ $status -ne 0 ]; then
    echo "seed failed with status $status" >&2
    exit $status
fi
echo "seed complete"
