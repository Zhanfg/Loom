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
ORIGIN_LOOP=""; SHADOW_LOOP=""; MAPPER="loom-stage42-${RANDOM}-${RANDOM}"
cleanup() {
  set +e
  mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT"
  sudo dmsetup info "$MAPPER" >/dev/null 2>&1 && sudo dmsetup remove "$MAPPER"
  [[ -n "$SHADOW_LOOP" ]] && sudo losetup -d "$SHADOW_LOOP"
  [[ -n "$ORIGIN_LOOP" ]] && sudo losetup -d "$ORIGIN_LOOP"
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 42 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR
mkdir -p "$ROOT" "$MNT"

python3 - "$ORIGINAL" "$REPLACEMENT" "$OVERFLOW" <<'PY'
import random,sys
SIZE=98181; E=32768; T=SIZE-2*E
pat=b'STAGE42-PARTIAL-FRAGMENT-'
origin=(pat*((SIZE//len(pat))+1))[:SIZE]
replacement=b'A'*E+b'B'*E+b'Z'*T
rng=random.Random(0x4200F10)
overflow=replacement[:2*E]+bytes(rng.randrange(256) for _ in range(T))
assert len(origin)==len(replacement)==len(overflow)==SIZE and T==32645
for path,data in zip(sys.argv[1:],(origin,replacement,overflow)):
    open(path,'wb').write(data)
PY
cp "$ORIGINAL" "$ROOT/000payload.bin"
for i in $(seq -w 0 499); do : > "$ROOT/z_dummy_${i}_for_directory_growth"; done
mkfs.erofs -b 4096 -zlz4 -E legacy-compress,fragments -T 0 \
  --max-extent-bytes 32768 "$IMG" "$ROOT" >/dev/null
fsck.erofs "$IMG" >/dev/null
DUMP="$(dump.erofs -e --path=/000payload.bin "$IMG")"
printf '%s\n' "$DUMP"
echo "$DUMP" | grep -q 'Size: 98181'
echo "$DUMP" | grep -q '/000payload.bin: 3 extents found'
echo "$DUMP" | grep -q '65536..   98181 |   32645'

META="$(python3 - "$IMG" <<'PY'
import stat,struct,sys
raw=open(sys.argv[1],'rb').read(); sb=1024; BS=4096; SIZE=98181
u16=lambda o: struct.unpack_from('<H',raw,o)[0]
u32=lambda o: struct.unpack_from('<I',raw,o)[0]
u64=lambda o: struct.unpack_from('<Q',raw,o)[0]
assert u32(sb)==0xE0F5E1E2 and u32(sb+0x50)==0x20
meta=u32(sb+0x28); inos=u64(sb+0x10); packed_nid=u64(sb+0x60); assert packed_nid

def inode(nid):
    off=meta*BS+nid*32; fmt=u16(off); ext=fmt&1; layout=(fmt>>1)&7; isize=64 if ext else 32
    mode=u16(off+4); size=u64(off+8) if ext else u32(off+8); xcnt=u16(off+2)
    xsize=0 if xcnt==0 else 12+(xcnt-1)*4
    return dict(nid=nid,off=off,layout=layout,isize=isize,mode=mode,size=size,xsize=xsize,word=u32(off+0x10))

def parse_full(x,lclusters):
    h=(x['off']+x['isize']+x['xsize']+7)&~7
    low=u32(h); advice=u16(h+4)
    assert raw[h+6]==0 and raw[h+7]==0 and raw[h+8:h+16]==bytes(8)
    entries=[]; data_heads=[]; start=h+16
    for lcn in range(lclusters):
        adv,co,word=struct.unpack_from('<HHI',raw,start+lcn*8); kind=adv&3
        assert adv & ~3 == 0,(lcn,adv)
        entries.append((lcn,kind,co,word))
        if kind==1: data_heads.append((lcn,co,word))
    return h,low,advice,data_heads,entries

target=None
for nid in range(int(inos)+32):
    x=inode(nid)
    if stat.S_IFMT(x['mode'])==stat.S_IFREG and x['size']==SIZE and x['layout']==1:
        target=x; break
assert target
theader,tlow,tadv,theads,tentries=parse_full(target,24)
assert tadv==0x30 and target['word']==2
assert theads==[(0,0,1),(8,0,2),(16,0,0)],theads
assert tentries[23]==(23,0,3973,0),tentries[23]
assert tlow==0
packed=inode(packed_nid)
assert stat.S_IFMT(packed['mode'])==stat.S_IFREG and packed['layout']==1
assert packed['size']==32645 and packed['word']==1,packed
pheader,plow,padv,pheads,pentries=parse_full(packed,8)
assert plow==0 and padv==0x10
assert len(pheads)==1 and pheads[0][0:2]==(0,0),pheads
assert pentries[7]==(7,0,3973,0),pentries[7]
packed_pblk=pheads[0][2]
assert packed_pblk not in (1,2,0)
print(target['nid'],packed_nid,theads[0][2],theads[1][2],packed_pblk,theader,pheader)
PY
)"
read -r TARGET_NID PACKED_NID P0 P1 PACKED_PBLK TARGET_HEADER PACKED_HEADER <<< "$META"
printf 'Stage 42 raw partial fragment PASS target_nid=%s packed_nid=%s target_pblks=[%s,%s] packed_pblk=%s target_eof=3973 packed_eof=3973\n' \
  "$TARGET_NID" "$PACKED_NID" "$P0" "$P1" "$PACKED_PBLK"

HASH_BEFORE="$(sha256sum "$IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$IMG")"
sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MNT"
sudo cmp "$MNT/000payload.bin" "$ORIGINAL"
sudo umount "$MNT"

OUTPUT="$("$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$IMG" /000payload.bin "$REPLACEMENT" "$SHADOW" "$ORIGIN_LOOP" \
  LOOM_SHADOW_PLACEHOLDER "$TABLE")"
printf '%s\n' "$OUTPUT"
python3 - "$OUTPUT" "$P0" "$P1" "$PACKED_PBLK" <<'PY'
import ast,re,sys
out=sys.argv[1]; expected=[int(x) for x in sys.argv[2:5]]
def vec(name):
    m=re.search(rf'{name}=(\[[^]]*\])',out); assert m,(name,out); return ast.literal_eval(m.group(1))
assert 'mode=multi-encode' in out and 'physical_pclusters=3' in out and 'logical_lclusters=24' in out
assert vec('head_lclusters')==[0,8,16]
assert vec('origin_pclusters')==expected,(vec('origin_pclusters'),expected)
assert vec('replacement_pclusters')==expected,(vec('replacement_pclusters'),expected)
encoded=vec('encoded_bytes')
assert len(encoded)==3 and all(0<x<=4096 for x in encoded),encoded
assert 'shadow_blocks=3' in out,out
print(f'Stage 42 compiler routing PASS physical={expected} encoded={encoded}')
PY
[[ "$(stat -c %s "$SHADOW")" -eq 12288 ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MNT"
sudo cmp "$MNT/000payload.bin" "$REPLACEMENT"
sudo umount "$MNT"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null

sudo python3 - "$IMG" "/dev/mapper/$MAPPER" "$TARGET_HEADER" "$PACKED_HEADER" "$PACKED_PBLK" <<'PY'
import sys
origin=open(sys.argv[1],'rb').read(); th=int(sys.argv[3]); ph=int(sys.argv[4]); pblk=int(sys.argv[5]); BS=4096
with open(sys.argv[2],'rb',buffering=0) as f: effective=f.read()
# Target and packed map headers/index arrays, including both EOF sentinels, are metadata-only and must remain byte-identical.
assert effective[th:th+16+24*8]==origin[th:th+16+24*8]
assert effective[ph:ph+16+8*8]==origin[ph:ph+16+8*8]
assert effective[pblk*BS:(pblk+1)*BS]!=origin[pblk*BS:(pblk+1)*BS]
print('Stage 42 metadata sentinel preservation and packed pcluster replacement PASS')
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
  'Stage 42 single-owner partial fragment PASS' \
  '  logical bytes: 98181' \
  '  target fragment logical bytes: 32645' \
  '  target EOF sentinel: LCN23 / clusterofs3973 / word0' \
  '  packed inode logical bytes: 32645' \
  '  packed EOF sentinel: LCN7 / clusterofs3973 / word0' \
  "  packed pcluster: $PACKED_PBLK" \
  '  target + packed metadata unchanged: PASS' \
  '  effective replacement: PASS' \
  '  effective fsck.erofs: PASS' \
  '  partial fragment overflow rejection before materialization: PASS' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $HASH_AFTER"
