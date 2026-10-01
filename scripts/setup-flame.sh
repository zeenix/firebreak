#!/usr/bin/env bash
# Fetches the pinned Flame revision into ./flame-lib and applies Firebreak's local patches.
#
# Firebreak builds against Flame through local path dependencies, so this must run before the
# first `cargo build`. Re-running it is safe: patches that are already applied are skipped.
set -euo pipefail

FLAME_REPO="https://github.com/runflame/flame-lib"
FLAME_REV="8021a125da8febe0223a0f00e407887bd60d136f"

root="$(cd "$(dirname "$0")/.." && pwd)"
dir="$root/flame-lib"

if [ ! -d "$dir/.git" ]; then
    git clone "$FLAME_REPO" "$dir"
fi
cd "$dir"

if [ "$(git rev-parse HEAD)" != "$FLAME_REV" ]; then
    if ! git diff --quiet; then
        echo "flame-lib has local changes and is not at $FLAME_REV; refusing to switch." >&2
        exit 1
    fi
    git fetch origin
    git checkout --detach "$FLAME_REV"
fi

for patch in "$root"/patches/flame-lib/*.patch; do
    [ -e "$patch" ] || continue
    name="$(basename "$patch")"
    if git apply --reverse --check "$patch" 2>/dev/null; then
        echo "flame-lib: $name already applied"
    else
        git apply "$patch"
        echo "flame-lib: applied $name"
    fi
done
