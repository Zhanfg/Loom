#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
SOURCE="$WORK/root"
MOUNT_DIR="$WORK/mnt"
ORIGIN_LOOP=""
SHADOW_LOOP=""
MAPPER="loom-stage9-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then sudo umount "$MOUNT_DIR"; fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then sudo dmsetup remove "$MAPPER"; fi
  if [[ -n "$SHADOW_LOOP" ]]; then sudo losetup -d "$SHADOW_LOOP"; fi
  if [[ -n "$ORIGIN_LOOP" ]]; then sudo losetup -d "$ORIGIN_LOOP"; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$SOURCE" "$MOUNT_DIR"
STOCK="$WORK/stock.erofs"
ORIGINAL="$WORK/original.bin"
REPLACEMENT="$WORK/replacement.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

# Force the root directory beyond one directory block so an early-sorting target
# is reachable in externally block-backed directory data even if mkfs selects
# FLAT_INLINE for the directory tail.
dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'E' > "$ORIGINAL"
printf 'LOOM-STAGE9-STOCK' | dd of="$ORIGINAL" bs=1 seek=32 conv=notrunc status=none
cp "$ORIGINAL" "$SOURCE/000payload.bin"
for i in $(seq -w 0 499); do
  : > "$SOURCE/z_dummy_${i}_for_directory_growth"
done

cp "$ORIGINAL" "$REPLACEMENT"
printf 'LOOM-STAGE9-SHADOW' | dd of="$REPLACEMENT" bs=1 seek=32 conv=notrunc status=none
[[ "$(stat -c %s "$REPLACEMENT")" -eq 4096 ]]

mkfs.erofs -b 4096 "$STOCK" "$SOURCE" >/dev/null
fsck.erofs "$STOCK" >/dev/null

STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"

COMPILE_OUTPUT="$(
  "$LOOM" erofs-replace \
    "$STOCK" \
    /000payload.bin \
    "$REPLACEMENT" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'shadow_blocks=1'
[[ "$(stat -c %s "$SHADOW")" -eq 4096 ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"

sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"

sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null

STOCK_HASH_AFTER="$(sha256sum "$STOCK" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]

sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORIGINAL"
sudo umount "$MOUNT_DIR"

printf '%s\n' \
  'Stage 9 EROFS flat-plain PASS' \
  '  target bytes: 4096' \
  '  shadow blocks: 1' \
  "  shadow bytes: $(stat -c %s "$SHADOW")" \
  "  origin sha256: $STOCK_HASH_AFTER"
