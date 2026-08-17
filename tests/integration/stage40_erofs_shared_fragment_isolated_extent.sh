#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"
cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
ROOT="$WORK/root"; MNT="$WORK/mnt"
IMG="$WORK/origin.erofs"; BAD_IMG="$WORK/bad-offset.erofs"
ALPHA="$WORK/alpha.bin"; BETA="$WORK/beta.bin"
BETA_REPLACEMENT="$WORK/beta-replacement.bin"; BETA_OVERFLOW="$WORK/beta-overflow.bin"; ALPHA_REPLACEMENT="$WORK/alpha-replacement.bin"
SHADOW="$WORK/shadow.pack"; TABLE="$WORK/loom.table"
BAD_SHADOW="$WORK/bad.shadow"; BAD_TABLE="$WORK/bad.table"
ALPHA_SHADOW="$WORK/alpha.shadow"; ALPHA_TABLE="$WORK/alpha.table"
OFFSET_SHADOW="$WORK/offset.shadow"; OFFSET_TABLE="$WORK/offset.table"
ORIGIN_LOOP=""; SHADOW_LOOP=""; BAD_LOOP=""; MAPPER="loom-stage40-${RANDOM}-${RANDOM}"
cleanup() {
  set +e
  mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT"
  sudo dmsetup info "$MAPPER" >/dev/null 2>&1 && sudo dmsetup remove "$MAPPER"
  [[ -n "$SHADOW_LOOP" ]] && sudo losetup -d "$SHADOW_LOOP"
  [[ -n "$ORIGIN_LOOP" ]] && sudo losetup -d "$ORIGIN_LOOP"
  [[ -n "$BAD_LOOP" ]] && sudo losetup -d "$BAD_LOOP"
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 40 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR
mkdir -p "$ROOT" "$MNT"

python3 - "$ALPHA" "$BETA" "$BETA_REPLACEMENT" "$BETA_OVERFLOW" "$ALPHA_REPLACEMENT" <<'PY'
import random,sys
SIZE=98304; EXTENT=32768
def fill(pat): return (pat*((SIZE//len(pat))+1))[:SIZE]
alpha=fill(b'STAGE40-ALPHA-FRAGMENT-')
beta=fill(b'STAGE40-BETA-FRAGMENT-')
beta_replacement=b'A'*EXTENT + b'B'*EXTENT + b'Z'*EXTENT
alpha_replacement=b'Q'*EXTENT + b'R'*EXTENT + b'S'*EXTENT
rng=random.Random(0x4000F10)
beta_overflow=beta_replacement[:2*EXTENT] + bytes(rng.randrange(256) for _ in range(EXTENT))
for path,data in zip(sys.argv[1:],(alpha,beta,beta_replacement,beta_overflow,alpha_replacement)):
    assert len(data)==SIZE
    open(path,'wb').write(data)
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

def full(nid,lclusters):
    x=inode(nid); assert stat.S_IFMT(x['mode'])==stat.S_IFREG and x['layout']==1,x
    h=(x['off']+x['isize']+x['xsize']+7)&~7; low=u32(h); advice=u16(h+4)
    assert raw[h+6]==0 and raw[h+7]==0 and raw[h+8:h+16]==bytes(8)
    heads=[]; start=h+16
    for lcn in range(lclusters):
        adv,co,word=struct.unpack_from('<HHI',raw,start+lcn*8); kind=adv&3
        assert adv & ~3 == 0 and co==0,(nid,lcn,adv,co,word)
        if kind==1: heads.append((lcn,word))
        elif kind!=2: raise AssertionError((nid,lcn,kind,adv,co,word))
    return x,h,low,advice,heads

a,ah,alow,aadv,aheads=full(nids[0],24)
b,bh,blow,badv,bheads=full(nids[1],24)
assert a['size']==b['size']==98304 and a['word']==b['word']==2
assert aadv==badv==0x30
assert [x[0] for x in aheads]==[0,8,16] and [x[0] for x in bheads]==[0,8,16]
aoff=(aheads[-1][1]<<32)|alow; boff=(bheads[-1][1]<<32)|blow
assert aoff==0 and boff==32768,(aoff,boff)
packed=inode(packed_nid); assert packed['size']==65536 and packed['layout']==1 and packed['word']==2,packed
p,ph,plow,padv,pheads=full(packed_nid,16)
assert plow==0 and padv==0x10 and [x[0] for x in pheads]==[0,8],pheads
assert pheads[0][1] != pheads[1][1]
print(nids[0],nids[1],packed_nid,aheads[0][1],aheads[1][1],bheads[0][1],bheads[1][1],pheads[0][1],pheads[1][1],bh)
PY
)"
read -r NID_ALPHA NID_BETA PACKED_NID A0 A1 B0 B1 P0 P1 BETA_HEADER <<< "$META"
printf 'Stage 40 raw shared topology PASS alpha_nid=%s beta_nid=%s packed_nid=%s alpha=[%s,%s] beta=[%s,%s] packed=[%s,%s] beta_fragmentoff=32768\n' \
  "$NID_ALPHA" "$NID_BETA" "$PACKED_NID" "$A0" "$A1" "$B0" "$B1" "$P0" "$P1"

HASH_BEFORE="$(sha256sum "$IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$IMG")"
sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MNT"
sudo cmp "$MNT/000alpha.bin" "$ALPHA"
sudo cmp "$MNT/001beta.bin" "$BETA"
sudo umount "$MNT"

OUTPUT="$("$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$IMG" /001beta.bin "$BETA_REPLACEMENT" "$SHADOW" "$ORIGIN_LOOP" \
  LOOM_SHADOW_PLACEHOLDER "$TABLE")"
printf '%s\n' "$OUTPUT"
python3 - "$OUTPUT" "$B0" "$B1" "$P1" <<'PY'
import ast,re,sys
out=sys.argv[1]; expected=[int(x) for x in sys.argv[2:5]]
def vec(name):
    m=re.search(rf'{name}=(\[[^]]*\])',out); assert m,(name,out); return ast.literal_eval(m.group(1))
assert 'mode=multi-encode' in out and 'physical_pclusters=3' in out and 'logical_lclusters=24' in out
assert vec('head_lclusters')==[0,8,16]
assert vec('origin_pclusters')==expected,(vec('origin_pclusters'),expected)
assert vec('replacement_pclusters')==expected,(vec('replacement_pclusters'),expected)
encoded=vec('encoded_bytes'); assert len(encoded)==3 and all(0<x<=4096 for x in encoded),encoded
assert 'shadow_blocks=3' in out,out
print(f'Stage 40 isolated shared compiler routing PASS physical={expected} encoded={encoded}')
PY
[[ "$(stat -c %s "$SHADOW")" -eq 12288 ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MNT"
sudo cmp "$MNT/000alpha.bin" "$ALPHA"
sudo cmp "$MNT/001beta.bin" "$BETA_REPLACEMENT"
sudo umount "$MNT"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null

sudo python3 - "$IMG" "/dev/mapper/$MAPPER" "$A0" "$A1" "$B0" "$B1" "$P0" "$P1" <<'PY'
import sys
origin=open(sys.argv[1],'rb').read(); blocks=[int(x) for x in sys.argv[3:]]; BS=4096
with open(sys.argv[2],'rb',buffering=0) as f:
    effective=f.read()
assert effective[:BS]==origin[:BS]
for p in (blocks[0],blocks[1],blocks[4]):
    assert effective[p*BS:(p+1)*BS]==origin[p*BS:(p+1)*BS],p
for p in (blocks[2],blocks[3],blocks[5]):
    assert effective[p*BS:(p+1)*BS]!=origin[p*BS:(p+1)*BS],p
print('Stage 40 isolation PASS alpha data + packed extent0 unchanged; beta data + packed extent1 replaced')
PY

sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"; SHADOW_LOOP=""
HASH_AFTER="$(sha256sum "$IMG" | awk '{print $1}')"; [[ "$HASH_BEFORE" == "$HASH_AFTER" ]]

# Incompressible beta fragment must fail before materialization.
rm -f "$BAD_SHADOW" "$BAD_TABLE"
if BAD_OUTPUT="$("$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$IMG" /001beta.bin "$BETA_OVERFLOW" "$BAD_SHADOW" "$ORIGIN_LOOP" UNUSED "$BAD_TABLE" 2>&1)"; then
  BAD_STATUS=0
else
  BAD_STATUS=$?
fi
printf '%s\n' "$BAD_OUTPUT"
[[ "$BAD_STATUS" -ne 0 ]]
echo "$BAD_OUTPUT" | grep -q 'HEAD lcluster 16 does not fit existing pcluster'
echo "$BAD_OUTPUT" | grep -q 'capacity 4096'
[[ ! -e "$BAD_SHADOW" && ! -e "$BAD_TABLE" ]]

# Shared offset-zero alpha remains outside Stage 40 and must keep Stage 39 fail-closed behavior.
rm -f "$ALPHA_SHADOW" "$ALPHA_TABLE"
if ALPHA_OUTPUT="$("$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$IMG" /000alpha.bin "$ALPHA_REPLACEMENT" "$ALPHA_SHADOW" "$ORIGIN_LOOP" UNUSED "$ALPHA_TABLE" 2>&1)"; then
  ALPHA_STATUS=0
else
  ALPHA_STATUS=$?
fi
printf '%s\n' "$ALPHA_OUTPUT"
[[ "$ALPHA_STATUS" -ne 0 ]]
echo "$ALPHA_OUTPUT" | grep -q 'target fragment to occupy the entire packed inode'
[[ ! -e "$ALPHA_SHADOW" && ! -e "$ALPHA_TABLE" ]]

# A nonzero fragment offset that no longer starts on a packed HEAD boundary must be rejected.
cp "$IMG" "$BAD_IMG"
python3 - "$BAD_IMG" "$BETA_HEADER" <<'PY'
import struct,sys
path=sys.argv[1]; off=int(sys.argv[2])
with open(path,'r+b') as f:
    f.seek(off); f.write(struct.pack('<I',4096))
PY
BAD_LOOP="$(sudo losetup --find --show --read-only "$BAD_IMG")"
rm -f "$OFFSET_SHADOW" "$OFFSET_TABLE"
if OFFSET_OUTPUT="$("$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$BAD_IMG" /001beta.bin "$BETA_REPLACEMENT" "$OFFSET_SHADOW" "$BAD_LOOP" UNUSED "$OFFSET_TABLE" 2>&1)"; then
  OFFSET_STATUS=0
else
  OFFSET_STATUS=$?
fi
printf '%s\n' "$OFFSET_OUTPUT"
[[ "$OFFSET_STATUS" -ne 0 ]]
echo "$OFFSET_OUTPUT" | grep -q 'shared fragment does not begin at a packed HEAD boundary'
[[ ! -e "$OFFSET_SHADOW" && ! -e "$OFFSET_TABLE" ]]
sudo losetup -d "$BAD_LOOP"; BAD_LOOP=""

printf '%s\n' \
  'Stage 40 isolated nonzero shared fragment PASS' \
  '  two target files: 98304 bytes each' \
  '  superblock incompat: 0x20 (FRAGMENTS)' \
  '  alpha fragment offset: 0; beta fragment offset: 32768' \
  '  packed inode: 65536 bytes / HEAD lclusters [0, 8] / two physical pclusters' \
  "  packed physical pclusters: [$P0, $P1]" \
  '  beta maps exactly to packed HEAD extent 1' \
  '  alpha ordinary data + packed extent 0 unchanged: PASS' \
  '  beta ordinary data + packed extent 1 replaced: PASS' \
  '  effective alpha preservation: PASS' \
  '  effective beta replacement: PASS' \
  '  effective fsck.erofs: PASS' \
  '  beta fragment overflow rejection before materialization: PASS' \
  '  shared offset-zero alpha remains fail-closed: PASS' \
  '  non-HEAD-boundary fragment offset rejection before materialization: PASS' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $HASH_AFTER"
