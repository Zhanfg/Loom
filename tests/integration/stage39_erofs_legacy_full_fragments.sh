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
ORIGIN_LOOP=""; SHADOW_LOOP=""; MAPPER="loom-stage39-${RANDOM}-${RANDOM}"
cleanup() {
  set +e
  mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT"
  sudo dmsetup info "$MAPPER" >/dev/null 2>&1 && sudo dmsetup remove "$MAPPER"
  [[ -n "$SHADOW_LOOP" ]] && sudo losetup -d "$SHADOW_LOOP"
  [[ -n "$ORIGIN_LOOP" ]] && sudo losetup -d "$ORIGIN_LOOP"
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 39 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR
mkdir -p "$ROOT" "$MNT"

python3 - "$ORIGINAL" "$REPLACEMENT" "$OVERFLOW" <<'PY'
import random,sys
SIZE=98304; EXTENT=32768
pat=b'LOOM-STAGE39-FRAGMENTS-'
origin=(pat*((SIZE//len(pat))+1))[:SIZE]
replacement=b'A'*EXTENT + b'B'*EXTENT + b'Z'*EXTENT
rng=random.Random(0x3900F10)
overflow=replacement[:2*EXTENT] + bytes(rng.randrange(256) for _ in range(EXTENT))
assert len(origin)==len(replacement)==len(overflow)==SIZE
open(sys.argv[1],'wb').write(origin)
open(sys.argv[2],'wb').write(replacement)
open(sys.argv[3],'wb').write(overflow)
PY
cp "$ORIGINAL" "$ROOT/000payload.bin"
for i in $(seq -w 0 499); do : > "$ROOT/z_dummy_${i}_for_directory_growth"; done
mkfs.erofs -b 4096 -zlz4 -E legacy-compress,fragments -T 0 \
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
raw=open(sys.argv[1],'rb').read(); sb=1024; BS=4096; SIZE=98304
u16=lambda o: struct.unpack_from('<H',raw,o)[0]
u32=lambda o: struct.unpack_from('<I',raw,o)[0]
u64=lambda o: struct.unpack_from('<Q',raw,o)[0]
assert u32(sb)==0xE0F5E1E2
assert u32(sb+0x50)==0x20,hex(u32(sb+0x50))
meta=u32(sb+0x28); inos=u64(sb+0x10); packed_nid=u64(sb+0x60)
assert packed_nid!=0

def inode(nid):
    off=meta*BS+nid*32
    fmt=u16(off); ext=fmt&1; layout=(fmt>>1)&7; isize=64 if ext else 32
    mode=u16(off+4); size=u64(off+8) if ext else u32(off+8); xcnt=u16(off+2)
    xsize=0 if xcnt==0 else 12+(xcnt-1)*4
    return dict(nid=nid,off=off,layout=layout,isize=isize,mode=mode,size=size,xsize=xsize,word=u32(off+0x10))

target=None
for nid in range(int(inos)+32):
    x=inode(nid)
    if stat.S_IFMT(x['mode'])==stat.S_IFREG and x['size']==SIZE and x['layout']==1:
        target=x; break
assert target
theader=(target['off']+target['isize']+target['xsize']+7)&~7
assert u32(theader)==0
assert u16(theader+4)==0x30,hex(u16(theader+4))
assert raw[theader+6]==0 and raw[theader+7]==0 and raw[theader+8:theader+16]==bytes(8)
start=theader+16; heads=[]
for lcn in range(24):
    adv,co,word=struct.unpack_from('<HHI',raw,start+lcn*8); kind=adv&3
    assert adv & ~3 == 0 and co==0
    if kind==1: heads.append((lcn,word))
    elif kind!=2: raise AssertionError((lcn,kind,adv,co,word))
assert [x[0] for x in heads]==[0,8,16],heads
assert target['word']==2,target
assert heads[-1][1]==0,heads

packed=inode(packed_nid)
assert stat.S_IFMT(packed['mode'])==stat.S_IFREG
assert packed['layout']==1 and packed['size']==32768 and packed['word']==1,packed
pheader=(packed['off']+packed['isize']+packed['xsize']+7)&~7
assert u32(pheader)==0
assert u16(pheader+4)==0x10,hex(u16(pheader+4))
assert raw[pheader+6]==0 and raw[pheader+7]==0 and raw[pheader+8:pheader+16]==bytes(8)
pstart=pheader+16; pheads=[]
for lcn in range(8):
    adv,co,word=struct.unpack_from('<HHI',raw,pstart+lcn*8); kind=adv&3
    assert adv & ~3 == 0 and co==0
    if kind==1: pheads.append((lcn,word))
    elif kind!=2: raise AssertionError((lcn,kind,adv,co,word))
assert len(pheads)==1 and pheads[0][0]==0,pheads
assert pheads[0][1] not in (0,heads[0][1],heads[1][1]),(heads,pheads)
print(target['nid'],packed_nid,heads[0][1],heads[1][1],pheads[0][1])
PY
)"
read -r TARGET_NID PACKED_NID PBLK0 PBLK1 PACKED_PBLK <<< "$META"
printf 'Stage 39 raw fragment topology PASS target_nid=%s packed_nid=%s target_pblks=[%s,%s] packed_pblk=%s\n' \
  "$TARGET_NID" "$PACKED_NID" "$PBLK0" "$PBLK1" "$PACKED_PBLK"

HASH_BEFORE="$(sha256sum "$IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$IMG")"
sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MNT"
sudo cmp "$MNT/000payload.bin" "$ORIGINAL"
sudo umount "$MNT"

OUTPUT="$("$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$IMG" /000payload.bin "$REPLACEMENT" "$SHADOW" "$ORIGIN_LOOP" \
  LOOM_SHADOW_PLACEHOLDER "$TABLE")"
printf '%s\n' "$OUTPUT"
python3 - "$OUTPUT" "$PBLK0" "$PBLK1" "$PACKED_PBLK" <<'PY'
import ast,re,sys
out=sys.argv[1]; expected=[int(x) for x in sys.argv[2:5]]
def vec(name):
    m=re.search(rf'{name}=(\[[^]]*\])',out)
    assert m,(name,out)
    return ast.literal_eval(m.group(1))
assert 'mode=multi-encode' in out
assert 'physical_pclusters=3' in out
assert 'logical_lclusters=24' in out
assert vec('head_lclusters')==[0,8,16]
assert vec('origin_pclusters')==expected,(vec('origin_pclusters'),expected)
assert vec('replacement_pclusters')==expected,(vec('replacement_pclusters'),expected)
encoded=vec('encoded_bytes')
assert len(encoded)==3 and all(0<x<=4096 for x in encoded),encoded
assert 'shadow_blocks=3' in out,out
print(f'Stage 39 cross-inode compiler routing PASS physical={expected} encoded={encoded}')
PY
[[ "$(stat -c %s "$SHADOW")" -eq 12288 ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MNT"
sudo cmp "$MNT/000payload.bin" "$REPLACEMENT"
sudo umount "$MNT"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null

sudo python3 - "$IMG" "/dev/mapper/$MAPPER" "$PACKED_PBLK" <<'PY'
import sys
origin=open(sys.argv[1],'rb').read(); pblk=int(sys.argv[3]); BS=4096
with open(sys.argv[2],'rb',buffering=0) as f:
    effective0=f.read(BS)
    f.seek(pblk*BS); packed=f.read(BS)
assert effective0==origin[:BS]
assert packed!=origin[pblk*BS:(pblk+1)*BS]
print('Stage 39 effective metadata unchanged and packed pcluster replaced PASS')
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
echo "$BAD_OUTPUT" | grep -q 'capacity 4096'
[[ ! -e "$BAD_SHADOW" && ! -e "$BAD_TABLE" ]]

printf '%s\n' \
  'Stage 39 legacy full-index single-owner fragment PASS' \
  '  logical bytes: 98304' \
  '  superblock incompat: 0x20 (FRAGMENTS)' \
  '  target map advice: 0x30 (FRAGMENT_PCLUSTER + INTERLACED_PCLUSTER)' \
  '  target HEAD lclusters: [0, 8, 16]' \
  '  target data_word: 2; final HEAD physical length: 0' \
  "  packed nid: $PACKED_NID; packed pcluster: $PACKED_PBLK" \
  '  packed inode: 32768 bytes / one LZ4 HEAD / one physical pcluster' \
  '  cross-inode packed pcluster replacement: PASS' \
  '  metadata block zero unchanged: PASS' \
  '  effective replacement: PASS' \
  '  effective fsck.erofs: PASS' \
  '  fragment overflow rejection before materialization: PASS' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $HASH_AFTER"
