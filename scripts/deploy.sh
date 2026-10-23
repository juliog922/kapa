#!/usr/bin/env bash
# =============================================================================
#  Kapa - deploy the repository to the deployment host
#  1. preflight checks   2. rsync files   3. make run   4. write release marker
# =============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "$ROOT/scripts/lib/common.sh"
cd "$ROOT"

command -v rsync >/dev/null || die "rsync is required on this machine."

# --- Release metadata --------------------------------------------------------
if [[ -n "${GITHUB_SHA:-}" ]]; then
  SHA="${GITHUB_SHA:0:7}"; BRANCH="${GITHUB_REF_NAME:-unknown}"; ACTOR="github:${GITHUB_ACTOR:-ci}"
else
  SHA="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
  BRANCH="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
  ACTOR="$(whoami)@$(hostname)"
  if [[ -n "$(git status --porcelain 2>/dev/null)" ]]; then
    warn "Working tree has uncommitted changes - they will be deployed."
    SHA="$SHA-dirty"
  fi
fi

DIR="$(q "$DEPLOY_DIR")"

# --- Preflight ----------------------------------------------------------------
log "Preflight checks on $DEPLOY_HOST"
require_remote
remote 'command -v docker >/dev/null && docker compose version >/dev/null 2>&1' \
  || die "Docker / Compose not available on $DEPLOY_HOST. Run: make deploy-setup"
remote 'command -v make >/dev/null && command -v rsync >/dev/null' \
  || die "make / rsync missing on $DEPLOY_HOST. Run: make deploy-setup"
remote "test -d $DIR && test -w $DIR" \
  || die "$DEPLOY_DIR missing or not writable on $DEPLOY_HOST. Run: make deploy-setup"
remote "test -f $DIR/.env" \
  || die "No production config at $DEPLOY_HOST:$DEPLOY_DIR/.env. Run: make deploy-env"
ok "Host ready"

# --- Sync ---------------------------------------------------------------------
log "Syncing $SHA ($BRANCH) to $DEPLOY_HOST:$DEPLOY_DIR"
rsync -az --delete --exclude-from="$ROOT/.deployignore" \
  -e "${SSH_CMD[*]}" "$ROOT/" "$DEPLOY_HOST:$DEPLOY_DIR/"
ok "Files synced"

# --- Start --------------------------------------------------------------------
log "Starting services on $DEPLOY_HOST"
remote "cd $DIR && make run"

remote "cat > $DIR/.release" <<RELEASE
sha=$SHA
branch=$BRANCH
deployed_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
deployed_by=$ACTOR
RELEASE

ok "Deployed $SHA to $DEPLOY_HOST"
