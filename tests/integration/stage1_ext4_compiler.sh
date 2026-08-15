#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
MAPPER="loom-stage1-${RANDOM}-${RANDOM}"
MOUNT_DIR="$WORK/mnt"
ORIGIN_LOOP=""
SHADOW_LOOP=""

cleanup() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then
    sudo umount "$MOUNT_DIR"
  fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then
    sudo dmsetup remove "$MAPPER"
  fi
  if [[ -n "$SHADOW_LOOP" ]]; then
    sudo losetup -d "$SHADOW_LOOP"
  fi
  if [[ -n "$ORIGIN_LOOP" ]]; then
    sudo losetup -d "$ORIGIN_LOOP"
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$MOUNT_DIR"
STOCK="$WORK/stock.ext4"
ORIGINAL="$WORK/original.bin"
REPLACEMENT="$WORK/replacement.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

truncate -s 64M "$STOCK"
mkfs.ext4 -q -F -b 4096 "$STOCK"

dd if=/dev/zero of="$ORIGINAL" bs=4096 count=3 status=none
printf 'LOOM-STAGE1-ORIGINAL-BLOCK-0' | dd of="$ORIGINAL" bs=1 seek=0 conv=notrunc status=none
printf 'LOOM-STAGE1-ORIGINAL-BLOCK-1' | dd of="$ORIGINAL" bs=1 seek=4096 conv=notrunc status=none
printf 'LOOM-STAGE1-ORIGINAL-BLOCK-2' | dd of="$ORIGINAL" bs=1 seek=8192 conv=notrunc status=none

debugfs -w -R 'mkdir /system' "$STOCK" >/dev/null 2>&1
debugfs -w -R 'mkdir /system/etc' "$STOCK" >/dev/null 2>&1
debugfs -w -R "write $ORIGINAL /system/etc/payload.bin" "$STOCK" >/dev/null 2>&1

set +e
e2fsck -fy "$STOCK" >/dev/null
FSCK_RC=$?
set -e
if (( FSCK_RC > 1 )); then
  echo "stock ext4 validation failed with e2fsck rc=$FSCK_RC" >&2
  exit "$FSCK_RC"
fi

STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"

dd if=/dev/zero of="$REPLACEMENT" bs=4096 count=3 status=none
printf 'LOOM-STAGE1-REPLACED-BLOCK-0' | dd of="$REPLACEMENT" bs=1 seek=0 conv=notrunc status=none
printf 'LOOM-STAGE1-REPLACED-BLOCK-1' | dd of="$REPLACEMENT" bs=1 seek=4096 conv=notrunc status=none
printf 'LOOM-STAGE1-REPLACED-BLOCK-2' | dd of="$REPLACEMENT" bs=1 seek=8192 conv=notrunc status=none

ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"

COMPILE_OUTPUT="$(
  "$LOOM" ext4-replace \
    "$STOCK" \
    /system/etc/payload.bin \
    "$REPLACEMENT" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'data_blocks=3'
if [[ "$(stat -c %s "$SHADOW")" -ne 12288 ]]; then
  echo "unexpected Stage 1 shadow size" >&2
  exit 1
fi

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"

sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t ext4 -o ro,noload "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp -n 12288 "$MOUNT_DIR/system/etc/payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"

sudo e2fsck -fn "/dev/mapper/$MAPPER" >/dev/null

STOCK_HASH_AFTER="$(sha256sum "$STOCK" | awk '{print $1}')"
if [[ "$STOCK_HASH_BEFORE" != "$STOCK_HASH_AFTER" ]]; then
  echo "origin image changed: $STOCK_HASH_BEFORE -> $STOCK_HASH_AFTER" >&2
  exit 1
fi

DUMPED_ORIGIN="$WORK/origin-after.bin"
debugfs -R "dump /system/etc/payload.bin $DUMPED_ORIGIN" "$STOCK" >/dev/null 2>&1
cmp -n 12288 "$DUMPED_ORIGIN" "$ORIGINAL"

printf '%s\n' \
  "Stage 1 PASS" \
  "  target: /system/etc/payload.bin" \
  "  replacement bytes: 12288" \
  "  shadow bytes: $(stat -c %s "$SHADOW")" \
  "  origin sha256: $STOCK_HASH_AFTER"
