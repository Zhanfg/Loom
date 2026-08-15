#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
ORIGIN_ROOT="$WORK/origin-root"
REPLACEMENT_ROOT="$WORK/replacement-root"
NORMAL_ROOT="$WORK/normal-root"
MOUNT_DIR="$WORK/mnt"
ORIGIN_LOOP=""
SHADOW_LOOP=""
MAPPER="loom-stage19-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then sudo umount "$MOUNT_DIR"; fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then sudo dmsetup remove "$MAPPER"; fi
  if [[ -n "$SHADOW_LOOP" ]]; then sudo losetup -d "$SHADOW_LOOP"; fi
  if [[ -n "$ORIGIN_LOOP" ]]; then sudo losetup -d "$ORIGIN_LOOP"; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$ORIGIN_ROOT" "$REPLACEMENT_ROOT" "$NORMAL_ROOT" "$MOUNT_DIR"
ORIGINAL="$WORK/original.bin"
REPLACEMENT="$WORK/replacement.bin"
ORIGIN_IMG="$WORK/origin.erofs"
REPLACEMENT_IMG="$WORK/replacement.erofs"
NORMAL_IMG="$WORK/normal.erofs"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

# A repeated 6000-byte deterministic random period yields an LZ4 block larger than
# 4 KiB but below 8 KiB for the 32 KiB logical payload. This forces a two-block
# pcluster under -C 8192 instead of degenerating back to the normal one-block case.
python3 - "$ORIGINAL" "$REPLACEMENT" <<'PY'
import random
import sys

def payload(seed, marker):
    rng = random.Random(seed)
    period = bytes(rng.randrange(256) for _ in range(6000))
    data = bytearray((period * 6)[:32768])
    data[64:64 + len(marker)] = marker
    return data

open(sys.argv[1], 'wb').write(payload(0x190001, b'LOOM-STAGE19-ORIGIN'))
open(sys.argv[2], 'wb').write(payload(0x190002, b'LOOM-STAGE19-REPLACEMENT'))
PY

cp "$ORIGINAL" "$ORIGIN_ROOT/000payload.bin"
cp "$REPLACEMENT" "$REPLACEMENT_ROOT/000payload.bin"
cp "$REPLACEMENT" "$NORMAL_ROOT/000payload.bin"

# Keep path traversal out of the big-pcluster proof itself. A single-entry EROFS root
# may be emitted with an inline directory tail, which this deliberately narrow parser
# refuses; dummy entries force all three roots onto ordinary directory data blocks.
for i in $(seq -w 0 499); do
  : > "$ORIGIN_ROOT/z_dummy_${i}_for_directory_growth"
  : > "$REPLACEMENT_ROOT/z_dummy_${i}_for_directory_growth"
  : > "$NORMAL_ROOT/z_dummy_${i}_for_directory_growth"
done

mkfs.erofs -b 4096 -C 8192 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 32768 "$ORIGIN_IMG" "$ORIGIN_ROOT" >/dev/null
mkfs.erofs -b 4096 -C 8192 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 32768 "$REPLACEMENT_IMG" "$REPLACEMENT_ROOT" >/dev/null
mkfs.erofs -b 4096 -C 4096 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 32768 "$NORMAL_IMG" "$NORMAL_ROOT" >/dev/null

fsck.erofs "$ORIGIN_IMG" >/dev/null
fsck.erofs "$REPLACEMENT_IMG" >/dev/null
fsck.erofs "$NORMAL_IMG" >/dev/null

# Independently prove the replacement image decodes before Loom sees it.
REPLACEMENT_LOOP="$(sudo losetup --find --show --read-only "$REPLACEMENT_IMG")"
sudo mount -t erofs -o ro "$REPLACEMENT_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo losetup -d "$REPLACEMENT_LOOP"

STOCK_HASH_BEFORE="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$ORIGIN_IMG")"

COMPILE_OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --big-oracle \
    "$ORIGIN_IMG" \
    /000payload.bin \
    "$REPLACEMENT_IMG" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'mode=big-oracle'
echo "$COMPILE_OUTPUT" | grep -q 'encoded_bytes=8192'
echo "$COMPILE_OUTPUT" | grep -q 'logical_lclusters=8'
echo "$COMPILE_OUTPUT" | grep -q 'shadow_blocks=2'
[[ "$(stat -c %s "$SHADOW")" -eq 8192 ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"

sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null

STOCK_HASH_AFTER="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]

sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"
SHADOW_LOOP=""

sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORIGINAL"
sudo umount "$MOUNT_DIR"

# A normal -C 4096 compact image must not be accepted as a big-pcluster oracle.
rm -f "$WORK/reject.shadow" "$WORK/reject.table" "$WORK/reject.err"
if "$LOOM" erofs-compact-pcluster-swap --big-oracle \
  "$ORIGIN_IMG" /000payload.bin "$NORMAL_IMG" \
  "$WORK/reject.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/reject.table" \
  >"$WORK/reject.out" 2>"$WORK/reject.err"; then
  echo 'Stage 19 expected normal-pcluster replacement rejection' >&2
  exit 1
fi
grep -Eq 'big-pcluster|two encoded physical blocks|incompatible' "$WORK/reject.err"
[[ ! -e "$WORK/reject.shadow" ]]
[[ ! -e "$WORK/reject.table" ]]

printf '%s\n' \
  'Stage 19 compact big-pcluster oracle PASS' \
  '  logical bytes: 32768' \
  '  logical lclusters: 8' \
  '  CBLKCNT physical blocks: 2' \
  '  physical shadow blocks: 2' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $STOCK_HASH_AFTER"
