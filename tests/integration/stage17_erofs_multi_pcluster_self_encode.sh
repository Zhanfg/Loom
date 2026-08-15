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
MAPPER="loom-stage17-${RANDOM}-${RANDOM}"

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
PARTIAL_FAIL="$WORK/partial-fail.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

# The 32 KiB decompressed-extent ceiling forces at least three extents across
# this 96 KiB file, but the test deliberately does not assume exact HEAD positions.
dd if=/dev/zero bs=4096 count=24 status=none | tr '\000' 'M' > "$ORIGINAL"
printf 'LOOM-STAGE17-STOCK-MULTI-ENCODE' | dd of="$ORIGINAL" bs=1 seek=64 conv=notrunc status=none
cp "$ORIGINAL" "$SOURCE/000payload.bin"

dd if=/dev/zero bs=4096 count=24 status=none | tr '\000' 'N' > "$REPLACEMENT"
printf 'LOOM-STAGE17-REPLACEMENT-MULTI-ENCODE' | dd of="$REPLACEMENT" bs=1 seek=64 conv=notrunc status=none

for i in $(seq -w 0 499); do
  : > "$SOURCE/z_dummy_${i}_for_directory_growth"
done

mkfs.erofs -b 4096 -C 4096 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 32768 "$STOCK" "$SOURCE" >/dev/null
fsck.erofs "$STOCK" >/dev/null

STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"

COMPILE_OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --multi-encode \
    "$STOCK" \
    /000payload.bin \
    "$REPLACEMENT" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'mode=multi-encode'
echo "$COMPILE_OUTPUT" | grep -q 'logical_lclusters=24'
echo "$COMPILE_OUTPUT" | grep -q 'compact_2b_entries=16'
echo "$COMPILE_OUTPUT" | grep -q 'head_lclusters=\[0,'

PCLUSTER_COUNT="$(printf '%s\n' "$COMPILE_OUTPUT" | sed -n 's/.*physical_pclusters=\([0-9][0-9]*\).*/\1/p')"
SHADOW_BLOCKS="$(printf '%s\n' "$COMPILE_OUTPUT" | sed -n 's/.*shadow_blocks=\([0-9][0-9]*\).*/\1/p')"
HEAD_VECTOR="$(printf '%s\n' "$COMPILE_OUTPUT" | sed -n 's/.*head_lclusters=\(\[[^]]*\]\).*/\1/p')"
ENCODED_VECTOR="$(printf '%s\n' "$COMPILE_OUTPUT" | sed -n 's/.*encoded_bytes=\(\[[^]]*\]\).*/\1/p')"
[[ -n "$PCLUSTER_COUNT" && -n "$SHADOW_BLOCKS" && -n "$HEAD_VECTOR" && -n "$ENCODED_VECTOR" ]]
[[ "$PCLUSTER_COUNT" -ge 3 ]]
[[ "$SHADOW_BLOCKS" -eq "$PCLUSTER_COUNT" ]]
[[ "$(stat -c %s "$SHADOW")" -eq $((PCLUSTER_COUNT * 4096)) ]]

python3 - "$SHADOW" "$HEAD_VECTOR" "$ENCODED_VECTOR" <<'PY'
import ast
import sys
shadow = open(sys.argv[1], 'rb').read()
heads = ast.literal_eval(sys.argv[2])
sizes = ast.literal_eval(sys.argv[3])
assert len(heads) >= 3
assert heads[0] == 0
assert heads == sorted(heads)
assert len(sizes) == len(heads)
assert len(shadow) == 4096 * len(sizes)
for index, encoded in enumerate(sizes):
    assert 0 < encoded < 4096
    block = shadow[index * 4096:(index + 1) * 4096]
    prefix = 4096 - encoded
    assert block[:prefix] == b'\x00' * prefix
    assert block[prefix] != 0
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

# Corrupt the second *recovered* logical extent with deterministic random bytes.
# Earlier extents remain compressible. Stage 17 must finish all pre-encoding before
# opening the effective block store, so this later footprint failure leaves no artifacts.
cp "$REPLACEMENT" "$PARTIAL_FAIL"
python3 - "$PARTIAL_FAIL" "$HEAD_VECTOR" <<'PY'
import ast
import random
import sys
path = sys.argv[1]
heads = ast.literal_eval(sys.argv[2])
assert len(heads) >= 3
start = heads[1] * 4096
end = heads[2] * 4096
assert end > start
rng = random.Random(0x53544147453137)
data = bytearray(open(path, 'rb').read())
data[start:end] = bytes(rng.randrange(256) for _ in range(end - start))
open(path, 'wb').write(data)
PY

rm -f "$WORK/fail.shadow" "$WORK/fail.table" "$WORK/fail.err"
if "$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$STOCK" /000payload.bin "$PARTIAL_FAIL" \
  "$WORK/fail.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/fail.table" \
  >"$WORK/fail.out" 2>"$WORK/fail.err"; then
  echo 'Stage 17 expected per-extent footprint rejection' >&2
  exit 1
fi
grep -q 'does not fit existing pcluster' "$WORK/fail.err"
[[ ! -e "$WORK/fail.shadow" ]]
[[ ! -e "$WORK/fail.table" ]]

printf '%s\n' \
  'Stage 17 compact multi-pcluster self-encode PASS' \
  '  logical bytes: 98304' \
  '  logical lclusters: 24' \
  "  physical pclusters: $PCLUSTER_COUNT" \
  "  HEAD lclusters: $HEAD_VECTOR" \
  "  per-extent encoded bytes: $ENCODED_VECTOR" \
  "  physical shadow blocks: $SHADOW_BLOCKS" \
  '  replacement-image oracle: removed' \
  '  later-extent footprint failure is transactional: PASS' \
  "  origin sha256: $STOCK_HASH_AFTER"
