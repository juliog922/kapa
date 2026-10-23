#!/usr/bin/env bash
# =============================================================================
#  Kapa - production logs: follow, search and filter
#
#    make deploy-logs                                 follow every service
#    make deploy-logs SERVICE=postgres,dozzle         only these services
#    make deploy-logs LEVEL=error                     only ERROR/FATAL/PANIC/...
#    make deploy-logs GREP='connection|timeout'       regex filter (case-insensitive, highlighted)
#    make deploy-logs SINCE=2h FOLLOW=0 GREP=auth     search history, then exit
#    make deploy-logs FOLLOW=0 LOG_LINES=500 > out.log
#
#  SINCE accepts 30m, 2h, 1d or 2026-10-23T20:00:00. Without SINCE, LOG_LINES sets the backlog.
# =============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/common.sh
source "$ROOT/scripts/lib/common.sh"

SERVICE="${SERVICE:-}" GREP="${GREP:-}" LEVEL="${LEVEL:-}" SINCE="${SINCE:-}"
FOLLOW="${FOLLOW:-1}" LOG_LINES="${LOG_LINES:-50}"

case "${LEVEL,,}" in
  "")               level_re="" ;;
  error|err)        level_re='ERROR|FATAL|PANIC|CRITICAL|Traceback|Exception' ;;
  warn|warning)     level_re='WARN|ERROR|FATAL|PANIC|CRITICAL|Traceback|Exception' ;;
  *)                die "LEVEL must be 'error' or 'warn'." ;;
esac
[[ "$FOLLOW" =~ ^[01]$ ]] || die "FOLLOW must be 0 or 1."

if [[ -n "$KAPA_COLOR" ]]; then ansi=always; gcolor=always; else ansi=never; gcolor=never; fi

args=(--timestamps)
[[ "$FOLLOW" == 1 ]] && args+=(--follow)
if [[ -n "$SINCE" ]]; then args+=(--since "$SINCE"); else args+=(--tail "$LOG_LINES"); fi

services=()
[[ -n "$SERVICE" ]] && IFS=',' read -r -a services <<< "$SERVICE"

cmd="cd $(q "$DEPLOY_DIR") && docker compose --ansi $ansi -p $(q "$PROJECT") logs"
for a in "${args[@]}" "${services[@]}"; do cmd+=" $(q "$a")"; done
cmd+=" 2>&1"
# The last filter highlights its matches; earlier filters only narrow.
if [[ -n "$level_re" && -n "$GREP" ]]; then
  cmd+=" | grep --line-buffered -iE $(q "$level_re") | grep --line-buffered -iE --color=$gcolor $(q "$GREP")"
elif [[ -n "$level_re" ]]; then
  cmd+=" | grep --line-buffered -iE --color=$gcolor $(q "$level_re")"
elif [[ -n "$GREP" ]]; then
  cmd+=" | grep --line-buffered -iE --color=$gcolor $(q "$GREP")"
fi

require_remote
desc="services=${SERVICE:-all}"
[[ -n "$LEVEL" ]] && desc+=" level=$LEVEL"
[[ -n "$GREP" ]]  && desc+=" grep=/$GREP/"
if [[ -n "$SINCE" ]]; then desc+=" since=$SINCE"; else desc+=" tail=$LOG_LINES"; fi
[[ "$FOLLOW" == 1 ]] && desc+=" · following (Ctrl+C to exit)"
log "Logs on $DEPLOY_HOST · $desc" >&2

rc=0
if [[ -t 1 ]]; then remote_tty "$cmd" || rc=$?; else remote "$cmd" || rc=$?; fi
if [[ "$rc" == 1 && ( -n "$GREP" || -n "$level_re" ) ]]; then
  warn "No matching log lines."
  exit 0
fi
exit "$rc"
