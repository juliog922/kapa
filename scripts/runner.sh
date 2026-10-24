#!/usr/bin/env bash
# =============================================================================
#  Kapa - on-demand GitHub Actions self-hosted runner (no systemd service)
#
#    make runner-start     start in background; auto-stops after RUNNER_IDLE min idle
#    make runner-stop      stop (asks first if a job is running; FORCE=1 skips)
#    make runner-status    online/offline, busy/idle, recent log
#    make runner-logs      follow the runner log
#    make runner           run in the foreground until Ctrl+C
#
#  RUNNER_IDLE=0 disables auto-stop. Jobs queued while the runner is offline
#  wait on GitHub and are picked up as soon as it starts.
# =============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SELF="$ROOT/scripts/runner.sh"
# shellcheck source=scripts/lib/common.sh
source "$ROOT/scripts/lib/common.sh"

RUNNER_USER="${RUNNER_USER:-github-runner}"
RUNNER_DIR="${RUNNER_DIR:-/home/$RUNNER_USER/actions-runner}"
RUNNER_IDLE="${RUNNER_IDLE:-30}"
LOG="$RUNNER_DIR/kapa-runner.log"
WATCH_PID="$RUNNER_DIR/.kapa-runner-watch.pid"

if [[ $EUID -ne 0 ]]; then
  exec sudo --preserve-env=RUNNER_USER,RUNNER_DIR,RUNNER_IDLE,FORCE,FORCE_COLOR,NO_COLOR bash "$SELF" "$@"
fi

listener_pid() { pgrep -u "$RUNNER_USER" -f 'bin/Runner.Listener' | head -1 || true; }
busy()         { pgrep -u "$RUNNER_USER" -f 'bin/Runner.Worker' >/dev/null 2>&1; }
watcher_pid()  { [[ -f "$WATCH_PID" ]] && kill -0 "$(cat "$WATCH_PID")" 2>/dev/null && cat "$WATCH_PID" || true; }

preflight() {
  id "$RUNNER_USER" >/dev/null 2>&1 || die "User '$RUNNER_USER' does not exist (see docs/DEPLOYMENT.md, runner setup)."
  [[ -x "$RUNNER_DIR/run.sh" ]]     || die "No runner at $RUNNER_DIR (download it as $RUNNER_USER first)."
  [[ -f "$RUNNER_DIR/.runner" ]]    || die "Runner not registered yet: run ./config.sh in $RUNNER_DIR as $RUNNER_USER."
  if compgen -G "/etc/systemd/system/actions.runner.*.service" >/dev/null; then
    die "A systemd runner service is installed. Remove it first:
    cd $RUNNER_DIR && sudo ./svc.sh stop && sudo ./svc.sh uninstall"
  fi
}

stop_listener() {
  local pid pgid _
  pid="$(listener_pid)"; [[ -n "$pid" ]] || return 0
  pgid="$(ps -o pgid= -p "$pid" | tr -d ' ')"
  kill -INT -- "-$pgid" 2>/dev/null || kill -INT "$pid" 2>/dev/null || true
  for _ in $(seq 1 40); do [[ -z "$(listener_pid)" ]] && return 0; sleep 0.5; done
  kill -TERM -- "-$pgid" 2>/dev/null || true; sleep 3
  [[ -z "$(listener_pid)" ]] || kill -KILL -- "-$pgid" 2>/dev/null || true
}

stop_watcher() {
  local w; w="$(watcher_pid)"
  [[ -n "$w" && "$w" != "$$" ]] && kill "$w" 2>/dev/null || true
  rm -f "$WATCH_PID"
}

