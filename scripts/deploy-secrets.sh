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

# Numeric owners go through chown/chmod (POSIX requires numeric IDs there): `install -o/-g`
# rejects IDs without a named group on some coreutils (e.g. Ubuntu's Rust uutils).
# Every file is checked afterwards: wrong ownership or mode fails the upload.
install_cmd="set -e; umask 077"
for f in "${files[@]}"; do
  dir="$(q "$DEPLOY_DIR/$(dirname "$f")")" src="$(q "$stage/${f//\//__}")" dst="$(q "$DEPLOY_DIR/$f")"
  install_cmd+="; mkdir -p $dir; chown \"\$SUDO_USER:$CONTAINER_UID\" $dir; chmod 0750 $dir"
  install_cmd+="; cp $src $dst; chown $CONTAINER_UID:$CONTAINER_UID $dst; chmod 0400 $dst"
  install_cmd+="; test \"\$(stat -c %u:%g:%a $dst)\" = $CONTAINER_UID:$CONTAINER_UID:400"
  install_cmd+="; echo \"  \$(stat -c '%A %u:%g' $dst) $f\""
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
