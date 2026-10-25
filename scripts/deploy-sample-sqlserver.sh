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
log "Exporting Sage schema + sample ($ORDERS latest orders) from $DEPLOY_HOST"
if remote "docker exec -e SAMPLE_ORDERS=$(q "$ORDERS") -e SAMPLE_RAW=$(q "$RAW") $(q "$id") /core --sample-sqlserver" > "$OUT.tmp"; then
  mv "$OUT.tmp" "$OUT"
  ok "Saved ${OUT#"$ROOT"/} ($(wc -c < "$OUT") bytes, gitignored)"
else
  rm -f "$OUT.tmp"
  die "Export failed (see the message above)."
fi
