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
GROWN="$WORK/grown.bin"
TOO_LARGE="$WORK/too-large.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

truncate -s 64M "$STOCK"
mkfs.ext4 -q -F -b 4096 "$STOCK"

dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'A' > "$ORIGINAL"
printf 'LOOM-STAGE3-STOCK' | dd of="$ORIGINAL" bs=1 seek=31 conv=notrunc status=none

debugfs -w -R 'mkdir /system' "$STOCK" >/dev/null 2>&1
debugfs -w -R 'mkdir /system/etc' "$STOCK" >/dev/null 2>&1
debugfs -w -R "write $ORIGINAL /system/etc/grow.bin" "$STOCK" >/dev/null 2>&1

set +e
e2fsck -fy "$STOCK" >/dev/null
FSCK_RC=$?
set -e
if (( FSCK_RC > 1 )); then
  echo "Stage 3 fixture e2fsck failed: rc=$FSCK_RC" >&2
  exit "$FSCK_RC"
fi

[[ "$(debugfs -R 'blocks /system/etc/grow.bin' "$STOCK" 2>/dev/null | wc -w)" -eq 1 ]]
dumpe2fs -h "$STOCK" 2>/dev/null | grep -q 'metadata_csum'
STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"

# Preserve logical block 0 byte-for-byte and add exactly one new logical block.
cp "$ORIGINAL" "$GROWN"
dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'B' >> "$GROWN"
printf 'LOOM-STAGE3-NEW-BLOCK' | dd of="$GROWN" bs=1 seek=4128 conv=notrunc status=none

ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"
OUTPUT="$(
  "$LOOM" ext4-grow-one \
    "$STOCK" \
    /system/etc/grow.bin \
    "$GROWN" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$OUTPUT" | grep -q 'original_data_blocks=1'
echo "$OUTPUT" | grep -q 'effective_data_blocks=2'
echo "$OUTPUT" | grep -q 'new_data_blocks=1'
echo "$OUTPUT" | grep -q 'existing_data_shadow_blocks=0'
echo "$OUTPUT" | grep -q 'inode_metadata_blocks=1'
echo "$OUTPUT" | grep -q 'allocator_metadata_blocks=3'
echo "$OUTPUT" | grep -q 'shadow_blocks=5'
[[ "$(stat -c %s "$SHADOW")" -eq 20480 ]]
[[ "$(grep -c 'LOOM_SHADOW_PLACEHOLDER' "$TABLE")" -eq 5 ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t ext4 -o ro,noload "/dev/mapper/$MAPPER" "$MOUNT_DIR"
[[ "$(sudo stat -c %s "$MOUNT_DIR/system/etc/grow.bin")" -eq 8192 ]]
sudo cmp "$MOUNT_DIR/system/etc/grow.bin" "$GROWN"
sudo umount "$MOUNT_DIR"

# e2fsck is the integrity oracle for the full allocator metadata closure.
sudo e2fsck -fn "/dev/mapper/$MAPPER" >/dev/null

STOCK_HASH_AFTER="$(sha256sum "$STOCK" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]
DUMPED="$WORK/origin-after.bin"
debugfs -R "dump /system/etc/grow.bin $DUMPED" "$STOCK" >/dev/null 2>&1
[[ "$(stat -c %s "$DUMPED")" -eq 4096 ]]
cmp "$DUMPED" "$ORIGINAL"

# Stage 3 is deliberately one-block-only; requiring two new blocks must reject
# before any output artifact is created.
cat "$GROWN" > "$TOO_LARGE"
dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'C' >> "$TOO_LARGE"
set +e
REJECT_OUTPUT="$(
  "$LOOM" ext4-grow-one \
    "$STOCK" \
    /system/etc/grow.bin \
    "$TOO_LARGE" \
    "$WORK/reject-shadow.pack" \
    ORIGIN_PLACEHOLDER \
    SHADOW_PLACEHOLDER \
    "$WORK/reject.table" 2>&1
)"
REJECT_RC=$?
set -e
if (( REJECT_RC == 0 )); then
  echo "two-block allocator growth was accepted unexpectedly" >&2
  exit 1
fi
echo "$REJECT_OUTPUT" | grep -q 'exactly one new data block'
[[ ! -e "$WORK/reject-shadow.pack" ]]
[[ ! -e "$WORK/reject.table" ]]

printf '%s\n' \
  "Stage 3 one-block allocator PASS" \
  "  original data blocks: 1" \
  "  effective data blocks: 2" \
  "  shadow blocks: 5" \
  "  rejected multi-block growth: PASS" \
  "  origin sha256: $STOCK_HASH_AFTER"
