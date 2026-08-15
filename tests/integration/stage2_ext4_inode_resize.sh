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
MAPPER=""

cleanup_case() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then
    sudo umount "$MOUNT_DIR"
  fi
  if [[ -n "$MAPPER" ]] && sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then
    sudo dmsetup remove "$MAPPER"
  fi
  if [[ -n "$SHADOW_LOOP" ]]; then
    sudo losetup -d "$SHADOW_LOOP"
  fi
  SHADOW_LOOP=""
  MAPPER=""
  set -e
}

cleanup() {
  cleanup_case
  set +e
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
SHRUNK="$WORK/shrunk.bin"

truncate -s 64M "$STOCK"
mkfs.ext4 -q -F -b 4096 "$STOCK"

dd if=/dev/zero bs=3000 count=1 status=none | tr '\000' 'A' > "$ORIGINAL"
printf 'LOOM-STAGE2-STOCK' | dd of="$ORIGINAL" bs=1 seek=16 conv=notrunc status=none

debugfs -w -R 'mkdir /system' "$STOCK" >/dev/null 2>&1
debugfs -w -R 'mkdir /system/etc' "$STOCK" >/dev/null 2>&1
debugfs -w -R "write $ORIGINAL /system/etc/resizable.bin" "$STOCK" >/dev/null 2>&1

set +e
e2fsck -fy "$STOCK" >/dev/null
FSCK_RC=$?
set -e
if (( FSCK_RC > 1 )); then
  echo "Stage 2 stock fixture e2fsck failed: rc=$FSCK_RC" >&2
  exit "$FSCK_RC"
fi

# The Stage 2 fixture must have exactly one allocated data block and modern
# metadata checksumming enabled; otherwise it is not testing the intended path.
[[ "$(debugfs -R 'blocks /system/etc/resizable.bin' "$STOCK" 2>/dev/null | wc -w)" -eq 1 ]]
dumpe2fs -h "$STOCK" 2>/dev/null | grep -q 'metadata_csum'

STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"

run_resize_case() {
  local name="$1"
  local replacement="$2"
  local expected_size="$3"
  local shadow="$WORK/$name.shadow.pack"
  local table="$WORK/$name.table"
  local compile_output

  MAPPER="loom-stage2-${name}-${RANDOM}-${RANDOM}"

  compile_output="$(
    "$LOOM" ext4-resize \
      "$STOCK" \
      /system/etc/resizable.bin \
      "$replacement" \
      "$shadow" \
      "$ORIGIN_LOOP" \
      LOOM_SHADOW_PLACEHOLDER \
      "$table"
  )"

  echo "$compile_output" | grep -q 'data_blocks=1'
  echo "$compile_output" | grep -q 'metadata_blocks=1'
  echo "$compile_output" | grep -q 'shadow_blocks=2'
  [[ "$(stat -c %s "$shadow")" -eq 8192 ]]
  [[ "$(grep -c 'LOOM_SHADOW_PLACEHOLDER' "$table")" -eq 2 ]]

  SHADOW_LOOP="$(sudo losetup --find --show --read-only "$shadow")"
  sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$table"
  sudo dmsetup create "$MAPPER" < "$table"

  sudo mount -t ext4 -o ro,noload "/dev/mapper/$MAPPER" "$MOUNT_DIR"
  [[ "$(sudo stat -c %s "$MOUNT_DIR/system/etc/resizable.bin")" -eq "$expected_size" ]]
  sudo cmp "$MOUNT_DIR/system/etc/resizable.bin" "$replacement"
  sudo umount "$MOUNT_DIR"

  # This is the checksum oracle for Stage 2: a wrong inode checksum or malformed
  # size/extent relationship must be rejected by e2fsck.
  sudo e2fsck -fn "/dev/mapper/$MAPPER" >/dev/null

  cleanup_case
}

# Growth stays within the already allocated 4 KiB data block.
dd if=/dev/zero bs=3500 count=1 status=none | tr '\000' 'G' > "$GROWN"
printf 'LOOM-STAGE2-GROWN' | dd of="$GROWN" bs=1 seek=16 conv=notrunc status=none
run_resize_case grow "$GROWN" 3500

# Shrink also remains within the same allocated block; no allocator state is touched.
dd if=/dev/zero bs=2800 count=1 status=none | tr '\000' 'S' > "$SHRUNK"
printf 'LOOM-STAGE2-SHRUNK' | dd of="$SHRUNK" bs=1 seek=16 conv=notrunc status=none
run_resize_case shrink "$SHRUNK" 2800

STOCK_HASH_AFTER="$(sha256sum "$STOCK" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]

# The authoritative origin still reports its original size and bytes.
DUMPED="$WORK/origin-after.bin"
debugfs -R "dump /system/etc/resizable.bin $DUMPED" "$STOCK" >/dev/null 2>&1
[[ "$(stat -c %s "$DUMPED")" -eq 3000 ]]
cmp "$DUMPED" "$ORIGINAL"

printf '%s\n' \
  "Stage 2 inode-resize PASS" \
  "  origin size: 3000" \
  "  grown size: 3500" \
  "  shrunk size: 2800" \
  "  allocator changes: 0" \
  "  origin sha256: $STOCK_HASH_AFTER"
