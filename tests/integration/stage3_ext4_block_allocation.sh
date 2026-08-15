#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
MOUNT_DIR="$WORK/mnt"
ORIGIN_LOOP=""
SHADOW_LOOP=""
MAPPER="loom-stage3-${RANDOM}-${RANDOM}"

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
mkfs.ext4 -q -F -b 4096 -O metadata_csum,64bit,extent,^bigalloc "$STOCK"

dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'A' > "$ORIGINAL"
printf 'LOOM-STAGE3-STOCK' | dd of="$ORIGINAL" bs=1 seek=32 conv=notrunc status=none

debugfs -w -R 'mkdir /system' "$STOCK" >/dev/null 2>&1
debugfs -w -R 'mkdir /system/etc' "$STOCK" >/dev/null 2>&1
debugfs -w -R "write $ORIGINAL /system/etc/grow.bin" "$STOCK" >/dev/null 2>&1

set +e
e2fsck -fy "$STOCK" >/dev/null
FSCK_RC=$?
set -e
if (( FSCK_RC > 1 )); then
  echo "Stage 3 stock fixture e2fsck failed: rc=$FSCK_RC" >&2
  exit "$FSCK_RC"
fi

[[ "$(debugfs -R 'blocks /system/etc/grow.bin' "$STOCK" 2>/dev/null | wc -w)" -eq 1 ]]
dumpe2fs -h "$STOCK" 2>/dev/null | grep -q 'metadata_csum'
! dumpe2fs -h "$STOCK" 2>/dev/null | grep -q 'bigalloc'

cp "$ORIGINAL" "$REPLACEMENT"
dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'B' >> "$REPLACEMENT"
printf 'LOOM-STAGE3-ALLOCATED' | dd of="$REPLACEMENT" bs=1 seek=4128 conv=notrunc status=none
[[ "$(stat -c %s "$REPLACEMENT")" -eq 8192 ]]

STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"
STOCK_FREE_BEFORE="$(dumpe2fs -h "$STOCK" 2>/dev/null | awk -F: '/^Free blocks:/ {gsub(/ /, "", $2); print $2}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"

COMPILE_OUTPUT="$(
  "$LOOM" ext4-grow \
    "$STOCK" \
    /system/etc/grow.bin \
    "$REPLACEMENT" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'original_size=4096'
echo "$COMPILE_OUTPUT" | grep -q 'effective_size=8192'
echo "$COMPILE_OUTPUT" | grep -q 'original_data_blocks=1'
echo "$COMPILE_OUTPUT" | grep -q 'data_shadow_blocks=1'
echo "$COMPILE_OUTPUT" | grep -q 'metadata_blocks=4'
echo "$COMPILE_OUTPUT" | grep -q 'shadow_blocks=5'
[[ "$(stat -c %s "$SHADOW")" -eq $((5 * 4096)) ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"

sudo mount -t ext4 -o ro,noload "/dev/mapper/$MAPPER" "$MOUNT_DIR"
[[ "$(sudo stat -c %s "$MOUNT_DIR/system/etc/grow.bin")" -eq 8192 ]]
sudo cmp "$MOUNT_DIR/system/etc/grow.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"

# e2fsck is the structural oracle: inode extent/i_blocks, bitmap checksum,
# group descriptor accounting/checksum and primary superblock accounting/checksum
# must all describe one coherent read-only effective filesystem.
sudo e2fsck -fn "/dev/mapper/$MAPPER" >/dev/null

EFFECTIVE_FREE="$(sudo dumpe2fs -h "/dev/mapper/$MAPPER" 2>/dev/null | awk -F: '/^Free blocks:/ {gsub(/ /, "", $2); print $2}')"
[[ "$EFFECTIVE_FREE" -eq $((STOCK_FREE_BEFORE - 1)) ]]

STOCK_HASH_AFTER="$(sha256sum "$STOCK" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]

DUMPED="$WORK/origin-after.bin"
debugfs -R "dump /system/etc/grow.bin $DUMPED" "$STOCK" >/dev/null 2>&1
[[ "$(stat -c %s "$DUMPED")" -eq 4096 ]]
cmp "$DUMPED" "$ORIGINAL"

printf '%s\n' \
  "Stage 3 ext4 allocation PASS" \
  "  origin size: 4096" \
  "  effective size: 8192" \
  "  effective free blocks: $STOCK_FREE_BEFORE -> $EFFECTIVE_FREE" \
  "  shadow bytes: $(stat -c %s "$SHADOW")" \
  "  origin sha256: $STOCK_HASH_AFTER"
