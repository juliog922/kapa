#!/usr/bin/env bash
# =============================================================================
#  Kapa - one-time provisioning of the deployment host (idempotent)
#  Uploads scripts/remote/setup-host.sh and runs it with sudo.
# =============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "$ROOT/scripts/lib/common.sh"

require_remote
log "Uploading provisioning script to $DEPLOY_HOST"
tmp="$(remote 'mktemp /tmp/kapa-setup.XXXXXX')"
remote "cat > $tmp" < "$ROOT/scripts/remote/setup-host.sh"

log "Provisioning $DEPLOY_HOST (you will be asked for your sudo password)"
rc=0
remote_tty "sudo bash $tmp \"\$USER\" $(q "$DEPLOY_DIR")" || rc=$?
remote "rm -f $tmp" || true
exit "$rc"
