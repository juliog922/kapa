# =============================================================================
#  Kapa - WhatsApp gateway
#  Every target runs inside the tools image (Dockerfile target "tools"):
#  the only host requirement is Docker. Called directly or from the root Makefile.
# =============================================================================

SHELL         := /bin/bash
.SHELLFLAGS   := -eu -o pipefail -c
.DEFAULT_GOAL := help
MAKEFLAGS     += --no-print-directory

SERVICE   := whatsapp
ROOT      := $(abspath ../..)
UID       := $(shell id -u)
GID       := $(shell id -g)
TOOLS_IMG := kapa-$(SERVICE)-tools
TTY       := $(shell [ -t 0 ] && echo -t)
PROTO     := ../../proto
PROTO_PKG := $(PROTO)/kapa/whatsapp/v1
GO_FILES  := main.go main_test.go integration_test.go bench_test.go

# Persistent test database: holds the sessions of the two linked test devices.
TEST_NET  := kapa-wa-test
TEST_DB   := kapa-wa-testdb
TEST_VOL  := kapa-wa-test-sessions
TEST_PG_DSN := host=$(TEST_DB) user=kapa_test password=kapa_test sslmode=disable
ENV_TEST  := $(wildcard .env.test)
INTEGRATION_TESTS := ^Test(StoreMigrations|DeleteWipes|AllowlistPersistence|UpstreamProtocol|Live)

DOCKER    := docker run --rm -v $(ROOT):/src -w /src/services/$(SERVICE)
RUN       := $(DOCKER) $(TTY) -u $(UID):$(GID) -v kapa-go-cache-$(UID):/cache $(TOOLS_IMG)
TEST_ENV  := --network $(TEST_NET) -e TEST_PG_DSN="$(TEST_PG_DSN)" $(if $(ENV_TEST),--env-file .env.test) \
             -e WA_TEST_PHONE -e WA_TEST_PAIRING -e WA_LOG_LEVEL
RUN_DB    := $(DOCKER) $(TTY) -u $(UID):$(GID) -v kapa-go-cache-$(UID):/cache $(TEST_ENV) $(TOOLS_IMG)
RUN_DB_IT := $(DOCKER) -it -u $(UID):$(GID) -v kapa-go-cache-$(UID):/cache $(TEST_ENV) -e WA_PAIR=1 $(TOOLS_IMG)
# perf needs root + privileges; it gets its own cache volume so root never owns the shared one.
RUN_PERF  := $(DOCKER) $(TTY) --privileged -u 0 -v kapa-go-cache-perf:/cache -e BENCH -e BENCHTIME $(TOOLS_IMG)

.PHONY: help tools deps proto format lint check-docs test test-unit test-integration \
        testdb testdb-stop test-reset pair-test bench bench-baseline update-whatsmeow build clean

