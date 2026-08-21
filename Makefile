# Build, test, and benchmark targets.
#
# Two build systems are involved: cargo for the core, SwiftPM for the macOS
# front-end. The front-end links the Rust staticlib, so its targets depend on
# the corresponding cargo build — never invoke `swift build` directly or you may
# link a stale library.

SHELL := /bin/bash
.DEFAULT_GOAL := help

APP_DIR   := apps/macos
APP_BIN   := $(APP_DIR)/.build/release/DbClient
APP_DEBUG := $(APP_DIR)/.build/debug/DbClient

# SwiftPM emits a bare executable, which macOS hands to Terminal instead of
# launching as an application. The shippable artefact is the bundle assembled by
# `make package`, not $(APP_BIN).
#
# Running the bundle's inner executable directly gives full bundle context —
# `Bundle.main` resolves from the executable's path — while keeping stdout on
# the terminal and LaunchServices out of the way. That is what the screenshot
# tooling wants: the artefact users get, without the launch indirection.
APP_BUNDLE     := dist/DbClient.app
APP_BUNDLE_BIN := $(APP_BUNDLE)/Contents/MacOS/DbClient

# swift-format ships inside the Xcode toolchain and is not on PATH, so the
# `command -v` test this replaced never found it: `make fmt` printed a note and
# exited 0 for as long as there have been Swift sources, and none of them were
# ever formatted. A quality gate that silently does nothing is worse than no
# gate, because it reports success — hence the fallback, and hence a missing
# formatter being an error rather than a note.
SWIFT_FORMAT := $(shell command -v swift-format 2>/dev/null || xcrun --find swift-format 2>/dev/null)
SWIFT_FORMAT_MISSING := swift-format not found on PATH or in the Xcode toolchain (xcrun --find swift-format)

# What each test server is called, which port it is published on, which image it
# comes from and what its password is. Read from .env rather than written here,
# because `docker compose` reads that same file to start the containers: a port
# is published by one tool and checked by another, and a number written down
# twice is a number that will one day disagree with itself.
include .env

# Under target/ because it is generated, disposable and already ignored.
PGTLS_CERTS := $(CURDIR)/target/pgtls
# Exported so the tests read the CA from where this put it, rather than from a
# path compiled into them. Their fallback is relative to their own crate, which
# is the same file until `target/` is shared between git worktrees — then a
# cached test binary names the worktree it was built in, and that worktree may
# be gone.
export PGTLS_CA := $(PGTLS_CERTS)/ca.crt

# Where `db-up-ssh` writes the SSH fixture's host keys, exported for the same
# reason as PGTLS_CA above: the tests read the file from where the Makefile put
# it, rather than from a path compiled into them.
export SSH_KNOWN_HOSTS := $(CURDIR)/target/ssh/known_hosts
# Where `db-up-ssh` generates the fixture's key pairs, exported for the same
# reason. Three of them, because the three answers a key can produce need three
# different keys to produce them: one the server knows, one behind a passphrase,
# and one nobody has authorised.
export SSH_KEY_DIR := $(CURDIR)/target/ssh

# How every target that launches the app reaches that database. The application
# has no built-in connection: without --conn it opens the connection form and
# waits for someone to type into it, which no script can do. Derived from
# PG_PORT rather than written out again, so moving the port moves this too.
#
# A URL rather than a libpq keyword string: the scheme is how the core picks a
# driver, and there is deliberately no fallback for a string that names none.
PG_CONN := postgres://bench:bench@127.0.0.1:$(PG_PORT)/bench

# What every `db-check-*` target asks with, on top of whatever it already asked.
#
# The existing checks answer whether the server has finished starting, and most
# of them answer it from inside the container. That is worth knowing and it is
# not what a test needs to know: a client running in the container never crosses
# the port forward, so a gate built only on it reports a healthy server while
# every test fails to reach one. `nc -z` has the opposite problem — Docker's
# forwarder accepts the connection itself, so it succeeds whether or not
# anything is behind it.
#
# `dbcheck` opens the database through the same registry the application uses
# and then makes a round trip, from this process on this machine. Both halves
# stay: keeping the readiness check means "not started yet" and "started, but
# you cannot get to it from here" arrive as different failures.
#
# Ten seconds, which is more than a ready server needs and is not the number
# being defended against. On macOS the first connection a fresh process makes to
# a Docker-forwarded port costs seconds — measured at about four here, against
# well under a millisecond for every connection after it — so a limit chosen for
# how fast a ready database answers would be a limit that fires on the forwarder
# instead, and the gate would fail on a server that is fine.
DBCHECK := cargo run -q -p dbffi --bin dbcheck --
DBCHECK_TIMEOUT := 10

# One URL per server, beside the port it is built from. Written here rather than
# in each target so that the address a gate checks and the address the tests use
# have one place to disagree, and so moving a port moves both.
#
# These are the strings the registry parses, which are not always the strings the
# drivers' own tests pass: ClickHouse and Trino are reached as `clickhouse://`
# and `trino://` here and rewritten to HTTP inside the registry, while those
# tests build their sources directly and hand them `http://`.
PG_URL         := $(PG_CONN)
PGTLS_URL      := postgres://bench:bench@127.0.0.1:$(PGTLS_PORT)/bench?sslmode=require
MONGO_URL      := mongodb://127.0.0.1:$(MONGO_PORT)
REDIS_URL      := redis://127.0.0.1:$(REDIS_PORT)/0
CASSANDRA_URL  := cassandra://127.0.0.1:$(CASSANDRA_PORT)
TRINO_URL      := trino://127.0.0.1:$(TRINO_PORT)
FLIGHTSQL_URL  := flightsql://flight_username:$(FLIGHTSQL_PASSWORD)@127.0.0.1:$(FLIGHTSQL_PORT)/

