#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

STOCK="$WORK/stock.ext4"
ORIGINAL="$WORK/original.bin"
TOO_LARGE="$WORK/too-large.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

truncate -s 64M "$STOCK"
mkfs.ext4 -q -F -b 4096 "$STOCK"

dd if=/dev/zero bs=3000 count=1 status=none | tr '\000' 'A' > "$ORIGINAL"
debugfs -w -R 'mkdir /system' "$STOCK" >/dev/null 2>&1
debugfs -w -R 'mkdir /system/etc' "$STOCK" >/dev/null 2>&1
debugfs -w -R "write $ORIGINAL /system/etc/resizable.bin" "$STOCK" >/dev/null 2>&1

set +e
e2fsck -fy "$STOCK" >/dev/null
FSCK_RC=$?
set -e
if (( FSCK_RC > 1 )); then
  echo "resize-reject fixture e2fsck failed: rc=$FSCK_RC" >&2
  exit "$FSCK_RC"
fi

[[ "$(debugfs -R 'blocks /system/etc/resizable.bin' "$STOCK" 2>/dev/null | wc -w)" -eq 1 ]]
STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"

dd if=/dev/zero bs=5000 count=1 status=none | tr '\000' 'B' > "$TOO_LARGE"

set +e
ERROR_OUTPUT="$(
  "$LOOM" ext4-resize \
    "$STOCK" \
    /system/etc/resizable.bin \
    "$TOO_LARGE" \
    "$SHADOW" \
    ORIGIN_PLACEHOLDER \
    SHADOW_PLACEHOLDER \
    "$TABLE" 2>&1
)"
RC=$?
set -e

if (( RC == 0 )); then
  echo "resize crossing allocation boundary was accepted unexpectedly" >&2
  exit 1
fi
echo "$ERROR_OUTPUT" | grep -q 'changes the logical allocation boundary'

# Compiler failure must be transactional: output files are created only after
# the compiler returns a successful generation.
[[ ! -e "$SHADOW" ]]
[[ ! -e "$TABLE" ]]

STOCK_HASH_AFTER="$(sha256sum "$STOCK" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]

printf '%s\n' \
  "Stage 2 allocation-boundary rejection PASS" \
  "  original bytes: 3000" \
  "  rejected bytes: 5000" \
  "  output side effects: 0" \
  "  origin sha256: $STOCK_HASH_AFTER"