help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"; printf "WhatsApp gateway targets:\n"} /^[a-zA-Z_-]+:.*##/ { printf "  \033[32m%-18s\033[0m%s\n", $$1, $$2 }' $(MAKEFILE_LIST)

tools: ## Build the tools image (first build takes a few minutes; cached afterwards)
	@if docker image inspect $(TOOLS_IMG) >/dev/null 2>&1; then \
	   docker build -q --target tools -t $(TOOLS_IMG) . >/dev/null; \
	 else \
	   printf '\033[34m\033[1m==>\033[0m Building %s (first time only: Go toolchain, linters, perf; a few minutes)\n' '$(TOOLS_IMG)'; \
	   docker build --progress=plain --target tools -t $(TOOLS_IMG) .; \
	 fi

# Only when missing (fresh clone before the first `make deps`): never rebuilt on servers.
go.sum:
	@$(MAKE) tools
	$(RUN) go mod tidy

deps: tools ## Resolve dependencies (go mod tidy); commit go.mod and go.sum
	$(RUN) go mod tidy

proto: tools ## Regenerate pb/ from proto/kapa/whatsapp/v1
	$(RUN) buf generate $(PROTO) --template buf.gen.yaml --path $(PROTO_PKG)

format: tools go.sum ## gofumpt + goimports, buf format
	$(RUN) sh -c 'golangci-lint fmt ./... && buf format -w $(PROTO) --path $(PROTO_PKG)'

lint: tools go.sum ## golangci-lint, buf lint, lizard complexity, generated code up to date
	$(RUN) sh -c '\
	  golangci-lint run ./... && \
	  buf format -d --exit-code $(PROTO) --path $(PROTO_PKG) && \
	  buf lint $(PROTO) --path $(PROTO_PKG) && \
	  lizard --CCN 10 --length 40 --arguments 6 --warnings_only main.go && \
	  rm -rf /tmp/pbcheck && buf generate $(PROTO) --template buf.gen.yaml --path $(PROTO_PKG) -o /tmp/pbcheck && \
	  diff -r pb /tmp/pbcheck/pb >/dev/null || { echo "pb/ is stale: run make proto"; exit 1; }'

check-docs: tools ## Doc comment on every Go declaration; proto documented (buf COMMENTS)
	$(RUN) sh -c 'bash ../../scripts/ci/check-go-docs.sh $(GO_FILES) && buf lint $(PROTO) --path $(PROTO_PKG)'

test: test-unit test-integration ## Unit + integration tests, race detector on

test-unit: tools go.sum ## Unit tests: no database, no network
	$(RUN) go test -race -count=1 ./...

test-integration: tools go.sum ## PostgreSQL + WhatsApp (live part needs WA_TEST_PHONE and make pair-test)
	@$(MAKE) testdb
	@rc=0; $(RUN_DB) go test -tags integration -race -count=1 -timeout 15m -v -run '$(INTEGRATION_TESTS)' . || rc=$$?; \
	 $(MAKE) testdb-stop; exit $$rc

pair-test: tools go.sum ## One-time: link the test phone twice (devices A and B) for the live tests
	@phone="$${WA_TEST_PHONE:-$$(sed -n 's/^WA_TEST_PHONE=//p' .env.test 2>/dev/null)}"; \
	 if [ -z "$$phone" ]; then \
	   echo "WA_TEST_PHONE is not set: put your test number in services/whatsapp/.env.test"; \
	   echo "  (cp .env.test.example .env.test, then WA_TEST_PHONE=+34600111222)"; exit 1; \
	 fi; \
	 echo "Pairing test devices for +$${phone#+}: keep WhatsApp open on that phone."
	@$(MAKE) testdb
	@printf '\033[34m\033[1m==>\033[0m Pairing the test phone as devices A and B.\n'
	@printf '    Compiling, then a pairing code appears below for each device (lives ~1 min; fresh ones are printed).\n'
	@printf '    On the phone: WhatsApp > Linked devices > Link a device > Link with phone number instead.\n\n'
	@rc=0; $(RUN_DB_IT) go test -tags integration -count=1 -timeout 10m -v -run '^TestPairTestDevices$$' . || rc=$$?; \
	 $(MAKE) testdb-stop; exit $$rc

testdb:
	@docker network inspect $(TEST_NET) >/dev/null 2>&1 || docker network create $(TEST_NET) >/dev/null
	@if [ "$$(docker inspect -f '{{.State.Running}}' $(TEST_DB) 2>/dev/null)" != "true" ]; then \
	   docker rm -f $(TEST_DB) >/dev/null 2>&1 || true; \
	   docker run -d --name $(TEST_DB) --network $(TEST_NET) -e POSTGRES_USER=kapa_test -e POSTGRES_PASSWORD=kapa_test \
	     -v $(TEST_VOL):/var/lib/postgresql postgres:18-alpine >/dev/null; \
	 fi
	@for _ in $$(seq 1 60); do \
	   docker exec $(TEST_DB) pg_isready -h 127.0.0.1 -U kapa_test -d postgres >/dev/null 2>&1 && exit 0; sleep 1; \
	 done; echo "test database did not become ready"; exit 1

testdb-stop: ## Stop the test database (paired test sessions are kept)
	@docker rm -f $(TEST_DB) >/dev/null 2>&1 || true

test-reset: testdb-stop ## Forget the paired test sessions (then remove them on the phone too)
	@docker volume rm $(TEST_VOL) >/dev/null 2>&1 || true
	@echo "Test sessions removed. Also remove the 2 'Kapa' entries in WhatsApp > Linked devices."

bench: tools go.sum ## Benchmarks under perf (BENCH=regex BENCHTIME=1s) -> bench/report.txt
	@rc=0; $(RUN_PERF) sh -c 'bash scripts/bench.sh; rc=$$?; chown -R $(UID):$(GID) bench; exit $$rc' || rc=$$?; exit $$rc

bench-baseline: ## Keep the last bench run as the baseline for future comparisons
	@cp bench/latest.txt bench/baseline.txt && echo "bench/baseline.txt updated"

update-whatsmeow: tools ## Move whatsmeow to WHATSMEOW=<commit|main> deliberately, then unit-test it
	@test -n "$(WHATSMEOW)" || { echo "Usage: make update-whatsmeow WHATSMEOW=<commit|main>  (current: $$(grep 'go.mau.fi/whatsmeow ' go.mod))"; exit 1; }
	$(RUN) sh -c 'go get go.mau.fi/whatsmeow@$(WHATSMEOW) && go mod tidy'
	@$(MAKE) test-unit
	@echo "whatsmeow moved to $(WHATSMEOW). Validate before committing: make test (live), and one pairing"
	@echo "check with WA_TEST_PAIRING=1 (costs one code). Then commit go.mod + go.sum and open a PR."

build: ## Build the runtime image
	docker build --target runtime -t kapa-$(SERVICE):local .

clean: testdb-stop ## Remove tools image and build caches (test sessions are kept)
	@docker rmi -f $(TOOLS_IMG) >/dev/null 2>&1 || true
	@docker volume rm kapa-go-cache-$(UID) kapa-go-cache-perf >/dev/null 2>&1 || true
	@rm -rf bench
