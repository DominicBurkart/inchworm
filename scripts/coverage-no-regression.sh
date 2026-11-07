#!/bin/bash
set -euo pipefail

unset GIT_INDEX_FILE GIT_DIR GIT_WORK_TREE GIT_PREFIX

base="${1:-main}"
root="$(git rev-parse --show-toplevel)"

coverage() {
    local output
    output="$(cd "$1" && cargo tarpaulin --skip-clean --ignore-tests 2>&1)" || {
        printf '%s\n' "$output" >&2
        return 1
    }
    printf '%s\n' "$output" | grep -oP '\d+\.\d+(?=% coverage)' | tail -1
}

new="$(coverage "$root")"
tmp="$(mktemp -d)"
trap 'git -C "$root" worktree remove --force "$tmp" 2>/dev/null || true; rm -rf "$tmp"' EXIT
git -C "$root" worktree add --detach -q "$tmp/base" "$base"
old="$(coverage "$tmp/base")"

if awk -v n="$new" -v o="$old" 'BEGIN { exit !(n >= o) }'; then
    echo "Coverage: ${new}% >= ${old}%"
else
    echo "Coverage dropped: ${new}% < ${old}%"
    exit 1
fi
