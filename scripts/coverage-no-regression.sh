#!/bin/bash
set -euo pipefail

unset GIT_INDEX_FILE GIT_DIR GIT_WORK_TREE GIT_PREFIX

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <base-ref-or-sha>" >&2
    exit 2
fi
base="$1"
root="$(git rev-parse --show-toplevel)"

if [[ "$base" =~ ^0+$ ]]; then
    echo "No baseline commit (all-zero sha); skipping the coverage comparison."
    exit 0
fi
if ! git -C "$root" rev-parse --verify --quiet "${base}^{commit}" >/dev/null; then
    echo "Cannot resolve the coverage baseline '$base'." >&2
    exit 2
fi

coverage() {
    local output value
    output="$(cd "$1" && cargo tarpaulin --skip-clean --ignore-tests 2>&1)" || {
        printf '%s\n' "$output" >&2
        echo "cargo tarpaulin failed in $1." >&2
        return 1
    }
    value="$(printf '%s\n' "$output" | grep -oP '\d+\.\d+(?=% coverage)' | tail -1 || true)"
    if [ -z "$value" ]; then
        printf '%s\n' "$output" | tail -n 5 >&2
        echo "No coverage line in the tarpaulin output for $1." >&2
        return 1
    fi
    printf '%s\n' "$value"
}

new="$(coverage "$root")"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"; git -C "$root" worktree prune' EXIT
git -C "$root" worktree add --detach -q "$tmp/base" "$base"
old="$(coverage "$tmp/base")"

if awk -v n="$new" -v o="$old" 'BEGIN { exit !(n >= o) }'; then
    echo "Coverage: ${new}% >= ${old}% (baseline $base)"
else
    echo "Coverage dropped: ${new}% < ${old}% (baseline $base)"
    exit 1
fi
