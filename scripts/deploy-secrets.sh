#!/usr/bin/env bash
# =============================================================================
#  Kapa - upload service secrets to the deployment host
#
#  Every services/<svc>/secrets/*.prod.env file is installed on the host as
#  $DEPLOY_DIR/services/<svc>/secrets/<file>, owned by uid/gid 65532 (the non-root
#  user of the distroless containers) with mode 0400: only that service (and root)
#  can read it. The directory is 0750 <you>:65532. rsync never touches these files
#  (.deployignore), so deploys neither ship dev secrets nor delete production ones.
#  Running services are restarted so they read the new files.
# =============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "$ROOT/scripts/lib/common.sh"

CONTAINER_UID=65532
PLACEHOLDER='abcd efgh ijkl mnop'

mapfile -t files < <(cd "$ROOT" && ls services/*/secrets/*.prod.env 2>/dev/null || true)
((${#files[@]} > 0)) || die "No services/*/secrets/*.prod.env files to upload."
for f in "${files[@]}"; do
  grep -q "$PLACEHOLDER" "$ROOT/$f" && die "$f still contains the template password '$PLACEHOLDER'."
done

printf 'Files to install on %s:%s (owner %s, mode 0400):\n' "$DEPLOY_HOST" "$DEPLOY_DIR" "$CONTAINER_UID"
printf '  %s\n' "${files[@]}"
confirm "Upload and replace them?" || die "Aborted."

require_remote
stage="$(remote 'mktemp -d /tmp/kapa-secrets.XXXXXX')"
trap 'remote "rm -rf $(q "$stage")" >/dev/null 2>&1 || true' EXIT
for f in "${files[@]}"; do
  remote "umask 077 && cat > $(q "$stage/${f//\//__}")" < "$ROOT/$f"
done

install_cmd="set -e"
for f in "${files[@]}"; do
  dir="$DEPLOY_DIR/$(dirname "$f")"
  install_cmd+="; install -d -o \"\$SUDO_USER\" -g $CONTAINER_UID -m 0750 $(q "$dir")"
  install_cmd+="; install -o $CONTAINER_UID -g $CONTAINER_UID -m 0400 $(q "$stage/${f//\//__}") $(q "$DEPLOY_DIR/$f")"
done

log "Installing secrets on $DEPLOY_HOST (sudo password requested)"
remote_tty "sudo sh -c $(q "$install_cmd")"

for svc in $(printf '%s\n' "${files[@]}" | cut -d/ -f2 | sort -u); do
  ids="$(remote "docker ps -q --filter label=com.docker.compose.project=$(q "$PROJECT") --filter label=com.docker.compose.service=$(q "$svc")")"
  if [[ -n "$ids" ]]; then
    log "Restarting $svc to load the new secrets"
    remote "docker restart $ids >/dev/null"
  fi
done
ok "Secrets installed on $DEPLOY_HOST. Check with: make monitor"
