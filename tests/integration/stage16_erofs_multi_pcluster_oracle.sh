#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
STOCK_SRC="$WORK/stock-root"
REPL_SRC="$WORK/repl-root"
MISMATCH_SRC="$WORK/mismatch-root"
MOUNT_DIR="$WORK/mnt"
ORIGIN_LOOP=""
REPL_LOOP=""
SHADOW_LOOP=""
MAPPER="loom-stage16-${RANDOM}-${RANDOM}"

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

mkdir -p "$STOCK_SRC" "$REPL_SRC" "$MISMATCH_SRC" "$MOUNT_DIR"
STOCK_IMG="$WORK/stock.erofs"
REPL_IMG="$WORK/replacement.erofs"
MISMATCH_IMG="$WORK/mismatch.erofs"
ORIGINAL="$WORK/original.bin"
REPLACEMENT="$WORK/replacement.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

# 96 KiB with a 32 KiB decompressed-extent ceiling yields multiple logical extents.
# Each extent is strongly compressible enough to occupy one physical pcluster.
dd if=/dev/zero bs=4096 count=24 status=none | tr '\000' 'K' > "$ORIGINAL"
printf 'LOOM-STAGE16-STOCK-MULTI' | dd of="$ORIGINAL" bs=1 seek=64 conv=notrunc status=none
dd if=/dev/zero bs=4096 count=24 status=none | tr '\000' 'L' > "$REPLACEMENT"
printf 'LOOM-STAGE16-REPLACEMENT-MULTI' | dd of="$REPLACEMENT" bs=1 seek=64 conv=notrunc status=none
cp "$ORIGINAL" "$STOCK_SRC/000payload.bin"
cp "$REPLACEMENT" "$REPL_SRC/000payload.bin"
cp "$REPLACEMENT" "$MISMATCH_SRC/000payload.bin"

for i in $(seq -w 0 499); do
  : > "$STOCK_SRC/z_dummy_${i}_for_directory_growth"
  : > "$REPL_SRC/z_dummy_${i}_for_directory_growth"
  : > "$MISMATCH_SRC/z_dummy_${i}_for_directory_growth"
done

build_three_extent() {
  local output="$1"
  local source="$2"
  mkfs.erofs -b 4096 -C 4096 -zlz4 -E noinline_data -T 0 \
    --max-extent-bytes 32768 "$output" "$source" >/dev/null
}

build_three_extent "$STOCK_IMG" "$STOCK_SRC"
build_three_extent "$REPL_IMG" "$REPL_SRC"

# A different decompressed-extent ceiling deliberately changes the HEAD topology.
mkfs.erofs -b 4096 -C 4096 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 49152 "$MISMATCH_IMG" "$MISMATCH_SRC" >/dev/null

fsck.erofs "$STOCK_IMG" >/dev/null
fsck.erofs "$REPL_IMG" >/dev/null
fsck.erofs "$MISMATCH_IMG" >/dev/null

REPL_LOOP="$(sudo losetup --find --show --read-only "$REPL_IMG")"
sudo mount -t erofs -o ro "$REPL_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo losetup -d "$REPL_LOOP"
REPL_LOOP=""

STOCK_HASH_BEFORE="$(sha256sum "$STOCK_IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK_IMG")"

COMPILE_OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --multi \
    "$STOCK_IMG" \
    /000payload.bin \
    "$REPL_IMG" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'mode=multi'
echo "$COMPILE_OUTPUT" | grep -q 'physical_pclusters=3'
echo "$COMPILE_OUTPUT" | grep -q 'logical_lclusters=24'
echo "$COMPILE_OUTPUT" | grep -q 'compact_2b_entries=16'
echo "$COMPILE_OUTPUT" | grep -q 'head_lclusters=\[0,'
echo "$COMPILE_OUTPUT" | grep -q 'shadow_blocks=3'
[[ "$(stat -c %s "$SHADOW")" -eq 12288 ]]

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

# Same logical bytes but different HEAD-lcluster topology must fail before artifacts.
rm -f "$WORK/mismatch.shadow" "$WORK/mismatch.table" "$WORK/mismatch.err"
if "$LOOM" erofs-compact-pcluster-swap --multi \
  "$STOCK_IMG" /000payload.bin "$MISMATCH_IMG" \
  "$WORK/mismatch.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/mismatch.table" \
  >"$WORK/mismatch.out" 2>"$WORK/mismatch.err"; then
  echo 'Stage 16 expected topology-mismatch rejection' >&2
  exit 1
fi
grep -Eq 'physical pcluster counts differ|compressed HEAD-lcluster topology differs' "$WORK/mismatch.err"
[[ ! -e "$WORK/mismatch.shadow" ]]
[[ ! -e "$WORK/mismatch.table" ]]

# Self-encoding remains intentionally single-pcluster until Stage 17.
rm -f "$WORK/encode.shadow" "$WORK/encode.table" "$WORK/encode.err"
if "$LOOM" erofs-compact-pcluster-swap --encode \
  "$STOCK_IMG" /000payload.bin "$REPLACEMENT" \
  "$WORK/encode.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/encode.table" \
  >"$WORK/encode.out" 2>"$WORK/encode.err"; then
  echo 'Stage 16 expected multi-pcluster self-encode rejection' >&2
  exit 1
fi
grep -q 'requires exactly one encoded physical block' "$WORK/encode.err"
[[ ! -e "$WORK/encode.shadow" ]]
[[ ! -e "$WORK/encode.table" ]]

printf '%s\n' \
  'Stage 16 compact multi-pcluster oracle PASS' \
  '  logical bytes: 98304' \
  '  logical lclusters: 24' \
  '  compact 2B entries: 16' \
  '  physical pclusters: 3' \
  '  shadow blocks: 3' \
  '  topology mismatch rejection: PASS' \
  '  multi-pcluster self-encode rejection: PASS' \
  "  origin sha256: $STOCK_HASH_AFTER"
