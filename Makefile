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

# Benchmark database. Ports are non-default to avoid colliding with a local
# PostgreSQL install.
PG_CONTAINER := pg-bench
PG_PORT      := 55432
PG_IMAGE     := postgres:17

# MongoDB, for the driver's own tests and for its pass through the shared
# contract. Non-default port for the same reason as PostgreSQL's. It holds no
# benchmark data — every test seeds and drops the database it uses, so this
# container needs nothing done to it beyond being up.
MONGO_CONTAINER := mongo-test
MONGO_PORT      := 57017
MONGO_IMAGE     := mongo:7

# Redis, for the driver's own tests. Non-default port for the same reason as
# PostgreSQL's.
REDIS_CONTAINER := redis-test
REDIS_PORT      := 56379
REDIS_IMAGE     := redis:7

# Cassandra, for the driver's own tests. Non-default port for the same reason as
# PostgreSQL's. It takes a long time to start.
CASSANDRA_CONTAINER := cassandra-test
CASSANDRA_PORT      := 59042
CASSANDRA_IMAGE     := cassandra:5

# How every target that launches the app reaches that database. The application
# has no built-in connection: without --conn it opens the connection form and
# waits for someone to type into it, which no script can do. Derived from
# PG_PORT rather than written out again, so moving the port moves this too.
#
# A URL rather than a libpq keyword string: the scheme is how the core picks a
# driver, and there is deliberately no fallback for a string that names none.
PG_CONN := postgres://bench:bench@127.0.0.1:$(PG_PORT)/bench

TOOLS := tools
BASELINE := $(TOOLS)/baseline

.PHONY: help
help: ## Show available targets
	@awk 'BEGIN {FS = ":.*##"; printf "\nTargets:\n"} \
		/^[a-zA-Z_-]+:.*?##/ { printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2 } \
		/^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) }' $(MAKEFILE_LIST)
	@echo

##@ Build

.PHONY: build
build: ## Debug build of core and app
	cargo build
	RUST_PROFILE=debug swift build --package-path $(APP_DIR) -c debug

.PHONY: release
release: ## Release build of core and app
	cargo build --release
	swift build --package-path $(APP_DIR) -c release
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
test-integration: db-check db-check-compatible db-check-mongo db-check-clickhouse db-check-mysql db-check-mssql db-check-tidb db-check-starrocks ## Tests requiring a database server
	cargo test --workspace -- --ignored

# The same suite, split by which server has to be up. CI runs these as separate
# jobs rather than running the target above: nine servers on one runner is about
# fourteen gigabytes of images and more memory than StarRocks and SQL Server will
# share, and a runner that dies of that reports a failure that names nothing.
#
# `test-integration` stays the definition of what integration coverage is, and
# these five are a partition of it — a new crate with `#[ignore]`d tests is
# picked up by `--workspace` above and has to be added to one of these by hand.
#
# Two crates hold tests for more than one database and are split further. `dbddl`
# has a test file per database, so `--test` names its share exactly. The contract
# suite is one binary holding a subject per database, so that one is split by
# test name — `--exact`, because the compatibility subjects carry the driver's
# name as well as their own and a plain `mysql` filter would pull TiDB and
# StarRocks into a job where neither server is running.
.PHONY: test-postgres
test-postgres: db-check db-check-compatible ## Integration tests behind PostgreSQL and the servers read through its driver
	cargo test -p driver-postgres -p dbffi -- --ignored
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
	./$(APP_BIN) --verify-metadata

.PHONY: test-all
test-all: test test-integration test-swift ## Every test

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
	@docker start $(PG_CONTAINER) 2>/dev/null \
		|| docker run -d --name $(PG_CONTAINER) \
			-e POSTGRES_PASSWORD=bench -e POSTGRES_DB=bench -e POSTGRES_USER=bench \
			-p $(PG_PORT):5432 $(PG_IMAGE) \
			-c shared_buffers=2GB -c work_mem=256MB -c max_wal_size=8GB -c fsync=off
	@echo "waiting for postgres..."
	@for i in $$(seq 1 60); do \
		docker exec $(PG_CONTAINER) pg_isready -U bench -d bench >/dev/null 2>&1 && break; \
		sleep 1; \
	done
	@docker exec $(PG_CONTAINER) pg_isready -U bench -d bench