TOOLS := tools
BASELINE := $(TOOLS)/baseline

.PHONY: help
help: ## Show available targets
	@awk 'BEGIN {FS = ":.*##"; printf "\nTargets:\n"} \
		/^[a-zA-Z_-]+:.*?##/ { printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2 } \
		/^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) }' $(MAKEFILE_LIST)
	@echo

##@ Build

# SwiftPM does not treat the Rust staticlib as an input of its own: it relinks
# when a Swift source changes, and not when the library does. So a core that
# changed under an unchanged front-end links into nothing — the binary stays as
# it was, and every check run against it reports on the core before the change.
#
# Not theoretical. The window could not open a Redis connection for as long as no
# Swift file had changed since the driver was added: `make release` rebuilt the
# library, SwiftPM linked nothing, and the binary went on carrying a registry
# that had never heard of the scheme. Deleting the product when the library
# changes is what makes these two targets mean what they say.
#
# "Changes" cannot be read off a timestamp. Cargo keeps one artifact per profile
# fingerprint and swaps a cached one back into `target/` when a profile knob
# moves — restoring its original mtime with it, in 0.2s. So flipping `lto` and
# flipping it back leaves a library that is *older* than the product but was not
# what the product was linked against, and `-nt` answers "no relink needed" while
# the app still carries the other profile's code. That misread produced a whole
# round of size measurements attributed to the wrong profile.
#
# Recorded identity instead of ordering, so a swap in either time direction is
# caught. Size and mtime together name which cached artifact is in place; inode
# cannot, because cargo writes a fresh one on every swap even when restoring a
# library it has already built.
DBFFI_ID = stat -f '%z:%m' $(1) 2>/dev/null
RELINK   = [ -e $(1) ] && [ "$$($(call DBFFI_ID,$(2)))" = "$$(cat $(1).dbffi 2>/dev/null)" ] || rm -f $(1)
# Written only after SwiftPM returns, so a failed link leaves no claim that the
# product matches the library.
STAMP    = $(call DBFFI_ID,$(2)) > $(1).dbffi

.PHONY: build
build: ## Debug build of core and app
	cargo build
	@$(call RELINK,$(APP_DEBUG),target/debug/libdbffi.a)
	RUST_PROFILE=debug swift build --package-path $(APP_DIR) -c debug
	@$(call STAMP,$(APP_DEBUG),target/debug/libdbffi.a)

.PHONY: release
release: ## Release build of core and app
	cargo build --release
	@$(call RELINK,$(APP_BIN),target/release/libdbffi.a)
	swift build --package-path $(APP_DIR) -c release
	@$(call STAMP,$(APP_BIN),target/release/libdbffi.a)
	@echo "binary: $(APP_BIN)  (run 'make package' for the launchable .app)"

.PHONY: core
core: ## Release build of the Rust core only
	cargo build --release

.PHONY: icon
icon: ## Regenerate the app icon from tools/make-icon.swift
	swift $(TOOLS)/make-icon.swift $(APP_DIR)/Resources/AppIcon.icns

.PHONY: package
package: release ## Bundle + code-sign dist/DbClient.app (ad-hoc; CODESIGN_IDENTITY=... for Developer ID)
	bash $(APP_DIR)/scripts/package.sh

.PHONY: run
run: package ## Build and launch the app
	open "$(APP_BUNDLE)"

.PHONY: run-console
run-console: release ## Launch the raw binary, keeping stdout in the terminal
	./$(APP_BIN)

##@ Test

.PHONY: test
test: ## Unit tests (no database required)
	cargo test --workspace

# Both compatible-MySQL servers are prerequisites, because the suite behind this
# target runs their tests whether or not they are listed here — an unlisted
# server does not make `make test-integration` cheaper, it only makes the failure
# arrive as a connection refused from inside a test instead of as a line naming
# the target that fixes it. StarRocks earns its place on the same measurement:
# under a minute to become ready, and about as much memory as MySQL. Its image is
# five gigabytes, which is a download to do once rather than a cost per run.
.PHONY: test-integration
test-integration: db-check db-check-compatible db-check-mongo db-check-clickhouse db-check-mysql db-check-mssql db-check-tidb db-check-starrocks db-check-redis db-check-cassandra db-check-trino db-check-flightsql ## Tests requiring a database server
	cargo test --workspace -- --ignored

# The same suite, split by which server has to be up. CI runs these as separate
# jobs rather than running the target above: twelve servers on one runner is about
# fourteen gigabytes of images and more memory than StarRocks and SQL Server will
# share, and a runner that dies of that reports a failure that names nothing.
#
# `test-integration` stays the definition of what integration coverage is, and
# these eight are a partition of it — a new crate with `#[ignore]`d tests is
# picked up by `--workspace` above and has to be added to one of these by hand.
#
# Two crates hold tests for more than one database and are split further. `dbddl`
# has a test file per database, so `--test` names its share exactly. The contract
# suite is one binary holding a subject per database, so that one is split by
# test name — `--exact`, because the compatibility subjects carry the driver's
# name as well as their own and a plain `mysql` filter would pull TiDB and
# StarRocks into a job where neither server is running.
#
# Three containers rather than two. `cargo test -p driver-postgres -- --ignored`
# runs every ignored test in that crate, and five of them are the TLS ones,
# which want a second PostgreSQL on a second port — so the job that runs this
# has to start it, and the check below is what says so in one line instead of
# five refused connections.
.PHONY: test-postgres
test-postgres: db-check db-check-compatible db-check-pgtls ## Integration tests behind PostgreSQL and the servers read through its driver
	cargo test -p driver-postgres -p dbffi -p dbtransfer -- --ignored
	cargo test -p dbddl --test postgres -- --ignored
	cargo test -p dbconn --test contract -- --ignored --exact \
		postgres_satisfies_the_contract \
		cockroachdb_satisfies_the_contract_through_the_postgres_driver \
		greptimedb_reads_data_through_the_postgres_driver

