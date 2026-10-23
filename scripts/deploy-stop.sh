#!/usr/bin/env bash
# =============================================================================
#  Kapa - EMERGENCY stop of every service on the deployment host
#  Data volumes are kept. Bypass the prompt with FORCE=1.
# =============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "$ROOT/scripts/lib/common.sh"

printf '%s%s⚠  EMERGENCY STOP%s - all "%s" services on %s will be stopped (data is kept).\n' \
  "$C_RED" "$C_BOLD" "$C_RESET" "$PROJECT" "$DEPLOY_HOST"
confirm "Stop production now?" || die "Aborted."

require_remote
DIR="$(q "$DEPLOY_DIR")"

log "Stopping services on $DEPLOY_HOST"
if ! remote "cd $DIR && make stop"; then
  warn "'make stop' failed - stopping containers directly by compose label"
  remote "docker ps -q --filter label=com.docker.compose.project=$(q "$PROJECT") | xargs -r docker stop"
fi

running="$(remote "docker ps -q --filter label=com.docker.compose.project=$(q "$PROJECT") | wc -l")"
[[ "$running" == "0" ]] || die "$running container(s) still running on $DEPLOY_HOST!"
ok "All services stopped on $DEPLOY_HOST. Restore with: make deploy"
