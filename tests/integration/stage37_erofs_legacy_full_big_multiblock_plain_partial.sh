#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"
cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
ROOT="$WORK/root"; MNT="$WORK/mnt"
IMG="$WORK/origin.erofs"; BAD_IMG="$WORK/bad.erofs"
ORIGINAL="$WORK/original.bin"; REPLACEMENT="$WORK/replacement.bin"
SHADOW="$WORK/shadow.pack"; TABLE="$WORK/loom.table"
BAD_SHADOW="$WORK/bad.shadow"; BAD_TABLE="$WORK/bad.table"
ORIGIN_LOOP=""; SHADOW_LOOP=""; MAPPER="loom-stage37-${RANDOM}-${RANDOM}"
cleanup() {
  set +e
  mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT"
  sudo dmsetup info "$MAPPER" >/dev/null 2>&1 && sudo dmsetup remove "$MAPPER"
  [[ -n "$SHADOW_LOOP" ]] && sudo losetup -d "$SHADOW_LOOP"
  [[ -n "$ORIGIN_LOOP" ]] && sudo losetup -d "$ORIGIN_LOOP"
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 37 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR
mkdir -p "$ROOT" "$MNT"

python3 - "$ORIGINAL" "$REPLACEMENT" <<'PY'
import random, sys
SIZE=98304-123; EXTENT=32768; PERIOD=10000

def periodic(seed, marker):
    rng=random.Random(seed)
    period=bytes(rng.randrange(256) for _ in range(PERIOD))
    part=bytearray((period*4)[:EXTENT])
    part[64:64+len(marker)]=marker
    return part

def build(base, marker):
    out=bytearray()
    out.extend(periodic(base+1, marker+b'-0'))
    out.extend(periodic(base+2, marker+b'-1'))
    rng=random.Random(base+3)
    out.extend(bytes(rng.randrange(256) for _ in range(SIZE-len(out))))
    assert len(out)==SIZE
    return out
open(sys.argv[1],'wb').write(build(0x370100,b'LOOM-STAGE37-ORIGIN'))
open(sys.argv[2],'wb').write(build(0x370200,b'LOOM-STAGE37-REPLACEMENT'))
PY
cp "$ORIGINAL" "$ROOT/000payload.bin"
for i in $(seq -w 0 499); do : > "$ROOT/z_dummy_${i}_for_directory_growth"; done
mkfs.erofs -b 4096 -C 16384 -zlz4 -E legacy-compress,noinline_data -T 0 \
  --max-extent-bytes 32768 "$IMG" "$ROOT" >/dev/null
fsck.erofs "$IMG" >/dev/null
DUMP="$(dump.erofs -e --path=/000payload.bin "$IMG")"
printf '%s\n' "$DUMP"
echo "$DUMP" | grep -q 'Size: 98181'
echo "$DUMP" | grep -q 'On-disk size: 57344'
echo "$DUMP" | grep -q 'Layout: 1'
echo "$DUMP" | grep -q '/000payload.bin: 10 extents found'

python3 - "$IMG" <<'PY'
import stat,struct,sys
SIZE=98304-123
raw=open(sys.argv[1],'rb').read(); sb=1024
assert struct.unpack_from('<I',raw,sb)[0]==0xE0F5E1E2
assert struct.unpack_from('<I',raw,sb+0x50)[0]==0x2
meta=struct.unpack_from('<I',raw,sb+0x28)[0]; inos=struct.unpack_from('<Q',raw,sb+0x10)[0]
for nid in range(int(inos)+16):
    off=meta*4096+nid*32
    if off+32>len(raw): break
    fmt=struct.unpack_from('<H',raw,off)[0]
    if fmt & ~0x1f: continue
    ext=fmt&1; layout=(fmt>>1)&7; mode=struct.unpack_from('<H',raw,off+4)[0]
    if stat.S_IFMT(mode)!=stat.S_IFREG: continue
    size=struct.unpack_from('<Q' if ext else '<I',raw,off+8)[0]
    if size!=SIZE or layout!=1: continue
    isize=64 if ext else 32; xcnt=struct.unpack_from('<H',raw,off+2)[0]
    xsize=0 if xcnt==0 else 12+(xcnt-1)*4; blocks=struct.unpack_from('<I',raw,off+0x10)[0]
    header=(off+isize+xsize+7)&~7; h=raw[header:header+16]; start=header+16
    assert struct.unpack_from('<H',h,4)[0]==0x2 and h[6:8]==b'\0\0' and h[8:16]==bytes(8)
    break
else: raise AssertionError('target not found')
expected={0:(1,1,3),8:(1,4,3)}
for lcn in range(16,24): expected[lcn]=(0,lcn-9,1)
starts=[]
for lcn in range(24):
    advise,co,word=struct.unpack_from('<HHI',raw,start+lcn*8); kind=advise&3
    assert advise & ~3 == 0 and co==0
    if lcn in expected:
        ek,ep,_=expected[lcn]; assert (kind,word)==(ek,ep),(lcn,kind,word); starts.append(lcn)
    else:
        assert kind==2,(lcn,kind)
for head,next_head,cblk in [(0,8,3),(8,16,3)]:
    d0,d1=struct.unpack_from('<HH',raw,start+(head+1)*8+4)
    assert d0==(0x0800|cblk) and d1==next_head-head-1,(head,hex(d0),d1)
    for lcn in range(head+2,next_head):
        d0,d1=struct.unpack_from('<HH',raw,start+lcn*8+4)
        assert (d0,d1)==(lcn-head,next_head-lcn),(lcn,d0,d1)
assert blocks==14,blocks
assert starts==[0,8,16,17,18,19,20,21,22,23],starts
print(f'Stage 37 raw composition topology PASS starts={starts} pclusters={[expected[x][1] for x in starts]} footprints={[expected[x][2] for x in starts]} data_word={blocks}')
PY

HASH_BEFORE="$(sha256sum "$IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$IMG")"
sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MNT"
sudo cmp "$MNT/000payload.bin" "$ORIGINAL"
sudo umount "$MNT"

OUTPUT="$("$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$IMG" /000payload.bin "$REPLACEMENT" "$SHADOW" "$ORIGIN_LOOP" \
  LOOM_SHADOW_PLACEHOLDER "$TABLE")"
printf '%s\n' "$OUTPUT"
echo "$OUTPUT" | grep -q 'physical_pclusters=10'
echo "$OUTPUT" | grep -q 'logical_lclusters=24'
echo "$OUTPUT" | grep -q 'head_lclusters=\[0, 8, 16, 17, 18, 19, 20, 21, 22, 23\]'
echo "$OUTPUT" | grep -q 'origin_pclusters=\[1, 4, 7, 8, 9, 10, 11, 12, 13, 14\]'
SHADOW_BLOCKS="$(printf '%s\n' "$OUTPUT" | sed -n 's/.*shadow_blocks=\([0-9][0-9]*\).*/\1/p')"
[[ -n "$SHADOW_BLOCKS" && "$SHADOW_BLOCKS" -le 14 && "$SHADOW_BLOCKS" -ge 10 ]]
[[ "$(stat -c %s "$SHADOW")" -eq $((SHADOW_BLOCKS*4096)) ]]
ENCODED="$(printf '%s\n' "$OUTPUT" | sed -n 's/.*encoded_bytes=\(\[[^]]*\]\).*/\1/p')"
python3 - "$ENCODED" <<'PY'
import ast,sys
v=ast.literal_eval(sys.argv[1]); assert len(v)==10,v
assert 8192 < v[0] <= 12288 and 8192 < v[1] <= 12288,v
assert v[2:9]==[4096]*7,v
assert v[9]==3973,v
print(f'Stage 37 encoded composition PASS encoded={v}')
PY

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MNT"
sudo cmp "$MNT/000payload.bin" "$REPLACEMENT"
sudo umount "$MNT"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null
sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"; SHADOW_LOOP=""
HASH_AFTER="$(sha256sum "$IMG" | awk '{print $1}')"; [[ "$HASH_BEFORE" == "$HASH_AFTER" ]]

# Corrupt the first D0_CBLKCNT entry's delta1 while preserving its physical-block count.
cp "$IMG" "$BAD_IMG"
python3 - "$BAD_IMG" <<'PY'
import stat,struct,sys
SIZE=98304-123; path=sys.argv[1]; raw=bytearray(open(path,'rb').read()); sb=1024
meta=struct.unpack_from('<I',raw,sb+0x28)[0]; inos=struct.unpack_from('<Q',raw,sb+0x10)[0]
for nid in range(int(inos)+16):
    off=meta*4096+nid*32
    if off+32>len(raw): break
    fmt=struct.unpack_from('<H',raw,off)[0]
    if fmt & ~0x1f: continue
    ext=fmt&1; layout=(fmt>>1)&7; mode=struct.unpack_from('<H',raw,off+4)[0]
    if stat.S_IFMT(mode)!=stat.S_IFREG: continue
    size=struct.unpack_from('<Q' if ext else '<I',raw,off+8)[0]
    if size!=SIZE or layout!=1: continue
    isize=64 if ext else 32; xcnt=struct.unpack_from('<H',raw,off+2)[0]
    xsize=0 if xcnt==0 else 12+(xcnt-1)*4
    start=((off+isize+xsize+7)&~7)+16; p=start+1*8+4
    d0,d1=struct.unpack_from('<HH',raw,p); assert (d0,d1)==(0x0803,7)
    struct.pack_into('<HH',raw,p,d0,6); open(path,'wb').write(raw); break
else: raise AssertionError('target not found')
PY
rm -f "$BAD_SHADOW" "$BAD_TABLE"
if BAD_OUTPUT="$("$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$BAD_IMG" /000payload.bin "$REPLACEMENT" "$BAD_SHADOW" "$ORIGIN_LOOP" UNUSED "$BAD_TABLE" 2>&1)"; then BAD_STATUS=0; else BAD_STATUS=$?; fi
printf '%s\n' "$BAD_OUTPUT"
[[ "$BAD_STATUS" -ne 0 ]]
echo "$BAD_OUTPUT" | grep -q 'full big CBLKCNT entry delta1 disagrees with next HEAD'
[[ ! -e "$BAD_SHADOW" && ! -e "$BAD_TABLE" ]]

printf '%s\n' \
  'Stage 37 legacy full-big multiblock LZ4 + partial PLAIN composition PASS' \
  '  logical bytes: 98181' \
  '  LZ4 heads: [0, 8], CBLKCNT: [3, 3]' \
  '  PLAIN data lclusters: [16, 17, 18, 19, 20, 21, 22, 23]' \
  '  final PLAIN logical bytes: 3973; physical bytes: 4096' \
  '  physical capacity: 14 blocks' \
  "  materialized shadow blocks: $SHADOW_BLOCKS / 14" \
  '  effective replacement: PASS' \
  '  effective fsck.erofs: PASS' \
  '  malformed D0_CBLKCNT delta1 rejection before materialization: PASS' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $HASH_AFTER"