.PHONY: db-down
db-down: ## Stop and remove the benchmark container
	-docker rm -f $(PG_CONTAINER)

.PHONY: db-seed
db-seed: db-up ## Create the 1M-row benchmark table
	$(TOOLS)/seed-bench-db.sh

.PHONY: db-up-mongo
db-up-mongo: ## Start the MongoDB test container
	@docker start $(MONGO_CONTAINER) 2>/dev/null \
		|| docker run -d --name $(MONGO_CONTAINER) \
			-p $(MONGO_PORT):27017 $(MONGO_IMAGE)
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
	@docker start $(REDIS_CONTAINER) 2>/dev/null \
		|| docker run -d --name $(REDIS_CONTAINER) \
			-p $(REDIS_PORT):6379 $(REDIS_IMAGE)
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

.PHONY: db-up-cassandra
db-up-cassandra: ## Start the Cassandra test container
	@docker start $(CASSANDRA_CONTAINER) 2>/dev/null \
		|| docker run -d --name $(CASSANDRA_CONTAINER) \
			-p $(CASSANDRA_PORT):9042 $(CASSANDRA_IMAGE)
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

# The two databases that exist to prove protocol compatibility rather than to
# be supported: they are read by the PostgreSQL driver and no code of their own,
# so what these containers test is that the claim is true.
COCKROACH_CONTAINER := cockroach-test
COCKROACH_PORT      := 56257
GREPTIME_CONTAINER  := greptime-test
GREPTIME_PORT       := 54003

.PHONY: db-up-compatible
db-up-compatible: ## Start the PostgreSQL-compatible databases (CockroachDB, GreptimeDB)
	@docker start $(COCKROACH_CONTAINER) 2>/dev/null \
		|| docker run -d --name $(COCKROACH_CONTAINER) \
			-p $(COCKROACH_PORT):26257 cockroachdb/cockroach:v24.1.5 \
			start-single-node --insecure
	@docker start $(GREPTIME_CONTAINER) 2>/dev/null \
		|| docker run -d --name $(GREPTIME_CONTAINER) \
			-p $(GREPTIME_PORT):4003 greptime/greptimedb:latest standalone start \
			--postgres-addr 0.0.0.0:4003 --rpc-bind-addr 0.0.0.0:4001 --http-addr 0.0.0.0:4000
	@echo "waiting for the compatible databases..."
	@for i in $$(seq 1 60); do \
		nc -z 127.0.0.1 $(COCKROACH_PORT) >/dev/null 2>&1 \
			&& nc -z 127.0.0.1 $(GREPTIME_PORT) >/dev/null 2>&1 && break; \
		sleep 1; \
	done

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

# The same argument on the other protocol: TiDB and StarRocks are read by the
# MySQL driver and no code of their own. Both take `root` with no password,
# which is their own default rather than a setting chosen here.
TIDB_CONTAINER      := tidb-test
TIDB_PORT           := 54000
STARROCKS_CONTAINER := starrocks-test
STARROCKS_PORT      := 59030

.PHONY: db-up-tidb
db-up-tidb: ## Start the TiDB test container
	@docker start $(TIDB_CONTAINER) 2>/dev/null \
		|| docker run -d --name $(TIDB_CONTAINER) \
			-p $(TIDB_PORT):4000 pingcap/tidb:latest
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
	@docker start $(STARROCKS_CONTAINER) 2>/dev/null \
		|| docker run -d --name $(STARROCKS_CONTAINER) \
			-p $(STARROCKS_PORT):9030 starrocks/allin1-ubuntu:latest
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

# MySQL. No seeding here: the driver's own tests build the fixture themselves,
# because a fixture kept in a Makefile drifts away from the assertions that read
# it and nobody notices until one of them fails for the wrong reason.
MYSQL_CONTAINER := mysql-test
MYSQL_PORT      := 53306
MYSQL_IMAGE     := mysql:8

