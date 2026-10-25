# =============================================================================
#  Kapa - core service (Rust)
#  Every target runs inside the tools image (Dockerfile target "tools"):
#  the only host requirement is Docker. Called directly or from the root Makefile.
# =============================================================================

SHELL         := /bin/bash
.SHELLFLAGS   := -eu -o pipefail -c
.DEFAULT_GOAL := help
MAKEFLAGS     += --no-print-directory

SERVICE   := core
ROOT      := $(abspath ../..)
UID       := $(shell id -u)
GID       := $(shell id -g)
TOOLS_IMG := kapa-$(SERVICE)-tools
TTY       := $(shell [ -t 0 ] && echo -t)
SOURCES   := src/*.rs tests/*.rs benches/*.rs

# Ephemeral test database (no volume: every run starts clean).
TEST_NET  := kapa-core-test
TEST_DB   := kapa-core-testdb
TEST_URL  := postgres://kapa_test:kapa_test@$(TEST_DB):5432/postgres
# Live Gmail test: the dev accounts file (gitignored); skipped when absent.
DEV_ACCOUNTS := secrets/accounts.dev.env

DOCKER    := docker run --rm -v $(ROOT):/src -w /src/services/$(SERVICE)
RUN       := $(DOCKER) $(TTY) -u $(UID):$(GID) -v kapa-cargo-cache-$(UID):/cache $(TOOLS_IMG)
RUN_DB    := $(DOCKER) $(TTY) -u $(UID):$(GID) -v kapa-cargo-cache-$(UID):/cache --network $(TEST_NET) \
             -e TEST_DATABASE_URL=$(TEST_URL) $(if $(wildcard $(DEV_ACCOUNTS)),-e ACCOUNTS_FILE=$(DEV_ACCOUNTS)) $(TOOLS_IMG)
RUN_PERF  := $(DOCKER) $(TTY) --privileged -u 0 -v kapa-cargo-cache-perf:/cache -e BENCH -e BENCHTIME $(TOOLS_IMG)

.PHONY: help tools ready format lint check-docs test test-unit test-integration testdb testdb-stop bench secrets-perms build clean

help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"; printf "Core service targets:\n"} /^[a-zA-Z_-]+:.*##/ { printf "  \033[32m%-18s\033[0m%s\n", $$1, $$2 }' $(MAKEFILE_LIST)

tools: ## Build the tools image (first build takes a few minutes; cached afterwards)
	@if docker image inspect $(TOOLS_IMG) >/dev/null 2>&1; then \
	   docker build -q --target tools -t $(TOOLS_IMG) . >/dev/null; \
	 else \
	   printf '\033[34m\033[1m==>\033[0m Building %s (first time only: Rust toolchain, clippy, perf; a few minutes)\n' '$(TOOLS_IMG)'; \
	   docker build --progress=plain --target tools -t $(TOOLS_IMG) .; \
	 fi

ready: ## Dependencies resolved (Cargo.lock is committed: nothing to do)
	@test -f Cargo.lock

format: tools ## rustfmt
	$(RUN) cargo fmt

lint: tools ## rustfmt check, clippy (pedantic, warnings are errors), lizard complexity
	$(RUN) sh -c 'cargo fmt --check && cargo clippy --locked --all-targets -q -- -D warnings && \
	  lizard --CCN 10 --length 40 --arguments 6 --warnings_only $(SOURCES)'

check-docs: tools ## Every item documented (missing_docs + clippy) and rustdoc without warnings
	$(RUN) sh -c 'RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --document-private-items -q'

test: test-unit test-integration ## Unit + integration tests

test-unit: tools ## Unit tests: no database, no network (fake IMAP/SMTP servers)
	$(RUN) cargo test --locked --lib

test-integration: tools ## PostgreSQL + live Gmail (live part needs secrets/accounts.dev.env)
	@$(MAKE) testdb
	@rc=0; $(RUN_DB) cargo test --locked --test integration -- --nocapture || rc=$$?; $(MAKE) testdb-stop; exit $$rc

testdb:
	@docker network inspect $(TEST_NET) >/dev/null 2>&1 || docker network create $(TEST_NET) >/dev/null
	@docker rm -f $(TEST_DB) >/dev/null 2>&1 || true
	@docker run -d --name $(TEST_DB) --network $(TEST_NET) -e POSTGRES_USER=kapa_test -e POSTGRES_PASSWORD=kapa_test postgres:18-alpine >/dev/null
	@for _ in $$(seq 1 60); do \
	   docker exec $(TEST_DB) pg_isready -h 127.0.0.1 -U kapa_test -d postgres >/dev/null 2>&1 && exit 0; sleep 1; \
	 done; echo "test database did not become ready"; exit 1

testdb-stop: ## Remove the test database
	@docker rm -f $(TEST_DB) >/dev/null 2>&1 || true

bench: tools ## Benchmarks under perf (BENCH=case BENCHTIME=1s) -> bench/report.txt
	@rc=0; $(RUN_PERF) sh -c 'bash scripts/bench.sh; rc=$$?; chown -R $(UID):$(GID) bench; exit $$rc' || rc=$$?; exit $$rc

secrets-perms: ## Local `make run`: let the container user (uid 65532) read the secrets (0400)
	@chown 65532:65532 secrets/*.dev.env secrets/*.prod.env 2>/dev/null || sudo chown 65532:65532 secrets/*.dev.env secrets/*.prod.env
	@chmod 0400 secrets/*.dev.env secrets/*.prod.env
	@ls -l secrets/

build: ## Build the runtime image
	docker build --target runtime -t kapa-$(SERVICE):local .

clean: testdb-stop ## Remove tools image and build caches
	@docker rmi -f $(TOOLS_IMG) >/dev/null 2>&1 || true
	@docker volume rm kapa-cargo-cache-$(UID) kapa-cargo-cache-perf >/dev/null 2>&1 || true
	@rm -rf bench
