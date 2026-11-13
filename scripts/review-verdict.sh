#!/bin/bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <review-output-file>" >&2
    exit 2
fi
file="$1"

if [ ! -f "$file" ]; then
    echo "The security review output $file does not exist." >&2
    exit 1
fi

first="$(head -n 1 "$file" 2>/dev/null || true)"
if [ "$first" != "VERDICT: SAFE" ]; then
    echo "The security review must start with exactly 'VERDICT: SAFE' on its first line." >&2
    exit 1
fi

if [ "$(tr -d '\000' < "$file" | wc -c)" != "$(wc -c < "$file")" ]; then
    echo "The security review output contains a NUL byte." >&2
    exit 1
fi

if grep -qiE 'verdict.*unsafe|unsafe.*verdict' "$file" || LC_ALL=C grep -qE '(^|[^A-Za-z])UNSAFE([^A-Za-z]|$)' "$file"; then
    echo "The security review also contains an UNSAFE verdict." >&2
    exit 1
fi
if grep -qiE '(^|[^[:alnum:]])not[[:space:]]+safe([^[:alnum:]]|$)' "$file"; then
    echo "The security review says the change is not safe." >&2
    exit 1
fi
exit 0
