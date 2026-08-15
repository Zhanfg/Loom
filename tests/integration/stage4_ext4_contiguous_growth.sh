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
MAPPER="loom-stage4-${RANDOM}-${RANDOM}"

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
BLOCK_SIZE=4096
NEW_BLOCKS=8
EFFECTIVE_BLOCKS=$((NEW_BLOCKS + 1))
EXPECTED_SHADOW_BLOCKS=$((NEW_BLOCKS + 4))

truncate -s 96M "$STOCK"
mkfs.ext4 -q -F -b "$BLOCK_SIZE" "$STOCK"

dd if=/dev/zero bs="$BLOCK_SIZE" count=1 status=none | tr '\000' 'A' > "$ORIGINAL"
printf 'LOOM-STAGE4-STOCK' | dd of="$ORIGINAL" bs=1 seek=29 conv=notrunc status=none

debugfs -w -R 'mkdir /system' "$STOCK" >/dev/null 2>&1
debugfs -w -R 'mkdir /system/etc' "$STOCK" >/dev/null 2>&1
debugfs -w -R "write $ORIGINAL /system/etc/grow-run.bin" "$STOCK" >/dev/null 2>&1

set +e
e2fsck -fy "$STOCK" >/dev/null
FSCK_RC=$?
set -e
if (( FSCK_RC > 1 )); then
  echo "Stage 4 fixture e2fsck failed: rc=$FSCK_RC" >&2
  exit "$FSCK_RC"
fi

[[ "$(debugfs -R 'blocks /system/etc/grow-run.bin' "$STOCK" 2>/dev/null | wc -w)" -eq 1 ]]
dumpe2fs -h "$STOCK" 2>/dev/null | grep -q 'metadata_csum'
STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"

cp "$ORIGINAL" "$GROWN"
for index in $(seq 1 "$NEW_BLOCKS"); do
  dd if=/dev/zero bs="$BLOCK_SIZE" count=1 status=none | tr '\000' "$(printf \\$(printf '%03o' $((66 + index))))" >> "$GROWN"
done
printf 'LOOM-STAGE4-LAST-BLOCK' | \
  dd of="$GROWN" bs=1 seek=$(((EFFECTIVE_BLOCKS - 1) * BLOCK_SIZE + 37)) conv=notrunc status=none

ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"
OUTPUT="$(
  "$LOOM" ext4-grow-run \
    "$STOCK" \
    /system/etc/grow-run.bin \
    "$GROWN" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$OUTPUT" | grep -q 'original_data_blocks=1'
echo "$OUTPUT" | grep -q "effective_data_blocks=$EFFECTIVE_BLOCKS"
echo "$OUTPUT" | grep -q "new_data_blocks=$NEW_BLOCKS"
echo "$OUTPUT" | grep -q 'existing_data_shadow_blocks=0'
echo "$OUTPUT" | grep -q 'inode_metadata_blocks=1'
echo "$OUTPUT" | grep -q 'allocator_metadata_blocks=3'
echo "$OUTPUT" | grep -q "shadow_blocks=$EXPECTED_SHADOW_BLOCKS"
[[ "$(stat -c %s "$SHADOW")" -eq $((EXPECTED_SHADOW_BLOCKS * BLOCK_SIZE)) ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t ext4 -o ro,noload "/dev/mapper/$MAPPER" "$MOUNT_DIR"
[[ "$(sudo stat -c %s "$MOUNT_DIR/system/etc/grow-run.bin")" -eq $((EFFECTIVE_BLOCKS * BLOCK_SIZE)) ]]
sudo cmp "$MOUNT_DIR/system/etc/grow-run.bin" "$GROWN"
sudo umount "$MOUNT_DIR"
sudo e2fsck -fn "/dev/mapper/$MAPPER" >/dev/null

STOCK_HASH_AFTER="$(sha256sum "$STOCK" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]
DUMPED="$WORK/origin-after.bin"
debugfs -R "dump /system/etc/grow-run.bin $DUMPED" "$STOCK" >/dev/null 2>&1
[[ "$(stat -c %s "$DUMPED")" -eq "$BLOCK_SIZE" ]]
cmp "$DUMPED" "$ORIGINAL"

# Stage 4 is deliberately bounded. More than 64 new blocks is rejected before
# creating output artifacts so the PoC cannot silently become an unbounded allocator.
cp "$ORIGINAL" "$TOO_LARGE"
for _ in $(seq 1 65); do
  dd if=/dev/zero bs="$BLOCK_SIZE" count=1 status=none | tr '\000' 'Z' >> "$TOO_LARGE"
done
set +e
REJECT_OUTPUT="$(
  "$LOOM" ext4-grow-run \
    "$STOCK" \
    /system/etc/grow-run.bin \
    "$TOO_LARGE" \
    "$WORK/reject-shadow.pack" \
    ORIGIN_PLACEHOLDER \
    SHADOW_PLACEHOLDER \
    "$WORK/reject.table" 2>&1
)"
REJECT_RC=$?
set -e
if (( REJECT_RC == 0 )); then
  echo "unbounded Stage 4 growth was accepted unexpectedly" >&2
  exit 1
fi
echo "$REJECT_OUTPUT" | grep -q 'at most 64 new data blocks'
[[ ! -e "$WORK/reject-shadow.pack" ]]
[[ ! -e "$WORK/reject.table" ]]

printf '%s\n' \
  "Stage 4 contiguous growth PASS" \
  "  original data blocks: 1" \
  "  new data blocks: $NEW_BLOCKS" \
  "  effective data blocks: $EFFECTIVE_BLOCKS" \
  "  shadow blocks: $EXPECTED_SHADOW_BLOCKS" \
  "  origin sha256: $STOCK_HASH_AFTER"
