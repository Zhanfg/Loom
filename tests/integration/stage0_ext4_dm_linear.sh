#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
MAPPER="loom-stage0-${RANDOM}-${RANDOM}"
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

dd if=/dev/zero of="$ORIGINAL" bs=4096 count=1 status=none
printf 'LOOM-STAGE0-ORIGINAL' | dd of="$ORIGINAL" conv=notrunc status=none

debugfs -w -R "write $ORIGINAL /payload.bin" "$STOCK" >/dev/null 2>&1
set +e
e2fsck -fy "$STOCK" >/dev/null
FSCK_RC=$?
set -e
if (( FSCK_RC > 1 )); then
  echo "stock ext4 validation failed with e2fsck rc=$FSCK_RC" >&2
  exit "$FSCK_RC"
fi

FILE_BLOCK="$(
  debugfs -R 'blocks /payload.bin' "$STOCK" 2>/dev/null \
    | tr ' ' '\n' \
    | grep -E '^[0-9]+$' \
    | head -n 1
)"
if [[ -z "$FILE_BLOCK" ]]; then
  echo "failed to locate ext4 data block for /payload.bin" >&2
  exit 1
fi

STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"

dd if=/dev/zero of="$REPLACEMENT" bs=4096 count=1 status=none
printf 'LOOM-STAGE0-REPLACED' | dd of="$REPLACEMENT" conv=notrunc status=none
"$LOOM" pack-block "$REPLACEMENT" "$SHADOW" 4096

ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"
SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
TOTAL_SECTORS="$(sudo blockdev --getsz "$ORIGIN_LOOP")"
SECTORS_PER_BLOCK=$((4096 / 512))
REPLACE_START=$((FILE_BLOCK * SECTORS_PER_BLOCK))

"$LOOM" map-single \
  "$TOTAL_SECTORS" \
  "$REPLACE_START" \
  "$SECTORS_PER_BLOCK" \
  0 \
  "$ORIGIN_LOOP" \
  "$SHADOW_LOOP" \
  "$TABLE"

sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t ext4 -o ro,noload "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp -n 4096 "$MOUNT_DIR/payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"

sudo e2fsck -fn "/dev/mapper/$MAPPER" >/dev/null

STOCK_HASH_AFTER="$(sha256sum "$STOCK" | awk '{print $1}')"
if [[ "$STOCK_HASH_BEFORE" != "$STOCK_HASH_AFTER" ]]; then
  echo "origin image changed: $STOCK_HASH_BEFORE -> $STOCK_HASH_AFTER" >&2
  exit 1
fi

DUMPED_ORIGIN="$WORK/origin-after.bin"
debugfs -R "dump /payload.bin $DUMPED_ORIGIN" "$STOCK" >/dev/null 2>&1
cmp -n 4096 "$DUMPED_ORIGIN" "$ORIGINAL"

printf '%s\n' \
  "Stage 0 PASS" \
  "  ext4 data block: $FILE_BLOCK" \
  "  replaced sectors: $SECTORS_PER_BLOCK" \
  "  shadow bytes: $(stat -c %s "$SHADOW")" \
  "  origin sha256: $STOCK_HASH_AFTER"
