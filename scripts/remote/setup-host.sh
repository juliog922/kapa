#!/usr/bin/env bash
# =============================================================================
#  Kapa - deployment host provisioning (runs ON the host as root, idempotent)
#    - base tools: make, rsync, curl, jq, pciutils
#    - Docker Engine + Compose plugin (official Docker repository)
#    - NVIDIA driver (ubuntu-drivers) + NVIDIA Container Toolkit, if a GPU exists
#    - deployment directory owned by the deploy user
#  Usage: sudo bash setup-host.sh <deploy-user> [deploy-dir]
# =============================================================================
set -euo pipefail

DEPLOY_USER="${1:?usage: setup-host.sh <deploy-user> [deploy-dir]}"
DEPLOY_DIR="${2:-/opt/kapa}"
export DEBIAN_FRONTEND=noninteractive
REBOOT_REQUIRED=0

B=$'\033[1m' G=$'\033[32m' Y=$'\033[33m' C=$'\033[34m' X=$'\033[0m'
step() { printf '\n%s%s==> %s%s\n' "$B" "$C" "$*" "$X"; }
ok()   { printf '%s✔%s %s\n' "$G" "$X" "$*"; }
warn() { printf '%s!%s %s\n' "$Y" "$X" "$*"; }

[[ $EUID -eq 0 ]] || { echo "Must run as root (sudo)."; exit 1; }
id "$DEPLOY_USER" >/dev/null 2>&1 || { echo "User '$DEPLOY_USER' does not exist."; exit 1; }

# --- Base packages ------------------------------------------------------------
step "Base packages"
apt-get update -qq
apt-get install -y -qq ca-certificates curl gnupg make rsync jq pciutils ubuntu-drivers-common >/dev/null
ok "make, rsync, curl, jq, pciutils installed"

# --- Docker -------------------------------------------------------------------
step "Docker Engine + Compose"
if command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
  ok "Already installed: $(docker --version)"
else
  for pkg in docker.io docker-doc docker-compose docker-compose-v2 podman-docker containerd runc; do
    apt-get remove -y -qq "$pkg" >/dev/null 2>&1 || true
  done
  install -m 0755 -d /etc/apt/keyrings
  curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
  chmod a+r /etc/apt/keyrings/docker.asc
  codename="$(. /etc/os-release && echo "${UBUNTU_CODENAME:-$VERSION_CODENAME}")"
  if ! curl -fsI "https://download.docker.com/linux/ubuntu/dists/$codename/Release" >/dev/null 2>&1; then
    warn "Docker has no repository for '$codename' yet - falling back to 'noble'"
    codename=noble
  fi
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu $codename stable" \
    > /etc/apt/sources.list.d/docker.list
  apt-get update -qq
  apt-get install -y -qq docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin >/dev/null
  ok "Installed: $(docker --version)"
fi
if ! docker buildx version >/dev/null 2>&1; then
  # docker-ce ships docker-buildx-plugin; Ubuntu's docker.io uses docker-buildx
  apt-get install -y -qq docker-buildx-plugin >/dev/null 2>&1 \
    || apt-get install -y -qq docker-buildx >/dev/null
  ok "buildx installed: $(docker buildx version | head -1)"
fi
systemctl enable --now docker >/dev/null 2>&1
usermod -aG docker "$DEPLOY_USER"
ok "$DEPLOY_USER added to the docker group"

# --- NVIDIA -------------------------------------------------------------------
step "NVIDIA GPU"
if lspci | grep -qi nvidia; then
  lspci | grep -i nvidia | sed 's/^/  /'
  if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then
    ok "Driver loaded: $(nvidia-smi --query-gpu=driver_version --format=csv,noheader | head -1)"
  else
    echo "  Recommended drivers:"; ubuntu-drivers devices 2>/dev/null | sed 's/^/    /' || true
    ubuntu-drivers install
    REBOOT_REQUIRED=1
    ok "NVIDIA driver installed (active after reboot)"
    if command -v mokutil >/dev/null 2>&1 && mokutil --sb-state 2>/dev/null | grep -qi enabled; then
      warn "Secure Boot is enabled: you may need to enroll the MOK key on the console during reboot."
    fi
  fi

  step "NVIDIA Container Toolkit"
  if ! command -v nvidia-ctk >/dev/null 2>&1; then
    curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey \
      | gpg --dearmor --yes -o /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg
    curl -fsSL https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list \
      | sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://#g' \
      > /etc/apt/sources.list.d/nvidia-container-toolkit.list
    apt-get update -qq
    apt-get install -y -qq nvidia-container-toolkit >/dev/null
  fi
  nvidia-ctk runtime configure --runtime=docker >/dev/null
  systemctl restart docker
  ok "Docker configured with the NVIDIA runtime"
else
  warn "No NVIDIA GPU found on the PCI bus - skipping driver and container toolkit."
fi

# --- Deployment directory -----------------------------------------------------
step "Deployment directory"
install -d -o "$DEPLOY_USER" -g "$DEPLOY_USER" -m 0750 "$DEPLOY_DIR"
ok "$DEPLOY_DIR owned by $DEPLOY_USER"

# --- Summary ------------------------------------------------------------------
step "Done"
echo "  docker:  $(docker --version)"
echo "  compose: $(docker compose version --short)"
if ((REBOOT_REQUIRED)); then
  warn "REBOOT REQUIRED to load the NVIDIA driver:  sudo reboot"
  echo "  Afterwards verify with:  make monitor"
  echo "  GPU in containers:       docker run --rm --gpus all ubuntu nvidia-smi"
fi
