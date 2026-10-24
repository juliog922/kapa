#!/usr/bin/env bash
# =============================================================================
#  Core benchmarks under perf (run by `make bench` in the tools container)
#  1. benches/hot.rs: ns/op per case        2. perf stat per case (context switches,
#  page faults, cycles, cache misses...)     3. perf record: copy/allocation share
#  Results: bench/report.txt
# =============================================================================
set -euo pipefail
FILTER="${BENCH:-}"
mkdir -p bench
cargo bench --no-run --locked -q 2>/dev/null
BIN="$(ls -t "${CARGO_TARGET_DIR:-target}"/release/deps/hot-* | grep -v '\.d$' | head -1)"
PERF=""
for candidate in /usr/bin/perf_* /usr/bin/perf; do
  if [[ -x "$candidate" ]] && "$candidate" --version >/dev/null 2>&1; then PERF="$candidate"; break; fi
done
{
  echo "Kapa core benchmarks · $(date -u +%Y-%m-%dT%H:%M:%SZ) · $(rustc --version)"
  echo "CPU: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //') · BENCHTIME=${BENCHTIME:-1s}"
  echo
} > bench/report.txt
[[ -n "$PERF" ]] || echo "! perf not available: timings only" | tee -a bench/report.txt
events=task-clock,context-switches,cpu-migrations,page-faults,cycles,instructions,cache-misses,branch-misses
for case in allowlist_check parse_email_64k_attachment build_email_64k_attachment base64_1mib dot_stuff_64k; do
  [[ -z "$FILTER" || "$case" == *"$FILTER"* ]] || continue
  echo "==> $case"
  if [[ -n "$PERF" ]]; then
    "$PERF" stat -x, -e "$events" -o /tmp/core-perf.csv -- "$BIN" "$case" | tee -a bench/report.txt
    awk -F, 'NF > 2 && $1 !~ /^#/ { v = $1; if (v ~ /not (supported|counted)/) v = "n/a"; printf "    %-18s %s\n", $3, v }' /tmp/core-perf.csv >> bench/report.txt
  else
    "$BIN" "$case" | tee -a bench/report.txt
  fi
  echo >> bench/report.txt
done
if [[ -n "$PERF" ]]; then
  "$PERF" record -q -g -o /tmp/core-perf.data -- "$BIN" "$FILTER" >/dev/null 2>&1 || true
  {
    echo "CPU share of copy / allocation / scheduler symbols (perf record, all cases):"
    "$PERF" report -i /tmp/core-perf.data --stdio --no-children --sort symbol 2>/dev/null \
      | grep -E '^\s+[0-9.]+%' | grep -E 'memcpy|memmove|memset|malloc|realloc|free|futex|schedule' \
      | awk '{ printf "  %7s  %s\n", $1, $3 }' | head -15 || echo "  (no samples)"
  } >> bench/report.txt
fi
cat bench/report.txt
