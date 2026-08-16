#!/usr/bin/env bash
set -euo pipefail

WORK="$(mktemp -d)"
ROOT="$WORK/root"
IMG="$WORK/mixed.erofs"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$ROOT"

python3 - "$ROOT/000payload.bin" <<'PY'
import random
import sys

BLOCK = 4096
CHUNK = 8 * BLOCK
rng = random.Random(0x310001)
first = (b'LOOM-STAGE31-COMPRESS-A-' * ((CHUNK // 24) + 1))[:CHUNK]
middle = bytes(rng.randrange(256) for _ in range(CHUNK))
last = (b'LOOM-STAGE31-COMPRESS-B-' * ((CHUNK // 24) + 1))[:CHUNK]
data = first + middle + last
assert len(data) == 98304
open(sys.argv[1], 'wb').write(data)
PY
for i in $(seq -w 0 499); do : > "$ROOT/z_dummy_${i}_for_directory_growth"; done

mkfs.erofs -b 4096 -C 4096 -zlz4 -E legacy-compress,noinline_data -T 0 \
  --max-extent-bytes 32768 "$IMG" "$ROOT" >/dev/null
fsck.erofs "$IMG" >/dev/null

dump.erofs -e --path=/000payload.bin "$IMG"

python3 - "$IMG" <<'PY'
import stat
import struct
import sys

SIZE = 98304
raw = open(sys.argv[1], 'rb').read()
sb = 1024
assert struct.unpack_from('<I', raw, sb)[0] == 0xE0F5E1E2
print('STAGE31_SB incompat=', struct.unpack_from('<I', raw, sb + 0x50)[0])
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
assert target is not None, 'layout=1 target not found'
nid, off, isize, xsize, blocks = target
header = (off + isize + xsize + 7) & ~7
full_start = header + 16
print(f'STAGE31_TARGET nid={nid} blocks={blocks} header={header} full_start={full_start}')
print('STAGE31_HEADER', raw[header:header+16].hex())
heads = []
plain = []
for lcn in range(24):
    p = full_start + lcn * 8
    advise, clusterofs, word = struct.unpack_from('<HHI', raw, p)
    d0, d1 = struct.unpack_from('<HH', raw, p + 4)
    kind = advise & 3
    print(f'STAGE31_IDX lcn={lcn} advise=0x{advise:04x} type={kind} clusterofs={clusterofs} word=0x{word:08x} delta0={d0} delta1={d1}')
    if kind in (0, 1):
        heads.append((lcn, kind, clusterofs, word))
    if kind == 0:
        plain.append((lcn, clusterofs, word))
print('STAGE31_HEADS', heads)
print('STAGE31_PLAIN', plain)
assert plain, 'probe did not produce a PLAIN data head'
PY