.PHONY: test-mysql
test-mysql: db-check-mysql db-check-tidb db-check-starrocks ## Integration tests behind MySQL and the servers read through its driver
	cargo test -p driver-mysql -- --ignored
	cargo test -p dbddl --test mysql -- --ignored
	cargo test -p dbconn --test contract -- --ignored --exact \
		mysql_satisfies_the_contract \
		tidb_satisfies_the_contract_through_the_mysql_driver \
		starrocks_satisfies_the_contract_through_the_mysql_driver

.PHONY: test-mssql
test-mssql: db-check-mssql ## Integration tests behind SQL Server
	cargo test -p driver-mssql -- --ignored
	cargo test -p dbddl --test mssql -- --ignored
	cargo test -p dbconn --test contract -- --ignored --exact mssql_satisfies_the_contract

.PHONY: test-clickhouse
test-clickhouse: db-check-clickhouse ## Integration tests behind ClickHouse
	cargo test -p driver-clickhouse -- --ignored
	cargo test -p dbddl --test clickhouse -- --ignored
	cargo test -p dbconn --test contract -- --ignored --exact clickhouse_satisfies_the_contract

.PHONY: test-mongodb
test-mongodb: db-check-mongo ## Integration tests behind MongoDB
	cargo test -p driver-mongodb -- --ignored
	cargo test -p dbconn --test contract -- --ignored --exact mongodb_satisfies_the_contract

# No `dbddl` line: Redis has no DDL to generate. Its container is the cheapest
# of the lot, so this job is the one to add a second small server to if another
# ever needs a home.
.PHONY: test-redis
test-redis: db-check-redis ## Integration tests behind Redis
	cargo test -p driver-redis -- --ignored
	cargo test -p dbconn --test contract -- --ignored --exact redis_satisfies_the_contract

# Slowest container of the lot to become ready — a minute or so before it will
# answer — which is why it is its own job rather than a passenger on another.
.PHONY: test-cassandra
test-cassandra: db-check-cassandra ## Integration tests behind Cassandra
	cargo test -p driver-cassandra -- --ignored
	cargo test -p dbconn --test contract -- --ignored --exact cassandra_satisfies_the_contract

# The driver's own suite and the contract subject seed different schemas of the
# `memory` catalog, so the two halves of this target do not collide when
# `cargo test --workspace -- --ignored` runs them at once.
.PHONY: test-trino
test-trino: db-check-trino ## Integration tests behind Trino
	cargo test -p driver-trino -- --ignored
	cargo test -p dbconn --test contract -- --ignored --exact trino_satisfies_the_contract

.PHONY: test-flightsql
test-flightsql: db-check-flightsql ## Integration tests behind Arrow Flight SQL
	cargo test -p driver-flightsql -- --ignored
	cargo test -p dbconn --test contract -- --ignored --exact flightsql_satisfies_the_contract

# The SQL statement splitter's checks live behind a flag on the app binary
# rather than in a test target: Package.swift declares one executable target and
# it links the Rust staticlib, so a test target would have to reproduce that
# link. Kept out of `check`, which is a cargo-only gate that builds no Swift, and
# in `test-all`, because a check nothing runs is not a check.
.PHONY: test-swift
test-swift: release ## Swift-side checks, run inside the app binary
	./$(APP_BIN) --verify-splitter
	./$(APP_BIN) --verify-connection
	./$(APP_BIN) --verify-completion
	./$(APP_BIN) --verify-transaction
	./$(APP_BIN) --verify-editing
	./$(APP_BIN) --verify-clipboard
	./$(APP_BIN) --verify-goto
	./$(APP_BIN) --verify-favorites
	./$(APP_BIN) --verify-record
	./$(APP_BIN) --verify-value
	./$(APP_BIN) --verify-browse-state
	./$(APP_BIN) --verify-browse-restore
	./$(APP_BIN) --verify-metadata
	./$(APP_BIN) --verify-schema-metadata
	./$(APP_BIN) --verify-import
	./$(APP_BIN) --verify-preferences
	./$(APP_BIN) --verify-accessibility
	./$(APP_BIN) --verify-quitting
	./$(APP_BIN) --verify-connection-chooser
	./$(APP_BIN) --verify-history
	./$(APP_BIN) --verify-progressive
	./$(APP_BIN) --verify-filter-rows
	./$(APP_BIN) --verify-query-history
	./$(APP_BIN) --verify-query-buffers

# The settings checked against a live window rather than as rules on their own.
# Separate from `test-swift`, which is the set of checks that need no server:
# this one browses a real relation and presses Save, because which side of a
# switch a behaviour sits on is the mistake that compiles, passes every unit
# check, and is invisible until somebody loses a row.
#
# PostgreSQL specifically, and not because that is the container that happens to
# be up: a row of nothing but defaults needs a table whose primary key has a
# default, and `serial` is how the fixture gets one.
.PHONY: test-preferences
test-preferences: release db-check ## Drive each setting both ways through the window
	./$(APP_BIN) --preferences --conn "$(PG_CONN)" --relation prefs_probe

# The two history call sites checked against a live window, for the reason
# `test-preferences` is: `--verify-query-history` pins the store's rules and can
# say nothing about whether a browse or a Save reaches it. This one browses a
# table it made, deletes a row from it, and prints the list after each.
#
# `--history-store` because a probe has no business writing into the history the
# user's own windows share.
.PHONY: test-history
test-history: release db-check ## Prove a browse and a Save reach the statement history
	./$(APP_BIN) --history-probe --conn "$(PG_CONN)" --relation history_probe \
		--history-store dev.dbclient.historyprobe

