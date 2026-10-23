#!/usr/bin/env bash
# =============================================================================
#  Kapa - deployment dashboard
#  Streams scripts/remote/status.sh to the host over SSH and renders the result.
#    make monitor                 one-shot report (exit 1 if unhealthy)
#    make monitor WATCH=5         refresh every 5 seconds
#    make monitor SERVICE=postgres LOG_LINES=50
# =============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "$ROOT/scripts/lib/common.sh"

WATCH="${WATCH:-}"
envs=(PROJECT="$PROJECT" DEPLOY_DIR="$DEPLOY_DIR" LOG_LINES="${LOG_LINES:-20}" SERVICE="${SERVICE:-}")
if [[ -n "$KAPA_COLOR" ]]; then envs+=(FORCE_COLOR=1); else envs+=(NO_COLOR=1); fi
REMOTE_CMD="env $(printf '%q ' "${envs[@]}")bash -s"

report() { remote "$REMOTE_CMD" < "$ROOT/scripts/remote/status.sh"; }

require_remote

if [[ -z "$WATCH" ]]; then
  report
  exit $?
fi

[[ "$WATCH" =~ ^[1-9][0-9]*$ ]] || die "WATCH must be a positive number of seconds."
trap 'tput cnorm 2>/dev/null || true; printf "\n"; exit 0' INT TERM
tput civis 2>/dev/null || true
while true; do
  out="$(report 2>&1)" || true
  printf '\033[H\033[2J%s\n\n%sRefreshing every %ss · Ctrl+C to exit%s\n' "$out" "$C_DIM" "$WATCH" "$C_RESET"
  sleep "$WATCH"
done
