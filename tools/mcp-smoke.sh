#!/usr/bin/env bash
# Talks to the MCP server over a real socket, and says what came back.
#
# `--verify-mcp` holds every rule this server has and holds none of them over a
# wire: parsing, routing, the four walls and the dispatcher are pure functions,
# checked as pure functions. That leaves the listener, the connection-per-request
# read loop and the live data source — the parts that only exist once something
# connects — with no coverage at all. This is the something.
#
# SQLite rather than the benchmark database, so the fixture is a file this script
# writes and no password has to reach the server from anywhere: what is being
# checked is the wire, and a credential store in the middle of it would only add
# a way for the check to fail while the server is fine.
#
# Usage: tools/mcp-smoke.sh <path-to-DbClient-binary> [port]
set -uo pipefail

BIN=${1:?usage: mcp-smoke.sh <DbClient binary> [port]}
PORT=${2:-8791}
WORK=$(mktemp -d)
trap 'kill ${APP:-0} 2>/dev/null; rm -rf "$WORK"' EXIT

failures=0
say() { printf '%s\n' "$*"; }
expect() { # got, want, what
    if [ "$1" = "$2" ]; then
        say "  ok    $3"
    else
        failures=$((failures + 1))
        say "  FAIL  $3"
        say "        want: $2"
        say "        got:  $1"
    fi
}

# The fixture: two connections at the same file, one exposed and one not, so
# that the flag is doing the filtering rather than the file being short.
DB="$WORK/smoke.db"
sqlite3 "$DB" <<'SQL'
CREATE TABLE widgets (id integer primary key, name text not null, price real);
INSERT INTO widgets (name, price) VALUES ('bolt', 1.5), ('nut', 0.25), ('gasket', NULL);
CREATE VIEW cheap AS SELECT * FROM widgets WHERE price < 1;
SQL

mkdir -p "$WORK/config/dbclient"
cat > "$WORK/config/dbclient/connections.json" <<EOF
{"connections": [
  {"id": "2E7C4B1A-9F3D-4C88-B0A1-7E5D6C3F2A10", "name": "exposed",
   "scheme": "sqlite", "host": "", "port": "", "database": "", "user": "",
   "path": "$DB", "exposedToMCP": true},
  {"id": "3F8D5C2B-A04E-4D99-C1B2-8F6E7D4A3B21", "name": "hidden",
   "scheme": "sqlite", "host": "", "port": "", "database": "", "user": "",
   "path": "$DB", "exposedToMCP": false}
]}
EOF

XDG_CONFIG_HOME="$WORK/config" "$BIN" --mcp-probe "$PORT" >"$WORK/app.log" 2>&1 &
APP=$!
for _ in $(seq 1 60); do
    grep -q '^mcp: ' "$WORK/app.log" && break
    sleep 0.5
done
TOKEN=$(sed -n 's/^mcp: token //p' "$WORK/app.log" | head -1)
if [ -z "$TOKEN" ]; then
    say "mcp-smoke: the server never started"
    sed -n 1,20p "$WORK/app.log"
    exit 1
fi

URL="http://127.0.0.1:$PORT/mcp"
AUTH="Authorization: Bearer $TOKEN"
JSON="Content-Type: application/json"

status() { curl -s -o /dev/null -w '%{http_code}' "$@" "$URL"; }
# The tool's answer text, which is where a tool failure says why.
tool() { # body, extra headers...
    local body="$1"; shift
    curl -s -H "$AUTH" -H "$JSON" "$@" -d "$body" "$URL" \
        | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["content"][0]["text"])'
}
field() { python3 -c 'import json,sys; print(json.dumps(json.loads(sys.stdin.read())[sys.argv[1]]))' "$1"; }

say "the session"
curl -s -D "$WORK/init.head" -o "$WORK/init.body" -H "$AUTH" -H "$JSON" \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}' "$URL"
expect "$(head -1 "$WORK/init.head" | tr -d '\r')" "HTTP/1.1 200 OK" "initialize is answered"
SESSION=$(sed -n 's/^[Mm]cp-[Ss]ession-[Ii]d: //p' "$WORK/init.head" | tr -d '\r')
expect "$([ -n "$SESSION" ] && echo yes)" "yes" "and hands back a session id"
SID="Mcp-Session-Id: $SESSION"