# The only proof that a window can hold two connections at once. The `--verify-*`
# suites ask what can be asked without a server, which reaches the rules deciding
# whether a second tab appears and stops there: two live handles is the claim,
# and a claim about handles needs a server holding them.
.PHONY: test-sessions
test-sessions: release db-check ## Prove two connections in one window do not touch each other
	./$(APP_BIN) --sessions-probe --conn "$(PG_CONN)" \
		--history-store dev.dbclient.sessionsprobe

.PHONY: test-all
test-all: test test-integration test-swift test-preferences test-history test-sessions ## Every test

##@ Quality

.PHONY: fmt
fmt: ## Format Rust and Swift sources
	cargo fmt --all
	@test -n "$(SWIFT_FORMAT)" || { echo "$(SWIFT_FORMAT_MISSING)"; exit 1; }
	$(SWIFT_FORMAT) format -i -r $(APP_DIR)/Sources

.PHONY: fmt-check
fmt-check: ## Verify formatting without modifying files
	cargo fmt --all -- --check
	@test -n "$(SWIFT_FORMAT)" || { echo "$(SWIFT_FORMAT_MISSING)"; exit 1; }
	@fail=0; for f in $$(find $(APP_DIR)/Sources -name '*.swift'); do \
		$(SWIFT_FORMAT) format "$$f" | diff -q - "$$f" >/dev/null \
			|| { echo "needs formatting: $$f"; fail=1; }; \
	done; test $$fail -eq 0

.PHONY: lint
lint: ## Clippy with warnings denied
	cargo clippy --workspace --all-targets -- -D warnings

.PHONY: check
check: fmt-check lint test ## Everything CI should enforce

##@ Benchmark database

.PHONY: db-up
db-up: ## Start the benchmark PostgreSQL container
	@docker compose up -d pg
# Three minutes, and the container's own log if it runs out. A minute is enough
# on a warm laptop and is not enough on a cold CI runner doing initdb against a
# shared disk — and when it ran out, all that reached the log was `pg_isready`'s
# exit code 2 under a `make` that said "exit code 2". The timeout has to name
# itself, because the alternative is reading tea leaves from a number.
	@echo "waiting for postgres..."
	@for i in $$(seq 1 180); do \
		if docker exec $(PG_CONTAINER) pg_isready -U bench -d bench 2>/dev/null; then exit 0; fi; \
		sleep 1; \
	done; \
	echo "postgres never accepted connections; its last words were:" >&2; \
	docker logs --tail 30 $(PG_CONTAINER) >&2; \
	exit 1

.PHONY: db-down
db-down: ## Stop and remove the benchmark container
	-docker rm -f $(PG_CONTAINER)

.PHONY: db-seed
db-seed: db-up ## Create the 1M-row benchmark table
	$(TOOLS)/seed-bench-db.sh

.PHONY: db-up-pgtls
db-up-pgtls: ## Start the TLS-only PostgreSQL container
	@$(TOOLS)/make-pgtls-certs.sh $(PGTLS_CERTS)
# The certificates are generated first because the container copies them in as
# it starts and will not come up without them — which is also why this target,
# rather than `docker compose up -d pgtls`, is the way to start it.
	@docker compose up -d pgtls
	@echo "waiting for the TLS postgres..."
	@for i in $$(seq 1 180); do \
		if docker exec $(PGTLS_CONTAINER) pg_isready -U bench -d bench 2>/dev/null; then exit 0; fi; \
		sleep 1; \
	done; \
	echo "the TLS postgres never accepted connections; its last words were:" >&2; \
	docker logs --tail 30 $(PGTLS_CONTAINER) >&2; \
	exit 1

.PHONY: db-down-pgtls
db-down-pgtls: ## Stop and remove the TLS container, and its certificates
	-docker rm -f $(PGTLS_CONTAINER)
	-rm -rf $(PGTLS_CERTS)

.PHONY: test-pgtls
test-pgtls: db-up-pgtls ## Run the PostgreSQL TLS tests against that container
	cargo test -p driver-postgres --test tls -- --include-ignored

.PHONY: test-tunnel
test-tunnel: db-up-ssh db-up ## Run the SSH tunnel tests against that container
	cargo test -p dbtunnel -- --include-ignored
	cargo test -p dbffi --lib -- --include-ignored registry::tests

.PHONY: db-up-mongo
db-up-mongo: ## Start the MongoDB test container
	@docker compose up -d mongo
	@echo "waiting for mongodb..."
	@for i in $$(seq 1 60); do \
		docker exec $(MONGO_CONTAINER) mongosh --quiet --eval 'db.runCommand({ping:1})' \
			>/dev/null 2>&1 && break; \
		sleep 1; \
	done
	@docker exec $(MONGO_CONTAINER) mongosh --quiet --eval 'db.runCommand({ping:1}).ok'

.PHONY: db-down-mongo
db-down-mongo: ## Stop and remove the MongoDB test container
	-docker rm -f $(MONGO_CONTAINER)

.PHONY: db-up-redis
db-up-redis: ## Start the Redis test container
	@docker compose up -d redis
	@echo "waiting for redis..."
	@for i in $$(seq 1 60); do \
		docker exec $(REDIS_CONTAINER) redis-cli ping \
			>/dev/null 2>&1 && break; \
		sleep 1; \
	done
	@docker exec $(REDIS_CONTAINER) redis-cli ping

.PHONY: db-down-redis
db-down-redis: ## Stop and remove the Redis test container
	-docker rm -f $(REDIS_CONTAINER)

.PHONY: db-check-redis
db-check-redis: ## Fail unless the Redis test container is reachable
	@docker exec $(REDIS_CONTAINER) redis-cli ping \
		>/dev/null 2>&1 \
		|| { echo "redis not running; run 'make db-up-redis'"; exit 1; }
	@$(DBCHECK) "$(REDIS_URL)" $(DBCHECK_TIMEOUT)

