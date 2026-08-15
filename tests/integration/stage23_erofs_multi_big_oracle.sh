#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
ORIGIN_ROOT="$WORK/origin-root"
REPLACEMENT_ROOT="$WORK/replacement-root"
MOUNT_DIR="$WORK/mnt"
ORIGIN_LOOP=""
SHADOW_LOOP=""
MAPPER="loom-stage23-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then sudo umount "$MOUNT_DIR"; fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then sudo dmsetup remove "$MAPPER"; fi
  if [[ -n "$SHADOW_LOOP" ]]; then sudo losetup -d "$SHADOW_LOOP"; fi
  if [[ -n "$ORIGIN_LOOP" ]]; then sudo losetup -d "$ORIGIN_LOOP"; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$ORIGIN_ROOT" "$REPLACEMENT_ROOT" "$MOUNT_DIR"
ORIGINAL="$WORK/original.bin"
REPLACEMENT="$WORK/replacement.bin"
ORIGIN_IMG="$WORK/origin.erofs"
REPLACEMENT_IMG="$WORK/replacement.erofs"
MISMATCH_IMG="$WORK/mismatch.erofs"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

python3 - "$ORIGINAL" "$REPLACEMENT" <<'PY'
import random
import sys

SIZE = 32768
PERIOD = 10000

def build(seed_base, marker):
    data = bytearray()
    for extent in range(3):
        rng = random.Random(seed_base + extent)
        period = bytes(rng.randrange(256) for _ in range(PERIOD))
        part = bytearray((period * 4)[:SIZE])
        tag = marker + str(extent).encode()
        part[64:64 + len(tag)] = tag
        data.extend(part)
    return data

open(sys.argv[1], 'wb').write(build(0x230100, b'LOOM-STAGE23-ORIGIN-'))
open(sys.argv[2], 'wb').write(build(0x230200, b'LOOM-STAGE23-REPLACEMENT-'))
PY

cp "$ORIGINAL" "$ORIGIN_ROOT/000payload.bin"
cp "$REPLACEMENT" "$REPLACEMENT_ROOT/000payload.bin"
for i in $(seq -w 0 499); do
  : > "$ORIGIN_ROOT/z_dummy_${i}_for_directory_growth"
  : > "$REPLACEMENT_ROOT/z_dummy_${i}_for_directory_growth"
done

# 96 KiB logical file, forced into three 32 KiB variable-length extents. Each extent
# has a 16 KiB max pcluster and the deterministic payload materializes as CBLKCNT=3.
mkfs.erofs -b 4096 -C 16384 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 32768 "$ORIGIN_IMG" "$ORIGIN_ROOT" >/dev/null
mkfs.erofs -b 4096 -C 16384 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 32768 "$REPLACEMENT_IMG" "$REPLACEMENT_ROOT" >/dev/null
# Same logical shape with a smaller pcluster cap must be rejected as an incompatible
# oracle footprint rather than partially materialized.
mkfs.erofs -b 4096 -C 8192 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 32768 "$MISMATCH_IMG" "$REPLACEMENT_ROOT" >/dev/null

fsck.erofs "$ORIGIN_IMG" >/dev/null
fsck.erofs "$REPLACEMENT_IMG" >/dev/null
fsck.erofs "$MISMATCH_IMG" >/dev/null

REPLACEMENT_LOOP="$(sudo losetup --find --show --read-only "$REPLACEMENT_IMG")"
sudo mount -t erofs -o ro "$REPLACEMENT_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo losetup -d "$REPLACEMENT_LOOP"

STOCK_HASH_BEFORE="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$ORIGIN_IMG")"

OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --multi \
    "$ORIGIN_IMG" /000payload.bin "$REPLACEMENT_IMG" \
    "$SHADOW" "$ORIGIN_LOOP" LOOM_SHADOW_PLACEHOLDER "$TABLE"
)"
printf '%s\n' "$OUTPUT"

echo "$OUTPUT" | grep -q 'mode=multi'
echo "$OUTPUT" | grep -q 'physical_pclusters=3'
echo "$OUTPUT" | grep -q 'logical_lclusters=24'
echo "$OUTPUT" | grep -q 'head_lclusters=\[0, 8, 16\]'
echo "$OUTPUT" | grep -q 'encoded_bytes=\[12288, 12288, 12288\]'
echo "$OUTPUT" | grep -q 'shadow_blocks=9'
[[ "$(stat -c %s "$SHADOW")" -eq 36864 ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null
sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"
SHADOW_LOOP=""

STOCK_HASH_MID="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_MID" ]]

# The scalar big adapter remains intentionally single-extent and must fail before artifacts.
rm -f "$WORK/scalar.shadow" "$WORK/scalar.table" "$WORK/scalar.out" "$WORK/scalar.err"
if "$LOOM" erofs-compact-pcluster-swap --big-oracle \
  "$ORIGIN_IMG" /000payload.bin "$REPLACEMENT_IMG" \
  "$WORK/scalar.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/scalar.table" \
  >"$WORK/scalar.out" 2>"$WORK/scalar.err"; then
  echo 'Stage 23 expected scalar big-oracle rejection for multi-extent topology' >&2
  exit 1
fi
grep -q 'unexpected single-extent topology' "$WORK/scalar.err"
[[ ! -e "$WORK/scalar.shadow" ]]
[[ ! -e "$WORK/scalar.table" ]]

# The multi oracle must compare every HEAD footprint, not only logical size/HEAD locations.
rm -f "$WORK/mismatch.shadow" "$WORK/mismatch.table" "$WORK/mismatch.out" "$WORK/mismatch.err"
if "$LOOM" erofs-compact-pcluster-swap --multi \
  "$ORIGIN_IMG" /000payload.bin "$MISMATCH_IMG" \
  "$WORK/mismatch.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/mismatch.table" \
  >"$WORK/mismatch.out" 2>"$WORK/mismatch.err"; then
  echo 'Stage 23 expected incompatible multi-big footprint rejection' >&2
  exit 1
fi
grep -Eq 'incompatible compact replacement: .*big-pcluster|big-pcluster .* differs' "$WORK/mismatch.err"
[[ ! -e "$WORK/mismatch.shadow" ]]
[[ ! -e "$WORK/mismatch.table" ]]

STOCK_HASH_AFTER="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]

sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORIGINAL"
sudo umount "$MOUNT_DIR"

printf '%s\n' \
  'Stage 23 multi big-pcluster oracle PASS' \
  '  logical bytes: 98304' \
  '  logical lclusters: 24' \
  '  HEAD lclusters: [0, 8, 16]' \
  '  big pclusters: 3' \
  '  per-pcluster CBLKCNT blocks: [3, 3, 3]' \
  '  physical shadow blocks: 9' \
  '  shadow bytes: 36864' \
  '  scalar big adapter multi-extent rejection: PASS' \
  '  per-extent footprint mismatch rejection: PASS' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $STOCK_HASH_AFTER"
