# =============================================================================
#  Kapa - Root Makefile
#  Single entry point to operate every service in the repository.
# =============================================================================

SHELL         := /bin/bash
.SHELLFLAGS   := -eu -o pipefail -c
.DEFAULT_GOAL := help
MAKEFLAGS     += --no-print-directory

# -----------------------------------------------------------------------------
#  Project
# -----------------------------------------------------------------------------
PROJECT           := kapa
# Services with their own Makefile (format/lint/check-docs/test/bench/deps are delegated).
SERVICES          := whatsapp core tachyon
TEST_PROJECT      ?= $(PROJECT)-test
COMPOSE_FILE      := docker-compose.yml
COMPOSE_TEST_FILE := docker-compose.test.yml

PROD_ENV     := .env
DEV_ENV      := .env.dev
EXAMPLE_ENV  := .env.example

# stop/clean must work even if .env was never created, so fall back to the example.
PROD_ENV_SAFE := $(if $(wildcard $(PROD_ENV)),$(PROD_ENV),$(EXAMPLE_ENV))

COMPOSE      := docker compose -f $(COMPOSE_FILE)
PROD_PROFILES := --profile ops
COMPOSE_PROD := $(COMPOSE) --env-file $(PROD_ENV) -p $(PROJECT) $(PROD_PROFILES)
COMPOSE_SAFE := $(COMPOSE) --env-file $(PROD_ENV_SAFE) -p $(PROJECT) $(PROD_PROFILES)
COMPOSE_TEST := $(COMPOSE) -f $(COMPOSE_TEST_FILE) --env-file $(DEV_ENV) -p $(TEST_PROJECT)

# -----------------------------------------------------------------------------
#  Deployment (override on the command line, e.g. make monitor WATCH=5)
# -----------------------------------------------------------------------------
DEPLOY_HOST ?= prike
DEPLOY_DIR  ?= /opt/kapa
KAPA_SSH    ?= ssh
LOG_LINES   ?= 20
WATCH       ?=
SERVICE     ?=
GREP        ?=
LEVEL       ?=
SINCE       ?=
FOLLOW      ?= 1
LOGS_UI_PORT ?= 8090
FORCE       ?=

# Self-hosted GitHub Actions runner (on demand, this machine)
RUNNER_USER ?= github-runner
RUNNER_DIR  ?= /home/$(RUNNER_USER)/actions-runner
RUNNER_IDLE ?= 30
export RUNNER_USER RUNNER_DIR RUNNER_IDLE

export PROJECT DEPLOY_HOST DEPLOY_DIR KAPA_SSH LOG_LINES WATCH SERVICE \
       GREP LEVEL SINCE FOLLOW LOGS_UI_PORT FORCE

