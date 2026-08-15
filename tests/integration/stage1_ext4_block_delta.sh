#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
MAPPER="loom-delta-${RANDOM}-${RANDOM}"
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
MULTI="$WORK/multi.bin"
NOOP="$WORK/noop.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"
MULTI_SHADOW="$WORK/multi-shadow.pack"
MULTI_TABLE="$WORK/multi.table"
NOOP_SHADOW="$WORK/noop-shadow.pack"
NOOP_TABLE="$WORK/noop.table"

truncate -s 80M "$STOCK"
mkfs.ext4 -q -F -b 4096 "$STOCK"

# 1 MiB = 256 ext4 data blocks, all explicitly allocated/non-zero.
dd if=/dev/zero bs=1M count=1 status=none | tr '\000' 'A' > "$ORIGINAL"
debugfs -w -R 'mkdir /system' "$STOCK" >/dev/null 2>&1
debugfs -w -R 'mkdir /system/framework' "$STOCK" >/dev/null 2>&1
debugfs -w -R "write $ORIGINAL /system/framework/large.bin" "$STOCK" >/dev/null 2>&1

FIXTURE_BLOCKS="$(debugfs -R 'blocks /system/framework/large.bin' "$STOCK" 2>/dev/null | wc -w)"
if [[ "$FIXTURE_BLOCKS" -ne 256 ]]; then
  echo "delta fixture expected 256 dense blocks, got $FIXTURE_BLOCKS" >&2
  exit 1
fi

set +e
e2fsck -fy "$STOCK" >/dev/null
FSCK_RC=$?
set -e
if (( FSCK_RC > 1 )); then
  echo "delta fixture e2fsck failed with rc=$FSCK_RC" >&2
  exit "$FSCK_RC"
fi

STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"
cp "$ORIGINAL" "$REPLACEMENT"
cp "$ORIGINAL" "$MULTI"
cp "$ORIGINAL" "$NOOP"

# Change exactly one complete logical file block in the middle.
dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'B' | \
  dd of="$REPLACEMENT" bs=4096 seek=127 count=1 conv=notrunc status=none

ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"
COMPILE_OUTPUT="$(
  "$LOOM" ext4-replace \
    "$STOCK" \
    /system/framework/large.bin \
    "$REPLACEMENT" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'data_blocks=256'
echo "$COMPILE_OUTPUT" | grep -q 'shadow_blocks=1'
if [[ "$(stat -c %s "$SHADOW")" -ne 4096 ]]; then
  echo "1 MiB / one-block delta emitted more than one shadow block" >&2
  echo "$COMPILE_OUTPUT" >&2
  exit 1
fi

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t ext4 -o ro,noload "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/system/framework/large.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo e2fsck -fn "/dev/mapper/$MAPPER" >/dev/null
sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"
SHADOW_LOOP=""

# Change two non-adjacent file blocks. They must become two shadow blocks while
# the untouched range between them stays mapped to the origin.
dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'C' | \
  dd of="$MULTI" bs=4096 seek=10 count=1 conv=notrunc status=none
dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'D' | \
  dd of="$MULTI" bs=4096 seek=200 count=1 conv=notrunc status=none

MULTI_OUTPUT="$(
  "$LOOM" ext4-replace \
    "$STOCK" \
    /system/framework/large.bin \
    "$MULTI" \
    "$MULTI_SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_MULTI_SHADOW_PLACEHOLDER \
    "$MULTI_TABLE"
)"
echo "$MULTI_OUTPUT" | grep -q 'data_blocks=256'
echo "$MULTI_OUTPUT" | grep -q 'shadow_blocks=2'
[[ "$(stat -c %s "$MULTI_SHADOW")" -eq 8192 ]]
if [[ "$(grep -c 'LOOM_MULTI_SHADOW_PLACEHOLDER' "$MULTI_TABLE")" -ne 2 ]]; then
  echo "two non-adjacent changed blocks did not produce two shadow extents" >&2
  cat "$MULTI_TABLE" >&2
  exit 1
fi

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$MULTI_SHADOW")"
sed -i "s|LOOM_MULTI_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$MULTI_TABLE"
sudo dmsetup create "$MAPPER" < "$MULTI_TABLE"
sudo mount -t ext4 -o ro,noload "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/system/framework/large.bin" "$MULTI"
sudo umount "$MOUNT_DIR"
sudo e2fsck -fn "/dev/mapper/$MAPPER" >/dev/null
sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"
SHADOW_LOOP=""

# No-op replacement must not allocate any shadow block at all.
NOOP_OUTPUT="$(
  "$LOOM" ext4-replace \
    "$STOCK" \
    /system/framework/large.bin \
    "$NOOP" \
    "$NOOP_SHADOW" \
    "$ORIGIN_LOOP" \
    UNUSED_SHADOW_DEVICE \
    "$NOOP_TABLE"
)"
echo "$NOOP_OUTPUT" | grep -q 'data_blocks=256'
echo "$NOOP_OUTPUT" | grep -q 'shadow_blocks=0'
[[ "$(stat -c %s "$NOOP_SHADOW")" -eq 0 ]]
if grep -q 'UNUSED_SHADOW_DEVICE' "$NOOP_TABLE"; then
  echo "no-op map still references a shadow source" >&2
  exit 1
fi
if [[ "$(wc -l < "$NOOP_TABLE")" -ne 1 ]]; then
  echo "no-op map should collapse to one origin extent" >&2
  cat "$NOOP_TABLE" >&2
  exit 1
fi

STOCK_HASH_AFTER="$(sha256sum "$STOCK" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]

printf '%s\n' \
  "Stage 1 block-delta PASS" \
  "  logical file blocks: 256" \
  "  single changed block shadow: 4096 bytes" \
  "  two non-adjacent changed blocks shadow: 8192 bytes" \
  "  no-op shadow bytes: 0" \
  "  origin sha256: $STOCK_HASH_AFTER"