.PHONY: db-up-cassandra
db-up-cassandra: ## Start the Cassandra test container
	@docker compose up -d cassandra
	@echo "waiting for cassandra (this takes a while)..."
	@for i in $$(seq 1 120); do \
		docker exec $(CASSANDRA_CONTAINER) cqlsh -e "describe keyspaces" \
			>/dev/null 2>&1 && break; \
		sleep 2; \
	done
	@docker exec $(CASSANDRA_CONTAINER) cqlsh -e "describe keyspaces"

.PHONY: db-down-cassandra
db-down-cassandra: ## Stop and remove the Cassandra test container
	-docker rm -f $(CASSANDRA_CONTAINER)

.PHONY: db-check-cassandra
db-check-cassandra: ## Fail unless the Cassandra test container is reachable
	@docker exec $(CASSANDRA_CONTAINER) cqlsh -e "describe keyspaces" \
		>/dev/null 2>&1 \
		|| { echo "cassandra not running; run 'make db-up-cassandra'"; exit 1; }
	@$(DBCHECK) "$(CASSANDRA_URL)" $(DBCHECK_TIMEOUT)

# Readiness is asked over HTTP rather than through a client in the image,
# because the thing being waited for is the coordinator answering requests —
# which is what the driver does and what `/v1/info` reports with `starting`.
.PHONY: db-up-trino
db-up-trino: ## Start the Trino test container
	@docker compose up -d trino
	@echo "waiting for trino..."
	@for i in $$(seq 1 120); do \
		curl -sf http://127.0.0.1:$(TRINO_PORT)/v1/info \
			| grep -q '"starting":false' && break; \
		sleep 2; \
	done
	@curl -sf http://127.0.0.1:$(TRINO_PORT)/v1/info

.PHONY: db-down-trino
db-down-trino: ## Stop and remove the Trino test container
	-docker rm -f $(TRINO_CONTAINER)

.PHONY: db-check-trino
db-check-trino: ## Fail unless the Trino test container is reachable
	@curl -sf http://127.0.0.1:$(TRINO_PORT)/v1/info | grep -q '"starting":false' \
		|| { echo "trino not running; run 'make db-up-trino'"; exit 1; }
	@$(DBCHECK) "$(TRINO_URL)" $(DBCHECK_TIMEOUT)

.PHONY: db-up-flightsql
db-up-flightsql: ## Start the Arrow Flight SQL test container
	@docker compose up -d flightsql
	@echo "waiting for flight sql..."
	@for i in $$(seq 1 60); do \
		docker exec $(FLIGHTSQL_CONTAINER) flight_sql_client --command Execute \
			--query 'SELECT 1' --username flight_username \
			--password $(FLIGHTSQL_PASSWORD) --host localhost --port 31337 \
			>/dev/null 2>&1 && break; \
		sleep 2; \
	done
	@docker exec $(FLIGHTSQL_CONTAINER) flight_sql_client --command Execute \
		--query 'SELECT count(*) FROM lineitem' --username flight_username \
		--password $(FLIGHTSQL_PASSWORD) --host localhost --port 31337

.PHONY: db-down-flightsql
db-down-flightsql: ## Stop and remove the Arrow Flight SQL test container
	-docker rm -f $(FLIGHTSQL_CONTAINER)

.PHONY: db-check-flightsql
db-check-flightsql: ## Fail unless the Arrow Flight SQL test container is reachable
	@docker exec $(FLIGHTSQL_CONTAINER) flight_sql_client --command Execute \
		--query 'SELECT 1' --username flight_username \
		--password $(FLIGHTSQL_PASSWORD) --host localhost --port 31337 \
		>/dev/null 2>&1 \
		|| { echo "flight sql not running; run 'make db-up-flightsql'"; exit 1; }
	@$(DBCHECK) "$(FLIGHTSQL_URL)" $(DBCHECK_TIMEOUT)

.PHONY: db-up-compatible
db-up-compatible: ## Start the PostgreSQL-compatible databases (CockroachDB, GreptimeDB)
	@docker compose up -d cockroach greptime
# `break` where the other targets fail: running out of patience here left the
# recipe returning success with nothing listening, so the miss surfaced later as
# `db-check-compatible` telling someone to run the target they had just run.
	@echo "waiting for the compatible databases..."
	@for i in $$(seq 1 180); do \
		if nc -z 127.0.0.1 $(COCKROACH_PORT) >/dev/null 2>&1 \
			&& nc -z 127.0.0.1 $(GREPTIME_PORT) >/dev/null 2>&1; then exit 0; fi; \
		sleep 1; \
	done; \
	echo "cockroachdb or greptimedb never opened its port; their last words were:" >&2; \
	docker logs --tail 30 $(COCKROACH_CONTAINER) >&2; \
	docker logs --tail 30 $(GREPTIME_CONTAINER) >&2; \
	exit 1

.PHONY: db-down-compatible
db-down-compatible: ## Stop and remove the PostgreSQL-compatible containers
	-docker rm -f $(COCKROACH_CONTAINER) $(GREPTIME_CONTAINER)

# Missing until now, which made these two the one pair whose absence reached the
# suite as a connection refused from inside a test rather than as the line that
# names the target to run — the failure mode the comment on `test-integration`
# says the checks exist to prevent.
.PHONY: db-check-compatible
db-check-compatible: ## Fail unless the PostgreSQL-compatible containers are reachable
	@nc -z 127.0.0.1 $(COCKROACH_PORT) >/dev/null 2>&1 \
		|| { echo "cockroachdb not running; run 'make db-up-compatible'"; exit 1; }
	@nc -z 127.0.0.1 $(GREPTIME_PORT) >/dev/null 2>&1 \
		|| { echo "greptimedb not running; run 'make db-up-compatible'"; exit 1; }
	@$(DBCHECK) "postgres://root@127.0.0.1:$(COCKROACH_PORT)/defaultdb" $(DBCHECK_TIMEOUT)
	@$(DBCHECK) "postgres://greptime@127.0.0.1:$(GREPTIME_PORT)/public" $(DBCHECK_TIMEOUT)

