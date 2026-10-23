#!/usr/bin/env bash
# =============================================================================
#  Kapa - open the web log viewer (Dozzle) running on the host through an
#  SSH tunnel. Nothing is exposed on the network; Ctrl+C closes the tunnel.
# =============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "$ROOT/scripts/lib/common.sh"

LOCAL_PORT="${LOGS_UI_PORT:-8090}"
require_remote

running="$(remote "docker ps -q --filter label=com.docker.compose.project=$(q "$PROJECT") --filter label=com.docker.compose.service=dozzle")"
[[ -n "$running" ]] || die "Log viewer is not running on $DEPLOY_HOST. Run: make deploy"

remote_port="$(remote "sed -n 's/^DOZZLE_PORT=//p' $(q "$DEPLOY_DIR")/.env" | tr -d '[:space:]')"
remote_port="${remote_port:-8080}"

ok "Log viewer: ${C_BOLD}http://localhost:$LOCAL_PORT${C_RESET}  (tunnel to $DEPLOY_HOST:$remote_port · Ctrl+C to close)"
exec "${SSH_CMD[@]}" -N -o ExitOnForwardFailure=yes \
  -L "127.0.0.1:$LOCAL_PORT:127.0.0.1:$remote_port" "$DEPLOY_HOST"
