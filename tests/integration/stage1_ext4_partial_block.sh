#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
MAPPER="loom-stage1-partial-${RANDOM}-${RANDOM}"
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

head -c 9000 /dev/zero > "$ORIGINAL"
printf 'LOOM-PARTIAL-ORIGINAL-HEAD' | dd of="$ORIGINAL" bs=1 seek=0 conv=notrunc status=none
printf 'LOOM-PARTIAL-ORIGINAL-TAIL' | dd of="$ORIGINAL" bs=1 seek=8970 conv=notrunc status=none

debugfs -w -R 'mkdir /system' "$STOCK" >/dev/null 2>&1
debugfs -w -R 'mkdir /system/etc' "$STOCK" >/dev/null 2>&1
debugfs -w -R "write $ORIGINAL /system/etc/partial.bin" "$STOCK" >/dev/null 2>&1

set +e
e2fsck -fy "$STOCK" >/dev/null
FSCK_RC=$?
set -e
if (( FSCK_RC > 1 )); then
  echo "partial ext4 fixture validation failed with e2fsck rc=$FSCK_RC" >&2
  exit "$FSCK_RC"
fi

STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"

head -c 9000 /dev/zero > "$REPLACEMENT"
printf 'LOOM-PARTIAL-REPLACED-HEAD' | dd of="$REPLACEMENT" bs=1 seek=0 conv=notrunc status=none
printf 'LOOM-PARTIAL-REPLACED-TAIL' | dd of="$REPLACEMENT" bs=1 seek=8970 conv=notrunc status=none

ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"
COMPILE_OUTPUT="$(
  "$LOOM" ext4-replace \
    "$STOCK" \
    /system/etc/partial.bin \
    "$REPLACEMENT" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'data_blocks=3'
[[ "$(stat -c %s "$SHADOW")" -eq 12288 ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t ext4 -o ro,noload "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/system/etc/partial.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo e2fsck -fn "/dev/mapper/$MAPPER" >/dev/null

STOCK_HASH_AFTER="$(sha256sum "$STOCK" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]

printf '%s\n' \
  "Stage 1 partial-block PASS" \
  "  file bytes: 9000" \
  "  data blocks: 3" \
  "  shadow bytes: 12288" \
  "  origin sha256: $STOCK_HASH_AFTER"