.PHONY: db-up-tidb
db-up-tidb: ## Start the TiDB test container
	@docker compose up -d tidb
	@echo "waiting for tidb..."
	@for i in $$(seq 1 60); do \
		nc -z 127.0.0.1 $(TIDB_PORT) >/dev/null 2>&1 && break; \
		sleep 1; \
	done

.PHONY: db-down-tidb
db-down-tidb: ## Stop and remove the TiDB test container
	-docker rm -f $(TIDB_CONTAINER)

.PHONY: db-check-tidb
db-check-tidb: ## Fail unless the TiDB test container is reachable
	@nc -z 127.0.0.1 $(TIDB_PORT) >/dev/null 2>&1 \
		|| { echo "tidb not running; run 'make db-up-tidb'"; exit 1; }
	@$(DBCHECK) "mysql://root@127.0.0.1:$(TIDB_PORT)/" $(DBCHECK_TIMEOUT)

# StarRocks brings up a frontend and a backend inside one container and will not
# answer until both have registered with each other. Measured here: ready about
# eight seconds after `docker start`, and about fifty on the first run of a fresh
# container, when five gigabytes of image are still being read off disk. The
# budget is ten minutes anyway, because the number that matters is the one on the
# slowest machine that will ever run this and it is not this one.
#
# The poll is a real login rather than `nc` because the MySQL port opens well
# before the cluster will answer a query: a port check reports ready and the
# first statement then fails.
.PHONY: db-up-starrocks
db-up-starrocks: ## Start the StarRocks test container
	@docker compose up -d starrocks
	@echo "waiting for starrocks (a minute or so on a cold start)..."
	@for i in $$(seq 1 300); do \
		docker exec $(STARROCKS_CONTAINER) \
			mysql -h 127.0.0.1 -P 9030 -u root -e 'SELECT 1' >/dev/null 2>&1 && break; \
		sleep 2; \
	done
	@docker exec $(STARROCKS_CONTAINER) \
		mysql -h 127.0.0.1 -P 9030 -u root -e 'SELECT 1' >/dev/null \
		&& echo "starrocks ready"

.PHONY: db-down-starrocks
db-down-starrocks: ## Stop and remove the StarRocks test container
	-docker rm -f $(STARROCKS_CONTAINER)

.PHONY: db-check-starrocks
db-check-starrocks: ## Fail unless the StarRocks test container is reachable
	@docker exec $(STARROCKS_CONTAINER) \
		mysql -h 127.0.0.1 -P 9030 -u root -e 'SELECT 1' >/dev/null 2>&1 \
		|| { echo "starrocks not running; run 'make db-up-starrocks'"; exit 1; }
	@$(DBCHECK) "mysql://root@127.0.0.1:$(STARROCKS_PORT)/" $(DBCHECK_TIMEOUT)

.PHONY: db-up-mysql
db-up-mysql: ## Start the MySQL test container
	@docker compose up -d mysql
	@echo "waiting for mysql..."
	@for i in $$(seq 1 60); do \
		docker exec $(MYSQL_CONTAINER) mysqladmin ping -uroot -ptest --silent \
			>/dev/null 2>&1 && break; \
		sleep 1; \
	done

.PHONY: db-down-mysql
db-down-mysql: ## Stop and remove the MySQL test container
	-docker rm -f $(MYSQL_CONTAINER)

.PHONY: db-check-mysql
db-check-mysql: ## Fail unless the MySQL test container is reachable
	@docker exec $(MYSQL_CONTAINER) mysqladmin ping -uroot -ptest --silent >/dev/null 2>&1 \
		|| { echo "mysql not running; run 'make db-up-mysql'"; exit 1; }
# No database named, as in `db-check-tidb`. `bench` is created by the driver's
# own integration test, so asking for it here made the gate assert a fixture as
# well as a server — and since `test-mysql` depends on this gate, a container
# that had never run the test could never run it.
	@$(DBCHECK) "mysql://root:test@127.0.0.1:$(MYSQL_PORT)/" $(DBCHECK_TIMEOUT)

.PHONY: db-up-mssql
db-up-mssql: ## Start the SQL Server test container
	@docker compose up -d mssql
	@echo "waiting for sql server (emulated; this takes a while)..."
	@for i in $$(seq 1 180); do \
		nc -z 127.0.0.1 $(MSSQL_PORT) >/dev/null 2>&1 && break; \
		sleep 1; \
	done

.PHONY: db-down-mssql
db-down-mssql: ## Stop and remove the SQL Server test container
	-docker rm -f $(MSSQL_CONTAINER)

.PHONY: db-check-mssql
db-check-mssql: ## Fail unless the SQL Server test container is reachable
	@nc -z 127.0.0.1 $(MSSQL_PORT) >/dev/null 2>&1 \
		|| { echo "sql server not running; run 'make db-up-mssql'"; exit 1; }
	# `TrustServerCertificate` because the image signs its own certificate with
	# one nothing here has a reason to trust — the same parameter the contract
	# suite connects with, for the same reason.
# `master` rather than `dbclient_contract`, which the contract suite creates:
# see the note on `db-check-mysql`. Reaching for it on a fresh container did not
# even fail clearly — the missing database surfaced as a TDS read error.
	@$(DBCHECK) "sqlserver://sa:Str0ng%21Passw0rd@127.0.0.1:$(MSSQL_PORT)/master?TrustServerCertificate=true" \
		$(DBCHECK_TIMEOUT)

