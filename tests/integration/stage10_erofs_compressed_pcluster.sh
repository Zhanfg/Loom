#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
STOCK_SRC="$WORK/stock-root"
REPL_SRC="$WORK/repl-root"
MOUNT_DIR="$WORK/mnt"
ORIGIN_LOOP=""
REPL_LOOP=""
SHADOW_LOOP=""
MAPPER="loom-stage10-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then sudo umount "$MOUNT_DIR"; fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then sudo dmsetup remove "$MAPPER"; fi
  if [[ -n "$SHADOW_LOOP" ]]; then sudo losetup -d "$SHADOW_LOOP"; fi
  if [[ -n "$REPL_LOOP" ]]; then sudo losetup -d "$REPL_LOOP"; fi
  if [[ -n "$ORIGIN_LOOP" ]]; then sudo losetup -d "$ORIGIN_LOOP"; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$STOCK_SRC" "$REPL_SRC" "$MOUNT_DIR"
STOCK_IMG="$WORK/stock.erofs"
REPL_IMG="$WORK/replacement.erofs"
ORIGINAL="$WORK/original.bin"
REPLACEMENT="$WORK/replacement.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

# Exactly one logical lcluster, deliberately highly compressible so mkfs.erofs
# selects LZ4 compression. Both images have identical topology but different data.
dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'A' > "$ORIGINAL"
printf 'LOOM-STAGE10-STOCK' | dd of="$ORIGINAL" bs=1 seek=64 conv=notrunc status=none
dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'B' > "$REPLACEMENT"
printf 'LOOM-STAGE10-REPLACEMENT' | dd of="$REPLACEMENT" bs=1 seek=64 conv=notrunc status=none
cp "$ORIGINAL" "$STOCK_SRC/000payload.bin"
cp "$REPLACEMENT" "$REPL_SRC/000payload.bin"

# Keep the target in externally-backed directory data so path traversal does not
# depend on compressed-tail or inline-directory support in this stage.
for i in $(seq -w 0 499); do
  : > "$STOCK_SRC/z_dummy_${i}_for_directory_growth"
  : > "$REPL_SRC/z_dummy_${i}_for_directory_growth"
done

build_image() {
  local output="$1"
  local source="$2"
  rm -f "$output"
  mkfs.erofs -b 4096 -zlz4 -E legacy-compress -T 0 "$output" "$source" >/dev/null
}

build_image "$STOCK_IMG" "$STOCK_SRC"
build_image "$REPL_IMG" "$REPL_SRC"
fsck.erofs "$STOCK_IMG" >/dev/null
fsck.erofs "$REPL_IMG" >/dev/null

# Treat the replacement image as an encoding oracle only after native EROFS proves
# that its compressed pcluster decodes to the intended replacement bytes.
REPL_LOOP="$(sudo losetup --find --show --read-only "$REPL_IMG")"
sudo mount -t erofs -o ro "$REPL_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo losetup -d "$REPL_LOOP"
REPL_LOOP=""

STOCK_HASH_BEFORE="$(sha256sum "$STOCK_IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK_IMG")"

COMPILE_OUTPUT="$(
  "$LOOM" erofs-pcluster-swap \
    "$STOCK_IMG" \
    /000payload.bin \
    "$REPL_IMG" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'shadow_blocks=1'
echo "$COMPILE_OUTPUT" | grep -q 'block_size=4096'
[[ "$(stat -c %s "$SHADOW")" -eq 4096 ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"

sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null

STOCK_HASH_AFTER="$(sha256sum "$STOCK_IMG" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]

# Release the composed device before independently validating the authoritative lower.
sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"
SHADOW_LOOP=""

sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORIGINAL"
sudo umount "$MOUNT_DIR"

printf '%s\n' \
  'Stage 10 compressed EROFS pcluster swap PASS' \
  '  logical bytes: 4096' \
  '  encoded replacement source: independently native-mounted and verified' \
  '  shadow blocks: 1' \
  "  shadow bytes: $(stat -c %s "$SHADOW")" \
  "  origin sha256: $STOCK_HASH_AFTER"
