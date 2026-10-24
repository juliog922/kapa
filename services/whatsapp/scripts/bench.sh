#!/usr/bin/env bash
# =============================================================================
#  WhatsApp gateway - benchmarks under perf (run by `make bench` in the tools container)
#
#  1. Go benchmarks: ns/op, B/op, allocs/op (allocations = heap copies made per op).
#  2. perf stat per benchmark: task-clock, context switches, CPU migrations, page
#     faults and, on bare metal, cycles / instructions / cache and branch misses.
#  3. perf record over the suite: share of CPU in memmove / memclr / mallocgc (copies
#     and allocation) and in the scheduler.
#  4. benchstat against bench/baseline.txt when present (`make bench-baseline` saves one).
#
#  Results: bench/latest.txt (Go format, for benchstat) and bench/report.txt.
# =============================================================================
set -euo pipefail

BENCHTIME="${BENCHTIME:-1s}"
FILTER="${BENCH:-.}"
OUT=bench
BIN=/tmp/kapa-bench.test
mkdir -p "$OUT"

PERF=""
for candidate in /usr/bin/perf_* /usr/lib/linux-tools/*/perf /usr/bin/perf; do
  if [[ -x "$candidate" ]] && "$candidate" --version >/dev/null 2>&1; then PERF="$candidate"; break; fi
done

echo "==> Building benchmark binary"
CGO_ENABLED=0 go test -c -o "$BIN" .
mapfile -t benches < <("$BIN" -test.list '^Benchmark' | grep -E "$FILTER")

: > "$OUT/latest.txt"
{
  echo "Kapa WhatsApp gateway benchmarks · $(date -u +%Y-%m-%dT%H:%M:%SZ) · $(go version | cut -d' ' -f3)"
  echo "CPU: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //') · benchtime $BENCHTIME"
  echo
} > "$OUT/report.txt"

if [[ -z "$PERF" ]]; then
  echo "! perf not available: Go metrics only" | tee -a "$OUT/report.txt"
fi

events=task-clock,context-switches,cpu-migrations,page-faults,cycles,instructions,cache-misses,branch-misses
for b in "${benches[@]}"; do
  echo "==> $b"
  if [[ -n "$PERF" ]]; then
    "$PERF" stat -x, -e "$events" -o /tmp/kapa-perf.csv -- \
      "$BIN" -test.run '^$' -test.bench "^${b}\$" -test.benchmem -test.benchtime "$BENCHTIME" | tee /tmp/kapa-go.txt
  else
    "$BIN" -test.run '^$' -test.bench "^${b}\$" -test.benchmem -test.benchtime "$BENCHTIME" | tee /tmp/kapa-go.txt
  fi
  grep -E '^(goos|goarch|pkg|cpu|Benchmark)' /tmp/kapa-go.txt >> "$OUT/latest.txt" || true
  {
    grep -E '^Benchmark' /tmp/kapa-go.txt
    if [[ -n "$PERF" ]]; then
      awk -F, 'NF > 2 && $1 !~ /^#/ { v = $1; if (v ~ /not (supported|counted)/) v = "n/a"; printf "    %-18s %s\n", $3, v }' /tmp/kapa-perf.csv
    fi
    echo
  } >> "$OUT/report.txt"
done

if [[ -n "$PERF" ]]; then
  echo "==> perf record (copies, allocation and scheduling share)"
  "$PERF" record -q -g -o /tmp/kapa-perf.data -- \
    "$BIN" -test.run '^$' -test.bench "$FILTER" -test.benchtime "$BENCHTIME" >/dev/null 2>&1 || true
  {
    echo "CPU share of copy / allocation / scheduler symbols (perf record, whole suite):"
    "$PERF" report -i /tmp/kapa-perf.data --stdio --no-children --sort symbol 2>/dev/null \
      | grep -E '^\s+[0-9.]+%' \
      | grep -E 'memmove|memclr|mallocgc|growslice|futex|schedule|findRunnable|park_m|gcBgMarkWorker|scanobject' \
      | awk '{ printf "  %7s  %s\n", $1, $3 }' | head -15 || echo "  (no samples: perf record unavailable here)"
    echo
  } >> "$OUT/report.txt"
fi

if [[ -f "$OUT/baseline.txt" ]] && command -v benchstat >/dev/null 2>&1; then
  { echo "Change vs bench/baseline.txt:"; benchstat "$OUT/baseline.txt" "$OUT/latest.txt"; } >> "$OUT/report.txt"
fi

echo
cat "$OUT/report.txt"
echo "Saved: $OUT/report.txt, $OUT/latest.txt"
