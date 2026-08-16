#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"
cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
ROOT="$WORK/root"; MNT="$WORK/mnt"
IMG="$WORK/origin.erofs"
ORIGINAL="$WORK/original.bin"; REPLACEMENT="$WORK/replacement.bin"; OVERFLOW="$WORK/overflow.bin"
SHADOW="$WORK/shadow.pack"; TABLE="$WORK/loom.table"
BAD_SHADOW="$WORK/bad.shadow"; BAD_TABLE="$WORK/bad.table"
ORIGIN_LOOP=""; SHADOW_LOOP=""; MAPPER="loom-stage38-${RANDOM}-${RANDOM}"
cleanup() {
  set +e
  mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT"
  sudo dmsetup info "$MAPPER" >/dev/null 2>&1 && sudo dmsetup remove "$MAPPER"
  [[ -n "$SHADOW_LOOP" ]] && sudo losetup -d "$SHADOW_LOOP"
  [[ -n "$ORIGIN_LOOP" ]] && sudo losetup -d "$ORIGIN_LOOP"
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 38 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR
mkdir -p "$ROOT" "$MNT"

python3 - "$ORIGINAL" "$REPLACEMENT" "$OVERFLOW" <<'PY'
import random, sys
SIZE=98304; EXTENT=32768
pat=b'LOOM-STAGE38-ZTAILPACKING-'
origin=(pat*((SIZE//len(pat))+1))[:SIZE]
replacement=b'A'*EXTENT + b'B'*EXTENT + b'Z'*EXTENT
rng=random.Random(0x3800F10)
overflow=replacement[:2*EXTENT] + bytes(rng.randrange(256) for _ in range(EXTENT))
assert len(origin)==len(replacement)==len(overflow)==SIZE
open(sys.argv[1],'wb').write(origin)
open(sys.argv[2],'wb').write(replacement)
open(sys.argv[3],'wb').write(overflow)
PY
cp "$ORIGINAL" "$ROOT/000payload.bin"
for i in $(seq -w 0 499); do : > "$ROOT/z_dummy_${i}_for_directory_growth"; done
mkfs.erofs -b 4096 -zlz4 -E legacy-compress,ztailpacking -T 0 \
  --max-extent-bytes 32768 "$IMG" "$ROOT" >/dev/null
fsck.erofs "$IMG" >/dev/null
DUMP="$(dump.erofs -e --path=/000payload.bin "$IMG")"
printf '%s\n' "$DUMP"
echo "$DUMP" | grep -q 'Size: 98304'
echo "$DUMP" | grep -q 'On-disk size: 8192'
echo "$DUMP" | grep -q 'Layout: 1'
echo "$DUMP" | grep -q '/000payload.bin: 3 extents found'

META="$(python3 - "$IMG" <<'PY'
import stat,struct,sys
SIZE=98304
raw=open(sys.argv[1],'rb').read(); sb=1024
assert struct.unpack_from('<I',raw,sb)[0]==0xE0F5E1E2
assert struct.unpack_from('<I',raw,sb+0x50)[0]==0x10
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
    break
else: raise AssertionError('target not found')
assert struct.unpack_from('<H',h,0)[0]==0
idata=struct.unpack_from('<H',h,2)[0]
assert idata==165,idata
assert struct.unpack_from('<H',h,4)[0]==0x8
assert h[6]==0 and h[7]==0 and h[8:16]==bytes(8)
heads=[]
for lcn in range(24):
    advise,co,word=struct.unpack_from('<HHI',raw,start+lcn*8); kind=advise&3
    assert advise & ~3 == 0 and co==0
    if kind==1: heads.append((lcn,word))
    elif kind!=2: raise AssertionError((lcn,kind))
assert heads==[(0,1),(8,2),(16,3)],heads
assert blocks==2,blocks
inline=start+24*8
assert inline==3056,inline
assert header==2848,header
assert inline//4096==header//4096==0
assert inline+idata<=4096
assert any(raw[inline:inline+idata])
print(nid,header,inline,idata,blocks)
PY
)"
read -r NID HEADER_OFF INLINE_OFF INLINE_CAP DATA_WORD <<< "$META"
printf 'Stage 38 raw inline topology PASS nid=%s header=%s inline=%s capacity=%s data_word=%s\n' \
  "$NID" "$HEADER_OFF" "$INLINE_OFF" "$INLINE_CAP" "$DATA_WORD"

HASH_BEFORE="$(sha256sum "$IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$IMG")"
sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MNT"
sudo cmp "$MNT/000payload.bin" "$ORIGINAL"
sudo umount "$MNT"

OUTPUT="$("$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$IMG" /000payload.bin "$REPLACEMENT" "$SHADOW" "$ORIGIN_LOOP" \
  LOOM_SHADOW_PLACEHOLDER "$TABLE")"
printf '%s\n' "$OUTPUT"
echo "$OUTPUT" | grep -q 'mode=multi-encode'
echo "$OUTPUT" | grep -q 'physical_pclusters=3'
echo "$OUTPUT" | grep -q 'logical_lclusters=24'
echo "$OUTPUT" | grep -q 'head_lclusters=\[0, 8, 16\]'
echo "$OUTPUT" | grep -q 'origin_pclusters=\[1, 2, 3\]'
echo "$OUTPUT" | grep -q 'shadow_blocks=3'
[[ "$(stat -c %s "$SHADOW")" -eq 12288 ]]
ENCODED="$(printf '%s\n' "$OUTPUT" | sed -n 's/.*encoded_bytes=\(\[[^]]*\]\).*/\1/p')"
NEW_INLINE="$(python3 - "$ENCODED" "$INLINE_CAP" <<'PY'
import ast,sys
v=ast.literal_eval(sys.argv[1]); cap=int(sys.argv[2])
assert len(v)==3,v
assert 0<v[0]<=4096 and 0<v[1]<=4096,v
assert 0<v[2]<=cap,v
assert v[2]<cap,(v,cap)
print(v[2])
PY
)"
printf 'Stage 38 encoded inline extent PASS encoded=%s old_capacity=%s new_inline=%s\n' "$ENCODED" "$INLINE_CAP" "$NEW_INLINE"

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MNT"
sudo cmp "$MNT/000payload.bin" "$REPLACEMENT"
sudo umount "$MNT"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null

sudo python3 - "/dev/mapper/$MAPPER" "$HEADER_OFF" "$INLINE_OFF" "$INLINE_CAP" "$NEW_INLINE" <<'PY'
import struct,sys
path=sys.argv[1]; header=int(sys.argv[2]); inline=int(sys.argv[3]); cap=int(sys.argv[4]); new=int(sys.argv[5])
with open(path,'rb',buffering=0) as f:
    f.seek(header); h=f.read(8)
    f.seek(inline); payload=f.read(cap)
assert struct.unpack_from('<H',h,2)[0]==new,(h.hex(),new)
assert struct.unpack_from('<H',h,4)[0]==0x8
assert any(payload[:new])
assert payload[new:]==bytes(cap-new)
print(f'Stage 38 effective metadata PASS h_idata_size={new} zeroed_tail={cap-new}')
PY

sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"; SHADOW_LOOP=""
HASH_AFTER="$(sha256sum "$IMG" | awk '{print $1}')"; [[ "$HASH_BEFORE" == "$HASH_AFTER" ]]

rm -f "$BAD_SHADOW" "$BAD_TABLE"
if BAD_OUTPUT="$("$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$IMG" /000payload.bin "$OVERFLOW" "$BAD_SHADOW" "$ORIGIN_LOOP" UNUSED "$BAD_TABLE" 2>&1)"; then
  BAD_STATUS=0
else
  BAD_STATUS=$?
fi
printf '%s\n' "$BAD_OUTPUT"
[[ "$BAD_STATUS" -ne 0 ]]
echo "$BAD_OUTPUT" | grep -q 'HEAD lcluster 16 does not fit existing pcluster'
echo "$BAD_OUTPUT" | grep -q "capacity $INLINE_CAP"
[[ ! -e "$BAD_SHADOW" && ! -e "$BAD_TABLE" ]]

printf '%s\n' \
  'Stage 38 legacy full-index inline pcluster PASS' \
  '  logical bytes: 98304' \
  '  superblock incompat: 0x10 (ZTAILPACKING)' \
  '  map advice: 0x8 (INLINE_PCLUSTER)' \
  '  HEAD lclusters: [0, 8, 16]' \
  '  data-area physical blocks: 2; inline HEAD placeholder pblk: 3' \
  "  original inline capacity: $INLINE_CAP bytes" \
  "  replacement inline encoded bytes: $NEW_INLINE" \
  '  h_idata_size update: PASS' \
  '  old inline-capacity remainder zeroed: PASS' \
  '  shadow blocks: 3 (metadata block + two data pclusters)' \
  '  effective replacement: PASS' \
  '  effective fsck.erofs: PASS' \
  '  inline overflow rejection before materialization: PASS' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $HASH_AFTER"
