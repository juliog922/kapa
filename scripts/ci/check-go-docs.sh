#!/usr/bin/env bash
# =============================================================================
#  Kapa - Go documentation check (used by `make check-docs`)
#  Every file needs a package comment (in at least one file of the package) and
#  every top-level func, type, const and var must be preceded by a // comment.
#  Generated files ("Code generated ... DO NOT EDIT.") are skipped.
#  Usage: check-go-docs.sh file.go [file.go...]
# =============================================================================
set -euo pipefail

status=0
pkg_doc=0
for f in "$@"; do
  head -n 5 "$f" | grep -q '^// Code generated .* DO NOT EDIT\.$' && continue
  grep -qE '^// Package [A-Za-z0-9_]+ ' "$f" && pkg_doc=1
  awk -v file="$f" '
    /^(func|type|const|var) / && $0 !~ /^(const|var) \($/ && $0 !~ /^var _ / {
      if (prev !~ /^\/\//) { printf "%s:%d: missing doc comment: %s\n", file, FNR, $0; bad = 1 }
    }
    { prev = $0 }
    END { exit bad }
  ' "$f" || status=1
done
if ((pkg_doc == 0)); then
  echo "missing package comment (// Package <name> ...) in: $*"
  status=1
fi
exit "$status"
