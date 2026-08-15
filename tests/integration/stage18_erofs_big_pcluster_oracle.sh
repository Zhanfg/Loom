#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM_BIG="$REPO_ROOT/target/release/loom-big"

WORK="$(mktemp -d)"
STOCK_SRC="$WORK/stock-root"
REPL_SRC="$WORK/repl-root"
NONBIG_SRC="$WORK/nonbig-root"
MOUNT_DIR="$WORK/mnt"
ORIGIN_LOOP=""
REPL_LOOP=""
SHADOW_LOOP=""
MAPPER="loom-stage18-${RANDOM}-${RANDOM}"

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

mkdir -p "$STOCK_SRC" "$REPL_SRC" "$NONBIG_SRC" "$MOUNT_DIR"
STOCK_IMG="$WORK/stock.erofs"
REPL_IMG="$WORK/replacement.erofs"
NONBIG_IMG="$WORK/nonbig.erofs"
ORIGINAL="$WORK/original.bin"
REPLACEMENT="$WORK/replacement.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

# Repeat a deterministic 6000-byte pseudo-random seed to 96 KiB. The seed is
# intentionally larger than one 4 KiB block, so LZ4 output is >4096 but <8192.
python3 - "$ORIGINAL" "$REPLACEMENT" <<'PY'
import random
import sys

TOTAL = 98304
SEED = 6000

def payload(seed_value):
    rng = random.Random(seed_value)
    seed = bytes(rng.randrange(256) for _ in range(SEED))
    return (seed * ((TOTAL + SEED - 1) // SEED))[:TOTAL]

open(sys.argv[1], 'wb').write(payload(0x5354414745313841))
open(sys.argv[2], 'wb').write(payload(0x5354414745313842))
PY
cp "$ORIGINAL" "$STOCK_SRC/000payload.bin"
cp "$REPLACEMENT" "$REPL_SRC/000payload.bin"
cp "$REPLACEMENT" "$NONBIG_SRC/000payload.bin"

for i in $(seq -w 0 499); do
  : > "$STOCK_SRC/z_dummy_${i}_for_directory_growth"
  : > "$REPL_SRC/z_dummy_${i}_for_directory_growth"
  : > "$NONBIG_SRC/z_dummy_${i}_for_directory_growth"
done

build_big() {
  local output="$1"
  local source="$2"
  mkfs.erofs -b 4096 -C 8192 -zlz4 -E noinline_data -T 0 \
    --max-extent-bytes 98304 "$output" "$source" >/dev/null
}

build_big "$STOCK_IMG" "$STOCK_SRC"
build_big "$REPL_IMG" "$REPL_SRC"

# Same logical replacement without big-pcluster capability for negative coverage.
mkfs.erofs -b 4096 -C 4096 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 98304 "$NONBIG_IMG" "$NONBIG_SRC" >/dev/null

fsck.erofs "$STOCK_IMG" >/dev/null
fsck.erofs "$REPL_IMG" >/dev/null
fsck.erofs "$NONBIG_IMG" >/dev/null

# The replacement oracle is independently proven valid before Loom reads its
# encoded big-pcluster bytes.
REPL_LOOP="$(sudo losetup --find --show --read-only "$REPL_IMG")"
sudo mount -t erofs -o ro "$REPL_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo losetup -d "$REPL_LOOP"
REPL_LOOP=""

STOCK_HASH_BEFORE="$(sha256sum "$STOCK_IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK_IMG")"

COMPILE_OUTPUT="$(
  "$LOOM_BIG" \
    "$STOCK_IMG" \
    /000payload.bin \
    "$REPL_IMG" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'mode=big'
echo "$COMPILE_OUTPUT" | grep -q 'physical_blocks=2'
echo "$COMPILE_OUTPUT" | grep -q 'logical_lclusters=24'
echo "$COMPILE_OUTPUT" | grep -q 'compact_2b_entries=16'
echo "$COMPILE_OUTPUT" | grep -q 'head_lclusters=\[0\]'
echo "$COMPILE_OUTPUT" | grep -q 'shadow_blocks=2'
[[ "$(stat -c %s "$SHADOW")" -eq 8192 ]]

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

# A non-big replacement image must be rejected before any output artifact.
rm -f "$WORK/nonbig.shadow" "$WORK/nonbig.table" "$WORK/nonbig.err"
if "$LOOM_BIG" \
  "$STOCK_IMG" /000payload.bin "$NONBIG_IMG" \
  "$WORK/nonbig.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/nonbig.table" \
  >"$WORK/nonbig.out" 2>"$WORK/nonbig.err"; then
  echo 'Stage 18 expected non-big replacement rejection' >&2
  exit 1
fi
grep -Eq 'requires only LZ4_0PADDING \+ BIG_PCLUSTER|requires COMPACTED_2B \+ BIG_PCLUSTER' "$WORK/nonbig.err"
[[ ! -e "$WORK/nonbig.shadow" ]]
[[ ! -e "$WORK/nonbig.table" ]]

printf '%s\n' \
  'Stage 18 compact big-pcluster oracle PASS' \
  '  logical bytes: 98304' \
  '  logical lclusters: 24' \
  '  compact 2B entries: 16' \
  '  logical extents: 1' \
  '  physical blocks in big pcluster: 2' \
  '  shadow blocks: 2' \
  '  non-big replacement rejection: PASS' \
  "  origin sha256: $STOCK_HASH_AFTER"
