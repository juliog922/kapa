#!/usr/bin/env bash
# =============================================================================
#  Kapa - host status report
#  Runs ON the deployment host (streamed over SSH by scripts/monitor.sh).
#  Exit code: 0 = healthy, 1 = issues detected.
# =============================================================================
set -uo pipefail

PROJECT="${PROJECT:-kapa}"
DEPLOY_DIR="${DEPLOY_DIR:-/opt/kapa}"
LOG_LINES="${LOG_LINES:-20}"
SERVICE="${SERVICE:-}"

if [[ -z "${NO_COLOR:-}" ]] && { [[ -n "${FORCE_COLOR:-}" ]] || [[ -t 1 ]]; }; then
  R=$'\033[0m' B=$'\033[1m' D=$'\033[2m'
  RED=$'\033[31m' GRN=$'\033[32m' YEL=$'\033[33m' BLU=$'\033[34m' CYN=$'\033[36m'
else
  R='' B='' D='' RED='' GRN='' YEL='' BLU='' CYN=''
fi

ISSUES=0
WARNINGS=0
issue()   { ISSUES=$((ISSUES + 1)); }
warning() { WARNINGS=$((WARNINGS + 1)); }

# --- Rendering helpers --------------------------------------------------------
repeat()  { local s='' i; for ((i = 0; i < $2; i++)); do s+="$1"; done; printf '%s' "$s"; }
section() { printf '\n%s%s▌ %s%s\n' "$B" "$BLU" "$1" "$R"; }
field()   { printf '  %-9s %s\n' "$1" "$2"; }
trim()    { local s="$1"; s="${s#"${s%%[![:space:]]*}"}"; printf '%s' "${s%"${s##*[![:space:]]}"}"; }
to_int()  { local v="${1%%.*}"; v="${v//[^0-9]/}"; printf '%s' "${v:-0}"; }
is_num()  { [[ "$1" =~ ^[0-9]+(\.[0-9]+)?$ ]]; }
gib()     { awk -v k="$1" 'BEGIN { printf "%.1f GiB", k / 1048576 }'; }

level() { # level <pct> [warn] [crit]
  local p=$1 w=${2:-70} c=${3:-90}
  if ((p >= c)); then printf '%s' "$RED"; elif ((p >= w)); then printf '%s' "$YEL"; else printf '%s' "$GRN"; fi
}
bar() {
  local p=$1 w=24 f
  ((p < 0)) && p=0; ((p > 100)) && p=100
  f=$((p * w / 100))
  printf '%s%s%s%s%s' "$(level "$p")" "$(repeat '█' "$f")" "$D" "$(repeat '░' $((w - f)))" "$R"
}
meter() { # meter <label> <pct> <detail>
  printf '  %-9s %s %s%3d%%%s  %s\n' "$1" "$(bar "$2")" "$(level "$2")" "$2" "$R" "$3"
}

cpu_usage() {
  local -a a b; local i t1=0 t2=0 dt
  read -r -a a < /proc/stat; sleep 0.5; read -r -a b < /proc/stat
  for i in 1 2 3 4 5 6 7 8; do t1=$((t1 + a[i])); t2=$((t2 + b[i])); done
  dt=$((t2 - t1))
  if ((dt > 0)); then echo $(((100 * (dt - (b[4] + b[5] - a[4] - a[5]))) / dt)); else echo 0; fi
}

# --- Header -------------------------------------------------------------------
printf '%s%s KAPA MONITOR %s %s · %s%s\n' "$B" "$CYN" "$R$D" "$(hostname)" "$(date -u '+%Y-%m-%d %H:%M:%S UTC')" "$R"
field "Uptime" "$(uptime -p | sed 's/^up //')"
field "OS" "$(. /etc/os-release && echo "$PRETTY_NAME") · kernel $(uname -r)"
if [[ -f "$DEPLOY_DIR/.release" ]]; then
  rel() { sed -n "s/^$1=//p" "$DEPLOY_DIR/.release"; }
  field "Release" "$B$(rel sha)$R ($(rel branch)) · $(rel deployed_at) · by $(rel deployed_by)"
else
  field "Release" "${YEL}no release marker in $DEPLOY_DIR - never deployed?$R"
  warning
fi
if [[ -f /var/run/reboot-required ]]; then
  field "Reboot" "${YEL}required$R ${D}(pending updates/kernel - plan a maintenance window)$R"
  warning
fi

# --- System -------------------------------------------------------------------
section "System"
meter "CPU" "$(cpu_usage)" "$(nproc) cores · load $(cut -d' ' -f1-3 /proc/loadavg)"

read -r mt ma st sf < <(awk '/^MemTotal:/{t=$2} /^MemAvailable:/{a=$2} /^SwapTotal:/{s=$2} /^SwapFree:/{f=$2}
                             END {print t, a, s, f}' /proc/meminfo)
mu=$((mt - ma))
meter "RAM" $((mu * 100 / mt)) "$(gib "$mu") / $(gib "$mt")"
if ((st > 0)); then
  su=$((st - sf)); meter "Swap" $((su * 100 / st)) "$(gib "$su") / $(gib "$st")"
else
  field "Swap" "${D}disabled$R"
fi

temp="$(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | sort -n | tail -1)"
if [[ -n "$temp" ]]; then
  tc=$((temp / 1000)); field "Temp" "$(level "$tc" 70 85)${tc}°C$R ${D}(hottest sensor)$R"
fi

# --- Disk ---------------------------------------------------------------------
section "Disk"
targets=(/)
docker_root="$(docker info -f '{{.DockerRootDir}}' 2>/dev/null || true)"
[[ -n "$docker_root" ]] && targets+=("$docker_root")
[[ -d "$DEPLOY_DIR" ]] && targets+=("$DEPLOY_DIR")
declare -A seen=()
for t in "${targets[@]}"; do
  read -r mnt size used avail pct < <(df -hP "$t" 2>/dev/null | awk 'NR == 2 {print $6, $2, $3, $4, $5}') || continue
  [[ -z "${mnt:-}" || -n "${seen[$mnt]:-}" ]] && continue
  seen[$mnt]=1
  p="$(to_int "$pct")"
  meter "$mnt" "$p" "$used / $size · $avail free"
  ((p >= 90)) && issue
done

# --- GPU ----------------------------------------------------------------------
section "GPU"
gpu_q='index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,power.limit'
if command -v nvidia-smi >/dev/null 2>&1 \
   && gpu_csv="$(nvidia-smi --query-gpu="$gpu_q" --format=csv,noheader,nounits 2>/dev/null)"; then
  driver="$(nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>/dev/null | head -1)"
  while IFS=',' read -r idx name util mused mtotal gtemp pdraw plimit; do
    idx="$(trim "$idx")"; name="$(trim "$name")"; util="$(trim "$util")"
    mused="$(trim "$mused")"; mtotal="$(trim "$mtotal")"; gtemp="$(trim "$gtemp")"
    pdraw="$(trim "$pdraw")"; plimit="$(trim "$plimit")"
    power="n/a"; is_num "$pdraw" && power="$(to_int "$pdraw") W / $(to_int "$plimit") W"
    tcol=""; is_num "$gtemp" && tcol="$(level "$(to_int "$gtemp")" 75 85)"
    printf '  %s[%s] %s%s  %s%s°C%s · %s · driver %s\n' "$B" "$idx" "$name" "$R" "$tcol" "$gtemp" "$R" "$power" "$driver"
    is_num "$util" && meter "Util" "$(to_int "$util")" ""
    if is_num "$mused" && is_num "$mtotal" && (($(to_int "$mtotal") > 0)); then
      meter "VRAM" $(($(to_int "$mused") * 100 / $(to_int "$mtotal"))) "$mused / $mtotal MiB"
    fi
  done <<< "$gpu_csv"
elif command -v lspci >/dev/null 2>&1 && lspci | grep -qi nvidia; then
  printf '  %s! NVIDIA GPU present but nvidia-smi is unavailable or failing.%s\n' "$YEL" "$R"
  printf '    Run "make deploy-setup", then reboot the host.\n'
  warning
else
  printf '  %sNo NVIDIA GPU detected%s\n' "$D" "$R"
fi

# --- Services -----------------------------------------------------------------
section "Services · project \"$PROJECT\""
rows=()
if ! command -v docker >/dev/null 2>&1; then
  printf '  %s✘ docker is not installed - run "make deploy-setup"%s\n' "$RED" "$R"; issue
elif ! docker info >/dev/null 2>&1; then
  printf '  %s✘ cannot reach the docker daemon (is %s in the docker group?)%s\n' "$RED" "$(whoami)" "$R"; issue
else
  mapfile -t rows < <(docker ps -a --filter "label=com.docker.compose.project=$PROJECT" \
    --format '{{.Label "com.docker.compose.service"}}|{{.Names}}|{{.State}}|{{.Status}}' | sort)
  if ((${#rows[@]} == 0)); then
    printf '  %s✘ no containers found - not deployed (run "make deploy")%s\n' "$RED" "$R"; issue
  else
    declare -A s_cpu=() s_mem=()
    running=()
    for row in "${rows[@]}"; do
      IFS='|' read -r _ name state _ <<< "$row"
      [[ "$state" == running ]] && running+=("$name")
    done
    if ((${#running[@]} > 0)); then
      while IFS='|' read -r name c m; do s_cpu[$name]="$c"; s_mem[$name]="$m"; done \
        < <(docker stats --no-stream --format '{{.Name}}|{{.CPUPerc}}|{{.MemUsage}}' "${running[@]}" 2>/dev/null)
    fi
    printf '  %s%-16s %-11s %-10s %-16s %7s  %-7s %s%s\n' "$D" SERVICE STATE HEALTH UPTIME CPU "ERR/1h" MEMORY "$R"
    for row in "${rows[@]}"; do
      IFS='|' read -r svc name state status <<< "$row"
      case "$state" in
        running)            sc="$GRN" ;;
        restarting|paused)  sc="$YEL"; issue ;;
        *)                  sc="$RED"; issue ;;
      esac
      case "$status" in
        *"(healthy)"*)          h=healthy;   hc="$GRN" ;;
        *"(unhealthy)"*)        h=unhealthy; hc="$RED"; issue ;;
        *"health: starting"*)   h=starting;  hc="$YEL"; warning ;;
        *)                      h="-";       hc="$D" ;;
      esac
      if [[ "$state" == running ]]; then up="${status#Up }"; up="${up%% (*}"; else up="$status"; fi
      errs="$(docker logs --since 1h "$name" 2>&1 | grep -cE 'ERROR|FATAL|PANIC|CRITICAL|Traceback' || true)"
      ec="$GRN"; if ((errs > 0)); then ec="$RED"; warning; fi
      printf '  %-16s %s%-11s%s %s%-10s%s %-16s %7s  %s%-7s%s %s\n' \
        "$svc" "$sc" "$state" "$R" "$hc" "$h" "$R" "$up" "${s_cpu[$name]:--}" "$ec" "$errs" "$R" "${s_mem[$name]:--}"
    done
  fi
fi

# --- Logs ---------------------------------------------------------------------
if ((${#rows[@]} > 0)); then
  section "Recent logs · last $LOG_LINES lines${SERVICE:+ · $SERVICE}  ${D}(search & follow: make deploy-logs · web: make deploy-logs-ui)$R"
  for row in "${rows[@]}"; do
    IFS='|' read -r svc name _ _ <<< "$row"
    [[ -n "$SERVICE" && "$svc" != "$SERVICE" ]] && continue
    printf '  %s%s── %s %s%s\n' "$B" "$CYN" "$svc" "$(repeat '─' $((60 - ${#svc})))" "$R"
    logs="$(docker logs --tail "$LOG_LINES" --timestamps "$name" 2>&1)"
    if [[ -z "$logs" ]]; then
      printf '  %s(no output)%s\n' "$D" "$R"
      continue
    fi
    awk -v red="$RED" -v yel="$YEL" -v dim="$D" -v rst="$R" '{
      ts = substr($1, 6, 14); sub(/T/, " ", ts)
      line = substr($0, length($1) + 2)
      c = ""
      if (line ~ /(ERROR|FATAL|PANIC|Error|error:)/) c = red
      else if (line ~ /(WARN|Warning)/)             c = yel
      printf "  %s%s%s %s%s%s\n", dim, ts, rst, c, line, rst
    }' <<< "$logs"
  done
fi

# --- Verdict ------------------------------------------------------------------
printf '\n%s%s%s\n' "$D" "$(repeat '─' 78)" "$R"
note=""; ((WARNINGS > 0)) && note=" · $YEL$WARNINGS warning(s)$R"
if ((ISSUES == 0)); then
  printf '%s%s● HEALTHY%s  all checks passed%s\n' "$B" "$GRN" "$R" "$note"
  exit 0
fi
printf '%s%s● DEGRADED%s  %d issue(s) detected%s\n' "$B" "$RED" "$R" "$ISSUES" "$note"
exit 1
