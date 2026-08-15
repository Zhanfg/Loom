#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
ROOT="$WORK/root"
MOUNT_DIR="$WORK/mnt"
ORIGIN_IMG="$WORK/origin.erofs"
ORIGINAL="$WORK/original.bin"
REPLACEMENT="$WORK/replacement.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"
ORIGIN_LOOP=""
SHADOW_LOOP=""
MAPPER="loom-stage28-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then sudo umount "$MOUNT_DIR"; fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then sudo dmsetup remove "$MAPPER"; fi
  if [[ -n "$SHADOW_LOOP" ]]; then sudo losetup -d "$SHADOW_LOOP"; fi
  if [[ -n "$ORIGIN_LOOP" ]]; then sudo losetup -d "$ORIGIN_LOOP"; fi
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 28 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR

mkdir -p "$ROOT" "$MOUNT_DIR"

python3 - "$ORIGINAL" "$REPLACEMENT" <<'PY'
import random
import sys

SIZE = 32768
PERIOD = 10000

def periodic(seed, marker):
    rng = random.Random(seed)
    period = bytes(rng.randrange(256) for _ in range(PERIOD))
    copies = (SIZE + PERIOD - 1) // PERIOD
    data = bytearray((period * copies)[:SIZE])
    data[64:64 + len(marker)] = marker
    return data

open(sys.argv[1], 'wb').write(periodic(0x280001, b'LOOM-STAGE28-ORIGIN'))
open(sys.argv[2], 'wb').write(periodic(0x280002, b'LOOM-STAGE28-REPLACEMENT'))
PY
cp "$ORIGINAL" "$ROOT/000payload.bin"

# Force nonzero directory xattr ibody so Stage 28 must combine the Stage 26 xattr offset
# handling with flat-inline directory tail addressing.
python3 - "$ROOT" <<'PY'
import os
import sys
root = sys.argv[1]
os.setxattr(root, b'user.loom.stage28.root', b'inline-root-xattr-preserved')
assert os.getxattr(root, b'user.loom.stage28.root') == b'inline-root-xattr-preserved'
PY

# Keep the root directory tiny so its entire directory stream is tail-packed inline. The
# 32 KiB target remains compressed and uses the already-proven single big-pcluster path.
mkfs.erofs -b 4096 -C 16384 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 32768 "$ORIGIN_IMG" "$ROOT" >/dev/null
fsck.erofs "$ORIGIN_IMG" >/dev/null

# Prove from raw EROFS metadata that the root inode is actually DATA_FLAT_INLINE (layout=2),
# has a nonzero xattr ibody, and its complete directory stream is an inline tail (<4 KiB).
python3 - "$ORIGIN_IMG" <<'PY'
import struct
import sys

raw = open(sys.argv[1], 'rb').read()
sb = 1024
assert struct.unpack_from('<I', raw, sb)[0] == 0xE0F5E1E2
root_nid = struct.unpack_from('<H', raw, sb + 0x0e)[0]
meta_block = struct.unpack_from('<I', raw, sb + 0x28)[0]
iloc = meta_block * 4096 + root_nid * 32
fmt = struct.unpack_from('<H', raw, iloc)[0]
layout = (fmt >> 1) & 7
extended = fmt & 1
xattr_count = struct.unpack_from('<H', raw, iloc + 2)[0]
size = struct.unpack_from('<Q' if extended else '<I', raw, iloc + 8)[0]
isize = 64 if extended else 32
xattr_isize = 0 if xattr_count == 0 else 12 + (xattr_count - 1) * 4
tail_offset = iloc + isize + xattr_isize
assert layout == 2, (layout, fmt)
assert xattr_count > 0
assert 0 < size < 4096, size
assert (tail_offset % 4096) + size <= 4096
print(f'Stage 28 raw root layout PASS layout={layout} size={size} xattr_isize={xattr_isize} tail_offset={tail_offset}')
PY

STOCK_HASH_BEFORE="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$ORIGIN_IMG")"

sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORIGINAL"
sudo python3 - "$MOUNT_DIR" <<'PY'
import os
import sys
assert os.getxattr(sys.argv[1], b'user.loom.stage28.root') == b'inline-root-xattr-preserved'
PY
sudo umount "$MOUNT_DIR"

OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --big-encode \
    "$ORIGIN_IMG" /000payload.bin "$REPLACEMENT" \
    "$SHADOW" "$ORIGIN_LOOP" LOOM_SHADOW_PLACEHOLDER "$TABLE"
)"
printf '%s\n' "$OUTPUT"
echo "$OUTPUT" | grep -q 'mode=big-encode'
echo "$OUTPUT" | grep -q 'logical_lclusters=8'
echo "$OUTPUT" | grep -q 'shadow_blocks=3'
[[ "$(stat -c %s "$SHADOW")" -eq 12288 ]]
ENCODED_BYTES="$(printf '%s\n' "$OUTPUT" | sed -n 's/.*encoded_bytes=\([0-9][0-9]*\).*/\1/p')"
[[ -n "$ENCODED_BYTES" ]]
[[ "$ENCODED_BYTES" -gt 8192 && "$ENCODED_BYTES" -le 12288 ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo python3 - "$MOUNT_DIR" <<'PY'
import os
import sys
assert os.getxattr(sys.argv[1], b'user.loom.stage28.root') == b'inline-root-xattr-preserved'
PY
sudo umount "$MOUNT_DIR"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null

STOCK_HASH_AFTER="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]

printf '%s\n' \
  'Stage 28 flat-inline directory tail PASS' \
  '  traversed directory layout: DATA_FLAT_INLINE' \
  '  traversed directory full data blocks: 0' \
  '  directory xattr ibody: nonzero and preserved' \
  '  target path: /000payload.bin' \
  '  target CBLKCNT physical blocks: 3' \
  "  Loom raw-LZ4 bytes: $ENCODED_BYTES" \
  '  effective payload replacement: PASS' \
  '  effective root xattr preservation: PASS' \
  '  effective fsck.erofs: PASS' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $STOCK_HASH_AFTER"
