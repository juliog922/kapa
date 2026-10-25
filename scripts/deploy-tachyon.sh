#!/usr/bin/env bash
# =============================================================================
#  Kapa - run the tachyon CLI inside the running tachyon container on the host:
#    make deploy-tachyon ARGS="doctor"              driver, GPU, memory, certificates, the Hub
#    make deploy-tachyon ARGS="pull gemma-4-e4b"    download + convert into the models volume
#    make deploy-tachyon ARGS="ps"                  the daemon's models: where, VRAM, time left
# =============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "$ROOT/scripts/lib/common.sh"

(($# > 0)) || die 'Usage: make deploy-tachyon ARGS="doctor | catalog | pull <model> | list | ps | load <model> | ..."'
require_remote
id="$(remote "docker ps -q --filter label=com.docker.compose.project=$(q "$PROJECT") --filter label=com.docker.compose.service=tachyon" | head -1)"
[[ -n "$id" ]] || die "tachyon is not running on $DEPLOY_HOST: COMPOSE_PROFILES=gpu in its .env (make deploy-env), then make deploy."
args=""
for a in "$@"; do args+=" $(q "$a")"; done
log "tachyon$args on $DEPLOY_HOST"
remote_tty "docker exec -it $(q "$id") /tachyon$args"
