#!/usr/bin/env bash
# =============================================================================
#  Kapa - can the deployment host run Tachyon? The NVIDIA driver and a GPU, the
#  NVIDIA Container Toolkit, and Docker able to hand the GPU to a container.
# =============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "$ROOT/scripts/lib/common.sh"

require_remote
log "Checking GPU support on $DEPLOY_HOST"
fail=0
if out="$(remote 'nvidia-smi --query-gpu=name,driver_version,memory.total,compute_cap --format=csv,noheader' 2>&1)"; then
  ok "driver and GPU: $out"
else
  warn "nvidia-smi failed: install the NVIDIA driver (R525+): $out"; fail=1
fi
if out="$(remote 'nvidia-ctk --version 2>/dev/null | head -1')" && [[ -n "$out" ]]; then
  ok "NVIDIA Container Toolkit: $out"
else
  warn "NVIDIA Container Toolkit missing: https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html"; fail=1
fi
if remote "docker info --format '{{json .Runtimes}} {{json .CDISpecDirs}}'" | grep -q nvidia; then
  ok "Docker knows the nvidia runtime"
else
  warn "Docker does not know the nvidia runtime: sudo nvidia-ctk runtime configure --runtime=docker && sudo systemctl restart docker"; fail=1
fi
((fail == 0)) || die "Fix the items above, then: make deploy-check-gpu"
ok "Ready for Tachyon. Next: COMPOSE_PROFILES=gpu in the host's .env (make deploy-env), make deploy, make deploy-tachyon ARGS=doctor"