say "the walls"
expect "$(status -H "$JSON" -d '{}')" "401" "no token is no entry"
expect "$(status -H 'Authorization: Bearer wrong' -d '{}')" "401" "and a wrong one the same"
expect "$(status -H "$AUTH" -H 'Origin: https://evil.example' -d '{}')" "403" \
    "a browser origin is refused before the token is read"
expect "$(status -H "$AUTH" -H "$JSON" -d '{"jsonrpc":"2.0","id":9,"method":"tools/list"}')" "404" \
    "a call naming no session is told to initialize"
expect "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/health")" "200" \
    "health answers with no token at all"

say "the tools"
expect "$(tool '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_connections","arguments":{}}}' -H "$SID" | field connections)" \
    '["exposed"]' "only the marked connection exists"
expect "$(tool '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_relations","arguments":{"connection":"exposed"}}}' -H "$SID" \
    | python3 -c 'import json,sys; print(",".join(sorted(r["name"]+":"+r["kind"] for r in json.load(sys.stdin)["relations"])))')" \
    "cheap:view,widgets:table" "the table and the view, each named for what it is"
expect "$(tool '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"describe_relation","arguments":{"connection":"exposed","relation":"widgets"}}}' -H "$SID" \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); print(",".join(c["name"]+":"+str(c["nullable"]) for c in d["columns"]), "ddl" if d.get("ddl") else "no-ddl")')" \
    "id:True,name:False,price:True ddl" "columns with their nullability, and the DDL SQLite can write"
QUERY=$(tool '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"query","arguments":{"connection":"exposed","sql":"SELECT name, price FROM widgets ORDER BY id"}}}' -H "$SID")
expect "$(printf '%s' "$QUERY" | field rowCount)" "3" "every row comes back"
expect "$(printf '%s' "$QUERY" | python3 -c 'import json,sys; print(json.load(sys.stdin)["rows"][2]["price"])')" \
    "None" "and SQL NULL arrives as JSON null, not as the word"

say "the refusals"
expect "$(tool '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"query","arguments":{"connection":"exposed","sql":"DELETE FROM widgets"}}}' -H "$SID" | head -1)" \
    "Only reads run over MCP: SELECT, SHOW, EXPLAIN, DESCRIBE." "a write is refused with the guard's sentence"
expect "$(tool '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"list_relations","arguments":{"connection":"hidden"}}}' -H "$SID" | cut -d';' -f1)" \
    "No connection named hidden is exposed to MCP" "an unexposed connection is an absence, not a door"

say "the cap"
# Seven three-row tables is 2187 rows, which is over the default cap of 1000.
CAPPED=$(tool '{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"query","arguments":{"connection":"exposed","sql":"SELECT a.id, b.id, c.id, d.id, e.id, f.id, g.id FROM widgets a, widgets b, widgets c, widgets d, widgets e, widgets f, widgets g"}}}' -H "$SID")
expect "$(printf '%s' "$CAPPED" | field rowCount)" "1000" "a long result stops at the cap"
expect "$(printf '%s' "$CAPPED" | field truncated)" "true" "and says so"
expect "$(printf '%s' "$CAPPED" | field columns)" '["id", "id_2", "id_3", "id_4", "id_5", "id_6", "id_7"]' \
    "seven columns of one name are told apart"

say "the ending"
expect "$(status -X DELETE -H "$AUTH" -H "$SID")" "200" "DELETE ends the session"
expect "$(status -H "$AUTH" -H "$JSON" -H "$SID" -d '{"jsonrpc":"2.0","id":10,"method":"tools/list"}')" "404" \
    "and the id it named stops working"

if [ "$failures" -eq 0 ]; then
    say "mcp-smoke: all checks passed"
else
    say "mcp-smoke: $failures check(s) failed"
fi
exit $((failures > 0))