.PHONY: db-up-mysql
db-up-mysql: ## Start the MySQL test container
	@docker start $(MYSQL_CONTAINER) 2>/dev/null \
		|| docker run -d --name $(MYSQL_CONTAINER) \
			-e MYSQL_ROOT_PASSWORD=test -e MYSQL_DATABASE=test \
			-p $(MYSQL_PORT):3306 $(MYSQL_IMAGE)
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

# SQL Server. Microsoft publishes no ARM64 build — not for 2019, 2022 or 2025 —
# so on Apple silicon this runs under Rosetta and needs `--platform`. Azure SQL
# Edge is the usual ARM64 substitute and is the wrong fixture: it is a reduced
# engine, and half of what the tests exercise is the full `sys.*` surface and
# the CLR types it does not have.
MSSQL_CONTAINER := mssql-test
MSSQL_PORT      := 51433
MSSQL_PASSWORD  := Str0ng!Passw0rd
MSSQL_IMAGE     := mcr.microsoft.com/mssql/server:2022-latest

.PHONY: db-up-mssql
db-up-mssql: ## Start the SQL Server test container
	@docker start $(MSSQL_CONTAINER) 2>/dev/null \
		|| docker run -d --name $(MSSQL_CONTAINER) --platform linux/amd64 \
			-e ACCEPT_EULA=Y -e 'MSSQL_SA_PASSWORD=$(MSSQL_PASSWORD)' -e MSSQL_PID=Developer \
			-p $(MSSQL_PORT):1433 $(MSSQL_IMAGE)
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

# ClickHouse, whose fixture is a seed script rather than a few inserts: the
# driver's argument is about types, so the table has to contain the types it
# argues about.
CLICKHOUSE_CONTAINER := clickhouse-test
CLICKHOUSE_PORT      := 58123
CLICKHOUSE_IMAGE     := clickhouse/clickhouse-server:24

.PHONY: db-up-clickhouse
db-up-clickhouse: ## Start the ClickHouse test container and seed it
	@docker start $(CLICKHOUSE_CONTAINER) 2>/dev/null \
		|| docker run -d --name $(CLICKHOUSE_CONTAINER) \
			-p $(CLICKHOUSE_PORT):8123 -p 59000:9000 -e CLICKHOUSE_PASSWORD=test \
			--ulimit nofile=262144:262144 $(CLICKHOUSE_IMAGE)
	@echo "waiting for clickhouse..."
	@for i in $$(seq 1 60); do \
		curl -sf "http://default:test@127.0.0.1:$(CLICKHOUSE_PORT)/?query=SELECT+1" \
			>/dev/null 2>&1 && break; \
		sleep 1; \
	done
	@curl -sf "http://default:test@127.0.0.1:$(CLICKHOUSE_PORT)/" \
		--data-binary @crates/drivers/clickhouse/tests/seed.sql >/dev/null \
		&& echo "clickhouse seeded"

.PHONY: db-down-clickhouse
db-down-clickhouse: ## Stop and remove the ClickHouse test container
	-docker rm -f $(CLICKHOUSE_CONTAINER)

.PHONY: db-check-clickhouse
db-check-clickhouse: ## Fail unless the ClickHouse test container is reachable
	@curl -sf "http://default:test@127.0.0.1:$(CLICKHOUSE_PORT)/?query=SELECT+1" >/dev/null 2>&1 \
		|| { echo "clickhouse not running; run 'make db-up-clickhouse'"; exit 1; }

.PHONY: db-check
db-check: ## Fail unless the benchmark database is reachable
	@docker exec $(PG_CONTAINER) pg_isready -U bench -d bench >/dev/null 2>&1 \
		|| { echo "benchmark database not running; run 'make db-seed'"; exit 1; }

# Separate from db-check because the benchmarks want PostgreSQL and nothing
# else: making every `make bench` depend on a MongoDB container would be asking
# for a server that no benchmark touches.
.PHONY: db-check-mongo
db-check-mongo: ## Fail unless the MongoDB test container is reachable
	@docker exec $(MONGO_CONTAINER) mongosh --quiet --eval 'db.runCommand({ping:1})' \
		>/dev/null 2>&1 \
		|| { echo "mongodb not running; run 'make db-up-mongo'"; exit 1; }

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
