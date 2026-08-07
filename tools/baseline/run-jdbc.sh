#!/usr/bin/env bash
# Runs the JDBC baseline in both modes, at DBeaver's own default heap limit.
#
# The heap cap matters: measuring at -Xmx8g would hide the fact that a 1M-row
# result nearly exhausts what DBeaver actually ships with.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CACHE="${BASELINE_CACHE:-$HERE/.cache}"
JDBC_VERSION="${JDBC_VERSION:-42.7.4}"
JDBC_JAR="$CACHE/postgresql-$JDBC_VERSION.jar"
# DBeaver's dbeaver.ini default.
HEAP="${HEAP:--Xms64m -Xmx1024m}"

JAVA="${JAVA:-$(command -v java)}"
if [ -z "$JAVA" ]; then
    echo "no java on PATH; set JAVA=/path/to/java (a JDK, not a JRE)" >&2
    exit 1
fi

mkdir -p "$CACHE"
if [ ! -f "$JDBC_JAR" ]; then
    echo "fetching PostgreSQL JDBC $JDBC_VERSION..."
    curl -sfL -o "$JDBC_JAR" \
        "https://repo1.maven.org/maven2/org/postgresql/postgresql/$JDBC_VERSION/postgresql-$JDBC_VERSION.jar" \
        || { echo "download failed" >&2; exit 1; }
fi

echo "=== JDBC stream (heap: $HEAP) ==="
/usr/bin/time -l "$JAVA" $HEAP -cp "$JDBC_JAR" "$HERE/JdbcBench.java" 8192 2>&1 \
    | grep -E "first_row_ms|total_s|rows_per_s|heap_used|maximum resident"

echo
echo "=== JDBC retain 1M (heap: $HEAP) ==="
/usr/bin/time -l "$JAVA" $HEAP -cp "$JDBC_JAR" "$HERE/JdbcBench.java" 8192 --retain 2>&1 \
    | grep -E "first_row_ms|total_s|rows_per_s|retained|heap_used|maximum resident|OutOfMemory"
