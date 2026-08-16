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
MAPPER="loom-stage29-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then sudo umount "$MOUNT_DIR"; fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then sudo dmsetup remove "$MAPPER"; fi
  if [[ -n "$SHADOW_LOOP" ]]; then sudo losetup -d "$SHADOW_LOOP"; fi
  if [[ -n "$ORIGIN_LOOP" ]]; then sudo losetup -d "$ORIGIN_LOOP"; fi
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 29 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR

mkdir -p "$ROOT" "$MOUNT_DIR"

python3 - "$ORIGINAL" "$REPLACEMENT" <<'PY'
import random
import sys

SIZE = 98304
PERIOD = 512

def periodic(seed, marker):
    rng = random.Random(seed)
    period = bytes(rng.randrange(256) for _ in range(PERIOD))
    data = bytearray((period * ((SIZE + PERIOD - 1) // PERIOD))[:SIZE])
    data[64:64 + len(marker)] = marker
    return data

open(sys.argv[1], 'wb').write(periodic(0x290001, b'LOOM-STAGE29-ORIGIN'))
open(sys.argv[2], 'wb').write(periodic(0x290002, b'LOOM-STAGE29-REPLACEMENT'))
PY
cp "$ORIGINAL" "$ROOT/000payload.bin"
for i in $(seq -w 0 499); do : > "$ROOT/z_dummy_${i}_for_directory_growth"; done

mkfs.erofs -b 4096 -C 4096 -zlz4 -E legacy-compress,noinline_data -T 0 \
  --max-extent-bytes 32768 "$ORIGIN_IMG" "$ROOT" >/dev/null
fsck.erofs "$ORIGIN_IMG" >/dev/null

DUMP="$(dump.erofs -e --path=/000payload.bin "$ORIGIN_IMG")"
printf '%s\n' "$DUMP"
echo "$DUMP" | grep -q 'Layout: 1'
echo "$DUMP" | grep -q 'On-disk size: 12288'
echo "$DUMP" | grep -q '/000payload.bin: 3 extents found'

# Prove the exact full-index topology from raw metadata before Loom consumes it.
python3 - "$ORIGIN_IMG" <<'PY'
import stat
import struct
import sys

raw = open(sys.argv[1], 'rb').read()
sb = 1024
assert struct.unpack_from('<I', raw, sb)[0] == 0xE0F5E1E2
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
    if extended:
        if off + 64 > len(raw):
            continue
        size = struct.unpack_from('<Q', raw, off + 8)[0]
        isize = 64
    else:
        size = struct.unpack_from('<I', raw, off + 8)[0]
        isize = 32
    if size == 98304 and layout == 1:
        xcnt = struct.unpack_from('<H', raw, off + 2)[0]
        xsize = 0 if xcnt == 0 else 12 + (xcnt - 1) * 4
        target = (nid, off, isize, xsize, struct.unpack_from('<I', raw, off + 0x10)[0])
        break
assert target is not None
nid, off, isize, xsize, blocks = target
header = (off + isize + xsize + 7) & ~7
full_start = header + 16
assert raw[header:header + 16] == bytes(16)
heads = []
for lcn in range(24):
    p = full_start + lcn * 8
    advise, clusterofs, word = struct.unpack_from('<HHI', raw, p)
    kind = advise & 3
    assert advise & ~3 == 0
    assert clusterofs == 0
    if kind == 1:
        heads.append((lcn, word))
    else:
        assert kind == 2
        d0, d1 = struct.unpack_from('<HH', raw, p + 4)
        start = (lcn // 8) * 8
        end = min(start + 8, 24)
        assert (d0, d1) == (lcn - start, end - lcn)
assert heads == [(0, 1), (8, 2), (16, 3)], heads
assert blocks == 3
print(f'Stage 29 raw full-index topology PASS nid={nid} heads={heads} data_word={blocks} full_start={full_start}')
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
echo "$OUTPUT" | grep -q 'physical_pclusters=3'
echo "$OUTPUT" | grep -q 'logical_lclusters=24'
echo "$OUTPUT" | grep -q 'compact_2b_entries=0'
echo "$OUTPUT" | grep -q 'head_lclusters=\[0, 8, 16\]'
echo "$OUTPUT" | grep -q 'origin_pclusters=\[1, 2, 3\]'
echo "$OUTPUT" | grep -q 'shadow_blocks=3'
[[ "$(stat -c %s "$SHADOW")" -eq 12288 ]]

ENCODED="$(printf '%s\n' "$OUTPUT" | sed -n 's/.*encoded_bytes=\(\[[^]]*\]\).*/\1/p')"
python3 - "$ENCODED" <<'PY'
import ast
import sys
encoded = ast.literal_eval(sys.argv[1])
assert len(encoded) == 3
assert all(0 < n <= 4096 for n in encoded), encoded
print(f'Stage 29 Loom encoded lengths PASS encoded={encoded}')
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

# Corrupt only lcn1.delta1 (7 -> 6). Loom must reject the malformed full-index topology
# before CLI output files are created.
cp "$ORIGIN_IMG" "$CORRUPT_IMG"
python3 - "$CORRUPT_IMG" <<'PY'
import stat
import struct
import sys

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
    if size != 98304 or layout != 1:
        continue
    isize = 64 if extended else 32
    xcnt = struct.unpack_from('<H', raw, off + 2)[0]
    xsize = 0 if xcnt == 0 else 12 + (xcnt - 1) * 4
    full_start = ((off + isize + xsize + 7) & ~7) + 16
    entry = full_start + 8
    advise, clusterofs, word = struct.unpack_from('<HHI', raw, entry)
    assert advise == 2 and clusterofs == 0
    d0 = word & 0xffff
    d1 = word >> 16
    assert (d0, d1) == (1, 7)
    struct.pack_into('<I', raw, entry + 4, d0 | (6 << 16))
    open(path, 'wb').write(raw)
    break
else:
    raise AssertionError('full-index target inode not found')
PY
rm -f "$BAD_SHADOW" "$BAD_TABLE"
set +e
BAD_OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --multi-encode \
    "$CORRUPT_IMG" /000payload.bin "$REPLACEMENT" \
    "$BAD_SHADOW" "$ORIGIN_LOOP" LOOM_SHADOW_PLACEHOLDER "$BAD_TABLE" 2>&1
)"
BAD_STATUS=$?
set -e
printf '%s\n' "$BAD_OUTPUT"
[[ "$BAD_STATUS" -ne 0 ]]
echo "$BAD_OUTPUT" | grep -q 'forward/backward deltas disagree'
[[ ! -e "$BAD_SHADOW" ]]
[[ ! -e "$BAD_TABLE" ]]

printf '%s\n' \
  'Stage 29 legacy compressed-full index PASS' \
  '  inode layout: EROFS_INODE_COMPRESSED_FULL (1)' \
  '  logical lclusters: 24' \
  '  recovered HEAD lclusters: [0, 8, 16]' \
  '  recovered physical pclusters: [1, 2, 3]' \
  '  full-index NONHEAD delta0/delta1 validation: PASS' \
  '  shadow blocks: 3' \
  '  effective payload replacement: PASS' \
  '  effective fsck.erofs: PASS' \
  '  corrupted delta rejection before materialization: PASS' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $STOCK_HASH_AFTER"
