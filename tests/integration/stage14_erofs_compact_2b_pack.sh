#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
STOCK_SRC="$WORK/stock-root"
REPL_SRC="$WORK/repl-root"
MULTI_SRC="$WORK/multi-root"
MOUNT_DIR="$WORK/mnt"
ORIGIN_LOOP=""
REPL_LOOP=""
SHADOW_LOOP=""
MAPPER="loom-stage14-${RANDOM}-${RANDOM}"

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

mkdir -p "$STOCK_SRC" "$REPL_SRC" "$MULTI_SRC" "$MOUNT_DIR"
STOCK_IMG="$WORK/stock.erofs"
REPL_IMG="$WORK/replacement.erofs"
MULTI_IMG="$WORK/multi.erofs"
ORIGINAL="$WORK/original.bin"
REPLACEMENT="$WORK/replacement.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

# 24 x 4 KiB lclusters guarantee at least one full 16-entry 2B compact pack
# regardless of the inode/map-header 8-byte alignment within the 32-byte boundary.
dd if=/dev/zero bs=4096 count=24 status=none | tr '\000' 'G' > "$ORIGINAL"
printf 'LOOM-STAGE14-STOCK-2B-PACK' | dd of="$ORIGINAL" bs=1 seek=64 conv=notrunc status=none
dd if=/dev/zero bs=4096 count=24 status=none | tr '\000' 'H' > "$REPLACEMENT"
printf 'LOOM-STAGE14-REPLACEMENT-2B-PACK' | dd of="$REPLACEMENT" bs=1 seek=64 conv=notrunc status=none
cp "$ORIGINAL" "$STOCK_SRC/000payload.bin"
cp "$REPLACEMENT" "$REPL_SRC/000payload.bin"
cp "$ORIGINAL" "$MULTI_SRC/000payload.bin"

for i in $(seq -w 0 499); do
  : > "$STOCK_SRC/z_dummy_${i}_for_directory_growth"
  : > "$REPL_SRC/z_dummy_${i}_for_directory_growth"
  : > "$MULTI_SRC/z_dummy_${i}_for_directory_growth"
done

build_single_extent() {
  local output="$1"
  local source="$2"
  mkfs.erofs -b 4096 -C 4096 -zlz4 -E noinline_data -T 0 \
    --max-extent-bytes 98304 "$output" "$source" >/dev/null
}

build_single_extent "$STOCK_IMG" "$STOCK_SRC"
build_single_extent "$REPL_IMG" "$REPL_SRC"

# Deliberately force the same logical file across multiple compressed extents.
mkfs.erofs -b 4096 -C 4096 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 32768 "$MULTI_IMG" "$MULTI_SRC" >/dev/null

fsck.erofs "$STOCK_IMG" >/dev/null
fsck.erofs "$REPL_IMG" >/dev/null
fsck.erofs "$MULTI_IMG" >/dev/null

REPL_LOOP="$(sudo losetup --find --show --read-only "$REPL_IMG")"
sudo mount -t erofs -o ro "$REPL_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo losetup -d "$REPL_LOOP"
REPL_LOOP=""

STOCK_HASH_BEFORE="$(sha256sum "$STOCK_IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK_IMG")"

COMPILE_OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap \
    "$STOCK_IMG" \
    /000payload.bin \
    "$REPL_IMG" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'mode=oracle'
echo "$COMPILE_OUTPUT" | grep -q 'logical_lclusters=24'
echo "$COMPILE_OUTPUT" | grep -q 'compact_2b_entries=16'
echo "$COMPILE_OUTPUT" | grep -q 'shadow_blocks=1'
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

sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"
SHADOW_LOOP=""

sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORIGINAL"
sudo umount "$MOUNT_DIR"

# Current Stage 14 contract is deliberately one physical pcluster. A file split
# into multiple compressed extents must fail before any shadow/table output.
rm -f "$WORK/multi.shadow" "$WORK/multi.table" "$WORK/multi.err"
if "$LOOM" erofs-compact-pcluster-swap \
  "$MULTI_IMG" /000payload.bin "$REPL_IMG" \
  "$WORK/multi.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/multi.table" \
  >"$WORK/multi.out" 2>"$WORK/multi.err"; then
  echo 'Stage 14 expected multi-pcluster rejection' >&2
  exit 1
fi
grep -q 'requires exactly one encoded physical block' "$WORK/multi.err"
[[ ! -e "$WORK/multi.shadow" ]]
[[ ! -e "$WORK/multi.table" ]]

printf '%s\n' \
  'Stage 14 real EROFS compact 2B pack PASS' \
  '  logical bytes: 98304' \
  '  logical lclusters: 24' \
  '  compact 2B entries: 16' \
  '  encoded physical blocks: 1' \
  '  shadow blocks: 1' \
  '  forced multi-pcluster rejection: PASS' \
  "  origin sha256: $STOCK_HASH_AFTER"
