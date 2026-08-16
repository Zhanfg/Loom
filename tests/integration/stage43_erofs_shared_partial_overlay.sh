#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"
cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
ROOT="$WORK/root"; MNT="$WORK/mnt"
IMG="$WORK/origin.erofs"
ALPHA="$WORK/alpha.bin"; BETA="$WORK/beta.bin"
REPLACEMENT="$WORK/beta-replacement.bin"; OVERFLOW="$WORK/beta-overflow.bin"
SHADOW="$WORK/shadow.pack"; TABLE="$WORK/loom.table"
BAD_SHADOW="$WORK/bad.shadow"; BAD_TABLE="$WORK/bad.table"
ORIGIN_LOOP=""; SHADOW_LOOP=""; MAPPER="loom-stage43-${RANDOM}-${RANDOM}"
cleanup() {
  set +e
  mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT"
  sudo dmsetup info "$MAPPER" >/dev/null 2>&1 && sudo dmsetup remove "$MAPPER"
  [[ -n "$SHADOW_LOOP" ]] && sudo losetup -d "$SHADOW_LOOP"
  [[ -n "$ORIGIN_LOOP" ]] && sudo losetup -d "$ORIGIN_LOOP"
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 43 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR
mkdir -p "$ROOT" "$MNT"

python3 - "$ALPHA" "$BETA" "$REPLACEMENT" "$OVERFLOW" <<'PY'
import random,sys
SIZE=98181; E=32768; T=SIZE-2*E
def fill(pat): return (pat*((SIZE//len(pat))+1))[:SIZE]
alpha=fill(b'STAGE43-ALPHA-PARTIAL-')
beta=fill(b'STAGE43-BETA-PARTIAL-')
replacement=b'A'*E+b'B'*E+b'Z'*T
rng=random.Random(0x4300F10)
overflow=replacement[:2*E]+bytes(rng.randrange(256) for _ in range(T))
for path,data in zip(sys.argv[1:],(alpha,beta,replacement,overflow)):
    assert len(data)==SIZE
    open(path,'wb').write(data)
assert T==32645
PY
cp "$ALPHA" "$ROOT/000alpha.bin"
cp "$BETA" "$ROOT/001beta.bin"
for i in $(seq -w 0 499); do : > "$ROOT/z_dummy_${i}_for_directory_growth"; done
mkfs.erofs -b 4096 -zlz4 -E legacy-compress,fragments -T 0 \
  --max-extent-bytes 32768 "$IMG" "$ROOT" >/dev/null
fsck.erofs "$IMG" >/dev/null

DUMP_A="$(dump.erofs -e --path=/000alpha.bin "$IMG")"
DUMP_B="$(dump.erofs -e --path=/001beta.bin "$IMG")"
printf '%s\n' "$DUMP_A" "$DUMP_B"
NID_A="$(printf '%s\n' "$DUMP_A" | sed -n 's/^NID: \([0-9][0-9]*\).*/\1/p')"
NID_B="$(printf '%s\n' "$DUMP_B" | sed -n 's/^NID: \([0-9][0-9]*\).*/\1/p')"
[[ -n "$NID_A" && -n "$NID_B" && "$NID_A" != "$NID_B" ]]

META="$(python3 - "$IMG" "$NID_A" "$NID_B" <<'PY'
import stat,struct,sys
raw=open(sys.argv[1],'rb').read(); nids=[int(sys.argv[2]),int(sys.argv[3])]
sb=1024; BS=4096
u16=lambda o: struct.unpack_from('<H',raw,o)[0]
u32=lambda o: struct.unpack_from('<I',raw,o)[0]
u64=lambda o: struct.unpack_from('<Q',raw,o)[0]
assert u32(sb)==0xE0F5E1E2 and u32(sb+0x50)==0x20
meta=u32(sb+0x28); packed_nid=u64(sb+0x60); assert packed_nid

def inode(nid):
    off=meta*BS+nid*32; fmt=u16(off); ext=fmt&1; layout=(fmt>>1)&7; isize=64 if ext else 32
    mode=u16(off+4); size=u64(off+8) if ext else u32(off+8); xcnt=u16(off+2)
    xsize=0 if xcnt==0 else 12+(xcnt-1)*4
    return dict(nid=nid,off=off,layout=layout,isize=isize,mode=mode,size=size,xsize=xsize,word=u32(off+0x10))

def full(x,lclusters):
    h=(x['off']+x['isize']+x['xsize']+7)&~7; low=u32(h); advice=u16(h+4)
    assert raw[h+6]==0 and raw[h+7]==0 and raw[h+8:h+16]==bytes(8)
    entries=[]; lz4=[]; start=h+16
    for lcn in range(lclusters):
        adv,co,word=struct.unpack_from('<HHI',raw,start+lcn*8); kind=adv&3
        assert adv & ~3 == 0,(x['nid'],lcn,adv)
        entries.append((lcn,kind,co,word))
        if kind==1: lz4.append((lcn,co,word))
    return h,low,advice,lz4,entries

targets=[]
for nid in nids:
    x=inode(nid); assert stat.S_IFMT(x['mode'])==stat.S_IFREG and x['layout']==1 and x['size']==98181
    h,low,adv,heads,entries=full(x,24)
    assert adv==0x30 and x['word']==2
    assert heads==[(0,0,heads[0][2]),(8,0,heads[1][2]),(16,0,0)],heads
    assert entries[23]==(23,0,3973,0),entries[23]
    fragoff=(heads[-1][2]<<32)|low
    targets.append((x,h,heads,fragoff))
assert targets[0][3]==0 and targets[1][3]==32645,(targets[0][3],targets[1][3])
packed=inode(packed_nid); assert packed['layout']==1 and packed['size']==65290 and packed['word']==2,packed
ph,plow,padv,pheads,pentries=full(packed,16)
assert plow==0 and padv==0x10
assert [h[0] for h in pheads]==[0,8],pheads
assert pentries[15]==(15,0,3850,0),pentries[15]
assert pheads[0][2] != pheads[1][2]
aheads=targets[0][2]; bheads=targets[1][2]
print(nids[0],nids[1],packed_nid,aheads[0][2],aheads[1][2],bheads[0][2],bheads[1][2],pheads[0][2],pheads[1][2],targets[0][1],targets[1][1],ph)
PY
)"
read -r NID_ALPHA NID_BETA PACKED_NID A0 A1 B0 B1 P0 P1 A_HEADER B_HEADER P_HEADER <<< "$META"
printf 'Stage 43 raw overlap PASS alpha_nid=%s beta_nid=%s packed_nid=%s alpha=[%s,%s] beta=[%s,%s] packed=[%s,%s] offsets=[0,32645]\n' \
  "$NID_ALPHA" "$NID_BETA" "$PACKED_NID" "$A0" "$A1" "$B0" "$B1" "$P0" "$P1"

HASH_BEFORE="$(sha256sum "$IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$IMG")"
sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MNT"
sudo cmp "$MNT/000alpha.bin" "$ALPHA"
sudo cmp "$MNT/001beta.bin" "$BETA"
sudo umount "$MNT"

OUTPUT="$("$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$IMG" /001beta.bin "$REPLACEMENT" "$SHADOW" "$ORIGIN_LOOP" \
  LOOM_SHADOW_PLACEHOLDER "$TABLE")"
printf '%s\n' "$OUTPUT"
python3 - "$OUTPUT" "$B0" "$B1" "$P0" "$P1" <<'PY'
import ast,re,sys
out=sys.argv[1]; expected=[int(x) for x in sys.argv[2:6]]
def vec(name):
    m=re.search(rf'{name}=(\[[^]]*\])',out); assert m,(name,out); return ast.literal_eval(m.group(1))
assert 'mode=multi-encode' in out and 'physical_pclusters=4' in out and 'logical_lclusters=24' in out
assert vec('head_lclusters')==[0,8,16,16],vec('head_lclusters')
assert vec('origin_pclusters')==expected,(vec('origin_pclusters'),expected)
assert vec('replacement_pclusters')==expected,(vec('replacement_pclusters'),expected)
encoded=vec('encoded_bytes'); assert len(encoded)==4 and all(0<x<=4096 for x in encoded),encoded
assert 'shadow_blocks=4' in out,out
print(f'Stage 43 overlay compiler routing PASS physical={expected} encoded={encoded}')
PY
[[ "$(stat -c %s "$SHADOW")" -eq 16384 ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MNT"
sudo cmp "$MNT/000alpha.bin" "$ALPHA"
sudo cmp "$MNT/001beta.bin" "$REPLACEMENT"
sudo umount "$MNT"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null

sudo python3 - "$IMG" "/dev/mapper/$MAPPER" "$A0" "$A1" "$B0" "$B1" "$P0" "$P1" "$A_HEADER" "$B_HEADER" "$P_HEADER" <<'PY'
import sys
origin=open(sys.argv[1],'rb').read(); vals=[int(x) for x in sys.argv[3:]]; BS=4096
A0,A1,B0,B1,P0,P1,AH,BH,PH=vals
with open(sys.argv[2],'rb',buffering=0) as f: effective=f.read()
for p in (A0,A1):
    assert effective[p*BS:(p+1)*BS]==origin[p*BS:(p+1)*BS],p
for p in (B0,B1,P0,P1):
    assert effective[p*BS:(p+1)*BS]!=origin[p*BS:(p+1)*BS],p
# Target + packed index metadata, including partial EOF sentinels, stays unchanged.
assert effective[AH:AH+16+24*8]==origin[AH:AH+16+24*8]
assert effective[BH:BH+16+24*8]==origin[BH:BH+16+24*8]
assert effective[PH:PH+16+16*8]==origin[PH:PH+16+16*8]
print('Stage 43 physical isolation PASS alpha ordinary blocks unchanged; beta + both packed blocks changed; metadata unchanged')
PY

sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"; SHADOW_LOOP=""
HASH_AFTER="$(sha256sum "$IMG" | awk '{print $1}')"; [[ "$HASH_BEFORE" == "$HASH_AFTER" ]]

# Random beta fragment forces at least one recomposed packed extent beyond its 4-KiB footprint.
rm -f "$BAD_SHADOW" "$BAD_TABLE"
if BAD_OUTPUT="$("$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$IMG" /001beta.bin "$OVERFLOW" "$BAD_SHADOW" "$ORIGIN_LOOP" UNUSED "$BAD_TABLE" 2>&1)"; then
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
  'Stage 43 shared partial fragment overlay PASS' \
  '  two target files: 98181 bytes each' \
  '  fragment ranges: alpha [0,32645), beta [32645,65290)' \
  '  packed extents: [0,32768) and [32768,65290)' \
  '  beta overlap: 123 bytes in packed extent0 + 32522 bytes in extent1' \
  "  packed physical pclusters: [$P0, $P1]" \
  '  read-modify-reencode both packed extents: PASS' \
  '  alpha byte-for-byte preservation after shared pcluster rewrite: PASS' \
  '  beta byte-for-byte replacement: PASS' \
  '  target + packed metadata/sentinels unchanged: PASS' \
  '  effective fsck.erofs: PASS' \
  '  overlay overflow rejected before materialization: PASS' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $HASH_AFTER"
