#!/usr/bin/env bash
# =============================================================================
#  Kapa - upload the local .env as the production config on the host
# =============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "$ROOT/scripts/lib/common.sh"

ENV_FILE="$ROOT/.env"
[[ -f "$ENV_FILE" ]] || die "No local .env. Create it with: cp .env.example .env"
if grep -q 'change-me' "$ENV_FILE"; then
  die ".env still contains placeholder values ('change-me'). Set real credentials first."
fi

require_remote
DIR="$(q "$DEPLOY_DIR")"
remote "test -d $DIR && test -w $DIR" \
  || die "$DEPLOY_DIR missing or not writable on $DEPLOY_HOST. Run: make deploy-setup"

if remote "test -f $DIR/.env"; then
  confirm "A production .env already exists on $DEPLOY_HOST. Overwrite it?" || die "Aborted."
fi

log "Uploading .env to $DEPLOY_HOST:$DEPLOY_DIR/.env (mode 600)"
remote "umask 077 && cat > $DIR/.env.tmp && mv $DIR/.env.tmp $DIR/.env" < "$ENV_FILE"
ok "Production config uploaded. Changes apply on next: make deploy"
