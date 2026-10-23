#!/usr/bin/env bash
# =============================================================================
#  Kapa - DESTRUCTIVE: remove all services, built images AND data volumes on
#  the host. The synced code and the production .env are kept.
#  Requires typing the host name; FORCE=1 skips the prompt (automation only).
# =============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "$ROOT/scripts/lib/common.sh"

printf '%s%s☠  DESTRUCTIVE%s - containers, images and ALL DATA VOLUMES of "%s" on %s will be deleted.\n' \
  "$C_RED" "$C_BOLD" "$C_RESET" "$PROJECT" "$DEPLOY_HOST"
printf '   The production .env and synced code in %s are kept. This cannot be undone.\n' "$DEPLOY_DIR"

if [[ "${FORCE:-}" != "1" ]]; then
  [[ -t 0 ]] || die "Non-interactive shell: re-run with FORCE=1 to confirm."
  read -r -p "Type the host name ($DEPLOY_HOST) to confirm: " answer
  [[ "$answer" == "$DEPLOY_HOST" ]] || die "Aborted."
fi

require_remote
DIR="$(q "$DEPLOY_DIR")"
P="$(q "$PROJECT")"

log "Wiping $PROJECT on $DEPLOY_HOST"
if ! remote "cd $DIR && make clean"; then
  warn "'make clean' failed - removing by compose label"
  remote "docker ps -aq --filter label=com.docker.compose.project=$P | xargs -r docker rm -f; \
          docker volume ls -q --filter label=com.docker.compose.project=$P | xargs -r docker volume rm"
fi
remote "rm -f $DIR/.release"

left="$(remote "docker volume ls -q --filter label=com.docker.compose.project=$P | wc -l")"
[[ "$left" == "0" ]] || die "$left volume(s) still present on $DEPLOY_HOST."
ok "Production wiped on $DEPLOY_HOST. Start fresh with: make deploy"
