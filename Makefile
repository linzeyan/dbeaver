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

.PHONY: test-integration
test-integration: db-check ## Tests requiring the benchmark database
	cargo test --workspace -- --ignored

.PHONY: test-all
test-all: test test-integration ## Every test

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

.PHONY: db-check
db-check: ## Fail unless the benchmark database is reachable
	@docker exec $(PG_CONTAINER) pg_isready -U bench -d bench >/dev/null 2>&1 \
		|| { echo "benchmark database not running; run 'make db-seed'"; exit 1; }

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
	./$(APP_BIN) --bench

.PHONY: bench-verify
bench-verify: release db-check ## Prove result buffers cross the FFI without copying
	./$(APP_BIN) --bench --verify

# Screenshots are how rendering and layout defects get caught, so they capture
# the bundled app: Info.plist decides appearance and the menu's name, and a
# screenshot of the unbundled binary would not show either.
.PHONY: screenshot
screenshot: package db-check ## Capture the app window: make screenshot OUT=/tmp/grid.png TAB=content
	swift $(TOOLS)/capture-window.swift "$(or $(OUT),/tmp/grid.png)" \
		./$(APP_BUNDLE_BIN) --tab "$(or $(TAB),content)"

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
