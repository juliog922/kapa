#!/usr/bin/env bash
# =============================================================================
#  Kapa - run the SQL Server check from the deployment host, inside the running
#  core container (the Sage server is only reachable from the host's network).
#  Exit 0 only when the login is read-only and the Sage schema core uses is complete.
# =============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "$ROOT/scripts/lib/common.sh"

require_remote
id="$(remote "docker ps -q --filter label=com.docker.compose.project=$(q "$PROJECT") --filter label=com.docker.compose.service=core" | head -1)"
[[ -n "$id" ]] || die "core is not running on $DEPLOY_HOST: deploy first (and upload secrets with make deploy-secrets)."
# Bounded: a core build that predates this option must not hang the terminal.
LIMIT=120
log "Checking SQL Server from $DEPLOY_HOST (core container)"
rc=0
remote "timeout $LIMIT docker exec $(q "$id") /core --check-sqlserver" || rc=$?
if [[ $rc -ne 0 ]]; then
  if [[ $rc -eq 124 ]]; then
    warn "No answer within ${LIMIT}s: the deployed core is probably an older build without this option."
    log "Restarting core on $DEPLOY_HOST to clear any stray process"
    remote "docker restart $(q "$id") >/dev/null"
    die "Deploy the current core first (merge to main, or make deploy), then retry."
  fi
  [[ $rc -eq 2 ]] && die "The deployed core does not know --check-sqlserver: deploy the current core first."
  exit "$rc"
fi
