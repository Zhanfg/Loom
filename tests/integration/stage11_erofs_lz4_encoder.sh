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
MAPPER="loom-stage11-${RANDOM}-${RANDOM}"

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
SHORT="$WORK/short.bin"
INCOMPRESSIBLE="$WORK/incompressible.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

# Two logical 4 KiB lclusters compress into one existing 4 KiB physical pcluster.
dd if=/dev/zero bs=4096 count=2 status=none | tr '\000' 'A' > "$ORIGINAL"
printf 'LOOM-STAGE11-STOCK' | dd of="$ORIGINAL" bs=1 seek=64 conv=notrunc status=none
cp "$ORIGINAL" "$SOURCE/000payload.bin"

# Replacement is intentionally different but strongly compressible. Loom, not mkfs.erofs,
# must create the raw LZ4 block used in the shadow pcluster.
dd if=/dev/zero bs=4096 count=2 status=none | tr '\000' 'B' > "$REPLACEMENT"
printf 'LOOM-STAGE11-REPLACEMENT' | dd of="$REPLACEMENT" bs=1 seek=64 conv=notrunc status=none

# Keep the target in externally-backed directory data.
for i in $(seq -w 0 499); do
  : > "$SOURCE/z_dummy_${i}_for_directory_growth"
done

mkfs.erofs -b 4096 -C 4096 -zlz4 -E legacy-compress,noinline_data -T 0 \
  "$STOCK" "$SOURCE" >/dev/null
fsck.erofs "$STOCK" >/dev/null

STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"

COMPILE_OUTPUT="$(
  "$LOOM" erofs-lz4-replace \
    "$STOCK" \
    /000payload.bin \
    "$REPLACEMENT" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'shadow_blocks=1'
echo "$COMPILE_OUTPUT" | grep -q 'block_size=4096'
ENCODED_BYTES="$(printf '%s\n' "$COMPILE_OUTPUT" | sed -n 's/.*encoded_bytes=\([0-9][0-9]*\).*/\1/p')"
[[ -n "$ENCODED_BYTES" ]]
[[ "$ENCODED_BYTES" -gt 0 ]]
[[ "$ENCODED_BYTES" -le 4096 ]]
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

sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"
SHADOW_LOOP=""

sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORIGINAL"
sudo umount "$MOUNT_DIR"

# Size mismatch must fail before producing artifacts.
dd if=/dev/zero of="$SHORT" bs=4096 count=1 status=none
rm -f "$WORK/short.shadow" "$WORK/short.table" "$WORK/short.err"
if "$LOOM" erofs-lz4-replace \
  "$STOCK" /000payload.bin "$SHORT" \
  "$WORK/short.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/short.table" \
  >"$WORK/short.out" 2>"$WORK/short.err"; then
  echo 'Stage 11 expected size-mismatch rejection' >&2
  exit 1
fi
grep -q 'replacement size mismatch' "$WORK/short.err"
[[ ! -e "$WORK/short.shadow" ]]
[[ ! -e "$WORK/short.table" ]]

# Deterministic pseudo-random bytes are deliberately incompressible enough that a raw LZ4
# block cannot fit in the existing 4 KiB pcluster. Fail closed instead of rewriting indexes.
python3 - "$INCOMPRESSIBLE" <<'PY'
import random
import sys
rng = random.Random(0x4c4f4f4d)
with open(sys.argv[1], 'wb') as f:
    f.write(bytes(rng.randrange(256) for _ in range(8192)))
PY
rm -f "$WORK/random.shadow" "$WORK/random.table" "$WORK/random.err"
if "$LOOM" erofs-lz4-replace \
  "$STOCK" /000payload.bin "$INCOMPRESSIBLE" \
  "$WORK/random.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/random.table" \
  >"$WORK/random.out" 2>"$WORK/random.err"; then
  echo 'Stage 11 expected compression-footprint rejection' >&2
  exit 1
fi
grep -q 'does not fit existing pcluster' "$WORK/random.err"
[[ ! -e "$WORK/random.shadow" ]]
[[ ! -e "$WORK/random.table" ]]

printf '%s\n' \
  'Stage 11 Loom-owned EROFS LZ4 encoder PASS' \
  '  logical bytes: 8192' \
  "  encoded bytes: $ENCODED_BYTES" \
  '  physical shadow blocks: 1' \
  '  replacement-image oracle: removed' \
  '  size mismatch rejection: PASS' \
  '  footprint overflow rejection: PASS' \
  "  origin sha256: $STOCK_HASH_AFTER"
