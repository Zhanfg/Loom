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
CORRUPT_IMG="$WORK/corrupt.erofs"
ORIGINAL="$WORK/original.bin"
REPLACEMENT="$WORK/replacement.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"
BAD_SHADOW="$WORK/bad.shadow"
BAD_TABLE="$WORK/bad.table"
ORIGIN_LOOP=""
SHADOW_LOOP=""
MAPPER="loom-stage31-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then sudo umount "$MOUNT_DIR"; fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then sudo dmsetup remove "$MAPPER"; fi
  if [[ -n "$SHADOW_LOOP" ]]; then sudo losetup -d "$SHADOW_LOOP"; fi
  if [[ -n "$ORIGIN_LOOP" ]]; then sudo losetup -d "$ORIGIN_LOOP"; fi
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 31 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR

mkdir -p "$ROOT" "$MOUNT_DIR"

python3 - "$ORIGINAL" "$REPLACEMENT" <<'PY'
import random
import sys

BLOCK = 4096
CHUNK = 8 * BLOCK

def mixed(seed, tag):
    rng = random.Random(seed)
    first_pat = (tag + b'-COMPRESS-A-')
    last_pat = (tag + b'-COMPRESS-B-')
    first = (first_pat * ((CHUNK // len(first_pat)) + 1))[:CHUNK]
    middle = bytes(rng.randrange(256) for _ in range(CHUNK))
    last = (last_pat * ((CHUNK // len(last_pat)) + 1))[:CHUNK]
    data = first + middle + last
    assert len(data) == 98304
    return data

open(sys.argv[1], 'wb').write(mixed(0x310001, b'LOOM-STAGE31-ORIGIN'))
open(sys.argv[2], 'wb').write(mixed(0x310002, b'LOOM-STAGE31-REPLACEMENT'))
PY
cp "$ORIGINAL" "$ROOT/000payload.bin"
for i in $(seq -w 0 499); do : > "$ROOT/z_dummy_${i}_for_directory_growth"; done

mkfs.erofs -b 4096 -C 4096 -zlz4 -E legacy-compress,noinline_data -T 0 \
  --max-extent-bytes 32768 "$ORIGIN_IMG" "$ROOT" >/dev/null
fsck.erofs "$ORIGIN_IMG" >/dev/null

DUMP="$(dump.erofs -e --path=/000payload.bin "$ORIGIN_IMG")"
printf '%s\n' "$DUMP"
echo "$DUMP" | grep -q 'Size: 98304'
echo "$DUMP" | grep -q 'On-disk size: 40960'
echo "$DUMP" | grep -q 'Layout: 1'
echo "$DUMP" | grep -q '/000payload.bin: 10 extents found'

python3 - "$ORIGIN_IMG" <<'PY'
import stat
import struct
import sys

SIZE = 98304
raw = open(sys.argv[1], 'rb').read()
sb = 1024
assert struct.unpack_from('<I', raw, sb)[0] == 0xE0F5E1E2
assert struct.unpack_from('<I', raw, sb + 0x50)[0] == 0
meta = struct.unpack_from('<I', raw, sb + 0x28)[0]
inos = struct.unpack_from('<Q', raw, sb + 0x10)[0]
target = None
for nid in range(int(inos) + 16):
    off = meta * 4096 + nid * 32
    if off + 32 > len(raw):
        break
    fmt = struct.unpack_from('<H', raw, off)[0]
    if fmt & ~0x1f:
        continue
    extended = fmt & 1
    layout = (fmt >> 1) & 7
    mode = struct.unpack_from('<H', raw, off + 4)[0]
    if stat.S_IFMT(mode) != stat.S_IFREG:
        continue
    size = struct.unpack_from('<Q' if extended else '<I', raw, off + 8)[0]
    if size != SIZE or layout != 1:
        continue
    isize = 64 if extended else 32
    xcnt = struct.unpack_from('<H', raw, off + 2)[0]
    xsize = 0 if xcnt == 0 else 12 + (xcnt - 1) * 4
    blocks = struct.unpack_from('<I', raw, off + 0x10)[0]
    target = nid, off, isize, xsize, blocks
    break
assert target is not None
nid, off, isize, xsize, blocks = target
full_start = ((off + isize + xsize + 7) & ~7) + 16
heads = []
plain = []
for lcn in range(24):
    p = full_start + lcn * 8
    advise, clusterofs, word = struct.unpack_from('<HHI', raw, p)
    kind = advise & 3
    assert advise & ~3 == 0
    assert clusterofs == 0
    if kind in (0, 1):
        heads.append((lcn, kind, word))
    if kind == 0:
        plain.append((lcn, word))
    elif kind == 2:
        d0, d1 = struct.unpack_from('<HH', raw, p + 4)
        if lcn < 8:
            assert (d0, d1) == (lcn, 8 - lcn)
        elif lcn > 16:
            assert (d0, d1) == (lcn - 16, 24 - lcn)
expected_heads = [(0, 1, 1)] + [(lcn, 0, lcn - 6) for lcn in range(8, 16)] + [(16, 1, 10)]
assert heads == expected_heads, heads
assert plain == [(lcn, lcn - 6) for lcn in range(8, 16)], plain
assert blocks == 10
print(f'Stage 31 raw mixed topology PASS nid={nid} heads={heads} plain={plain} data_word={blocks}')
PY

STOCK_HASH_BEFORE="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$ORIGIN_IMG")"
sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORIGINAL"
sudo umount "$MOUNT_DIR"

OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --multi-encode \
    "$ORIGIN_IMG" /000payload.bin "$REPLACEMENT" \
    "$SHADOW" "$ORIGIN_LOOP" LOOM_SHADOW_PLACEHOLDER "$TABLE"
)"
printf '%s\n' "$OUTPUT"
echo "$OUTPUT" | grep -q 'mode=multi-encode'
echo "$OUTPUT" | grep -q 'physical_pclusters=10'
echo "$OUTPUT" | grep -q 'logical_lclusters=24'
echo "$OUTPUT" | grep -q 'compact_2b_entries=0'
echo "$OUTPUT" | grep -q 'head_lclusters=\[0, 8, 9, 10, 11, 12, 13, 14, 15, 16\]'
echo "$OUTPUT" | grep -q 'origin_pclusters=\[1, 2, 3, 4, 5, 6, 7, 8, 9, 10\]'
echo "$OUTPUT" | grep -q 'shadow_blocks=10'
[[ "$(stat -c %s "$SHADOW")" -eq 40960 ]]

ENCODED="$(printf '%s\n' "$OUTPUT" | sed -n 's/.*encoded_bytes=\(\[[^]]*\]\).*/\1/p')"
python3 - "$ENCODED" <<'PY'
import ast
import sys
encoded = ast.literal_eval(sys.argv[1])
assert len(encoded) == 10, encoded
assert 0 < encoded[0] < 4096, encoded
assert encoded[1:9] == [4096] * 8, encoded
assert 0 < encoded[9] < 4096, encoded
print(f'Stage 31 encoded mixed extents PASS encoded={encoded}')
PY

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null

STOCK_HASH_AFTER="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]

# A real PLAIN data head must remain block-aligned. Corrupt LCN 8 clusterofs
# from 0 to 1 and require failure before CLI output materialization.
cp "$ORIGIN_IMG" "$CORRUPT_IMG"
python3 - "$CORRUPT_IMG" <<'PY'
import stat
import struct
import sys

SIZE = 98304
path = sys.argv[1]
raw = bytearray(open(path, 'rb').read())
sb = 1024
meta = struct.unpack_from('<I', raw, sb + 0x28)[0]
inos = struct.unpack_from('<Q', raw, sb + 0x10)[0]
for nid in range(int(inos) + 16):
    off = meta * 4096 + nid * 32
    if off + 32 > len(raw):
        break
    fmt = struct.unpack_from('<H', raw, off)[0]
    if fmt & ~0x1f:
        continue
    extended = fmt & 1
    layout = (fmt >> 1) & 7
    mode = struct.unpack_from('<H', raw, off + 4)[0]
    if stat.S_IFMT(mode) != stat.S_IFREG:
        continue
    size = struct.unpack_from('<Q' if extended else '<I', raw, off + 8)[0]
    if size != SIZE or layout != 1:
        continue
    isize = 64 if extended else 32
    xcnt = struct.unpack_from('<H', raw, off + 2)[0]
    xsize = 0 if xcnt == 0 else 12 + (xcnt - 1) * 4
    full_start = ((off + isize + xsize + 7) & ~7) + 16
    entry = full_start + 8 * 8
    advise, clusterofs, word = struct.unpack_from('<HHI', raw, entry)
    assert (advise, clusterofs, word) == (0, 0, 2)
    struct.pack_into('<H', raw, entry + 2, 1)
    open(path, 'wb').write(raw)
    break
else:
    raise AssertionError('full-index target inode not found')
PY
rm -f "$BAD_SHADOW" "$BAD_TABLE"
if BAD_OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --multi-encode \
    "$CORRUPT_IMG" /000payload.bin "$REPLACEMENT" \
    "$BAD_SHADOW" "$ORIGIN_LOOP" LOOM_SHADOW_PLACEHOLDER "$BAD_TABLE" 2>&1
)"; then
  BAD_STATUS=0
else
  BAD_STATUS=$?
fi
printf '%s\n' "$BAD_OUTPUT"
[[ "$BAD_STATUS" -ne 0 ]]
echo "$BAD_OUTPUT" | grep -q 'full-index PLAIN data heads require zero cluster offsets'
[[ ! -e "$BAD_SHADOW" ]]
[[ ! -e "$BAD_TABLE" ]]

printf '%s\n' \
  'Stage 31 legacy full-index aligned PLAIN data PASS' \
  '  logical bytes: 98304' \
  '  inode layout: EROFS_INODE_COMPRESSED_FULL (1)' \
  '  physical extents / data_word: 10' \
  '  HEAD1 lclusters: [0, 16]' \
  '  PLAIN data lclusters: [8, 9, 10, 11, 12, 13, 14, 15]' \
  '  physical pclusters: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]' \
  '  PLAIN raw encoded bytes: 8 x 4096' \
  '  shadow blocks: 10' \
  '  effective mixed compressed/raw replacement: PASS' \
  '  effective fsck.erofs: PASS' \
  '  malformed PLAIN cluster offset rejection before materialization: PASS' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $STOCK_HASH_AFTER"
