#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
ROOT="$WORK/root"
MNT="$WORK/mnt"
IMG="$WORK/origin.erofs"
BAD_IMG="$WORK/bad.erofs"
ORIGINAL="$WORK/original.bin"
REPLACEMENT="$WORK/replacement.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"
BAD_SHADOW="$WORK/bad.shadow"
BAD_TABLE="$WORK/bad.table"
ORIGIN_LOOP=""
SHADOW_LOOP=""
MAPPER="loom-stage35-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if mountpoint -q "$MNT" 2>/dev/null; then sudo umount "$MNT"; fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then sudo dmsetup remove "$MAPPER"; fi
  if [[ -n "$SHADOW_LOOP" ]]; then sudo losetup -d "$SHADOW_LOOP"; fi
  if [[ -n "$ORIGIN_LOOP" ]]; then sudo losetup -d "$ORIGIN_LOOP"; fi
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 35 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR

mkdir -p "$ROOT" "$MNT"

python3 - "$ORIGINAL" "$REPLACEMENT" <<'PY'
import random
import sys

CHUNK = 32768
rng = random.Random(0x350001)
middle = bytearray(rng.randrange(256) for _ in range(CHUNK))

origin_first = bytearray((b'LOOM-STAGE35-COMPRESS-A-' * ((CHUNK // 24) + 2))[:CHUNK])
origin_last = bytearray((b'LOOM-STAGE35-COMPRESS-B-' * ((CHUNK // 24) + 2))[:CHUNK])
replacement_first = bytearray(origin_first)
replacement_middle = bytearray(middle)
replacement_last = bytearray(origin_last)

# Preserve the calibrated compressibility regime while making every kind of extent
# materially different. The random tail feeding the 4-block LZ4 extent remains mostly
# unchanged so its encoded stream stays safely inside the existing 16-KiB footprint.
replacement_first[64:88] = b'LOOM-STAGE35-REPL-A-0000'
for lcn in range(8, 13):
    off = (lcn - 8) * 4096 + 128
    replacement_middle[off:off + 24] = bytes([0xA0 + lcn]) * 24
replacement_middle[5 * 4096 + 256:5 * 4096 + 280] = b'STAGE35-LZ4-RANDOM-TAIL!'
replacement_last[64:88] = b'LOOM-STAGE35-REPL-B-0000'

origin = origin_first + middle + origin_last
replacement = replacement_first + replacement_middle + replacement_last
assert len(origin) == 98304 and len(replacement) == 98304
assert origin != replacement
open(sys.argv[1], 'wb').write(origin)
open(sys.argv[2], 'wb').write(replacement)
PY

cp "$ORIGINAL" "$ROOT/000payload.bin"
for i in $(seq -w 0 499); do : > "$ROOT/z_dummy_${i}_for_directory_growth"; done

mkfs.erofs -b 4096 -C 16384 -zlz4 -E legacy-compress,noinline_data -T 0 \
  --max-extent-bytes 32768 "$IMG" "$ROOT" >/dev/null
fsck.erofs "$IMG" >/dev/null

DUMP="$(dump.erofs -e --path=/000payload.bin "$IMG")"
printf '%s\n' "$DUMP"
echo "$DUMP" | grep -q 'Size: 98304'
echo "$DUMP" | grep -q 'On-disk size: 45056'
echo "$DUMP" | grep -q 'Layout: 1'
echo "$DUMP" | grep -q '/000payload.bin: 8 extents found'

python3 - "$IMG" <<'PY'
import stat
import struct
import sys

SIZE = 98304
raw = open(sys.argv[1], 'rb').read()
sb = 1024
assert struct.unpack_from('<I', raw, sb)[0] == 0xE0F5E1E2
incompat = struct.unpack_from('<I', raw, sb + 0x50)[0]
assert incompat == 0x2, hex(incompat)
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
header = (off + isize + xsize + 7) & ~7
raw_header = raw[header:header + 16]
assert struct.unpack_from('<H', raw_header, 4)[0] == 0x2
assert raw_header[6] == 0 and raw_header[7] == 0
assert raw_header[8:16] == bytes(8)
full_start = header + 16

expected_starts = {
    0:  (1, 2, 1),
    8:  (0, 3, 1),
    9:  (0, 4, 1),
    10: (0, 5, 1),
    11: (0, 6, 1),
    12: (0, 7, 1),
    13: (1, 8, 4),
    21: (1, 12, 1),
}
for lcn in range(24):
    p = full_start + lcn * 8
    advise, clusterofs, word = struct.unpack_from('<HHI', raw, p)
    kind = advise & 3
    assert advise & ~3 == 0
    assert clusterofs == 0
    if lcn in expected_starts:
        expected_kind, expected_pblk, _ = expected_starts[lcn]
        assert kind == expected_kind, (lcn, kind)
        assert word == expected_pblk, (lcn, word)
        continue
    assert kind == 2, (lcn, kind)

for head, next_head, cblkcnt in [(0, 8, 1), (13, 21, 4), (21, 24, 1)]:
    first = head + 1
    p = full_start + first * 8
    d0, d1 = struct.unpack_from('<HH', raw, p + 4)
    assert d0 == 0x0800 | cblkcnt, (head, hex(d0))
    assert d1 == next_head - first, (head, d1)
    for lcn in range(first + 1, next_head):
        p = full_start + lcn * 8
        d0, d1 = struct.unpack_from('<HH', raw, p + 4)
        assert d0 == lcn - head, (lcn, d0)
        assert d1 == next_head - lcn, (lcn, d1)

assert blocks == 11, blocks
print('Stage 35 raw mixed full-big topology PASS '
      f'nid={nid} starts={list(expected_starts)} blocks=[1,1,1,1,1,1,4,1] data_word={blocks}')
PY

HASH_BEFORE="$(sha256sum "$IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$IMG")"
sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MNT"
sudo cmp "$MNT/000payload.bin" "$ORIGINAL"
sudo umount "$MNT"

OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --multi-encode \
    "$IMG" /000payload.bin "$REPLACEMENT" \
    "$SHADOW" "$ORIGIN_LOOP" LOOM_SHADOW_PLACEHOLDER "$TABLE"
)"
printf '%s\n' "$OUTPUT"
echo "$OUTPUT" | grep -q 'mode=multi-encode'
echo "$OUTPUT" | grep -q 'physical_pclusters=8'
echo "$OUTPUT" | grep -q 'logical_lclusters=24'
echo "$OUTPUT" | grep -q 'head_lclusters=\[0, 8, 9, 10, 11, 12, 13, 21\]'
echo "$OUTPUT" | grep -q 'origin_pclusters=\[2, 3, 4, 5, 6, 7, 8, 12\]'
echo "$OUTPUT" | grep -q 'shadow_blocks=11'
[[ "$(stat -c %s "$SHADOW")" -eq 45056 ]]

ENCODED_LIST="$(echo "$OUTPUT" | sed -n 's/.*encoded_bytes=\[\([^]]*\)\].*/\1/p')"
[[ -n "$ENCODED_LIST" ]]
IFS=',' read -r E0 E8 E9 E10 E11 E12 E13 E21 <<< "$ENCODED_LIST"
for name in E0 E8 E9 E10 E11 E12 E13 E21; do
  value="${!name// /}"
  printf -v "$name" '%s' "$value"
done
for plain in "$E8" "$E9" "$E10" "$E11" "$E12"; do [[ "$plain" -eq 4096 ]]; done
[[ "$E0" -gt 0 && "$E0" -le 4096 ]]
[[ "$E13" -gt 0 && "$E13" -le 16384 ]]
[[ "$E21" -gt 0 && "$E21" -le 4096 ]]

python3 - "$SHADOW" "$REPLACEMENT" "$E0" "$E13" "$E21" <<'PY'
import sys
shadow = open(sys.argv[1], 'rb').read()
replacement = open(sys.argv[2], 'rb').read()
e0, e13, e21 = map(int, sys.argv[3:])
assert len(shadow) == 11 * 4096

# Extent ordering in the shadow is pblk order: LZ4@0, five PLAIN blocks,
# four-block LZ4@13, one-block LZ4@21.
for i, lcn in enumerate(range(8, 13), start=1):
    got = shadow[i * 4096:(i + 1) * 4096]
    want = replacement[lcn * 4096:(lcn + 1) * 4096]
    assert got == want, lcn

def check_legacy_start(offset, capacity, encoded):
    span = shadow[offset:offset + capacity]
    assert len(span) == capacity
    assert encoded > 0 and encoded <= capacity
    assert span[0] != 0
    assert span[encoded:] == b'\x00' * (capacity - encoded)

check_legacy_start(0, 4096, e0)
check_legacy_start(6 * 4096, 4 * 4096, e13)
check_legacy_start(10 * 4096, 4096, e21)
print(f'Stage 35 mixed shadow semantics PASS encoded=[{e0},4096,4096,4096,4096,4096,{e13},{e21}]')
PY

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MNT"
sudo cmp "$MNT/000payload.bin" "$REPLACEMENT"
sudo umount "$MNT"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null
sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"
SHADOW_LOOP=""

HASH_MID="$(sha256sum "$IMG" | awk '{print $1}')"
[[ "$HASH_BEFORE" == "$HASH_MID" ]]

# Corrupt the first PLAIN data extent (LCN8) by giving it a non-zero cluster offset.
# The mixed full-big parser must reject this before any materialization artifact exists.
cp "$IMG" "$BAD_IMG"
python3 - "$BAD_IMG" <<'PY'
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
    if off + 32 > len(raw): break
    fmt = struct.unpack_from('<H', raw, off)[0]
    if fmt & ~0x1f: continue
    extended = fmt & 1
    layout = (fmt >> 1) & 7
    mode = struct.unpack_from('<H', raw, off + 4)[0]
    if stat.S_IFMT(mode) != stat.S_IFREG: continue
    size = struct.unpack_from('<Q' if extended else '<I', raw, off + 8)[0]
    if size != SIZE or layout != 1: continue
    isize = 64 if extended else 32
    xcnt = struct.unpack_from('<H', raw, off + 2)[0]
    xsize = 0 if xcnt == 0 else 12 + (xcnt - 1) * 4
    full_start = ((off + isize + xsize + 7) & ~7) + 16
    p = full_start + 8 * 8
    advise, clusterofs, word = struct.unpack_from('<HHI', raw, p)
    assert (advise, clusterofs, word) == (0, 0, 3)
    struct.pack_into('<H', raw, p + 2, 1)
    open(path, 'wb').write(raw)
    break
else:
    raise AssertionError('mixed full-big target inode not found')
PY
rm -f "$BAD_SHADOW" "$BAD_TABLE"
if BAD_OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --multi-encode \
    "$BAD_IMG" /000payload.bin "$REPLACEMENT" \
    "$BAD_SHADOW" "$ORIGIN_LOOP" UNUSED "$BAD_TABLE" 2>&1
)"; then
  BAD_STATUS=0
else
  BAD_STATUS=$?
fi
printf '%s\n' "$BAD_OUTPUT"
[[ "$BAD_STATUS" -ne 0 ]]
echo "$BAD_OUTPUT" | grep -q 'full big-pcluster data entries require zero cluster offsets'
[[ ! -e "$BAD_SHADOW" ]]
[[ ! -e "$BAD_TABLE" ]]

HASH_AFTER="$(sha256sum "$IMG" | awk '{print $1}')"
[[ "$HASH_BEFORE" == "$HASH_AFTER" ]]
sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MNT"
sudo cmp "$MNT/000payload.bin" "$ORIGINAL"
sudo umount "$MNT"

printf '%s\n' \
  'Stage 35 legacy full-index big-pcluster mixed PLAIN data PASS' \
  '  logical bytes: 98304' \
  '  extent lclusters: [0, 8, 9, 10, 11, 12, 13, 21]' \
  '  extent kinds: [LZ4, PLAIN, PLAIN, PLAIN, PLAIN, PLAIN, LZ4, LZ4]' \
  '  physical footprints: [1, 1, 1, 1, 1, 1, 4, 1]' \
  '  physical pcluster starts: [2, 3, 4, 5, 6, 7, 8, 12]' \
  '  PLAIN raw-copy blocks: 5 x 4096' \
  "  LZ4 encoded bytes: [$E0, $E13, $E21]" \
  '  LegacyStart compressed-span placement: PASS' \
  '  shadow blocks: 11' \
  '  effective replacement: PASS' \
  '  effective fsck.erofs: PASS' \
  '  malformed PLAIN cluster offset rejection before materialization: PASS' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $HASH_AFTER"