cmd_start() {
  preflight
  local pid; pid="$(listener_pid)"
  if [[ -n "$pid" ]]; then ok "Runner already online (pid $pid)"; return 0; fi

  install -o "$RUNNER_USER" -g "$RUNNER_USER" -m 0644 /dev/null "$LOG"
  log "Starting runner ($RUNNER_DIR)"
  setsid -f runuser -u "$RUNNER_USER" -- bash -c 'cd "$1" && exec ./run.sh' _ "$RUNNER_DIR" \
    >>"$LOG" 2>&1 </dev/null

  local _
  for _ in $(seq 1 60); do
    grep -q "Listening for Jobs" "$LOG" 2>/dev/null && break
    sleep 0.5
  done
  if ! grep -q "Listening for Jobs" "$LOG" 2>/dev/null; then
    tail -n 20 "$LOG" >&2 || true
    stop_listener
    die "Runner did not come online (log above, full log: $LOG)."
  fi

  if [[ "$RUNNER_IDLE" =~ ^[0-9]+$ ]] && ((RUNNER_IDLE > 0)); then
    stop_watcher
    setsid -f bash "$SELF" _watch >/dev/null 2>&1 </dev/null
    ok "Runner online · listening for jobs · auto-stop after ${RUNNER_IDLE} min idle"
  else
    ok "Runner online · listening for jobs · auto-stop disabled (make runner-stop)"
  fi
}

cmd_watch() { # internal: stop the runner after RUNNER_IDLE minutes without jobs
  echo $$ > "$WATCH_PID"
  local idle_since now
  idle_since="$(date +%s)"
  while [[ -n "$(listener_pid)" ]]; do
    busy && idle_since="$(date +%s)"
    now="$(date +%s)"
    if ((now - idle_since >= RUNNER_IDLE * 60)); then
      echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) [kapa] idle for ${RUNNER_IDLE} min - stopping runner" >> "$LOG"
      stop_listener
      break
    fi
    sleep 20
  done
  rm -f "$WATCH_PID"
}

cmd_stop() {
  if [[ -z "$(listener_pid)" ]]; then stop_watcher; ok "Runner is already offline"; return 0; fi
  if busy; then
    warn "A job is running right now - stopping will cancel it."
    confirm "Stop the runner anyway?" || die "Aborted."
  fi
  log "Stopping runner"
  stop_watcher
  stop_listener
  [[ -z "$(listener_pid)" ]] || die "Runner is still running."
  ok "Runner offline"
}

cmd_status() {
  local pid w; pid="$(listener_pid)"; w="$(watcher_pid)"
  if [[ -z "$pid" ]]; then
    printf '%s●%s runner offline\n' "$C_DIM" "$C_RESET"
  elif busy; then
    printf '%s●%s runner online · %sbusy (job running)%s · pid %s\n' "$C_YEL" "$C_RESET" "$C_BOLD" "$C_RESET" "$pid"
  else
    printf '%s●%s runner online · idle · pid %s\n' "$C_GREEN" "$C_RESET" "$pid"
  fi
  [[ -n "$pid" ]] && { [[ -n "$w" ]] && echo "  auto-stop: after ${RUNNER_IDLE} min idle" || echo "  auto-stop: disabled"; }
  if [[ -f "$LOG" ]]; then
    printf '%s  last log lines (%s):%s\n' "$C_DIM" "$LOG" "$C_RESET"
    tail -n 6 "$LOG" | sed 's/^/    /'
  fi
}

cmd_logs() {
  [[ -f "$LOG" ]] || die "No runner log yet ($LOG)."
  tail -n 50 -f "$LOG"
}

cmd_run() {
  preflight
  [[ -z "$(listener_pid)" ]] || die "Runner already running in background - use: make runner-stop"
  log "Runner in foreground - Ctrl+C to stop"
  runuser -u "$RUNNER_USER" -- bash -c 'cd "$1" && exec ./run.sh' _ "$RUNNER_DIR"
}

case "${1:-}" in
  start)  cmd_start ;;
  stop)   cmd_stop ;;
  status) cmd_status ;;
  logs)   cmd_logs ;;
  run)    cmd_run ;;
  _watch) cmd_watch ;;
  *)      die "usage: runner.sh start|stop|status|logs|run" ;;
esac