# Pretty output
C_RESET := \033[0m
C_BOLD  := \033[1m
C_BLUE  := \033[34m
C_GREEN := \033[32m
C_RED   := \033[31m
C_YEL   := \033[33m

define log
	@printf "$(C_BLUE)$(C_BOLD)==>$(C_RESET) %s\n" "$(1)"
endef

.PHONY: help run stop clean format lint check-docs test test-infra test-clean bench deps \
        update-whatsmeow pair-test \
        deploy deploy-check-gpu deploy-tachyon deploy-tachyon-eval deploy-secrets deploy-check-sqlserver deploy-sample-sqlserver deploy-sync-sqlserver sage-summary deploy-stop deploy-clean deploy-logs deploy-logs-ui deploy-env deploy-setup monitor \
        runner runner-start runner-stop runner-status runner-logs \
        check-env check-docker

##@ General
help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"; printf "$(C_BOLD)Kapa$(C_RESET)  usage: make $(C_GREEN)<target>$(C_RESET)\n"} \
		/^[a-zA-Z_-]+:.*##/ { printf "  $(C_GREEN)%-16s$(C_RESET)%s\n", $$1, $$2 } \
		/^##@/ { printf "\n$(C_BOLD)%s$(C_RESET)\n", substr($$0, 5) }' $(MAKEFILE_LIST)
	@echo

##@ Local
run: check-docker check-env deps ## Build and start services with production credentials (.env)
	$(call log,Building and starting services [production])
	@$(COMPOSE_PROD) up -d --build --wait
	@printf "$(C_GREEN)✔ Services are up and healthy$(C_RESET)\n"
	@$(COMPOSE_PROD) ps

stop: check-docker ## Stop and remove running containers (data is kept)
	$(call log,Stopping services)
	@$(COMPOSE_SAFE) down --remove-orphans
	@printf "$(C_GREEN)✔ Services stopped$(C_RESET)\n"

clean: check-docker ## Remove containers, volumes and built images (DESTROYS DATA)
	$(call log,Removing containers + volumes + images)
	@$(COMPOSE_SAFE) down --volumes --rmi all --remove-orphans
	@$(COMPOSE_TEST) down --volumes --rmi all --remove-orphans 2>/dev/null || true
	@printf "$(C_GREEN)✔ Clean complete$(C_RESET)\n"

##@ Quality (every service)
format: ## Format code of every service
	@for svc in $(SERVICES); do $(MAKE) -C services/$$svc format; done

lint: ## Lint every service (linters, complexity, proto, generated code)
	@for svc in $(SERVICES); do $(MAKE) -C services/$$svc lint; done

check-docs: ## Check documentation of every service and proto contract
	@for svc in $(SERVICES); do $(MAKE) -C services/$$svc check-docs; done

test: test-infra ## Full test: stack smoke test + unit and integration tests of every service
	@for svc in $(SERVICES); do $(MAKE) -C services/$$svc test; done

bench: ## Benchmarks of every service under perf (BENCH=regex BENCHTIME=1s)
	@for svc in $(SERVICES); do $(MAKE) -C services/$$svc bench; done

deps: ## Make sure every service has resolved dependencies (go.sum, Cargo.lock...)
	@for svc in $(SERVICES); do $(MAKE) -C services/$$svc ready; done

update-whatsmeow: ## WhatsApp protocol changed? Upgrade whatsmeow and test it
	@$(MAKE) -C services/whatsapp update-whatsmeow

pair-test: ## One-time: link the WhatsApp test phone for the live tests
	@$(MAKE) -C services/whatsapp pair-test

test-infra: check-docker deps ## Stack smoke test: every container healthy, gateway protocol up to date
	$(call log,Starting isolated test environment [$(TEST_PROJECT)])
	@trap '$(COMPOSE_TEST) down --volumes --rmi all --remove-orphans >/dev/null 2>&1 || true; \
	       printf "$(C_BLUE)$(C_BOLD)==>$(C_RESET) Test environment cleaned\n"' EXIT; \
	$(COMPOSE_TEST) up -d --build --wait; \
	printf "$(C_BLUE)$(C_BOLD)==>$(C_RESET) Checking postgres\n"; \
	result=$$($(COMPOSE_TEST) exec -T postgres sh -c 'psql -U "$$POSTGRES_USER" -d "$$POSTGRES_DB" -tAc "SELECT 1"'); \
	if [ "$$result" != "1" ]; then \
		printf "$(C_RED)✘ postgres query check failed (got: %s)$(C_RESET)\n" "$$result"; exit 1; \
	fi; \
	printf "$(C_BLUE)$(C_BOLD)==>$(C_RESET) Checking whatsapp gateway (health + protocol)\n"; \
	$(COMPOSE_TEST) exec -T whatsapp /whatsapp -healthcheck; \
	rc=0; $(COMPOSE_TEST) exec -T whatsapp /whatsapp -status || rc=$$?; \
	if [ "$$rc" = 2 ]; then \
		printf "$(C_YEL)! whatsmeow upstream is ahead of the pinned build (warning only)$(C_RESET)\n"; \
	elif [ "$$rc" != 0 ]; then \
		printf "$(C_RED)✘ WhatsApp rejected the client or the gateway failed: see above$(C_RESET)\n"; exit 1; \
	fi; \
	printf "$(C_BLUE)$(C_BOLD)==>$(C_RESET) Checking core service (health + status)\n"; \
	$(COMPOSE_TEST) exec -T core /core --healthcheck; \
	$(COMPOSE_TEST) exec -T core /core --status; \
	printf "$(C_GREEN)✔ Stack smoke test passed$(C_RESET)\n"

test-clean: check-docker ## Force-remove leftovers of the test environments
	@$(COMPOSE_TEST) down --volumes --rmi all --remove-orphans >/dev/null 2>&1 || true
	@for svc in $(SERVICES); do $(MAKE) -C services/$$svc testdb-stop; done

##@ Deployment (host: DEPLOY_HOST, default prike)
deploy: ## Sync the repo to the host and (re)start services
	@bash scripts/deploy.sh

monitor: ## Dashboard: health, CPU/RAM/GPU/disk, logs (WATCH=5, SERVICE=, LOG_LINES=)
	@bash scripts/monitor.sh

deploy-logs: ## Logs: SERVICE=a,b GREP=re LEVEL=error|warn SINCE=1h FOLLOW=0
	@bash scripts/deploy-logs.sh

deploy-logs-ui: ## Open the web log viewer via SSH tunnel (http://localhost:LOGS_UI_PORT)
	@bash scripts/deploy-logs-ui.sh

deploy-stop: ## EMERGENCY: stop all services on the host (FORCE=1 skips prompt)
	@bash scripts/deploy-stop.sh

deploy-clean: ## DESTRUCTIVE: remove services, images AND data volumes on the host
	@bash scripts/deploy-clean.sh

deploy-check-sqlserver: ## Check the Sage SQL Server from the host: read-only login + schema
	@bash scripts/deploy-check-sqlserver.sh

deploy-sync-sqlserver: ## Live Sage check on the host, then export schema + masked sample to services/core/testdata
	@bash scripts/deploy-check-sqlserver.sh && bash scripts/deploy-sample-sqlserver.sh

deploy-sample-sqlserver: ## Sage schema + masked sample -> services/core/testdata/sage.local.json
	@bash scripts/deploy-sample-sqlserver.sh

sage-summary: ## Rebuild services/core/testdata/sage-summary.local.md from the Sage fixture
	@python3 scripts/sage-summary.py services/core/testdata/sage.local.json services/core/testdata/sage-summary.local.md
	@echo "services/core/testdata/sage-summary.local.md"

deploy-check-gpu: ## Check the host can run Tachyon: driver, GPU, NVIDIA Container Toolkit
	@bash scripts/deploy-check-gpu.sh

deploy-tachyon: ## Run the tachyon CLI on the host's container (ARGS="doctor" | "pull gemma-4-e4b" | "ps")
	@bash scripts/deploy-tachyon.sh $(ARGS)

deploy-tachyon-eval: ## Test + bench gemma-4-e4b, laya-multilingual and the embedding model on the host (PULL=1 APPROVE=1 SAVE_BASELINE=1 EMBED=)
	@PULL='$(PULL)' APPROVE='$(APPROVE)' SAVE_BASELINE='$(SAVE_BASELINE)' EMBED='$(EMBED)' bash scripts/deploy-tachyon-eval.sh

deploy-secrets: ## Upload services/*/secrets/*.prod.env to the host (asks for sudo there)
	@bash scripts/deploy-secrets.sh

deploy-env: ## Upload local .env to the host as the production config
	@bash scripts/deploy-env.sh

deploy-setup: ## One-time host setup: Docker, NVIDIA driver + container toolkit
	@bash scripts/deploy-setup.sh

##@ CI runner (on demand, this machine)
runner-start: ## Start the GitHub runner in background (auto-stops after RUNNER_IDLE min idle)
	@bash scripts/runner.sh start

runner-stop: ## Stop the GitHub runner (asks if a job is running; FORCE=1 skips)
	@bash scripts/runner.sh stop

runner-status: ## Show whether the runner is online, busy or idle
	@bash scripts/runner.sh status

runner-logs: ## Follow the runner log
	@bash scripts/runner.sh logs

runner: ## Run the GitHub runner in the foreground until Ctrl+C
	@bash scripts/runner.sh run

# -----------------------------------------------------------------------------
#  Internal checks
# -----------------------------------------------------------------------------
check-env:
	@if [ ! -f "$(PROD_ENV)" ]; then \
		printf "$(C_RED)✘ $(PROD_ENV) not found.$(C_RESET) Create it with: cp $(EXAMPLE_ENV) $(PROD_ENV)\n"; \
		exit 1; \
	fi

check-docker:
	@command -v docker >/dev/null 2>&1 || { printf "$(C_RED)✘ docker is not installed$(C_RESET)\n"; exit 1; }
	@docker compose version >/dev/null 2>&1 || { printf "$(C_RED)✘ docker compose v2 is required$(C_RESET)\n"; exit 1; }