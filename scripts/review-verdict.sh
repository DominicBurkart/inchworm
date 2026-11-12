#!/bin/bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <review-output-file>" >&2
    exit 2
fi
file="$1"

first="$(head -n 1 "$file" 2>/dev/null || true)"
if [ "$first" != "VERDICT: SAFE" ]; then
    echo "The security review must start with exactly 'VERDICT: SAFE' on its first line." >&2
    exit 1
fi
if grep -q '^VERDICT: UNSAFE' "$file"; then
    echo "The security review also contains 'VERDICT: UNSAFE'." >&2
    exit 1
fi
exit 0