# Alone among these targets this one only starts a server, because ClickHouse's
# HTTP interface refuses a body holding more than one statement. seed.sql can
# only be applied a statement per request, which is what the integration test
# already does with it.
.PHONY: db-up-clickhouse
db-up-clickhouse: ## Start the ClickHouse test container
	@docker compose up -d clickhouse
	@echo "waiting for clickhouse..."
	@for i in $$(seq 1 60); do \
		curl -sf "http://default:test@127.0.0.1:$(CLICKHOUSE_PORT)/?query=SELECT+1" \
			>/dev/null 2>&1 && break; \
		sleep 1; \
	done
	@curl -sf "http://default:test@127.0.0.1:$(CLICKHOUSE_PORT)/?query=SELECT+1" \
		|| { echo "$(CLICKHOUSE_CONTAINER) never answered; see 'docker logs $(CLICKHOUSE_CONTAINER)'"; exit 1; }

.PHONY: db-down-clickhouse
db-down-clickhouse: ## Stop and remove the ClickHouse test container
	-docker rm -f $(CLICKHOUSE_CONTAINER)

.PHONY: db-check-clickhouse
db-check-clickhouse: ## Fail unless the ClickHouse test container is reachable
	@curl -sf "http://default:test@127.0.0.1:$(CLICKHOUSE_PORT)/?query=SELECT+1" >/dev/null 2>&1 \
		|| { echo "clickhouse not running; run 'make db-up-clickhouse'"; exit 1; }
# `default` rather than `bench`, which the seed script creates: see the note on
# `db-check-mysql` for why a readiness gate must not also ask for a fixture.
	@$(DBCHECK) "clickhouse://default:test@127.0.0.1:$(CLICKHOUSE_PORT)/default" $(DBCHECK_TIMEOUT)

.PHONY: db-check
db-check: ## Fail unless the benchmark database is reachable
	@docker exec $(PG_CONTAINER) pg_isready -U bench -d bench >/dev/null 2>&1 \
		|| { echo "benchmark database not running; run 'make db-seed'"; exit 1; }
	@$(DBCHECK) "$(PG_URL)" $(DBCHECK_TIMEOUT)

# Its own check rather than a line in `db-check`, because it is a second server
# on a second port and the first one's answer says nothing about it. `nc` asks
# only whether anything is listening; the round trip after it asks in the one
# way that suits this container, with `sslmode=require` — a plaintext connection
# is what this server exists to refuse.
.PHONY: db-check-pgtls
db-check-pgtls: ## Fail unless the TLS-only PostgreSQL container is reachable
	@nc -z 127.0.0.1 $(PGTLS_PORT) >/dev/null 2>&1 \
		|| { echo "tls postgres not running; run 'make db-up-pgtls'"; exit 1; }
	@$(DBCHECK) "$(PGTLS_URL)" $(DBCHECK_TIMEOUT)

# Separate from db-check because the benchmarks want PostgreSQL and nothing
# else: making every `make bench` depend on a MongoDB container would be asking
# for a server that no benchmark touches.
.PHONY: db-check-mongo
db-check-mongo: ## Fail unless the MongoDB test container is reachable
	@docker exec $(MONGO_CONTAINER) mongosh --quiet --eval 'db.runCommand({ping:1})' \
		>/dev/null 2>&1 \
		|| { echo "mongodb not running; run 'make db-up-mongo'"; exit 1; }
	@$(DBCHECK) "$(MONGO_URL)" $(DBCHECK_TIMEOUT)

.PHONY: db-up-ssh
db-up-ssh: ## Start the SSH server the tunnel tests connect through
	@docker compose up -d ssh
	@echo "waiting for sshd..."
	@for i in $$(seq 1 60); do \
		docker exec $(SSH_CONTAINER) test -f /config/sshd/sshd_config 2>/dev/null && break; \
		sleep 1; \
	done
# The image ships `AllowTcpForwarding no`, and a forward is the whole reason
# this container exists — so without this it comes up healthy and refuses every
# tunnel with `administratively prohibited`, which reads like a bug in the
# client. Rewritten in place rather than mounted over, because the image
# generates this file on first start and would overwrite a bind mount; guarded
# by the grep so that starting an already-corrected container does not restart
# it for nothing.
	@docker exec $(SSH_CONTAINER) grep -q '^AllowTcpForwarding yes' /config/sshd/sshd_config \
		|| { docker exec $(SSH_CONTAINER) \
				sed -i 's/^AllowTcpForwarding no/AllowTcpForwarding yes/' /config/sshd/sshd_config \
			&& docker restart $(SSH_CONTAINER) >/dev/null; }
# Waited for by reading the banner rather than with `nc -z`, which Docker's own
# forwarder answers whether or not sshd is behind it yet: the port is open the
# moment the container starts, so a `-z` loop fell through immediately and the
# check below then failed against a server that was two seconds from ready.
	@for i in $$(seq 1 60); do \
		nc -w 5 127.0.0.1 $(SSH_PORT) </dev/null 2>/dev/null | grep -q '^SSH-2\.0' && break; \
		sleep 1; \
	done
# The host keys are written down here rather than left to whoever runs the
# tests, for the same reason the TLS certificates are: the tunnel refuses to
# send a password to a server it has no record of, so a fixture with no
# known_hosts file is a fixture that cannot be used at all. Rewritten every
# time, because `docker rm` throws the host keys away with the container and a
# stale file would fail as a changed key — which is the one failure here that
# is supposed to mean something.
	@mkdir -p $(dir $(SSH_KNOWN_HOSTS))
