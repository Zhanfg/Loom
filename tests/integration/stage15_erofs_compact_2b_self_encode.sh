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
MAPPER="loom-stage15-${RANDOM}-${RANDOM}"

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
INCOMPRESSIBLE="$WORK/incompressible.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

# 24 lclusters force a real 16-entry 2B compact region. The extent limit keeps
# the strongly-compressible file eligible to remain one physical pcluster.
dd if=/dev/zero bs=4096 count=24 status=none | tr '\000' 'I' > "$ORIGINAL"
printf 'LOOM-STAGE15-STOCK-SELF-ENCODE' | dd of="$ORIGINAL" bs=1 seek=64 conv=notrunc status=none
cp "$ORIGINAL" "$SOURCE/000payload.bin"

dd if=/dev/zero bs=4096 count=24 status=none | tr '\000' 'J' > "$REPLACEMENT"
printf 'LOOM-STAGE15-SELF-ENCODE-2B' | dd of="$REPLACEMENT" bs=1 seek=64 conv=notrunc status=none

for i in $(seq -w 0 499); do
  : > "$SOURCE/z_dummy_${i}_for_directory_growth"
done

mkfs.erofs -b 4096 -C 4096 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 98304 "$STOCK" "$SOURCE" >/dev/null
fsck.erofs "$STOCK" >/dev/null

STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"

COMPILE_OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --encode \
    "$STOCK" \
    /000payload.bin \
    "$REPLACEMENT" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'mode=encode'
echo "$COMPILE_OUTPUT" | grep -q 'logical_lclusters=24'
echo "$COMPILE_OUTPUT" | grep -q 'compact_2b_entries=16'
echo "$COMPILE_OUTPUT" | grep -q 'shadow_blocks=1'
ENCODED_BYTES="$(printf '%s\n' "$COMPILE_OUTPUT" | sed -n 's/.*encoded_bytes=\([0-9][0-9]*\).*/\1/p')"
[[ -n "$ENCODED_BYTES" ]]
[[ "$ENCODED_BYTES" -gt 0 ]]
[[ "$ENCODED_BYTES" -lt 4096 ]]
[[ "$(stat -c %s "$SHADOW")" -eq 4096 ]]

PREFIX_BYTES=$((4096 - ENCODED_BYTES))
python3 - "$SHADOW" "$PREFIX_BYTES" <<'PY'
import sys
path = sys.argv[1]
prefix = int(sys.argv[2])
data = open(path, 'rb').read()
assert len(data) == 4096
assert prefix > 0
assert data[:prefix] == b'\x00' * prefix
assert data[prefix] != 0
PY

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

# Large incompressible replacement must not cause index rewriting or a second
# physical block to appear in the effective view.
python3 - "$INCOMPRESSIBLE" <<'PY'
import random
import sys
rng = random.Random(0x53544147453135)
with open(sys.argv[1], 'wb') as f:
    f.write(bytes(rng.randrange(256) for _ in range(98304)))
PY
rm -f "$WORK/random.shadow" "$WORK/random.table" "$WORK/random.err"
if "$LOOM" erofs-compact-pcluster-swap --encode \
  "$STOCK" /000payload.bin "$INCOMPRESSIBLE" \
  "$WORK/random.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/random.table" \
  >"$WORK/random.out" 2>"$WORK/random.err"; then
  echo 'Stage 15 expected 96KiB footprint rejection' >&2
  exit 1
fi
grep -q 'does not fit existing compact pcluster' "$WORK/random.err"
[[ ! -e "$WORK/random.shadow" ]]
[[ ! -e "$WORK/random.table" ]]

printf '%s\n' \
  'Stage 15 compact 2B self-encode PASS' \
  '  logical bytes: 98304' \
  '  logical lclusters: 24' \
  '  compact 2B entries: 16' \
  "  encoded bytes: $ENCODED_BYTES" \
  "  leading zero padding bytes: $PREFIX_BYTES" \
  '  physical shadow blocks: 1' \
  '  replacement-image oracle: removed' \
  '  96KiB footprint overflow rejection: PASS' \
  "  origin sha256: $STOCK_HASH_AFTER"
