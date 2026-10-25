#!/usr/bin/env bash
# =============================================================================
#  Kapa - export the Sage schema and a coherent sample of data from the host
#  (inside the running core container: the SQL Server is only reachable there)
#  into services/core/testdata/sage.local.json (gitignored), for tests and dev.
#    SAMPLE_ORDERS=N   latest orders to sample (default 10, max 200)
#    SAMPLE_RAW=1      keep personal data unmasked (asks for confirmation)
# =============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "$ROOT/scripts/lib/common.sh"

OUT="$ROOT/services/core/testdata/sage.local.json"
ORDERS="${SAMPLE_ORDERS:-10}"
RAW="${SAMPLE_RAW:-0}"
[[ "$ORDERS" =~ ^[0-9]+$ ]] || die "SAMPLE_ORDERS must be a number."
if [[ "$RAW" == "1" ]]; then
  warn "SAMPLE_RAW=1: REAL personal data (names, phones, e-mails, tax IDs) will be written to $OUT"
  confirm "Continue?" || die "Aborted."
fi

require_remote
id="$(remote "docker ps -q --filter label=com.docker.compose.project=$(q "$PROJECT") --filter label=com.docker.compose.service=core" | head -1)"
[[ -n "$id" ]] || die "core is not running on $DEPLOY_HOST: deploy first (and upload secrets with make deploy-secrets)."

mkdir -p "$(dirname "$OUT")"
umask 077
# Bounded: a core build that predates this option must not hang the terminal.
LIMIT=300
log "Exporting Sage schema + sample ($ORDERS latest orders) from $DEPLOY_HOST"
rc=0
remote "timeout $LIMIT docker exec -e SAMPLE_ORDERS=$(q "$ORDERS") -e SAMPLE_RAW=$(q "$RAW") $(q "$id") /core --sample-sqlserver" > "$OUT.tmp" || rc=$?
if [[ $rc -eq 0 ]]; then
  mv "$OUT.tmp" "$OUT"
  ok "Saved ${OUT#"$ROOT"/} ($(wc -c < "$OUT") bytes, gitignored)"
  exit 0
fi
rm -f "$OUT.tmp"
  if [[ $rc -eq 124 ]]; then
    warn "No answer within ${LIMIT}s: the deployed core is probably an older build without this option."
    log "Restarting core on $DEPLOY_HOST to clear any stray process"
    remote "docker restart $(q "$id") >/dev/null"
    die "Deploy the current core first (merge to main, or make deploy), then retry."
  fi
[[ $rc -eq 2 ]] && die "The deployed core does not know --sample-sqlserver: deploy the current core first."
die "Export failed (see the message above)."
