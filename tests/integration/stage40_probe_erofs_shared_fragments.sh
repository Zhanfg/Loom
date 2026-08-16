#!/usr/bin/env bash
set -euo pipefail

WORK="$(mktemp -d)"
ROOT="$WORK/root"; IMG="$WORK/shared.erofs"; MNT="$WORK/mnt"
cleanup() {
  set +e
  mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT"
  rm -rf "$WORK"
}
trap cleanup EXIT
mkdir -p "$ROOT" "$MNT"

python3 - "$ROOT/000alpha.bin" "$ROOT/001beta.bin" <<'PY'
from pathlib import Path
import sys
SIZE=98304
for path,pat in [(sys.argv[1],b'STAGE40-ALPHA-FRAGMENT-'),(sys.argv[2],b'STAGE40-BETA-FRAGMENT-')]:
    Path(path).write_bytes((pat*((SIZE//len(pat))+1))[:SIZE])
PY
for i in $(seq -w 0 499); do : > "$ROOT/z_dummy_${i}_for_directory_growth"; done

mkfs.erofs -b 4096 -zlz4 -E legacy-compress,fragments -T 0 \
  --max-extent-bytes 32768 "$IMG" "$ROOT" >/dev/null
fsck.erofs "$IMG"

DUMP_A="$(dump.erofs -e --path=/000alpha.bin "$IMG")"
DUMP_B="$(dump.erofs -e --path=/001beta.bin "$IMG")"
printf '%s\n' '--- alpha ---' "$DUMP_A" '--- beta ---' "$DUMP_B"
NID_A="$(printf '%s\n' "$DUMP_A" | sed -n 's/^NID: \([0-9][0-9]*\).*/\1/p')"
NID_B="$(printf '%s\n' "$DUMP_B" | sed -n 's/^NID: \([0-9][0-9]*\).*/\1/p')"
[[ -n "$NID_A" && -n "$NID_B" && "$NID_A" != "$NID_B" ]]
echo "$DUMP_A" | grep -q 'Size: 98304'
echo "$DUMP_A" | grep -q '/000alpha.bin: 3 extents found'
echo "$DUMP_B" | grep -q 'Size: 98304'
echo "$DUMP_B" | grep -q '/001beta.bin: 3 extents found'

sudo mount -t erofs -o ro "$IMG" "$MNT"
cmp "$ROOT/000alpha.bin" "$MNT/000alpha.bin"
cmp "$ROOT/001beta.bin" "$MNT/001beta.bin"
sudo umount "$MNT"

python3 - "$IMG" "$NID_A" "$NID_B" <<'PY'
import stat,struct,sys
raw=open(sys.argv[1],'rb').read(); target_nids=[int(sys.argv[2]),int(sys.argv[3])]
sb=1024; BS=4096
u16=lambda o: struct.unpack_from('<H',raw,o)[0]
u32=lambda o: struct.unpack_from('<I',raw,o)[0]
u64=lambda o: struct.unpack_from('<Q',raw,o)[0]
assert u32(sb)==0xE0F5E1E2
incompat=u32(sb+0x50); meta=u32(sb+0x28); packed_nid=u64(sb+0x60)
assert incompat==0x20,hex(incompat)
assert packed_nid!=0

def inode(nid):
    off=meta*BS+nid*32
    fmt=u16(off); ext=fmt&1; layout=(fmt>>1)&7; isize=64 if ext else 32
    mode=u16(off+4); size=u64(off+8) if ext else u32(off+8); xcnt=u16(off+2)
    xsize=0 if xcnt==0 else 12+(xcnt-1)*4
    return dict(nid=nid,off=off,fmt=fmt,layout=layout,isize=isize,mode=mode,size=size,xsize=xsize,word=u32(off+0x10))

def full(nid,lclusters):
    x=inode(nid)
    assert stat.S_IFMT(x['mode'])==stat.S_IFREG and x['layout']==1,x
    h=(x['off']+x['isize']+x['xsize']+7)&~7
    fraglow=u32(h); advise=u16(h+4); alg0=raw[h+6]; alg1=raw[h+7]; lbits=raw[h+8]
    entries=[]; heads=[]; start=h+16
    for lcn in range(lclusters):
        adv,co,word=struct.unpack_from('<HHI',raw,start+lcn*8); kind=adv&3
        entries.append((lcn,kind,co,word,adv))
        if kind in (0,1): heads.append((lcn,kind,co,word,adv))
    return x,h,fraglow,advise,alg0,alg1,lbits,heads,entries

frags=[]
for nid in target_nids:
    x,h,low,adv,a0,a1,lbits,heads,entries=full(nid,24)
    assert x['size']==98304 and x['word']==2,x
    assert adv==0x30 and a0==0 and a1==0 and lbits==0,(adv,a0,a1,lbits)
    assert [v[0] for v in heads]==[0,8,16],heads
    assert heads[-1][1]==1 and heads[-1][2]==0
    fragoff=(heads[-1][3]<<32)|low
    frags.append((nid,fragoff,heads))
    print(f'target nid={nid} header={h} fragmentoff={fragoff} heads={heads}')

packed=inode(packed_nid)
assert stat.S_IFMT(packed['mode'])==stat.S_IFREG and packed['layout']==1,packed
packed_lclusters=(packed['size']+BS-1)//BS
px,ph,plow,padv,pa0,pa1,plbits,pheads,pentries=full(packed_nid,packed_lclusters)
assert padv==0x10 and plow==0 and pa0==0 and pa1==0 and plbits==0,(padv,plow,pa0,pa1,plbits)
print(f'packed nid={packed_nid} size={packed["size"]} data_word={packed["word"]} header={ph}')
print(f'packed heads={pheads}')
print(f'packed entries={pentries}')
print(f'fragment offsets={sorted(v[1] for v in frags)}')

# Report whether the two target fragments align to independent 32-KiB packed extents.
offsets=sorted(v[1] for v in frags)
assert offsets[0]==0,offsets
print(f'packed_independent_candidate={offsets==[0,32768] and packed["size"]==65536 and [h[0] for h in pheads]==[0,8] and packed["word"]==2}')
PY

echo 'Stage 40 shared-fragment topology probe PASS'
