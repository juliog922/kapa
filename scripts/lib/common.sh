#!/usr/bin/env bash
# =============================================================================
#  Kapa - shared helpers for local scripts (sourced, not executed)
# =============================================================================
# shellcheck disable=SC2034

if { [[ -t 1 ]] || [[ -n "${FORCE_COLOR:-}" ]]; } && [[ -z "${NO_COLOR:-}" ]]; then
  KAPA_COLOR=1
  C_RESET=$'\033[0m' C_BOLD=$'\033[1m' C_DIM=$'\033[2m'
  C_RED=$'\033[31m' C_GREEN=$'\033[32m' C_YEL=$'\033[33m' C_BLUE=$'\033[34m'
else
  KAPA_COLOR=
  C_RESET='' C_BOLD='' C_DIM='' C_RED='' C_GREEN='' C_YEL='' C_BLUE=''
fi

log()  { printf '%s==>%s %s\n' "$C_BLUE$C_BOLD" "$C_RESET" "$*"; }
ok()   { printf '%s✔%s %s\n'   "$C_GREEN" "$C_RESET" "$*"; }
warn() { printf '%s!%s %s\n'   "$C_YEL" "$C_RESET" "$*" >&2; }
die()  { printf '%s✘%s %s\n'   "$C_RED" "$C_RESET" "$*" >&2; exit 1; }
q()    { printf '%q' "$1"; }

# Deployment settings (exported by the Makefile; defaults for direct use)
: "${PROJECT:=kapa}"
: "${DEPLOY_HOST:=prike}"
: "${DEPLOY_DIR:=/opt/kapa}"
: "${KAPA_SSH:=ssh}"

read -r -a SSH_CMD <<< "$KAPA_SSH"
SSH_CMD+=(-o ConnectTimeout=10)

remote()     { "${SSH_CMD[@]}" "$DEPLOY_HOST" "$@"; }
remote_tty() { "${SSH_CMD[@]}" -t "$DEPLOY_HOST" "$@"; }

require_remote() {
  remote true 2>/dev/null \
    || die "Cannot reach '$DEPLOY_HOST' using '$KAPA_SSH'. Check your SSH config."
}

confirm() { # confirm "question"  -> skipped with FORCE=1
  [[ "${FORCE:-}" == "1" ]] && return 0
  [[ -t 0 ]] || die "Non-interactive shell: re-run with FORCE=1 to confirm."
  local answer
  read -r -p "$1 [y/N] " answer
  [[ "$answer" =~ ^[Yy]$ ]]
}
