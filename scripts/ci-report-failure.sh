#!/usr/bin/env bash
# Turns the first test failure in a log into one GitHub annotation, so the
# cause can be read through the public API without downloading raw logs
# (GitHub keeps at most 10 error annotations per step).
set -uo pipefail
log="${1:-test-output.log}"
[[ -f "$log" ]] || exit 0

block=$(awk '
  /panicked at/ && !seen { seen = 1; n = 0 }
  seen && n < 25 { print; n++ }
' "$log")
if [[ -z "$block" ]]; then
  block=$(grep -E "^error(\[|:)" -A12 "$log" | head -30)
fi
[[ -n "$block" ]] || block=$(tail -30 "$log")

# Annotation messages encode newlines as %0A.
msg=$(printf '%s' "$block" | sed -e 's/%/%25/g' | awk '{ printf "%s%%0A", $0 }')
echo "::error title=First test failure::${msg}"
grep -E "^test .* FAILED" "$log" | head -5 | sed 's/^/::warning::/'
