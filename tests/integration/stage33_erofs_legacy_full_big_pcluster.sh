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
MAPPER="loom-stage33-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if mountpoint -q "$MNT" 2>/dev/null; then sudo umount "$MNT"; fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then sudo dmsetup remove "$MAPPER"; fi
  if [[ -n "$SHADOW_LOOP" ]]; then sudo losetup -d "$SHADOW_LOOP"; fi
  if [[ -n "$ORIGIN_LOOP" ]]; then sudo losetup -d "$ORIGIN_LOOP"; fi
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 33 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR

mkdir -p "$ROOT" "$MNT"

python3 - "$ORIGINAL" "$REPLACEMENT" <<'PY'
import random
import sys

EXTENT = 32768
PERIOD = 10000

def periodic(seed, marker):
    rng = random.Random(seed)
    period = bytes(rng.randrange(256) for _ in range(PERIOD))
    part = bytearray((period * 4)[:EXTENT])
    part[64:64 + len(marker)] = marker
    assert len(part) == EXTENT
    return part

origin = bytearray()
replacement = bytearray()
for extent in range(3):
    origin.extend(periodic(0x330100 + extent, f'LOOM-STAGE33-ORIGIN-{extent}'.encode()))
    replacement.extend(periodic(0x330200 + extent, f'LOOM-STAGE33-REPLACEMENT-{extent}'.encode()))
assert len(origin) == 98304 and len(replacement) == 98304
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
echo "$DUMP" | grep -q 'On-disk size: 36864'
echo "$DUMP" | grep -q 'Layout: 1'
echo "$DUMP" | grep -q '/000payload.bin: 3 extents found'

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
heads = []
for lcn in range(24):
    p = full_start + lcn * 8
    advise, clusterofs, word = struct.unpack_from('<HHI', raw, p)
    kind = advise & 3
    assert advise & ~3 == 0
    assert clusterofs == 0
    group = (lcn // 8) * 8
    end = group + 8
    if lcn in (0, 8, 16):
        assert kind == 1
        heads.append((lcn, word))
    elif lcn in (1, 9, 17):
        assert kind == 2
        d0, d1 = struct.unpack_from('<HH', raw, p + 4)
        assert d0 == 0x0803, (lcn, hex(d0))
        assert d1 == end - lcn == 7, (lcn, d1)
    else:
        assert kind == 2
        d0, d1 = struct.unpack_from('<HH', raw, p + 4)
        assert d0 == lcn - group, (lcn, d0)
        assert d1 == end - lcn, (lcn, d1)
assert heads == [(0, 1), (8, 4), (16, 7)], heads
assert blocks == 9, blocks
print(f'Stage 33 raw full-big topology PASS nid={nid} heads={heads} CBLKCNT=[3,3,3] data_word={blocks} incompat=0x{incompat:x}')
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
echo "$OUTPUT" | grep -q 'physical_pclusters=3'
echo "$OUTPUT" | grep -q 'logical_lclusters=24'
echo "$OUTPUT" | grep -q 'head_lclusters=\[0, 8, 16\]'
echo "$OUTPUT" | grep -q 'origin_pclusters=\[1, 4, 7\]'
echo "$OUTPUT" | grep -q 'shadow_blocks=9'
[[ "$(stat -c %s "$SHADOW")" -eq 36864 ]]

ENCODED_LIST="$(echo "$OUTPUT" | sed -n 's/.*encoded_bytes=\[\([^]]*\)\].*/\1/p')"
[[ -n "$ENCODED_LIST" ]]
IFS=',' read -r E0 E1 E2 <<< "$ENCODED_LIST"
E0="${E0// /}"; E1="${E1// /}"; E2="${E2// /}"
for encoded in "$E0" "$E1" "$E2"; do
  [[ "$encoded" -gt 8192 ]]
  [[ "$encoded" -le 12288 ]]
done

# Legacy full-big raw LZ4 starts at offset 0 and the rest of each 12-KiB
# physical span is trailing zero padding. This is the inverse of compact 0padding.
python3 - "$SHADOW" "$E0" "$E1" "$E2" <<'PY'
import sys
raw = open(sys.argv[1], 'rb').read()
encoded = [int(v) for v in sys.argv[2:]]
assert len(raw) == 3 * 12288
for i, n in enumerate(encoded):
    span = raw[i * 12288:(i + 1) * 12288]
    assert span[0] != 0, (i, n)
    assert n > 8192, (i, n)
    assert span[n:] == b'\x00' * (12288 - n), (i, n)
    assert any(span[4096:8192]), (i, n)
    assert any(span[8192:n]), (i, n)
print(f'Stage 33 LegacyStart span placement PASS encoded={encoded}')
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

# Corrupt only the first D0_CBLKCNT entry's forward distance (7 -> 6).
# The full-big parser must reject the topology before CLI artifacts exist.
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
    p = full_start + 8
    advise, clusterofs, word = struct.unpack_from('<HHI', raw, p)
    d0, d1 = struct.unpack_from('<HH', raw, p + 4)
    assert (advise, clusterofs, d0, d1) == (2, 0, 0x0803, 7)
    struct.pack_into('<H', raw, p + 6, 6)
    open(path, 'wb').write(raw)
    break
else:
    raise AssertionError('full-big target inode not found')
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
echo "$BAD_OUTPUT" | grep -q 'full big CBLKCNT entry delta1 disagrees with next HEAD'
[[ ! -e "$BAD_SHADOW" ]]
[[ ! -e "$BAD_TABLE" ]]

HASH_AFTER="$(sha256sum "$IMG" | awk '{print $1}')"
[[ "$HASH_BEFORE" == "$HASH_AFTER" ]]
sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MNT"
sudo cmp "$MNT/000payload.bin" "$ORIGINAL"
sudo umount "$MNT"

printf '%s\n' \
  'Stage 33 legacy full-index big-pcluster PASS' \
  '  logical bytes: 98304' \
  '  superblock incompat: 0x2 (BIG_PCLUSTER, no 0padding)' \
  '  map advice: 0x2 (BIG_PCLUSTER_1)' \
  '  HEAD lclusters: [0, 8, 16]' \
  '  physical pcluster starts: [1, 4, 7]' \
  '  recovered CBLKCNT blocks: [3, 3, 3]' \
  "  Loom raw-LZ4 bytes: [$E0, $E1, $E2]" \
  '  LegacyStart multi-block placement: PASS' \
  '  physical shadow blocks: 9' \
  '  effective replacement: PASS' \
  '  effective fsck.erofs: PASS' \
  '  malformed D0_CBLKCNT delta1 rejection before materialization: PASS' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $HASH_AFTER"
