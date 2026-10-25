# =============================================================================
#  Kapa - tachyon service: the Tachyon inference engine as a PREBUILT binary.
#  Tachyon's source stays in its own project (TACHYON_SRC); this repository holds
#  bin/tachyon and bin/VERSION (source commit, build, SHA-256), made by `import`.
#  Gates here check the binary (checksum, runs on the runtime base), not its code.
# =============================================================================

SHELL         := /bin/bash
.SHELLFLAGS   := -eu -o pipefail -c
.DEFAULT_GOAL := help
MAKEFLAGS     += --no-print-directory

SERVICE      := tachyon
UID          := $(shell id -u)
GID          := $(shell id -g)
RUST_VERSION := 1.98.1
TACHYON_SRC  ?= $(HOME)/projects/tachyon
RUNTIME_BASE := gcr.io/distroless/cc-debian12:nonroot
BIN          := bin/tachyon
TOOLS_IMG    := kapa-$(SERVICE)-tools
PY           := eval/*.py scripts/*.py
RUN          := docker run --rm -v $(CURDIR):/src -w /src -u $(UID):$(GID) $(TOOLS_IMG)

.PHONY: help tools import verify ready format lint check-docs test bench testdb-stop build seccomp clean

help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"; printf "Tachyon service targets:\n"} /^[a-zA-Z_-]+:.*##/ { printf "  \033[32m%-18s\033[0m%s\n", $$1, $$2 }' $(MAKEFILE_LIST)

tools: ## Build the tools image (Python, lizard, pyflakes) for Kapa's own files here
	@docker build -q --target tools -t $(TOOLS_IMG) . >/dev/null

import: ## Build bin/tachyon from TACHYON_SRC (default ~/projects/tachyon) in Debian 12; refuses uncommitted sources
	@test -f $(TACHYON_SRC)/Cargo.toml || { echo "No Tachyon project at $(TACHYON_SRC) (TACHYON_SRC=path)"; exit 1; }
	@if [ -n "$$(git -C $(TACHYON_SRC) status --porcelain)" ] && [ "$${ALLOW_DIRTY:-0}" != 1 ]; then \
	   echo "$(TACHYON_SRC) has uncommitted changes: commit them (a binary must name its source), or ALLOW_DIRTY=1"; exit 1; fi
	@printf '\033[34m\033[1m==>\033[0m Building tachyon from %s @ %s in rust:%s-bookworm (source read-only)\n' \
	   '$(TACHYON_SRC)' "$$(git -C $(TACHYON_SRC) rev-parse --short HEAD)" '$(RUST_VERSION)'
	docker run --rm -v $(abspath $(TACHYON_SRC)):/src:ro -v $(CURDIR)/bin:/out -v kapa-tachyon-build:/cache \
	  -e CARGO_HOME=/cache/cargo -e CARGO_TARGET_DIR=/cache/target -w /src rust:$(RUST_VERSION)-bookworm \
	  sh -c 'cargo build --release --locked -p tachyon-cli && install -m 0755 -o $(UID) -g $(GID) /cache/target/release/tachyon /out/tachyon'
	@{ commit=$$(git -C $(TACHYON_SRC) rev-parse HEAD); dirty=$$(git -C $(TACHYON_SRC) status --porcelain | head -c1); \
	   echo "source  github.com/juliog922/tachyon $$commit$${dirty:+ (uncommitted changes)}"; \
	   echo "built   rust:$(RUST_VERSION)-bookworm: cargo build --release --locked -p tachyon-cli"; \
	   echo "sha256  $$(sha256sum $(BIN) | cut -d' ' -f1)"; } > bin/VERSION
	@cat bin/VERSION
	@$(MAKE) verify

verify: ## The binary matches bin/VERSION and runs on the runtime base image
	@test -f $(BIN) && test -f bin/VERSION || { echo "No $(BIN): make -C services/tachyon import"; exit 1; }
	@want=$$(awk '$$1 == "sha256" {print $$2}' bin/VERSION); got=$$(sha256sum $(BIN) | cut -d' ' -f1); \
	 [ "$$want" = "$$got" ] || { echo "$(BIN) does not match bin/VERSION (sha256 $$got): re-run make import"; exit 1; }
	@docker run --rm -v $(CURDIR)/bin:/b:ro --entrypoint /b/tachyon $(RUNTIME_BASE) catalog >/dev/null \
	 || { echo "$(BIN) does not run on $(RUNTIME_BASE)"; exit 1; }
	@printf '\033[32m✔\033[0m tachyon %s: checksum ok, runs on %s\n' "$$(awk '$$1 == "source" {print substr($$3,1,12)}' bin/VERSION)" '$(RUNTIME_BASE)'

ready: ## The binary is present
	@test -f $(BIN) || { echo "No $(BIN): make -C services/tachyon import"; exit 1; }

lint: tools ## Kapa's own files here (eval/, scripts/): pyflakes, lizard; Tachyon's code is checked in its project
	$(RUN) sh -c 'python -m pyflakes $(PY) && lizard --CCN 10 --length 40 --arguments 6 --warnings_only $(PY)'

format check-docs bench: ## Nothing here: Tachyon's code, docs and benches live in its own project
	@echo "tachyon: prebuilt binary, $@ is Tachyon's own (in $(TACHYON_SRC): ./ci.sh)"

test: verify tools ## The binary (checksum, runs on the runtime base) + the evaluation's offline tests
	$(RUN) sh -c 'cd eval && python -m unittest -q test_eval'

testdb-stop: ## No test database (called by the root test-infra)
	@true

build: verify ## Build the runtime image (kapa-tachyon:local)
	docker build -t kapa-$(SERVICE):local .

seccomp: ## Regenerate seccomp.json from Docker's default profile (TAG=v27.3.1) plus io_uring
	python3 scripts/seccomp.py $${TAG:-v27.3.1} > seccomp.json

clean: ## Remove the image and the build cache
	-docker rmi kapa-$(SERVICE):local 2>/dev/null
	-docker volume rm kapa-tachyon-build 2>/dev/null
