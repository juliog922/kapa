#!/usr/bin/env bash
# =============================================================================
#  Kapa - test and benchmark the Tachyon models the platform uses, on the host:
#  gemma-4-e4b (text, images, audio), laya-multilingual (decisions) and the
#  embedding model, through the running daemon (services/tachyon/eval/eval.py).
#    PULL=1           pull the models that are missing first (GBs: takes a while)
#    APPROVE=1        accept changed Gemma answers as the new reviewed ones
#    SAVE_BASELINE=1  record this run's numbers as the bench baseline
#    EMBED=granite-embedding-97m   evaluate the smaller embedding model instead
# =============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "$ROOT/scripts/lib/common.sh"

MODELS=(gemma-4-e4b laya-multilingual "${EMBED:-granite-embedding-311m}")
command -v python3 >/dev/null || die "python3 is needed to run the evaluation."
require_remote
id="$(remote "docker ps -q --filter label=com.docker.compose.project=$(q "$PROJECT") --filter label=com.docker.compose.service=tachyon" | head -1)"
[[ -n "$id" ]] || die "tachyon is not running on $DEPLOY_HOST: COMPOSE_PROFILES=gpu in its .env (make deploy-env), then make deploy."

pulled="$(remote "docker exec $(q "$id") /tachyon list" 2>/dev/null || true)"
missing=()
for m in "${MODELS[@]}"; do grep -qw -- "$m" <<< "$pulled" || missing+=("$m"); done
if ((${#missing[@]})); then
  [[ "${PULL:-0}" == 1 ]] || die "Not pulled on $DEPLOY_HOST: ${missing[*]}. Run PULL=1 make deploy-tachyon-eval (or make deploy-tachyon ARGS=\"pull <model>\")."
  for m in "${missing[@]}"; do
    log "Pulling $m on $DEPLOY_HOST"
    remote_tty "docker exec -it $(q "$id") /tachyon pull $(q "$m")"
  done
fi

log "Evaluating ${MODELS[*]} on $DEPLOY_HOST (through the daemon)"
machine="$DEPLOY_HOST"
TACHYON_CONTAINER="$id" KAPA_SSH="$KAPA_SSH" DEPLOY_HOST="$DEPLOY_HOST" EVAL_MACHINE="$machine" EMBED="${MODELS[2]}" \
  python3 "$ROOT/services/tachyon/eval/eval.py"
