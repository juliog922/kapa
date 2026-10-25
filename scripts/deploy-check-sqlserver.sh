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
log "Checking SQL Server from $DEPLOY_HOST (core container)"
remote "docker exec $(q "$id") /core --check-sqlserver"