# Generated once and kept, because `ssh-keygen` will not overwrite and because
# there is nothing here worth regenerating: these live under target/, which is
# already ignored, and they authorise a container bound to the loopback address.
# `dbclient_stranger` is deliberately never installed — a key the server has
# never heard of is the only way to check that a refusal reads as a refusal.
	@test -f $(SSH_KEY_DIR)/dbclient_test \
		|| ssh-keygen -q -t ed25519 -N "" -C dbclient-test -f $(SSH_KEY_DIR)/dbclient_test
	@test -f $(SSH_KEY_DIR)/dbclient_locked \
		|| ssh-keygen -q -t ed25519 -N hunter2 -C dbclient-locked -f $(SSH_KEY_DIR)/dbclient_locked
	@test -f $(SSH_KEY_DIR)/dbclient_stranger \
		|| ssh-keygen -q -t ed25519 -N "" -C dbclient-stranger -f $(SSH_KEY_DIR)/dbclient_stranger
# Installed with `docker exec` rather than through the image's PUBLIC_KEY, for
# the same reason the forwarding fix above is: the image builds this file on
# first start, so a value passed at create time is one a recreated container
# silently loses. Rewritten whole every time, which is what keeps a regenerated
# key from stacking up beside the one it replaced.
	@cat $(SSH_KEY_DIR)/dbclient_test.pub $(SSH_KEY_DIR)/dbclient_locked.pub \
		| docker exec -i $(SSH_CONTAINER) sh -c \
			'mkdir -p /config/.ssh && cat >/config/.ssh/authorized_keys \
				&& chown -R $(SSH_USER) /config/.ssh \
				&& chmod 700 /config/.ssh && chmod 600 /config/.ssh/authorized_keys'
	@ssh-keyscan -p $(SSH_PORT) -H 127.0.0.1 >$(SSH_KNOWN_HOSTS) 2>/dev/null
	@test -s $(SSH_KNOWN_HOSTS) \
		|| { echo "ssh-keyscan wrote nothing to $(SSH_KNOWN_HOSTS)"; exit 1; }
	@$(MAKE) --no-print-directory db-check-ssh

.PHONY: db-down-ssh
db-down-ssh: ## Stop and remove the SSH test container
	-docker rm -f $(SSH_CONTAINER)

# Both halves, as everywhere else here: the config check answers whether this
# server will forward at all, and the banner read answers whether it can be
# reached from this machine through the port forward. `nc -z` alone would say
# yes to Docker's forwarder with nothing behind it.
.PHONY: db-check-ssh
db-check-ssh: ## Fail unless the SSH test container is up and will forward
	@docker exec $(SSH_CONTAINER) grep -q '^AllowTcpForwarding yes' /config/sshd/sshd_config \
		2>/dev/null \
		|| { echo "ssh server not running or not forwarding; run 'make db-up-ssh'"; exit 1; }
	@nc -w 5 127.0.0.1 $(SSH_PORT) </dev/null 2>/dev/null | grep -q '^SSH-2\.0' \
		|| { echo "ssh server did not answer on $(SSH_PORT); run 'make db-up-ssh'"; exit 1; }

# The one target that knows about all of them, which is only possible now that
# one file does. Worth having because the per-server `db-down-*` targets each
# name a container, so clearing the machine meant remembering which of fifteen
# had been started — and the ones left behind are exactly the ones nobody
# remembered.
#
# `--profile "*"` is not optional. Every service sits behind a profile so that a
# bare `docker compose up` cannot start all fifteen at once, and `down` obeys
# the same filter: without it this target selects nothing, removes nothing and
# exits 0 — a target that reports having cleared the machine and has not.
.PHONY: db-down-all
db-down-all: ## Stop and remove every test container
	docker compose --profile "*" down --remove-orphans

##@ Benchmarks

.PHONY: bench
bench: bench-core bench-app ## Every benchmark

.PHONY: bench-core
bench-core: db-check ## Core throughput: PostgreSQL to Arrow
	cargo run --release --example bench -p driver-postgres -- 8192
	@echo
	cargo run --release --example bench -p driver-postgres -- 8192 --retain

# The benchmarks stay on $(APP_BIN) deliberately. They measure the render path,
# where nothing in the bundle participates, and keeping them off `package` keeps
# a code-signing step out of the measurement loop.

.PHONY: bench-app
bench-app: release db-check ## Scroll frame times over 1M rows
	./$(APP_BIN) --bench --conn "$(PG_CONN)"

.PHONY: bench-verify
bench-verify: release db-check ## Prove result buffers cross the FFI without copying
	./$(APP_BIN) --bench --verify --conn "$(PG_CONN)"

# Screenshots are how rendering and layout defects get caught, so they capture
# the bundled app: Info.plist decides appearance and the menu's name, and a
# screenshot of the unbundled binary would not show either.
.PHONY: screenshot
screenshot: package db-check ## Capture the app window: make screenshot OUT=/tmp/grid.png TAB=content
	swift $(TOOLS)/capture-window.swift "$(or $(OUT),/tmp/grid.png)" \
		./$(APP_BUNDLE_BIN) --conn "$(PG_CONN)" --tab "$(or $(TAB),content)"

##@ Baseline

.PHONY: baseline
baseline: baseline-jdbc ## Baseline measurements against the Java implementation

.PHONY: baseline-jdbc
baseline-jdbc: db-check ## JDBC data-layer throughput, for comparison
	$(BASELINE)/run-jdbc.sh

.PHONY: baseline-startup
baseline-startup: ## Startup time of a GUI binary: make baseline-startup EXE=/path/to/bin
	@test -n "$(EXE)" || { echo "usage: make baseline-startup EXE=/path/to/binary"; exit 1; }
	swift $(BASELINE)/launchtime.swift 120 "$(EXE)" $(ARGS)

##@ Housekeeping

.PHONY: clean
clean: ## Remove build artifacts
	cargo clean
	rm -rf $(APP_DIR)/.build dist
