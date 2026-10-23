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

.PHONY: help run stop clean format lint test test-clean \
        deploy deploy-stop deploy-clean deploy-logs deploy-logs-ui deploy-env deploy-setup monitor \
        runner runner-start runner-stop runner-status runner-logs \
        check-env check-docker

##@ General
help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"; printf "$(C_BOLD)Kapa$(C_RESET)  usage: make $(C_GREEN)<target>$(C_RESET)\n"} \
		/^[a-zA-Z_-]+:.*##/ { printf "  $(C_GREEN)%-16s$(C_RESET)%s\n", $$1, $$2 } \
		/^##@/ { printf "\n$(C_BOLD)%s$(C_RESET)\n", substr($$0, 5) }' $(MAKEFILE_LIST)
	@echo

##@ Local
run: check-docker check-env ## Build and start services with production credentials (.env)
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

##@ Quality
format: ## Format code (not available yet)
	@printf "$(C_YEL)format: nothing to do yet (N/A)$(C_RESET)\n"

lint: ## Lint code (not available yet)
	@printf "$(C_YEL)lint: nothing to do yet (N/A)$(C_RESET)\n"

test: check-docker ## Run services with dev credentials, verify health, then clean up
	$(call log,Starting isolated test environment [$(TEST_PROJECT)])
	@trap '$(COMPOSE_TEST) down --volumes --rmi all --remove-orphans >/dev/null 2>&1 || true; \
	       printf "$(C_BLUE)$(C_BOLD)==>$(C_RESET) Test environment cleaned\n"' EXIT; \
	$(COMPOSE_TEST) up -d --build --wait; \
	printf "$(C_BLUE)$(C_BOLD)==>$(C_RESET) Checking postgres readiness\n"; \
	$(COMPOSE_TEST) exec -T postgres sh -c 'pg_isready -U "$$POSTGRES_USER" -d "$$POSTGRES_DB"'; \
	printf "$(C_BLUE)$(C_BOLD)==>$(C_RESET) Running query check\n"; \
	result=$$($(COMPOSE_TEST) exec -T postgres sh -c 'psql -U "$$POSTGRES_USER" -d "$$POSTGRES_DB" -tAc "SELECT 1"'); \
	if [ "$$result" != "1" ]; then \
		printf "$(C_RED)✘ postgres query check failed (got: %s)$(C_RESET)\n" "$$result"; exit 1; \
	fi; \
	printf "$(C_GREEN)✔ All tests passed$(C_RESET)\n"

test-clean: check-docker ## Force-remove leftovers of the test environment
	@$(COMPOSE_TEST) down --volumes --rmi all --remove-orphans >/dev/null 2>&1 || true

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