#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
MOUNT_DIR="$WORK/mnt"
STOCK="$WORK/stock.ext4"
REPLACEMENT="$WORK/replacement.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"
LOOP=""

cleanup() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then
    sudo umount "$MOUNT_DIR"
  fi
  if [[ -n "$LOOP" ]]; then
    sudo losetup -d "$LOOP"
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$MOUNT_DIR"
truncate -s 64M "$STOCK"
mkfs.ext4 -q -F -b 4096 "$STOCK"

# Build an intentional 3-logical-block file with the middle block left as a hole.
LOOP="$(sudo losetup --find --show "$STOCK")"
sudo mount -t ext4 "$LOOP" "$MOUNT_DIR"
sudo mkdir -p "$MOUNT_DIR/system/etc"
sudo truncate -s 12288 "$MOUNT_DIR/system/etc/sparse.bin"
dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'A' | \
  sudo dd of="$MOUNT_DIR/system/etc/sparse.bin" bs=4096 seek=0 count=1 conv=notrunc status=none
dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'C' | \
  sudo dd of="$MOUNT_DIR/system/etc/sparse.bin" bs=4096 seek=2 count=1 conv=notrunc status=none
sync
sudo umount "$MOUNT_DIR"
sudo losetup -d "$LOOP"
LOOP=""

PHYSICAL_BLOCKS="$(debugfs -R 'blocks /system/etc/sparse.bin' "$STOCK" 2>/dev/null | wc -w)"
if [[ "$PHYSICAL_BLOCKS" -ge 3 ]]; then
  echo "sparse rejection fixture unexpectedly has $PHYSICAL_BLOCKS physical blocks" >&2
  debugfs -R 'stat /system/etc/sparse.bin' "$STOCK" >&2 || true
  exit 1
fi

set +e
e2fsck -fy "$STOCK" >/dev/null
FSCK_RC=$?
set -e
if (( FSCK_RC > 1 )); then
  echo "sparse fixture e2fsck failed with rc=$FSCK_RC" >&2
  exit "$FSCK_RC"
fi

STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"
dd if=/dev/zero bs=12288 count=1 status=none | tr '\000' 'R' > "$REPLACEMENT"

set +e
ERROR_OUTPUT="$(
  "$LOOM" ext4-replace \
    "$STOCK" \
    /system/etc/sparse.bin \
    "$REPLACEMENT" \
    "$SHADOW" \
    ORIGIN_PLACEHOLDER \
    SHADOW_PLACEHOLDER \
    "$TABLE" 2>&1
)"
RC=$?
set -e

if (( RC == 0 )); then
  echo "sparse ext4 target was accepted unexpectedly" >&2
  exit 1
fi
echo "$ERROR_OUTPUT" | grep -q 'sparse ext4 files are not supported'

# Compilation failure must be side-effect free.
[[ ! -e "$SHADOW" ]]
[[ ! -e "$TABLE" ]]
STOCK_HASH_AFTER="$(sha256sum "$STOCK" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]

printf '%s\n' \
  "Stage 1 sparse rejection PASS" \
  "  logical blocks: 3" \
  "  allocated blocks: $PHYSICAL_BLOCKS" \
  "  origin sha256: $STOCK_HASH_AFTER"
