#!/usr/bin/env bash
# =============================================================================
#  Kapa - Tachyon on THIS machine's GPU, exactly as deployed (same image, seccomp,
#  volumes and uid), in its own compose project:
#    1. the container becomes healthy: GPU passthrough, io_uring, the daemon up
#    2. a second container as uid 65532 (like core) reaches it through the shared
#       socket and its shared memory; another uid is kept out
#    3. the models Kapa uses: checks and reviewed answers (services/tachyon/eval)
#  Skipped (with a warning) where Docker has no NVIDIA runtime, unless REQUIRE_GPU=1.
#  Models are pulled once into the volume kapa-tachyon-models (PULL=1), kept across runs.
#  EVAL_BENCH=1 adds the benchmark (make bench-gpu).
# =============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "$ROOT/scripts/lib/common.sh"

GPU_PROJECT=kapa-test-gpu
MODELS_VOLUME=kapa-tachyon-models
MODELS=(gemma-4-e4b laya-multilingual "${EMBED:-granite-embedding-311m}")
compose() {
  docker compose -f "$ROOT/docker-compose.yml" -f "$ROOT/docker-compose.test.yml" \
    --env-file "$ROOT/${DEV_ENV:-.env.dev}" -p "$GPU_PROJECT" --profile gpu "$@"
}

if ! docker info --format '{{json .Runtimes}} {{json .CDISpecDirs}}' 2>/dev/null | grep -q nvidia; then
  [[ "${REQUIRE_GPU:-0}" == 1 ]] && die "Docker has no NVIDIA runtime here (REQUIRE_GPU=1)."
  warn "Docker has no NVIDIA runtime here: Tachyon's GPU tests skipped (sudo nvidia-ctk runtime configure --runtime=docker)"
  exit 0
fi
docker volume create "$MODELS_VOLUME" >/dev/null
trap 'compose down -v --remove-orphans >/dev/null 2>&1 || true' EXIT

log "Starting tachyon on this machine's GPU [$GPU_PROJECT]"
if ! compose up -d --build --wait tachyon; then
  compose logs --tail=60 tachyon || true
  die "tachyon did not become healthy (log above)."
fi
id="$(compose ps -q tachyon)"
image="$(docker inspect --format '{{.Config.Image}}' "$id")"
docker exec "$id" /tachyon doctor | grep -E '^(driver|gpu|memory|models)' || true

log "A second container as uid 65532 (like core), through the shared socket and memory"
socket_volume="${GPU_PROJECT}_tachyon_run"
docker run --rm --user 65532:65532 -v "$socket_volume:/run/tachyon:ro" --entrypoint /tachyon "$image" ps \
  || die "a client container as uid 65532 could not use the daemon"
ok "client container served (socket, memfd, futex, same uid)"
if docker run --rm --user 1000:1000 -v "$socket_volume:/run/tachyon:ro" --entrypoint /tachyon "$image" ps >/dev/null 2>&1; then
  die "a container as uid 1000 reached the daemon: it must admit uid 65532 only"
fi
ok "another uid is kept out"

pulled="$(docker exec "$id" /tachyon list 2>/dev/null || true)"
missing=()
for m in "${MODELS[@]}"; do grep -qw -- "$m" <<< "$pulled" || missing+=("$m"); done
if ((${#missing[@]})); then
  if [[ "${PULL:-0}" != 1 ]]; then
    warn "Models not pulled on this machine: ${missing[*]}. PULL=1 make test-gpu (≈3.7 GB, once: volume $MODELS_VOLUME)"
    exit 0
  fi
  for m in "${missing[@]}"; do
    log "Pulling $m into $MODELS_VOLUME"
    docker exec "$id" /tachyon pull "$m"
  done
fi

log "Evaluating ${MODELS[*]} on $(hostname)"
EVAL_LOCAL=1 TACHYON_CONTAINER="$id" EVAL_MACHINE="$(hostname)" EVAL_BENCH="${EVAL_BENCH:-0}" EMBED="${MODELS[2]}" \
  python3 "$ROOT/services/tachyon/eval/eval.py"
