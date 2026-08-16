#!/usr/bin/env bash
set -euo pipefail

WORK="$(mktemp -d)"
ROOT="$WORK/root"; IMG="$WORK/partial-fragment.erofs"; MNT="$WORK/mnt"
cleanup() {
  set +e
  mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT"
  rm -rf "$WORK"
}
trap cleanup EXIT
mkdir -p "$ROOT" "$MNT"

python3 - "$ROOT/000payload.bin" <<'PY'
from pathlib import Path
import sys
SIZE=98181
pat=b'STAGE42-PARTIAL-FRAGMENT-'
Path(sys.argv[1]).write_bytes((pat*((SIZE//len(pat))+1))[:SIZE])
PY
for i in $(seq -w 0 499); do : > "$ROOT/z_dummy_${i}_for_directory_growth"; done

mkfs.erofs -b 4096 -zlz4 -E legacy-compress,fragments -T 0 \
  --max-extent-bytes 32768 "$IMG" "$ROOT" >/dev/null
fsck.erofs "$IMG"
DUMP="$(dump.erofs -e --path=/000payload.bin "$IMG")"
printf '%s\n' "$DUMP"
echo "$DUMP" | grep -q 'Size: 98181'

sudo mount -t erofs -o ro "$IMG" "$MNT"
cmp "$ROOT/000payload.bin" "$MNT/000payload.bin"
sudo umount "$MNT"

python3 - "$IMG" <<'PY'
import stat,struct,sys
raw=open(sys.argv[1],'rb').read(); sb=1024; BS=4096; SIZE=98181
u16=lambda o: struct.unpack_from('<H',raw,o)[0]
u32=lambda o: struct.unpack_from('<I',raw,o)[0]
u64=lambda o: struct.unpack_from('<Q',raw,o)[0]
assert u32(sb)==0xE0F5E1E2
compat=u32(sb+0x08); incompat=u32(sb+0x50); meta=u32(sb+0x28); inos=u64(sb+0x10); packed_nid=u64(sb+0x60)
print(f'super compat=0x{compat:x} incompat=0x{incompat:x} meta={meta} inos={inos} packed_nid={packed_nid}')
assert incompat==0x20,hex(incompat); assert packed_nid

def inode(nid):
    off=meta*BS+nid*32
    fmt=u16(off); ext=fmt&1; layout=(fmt>>1)&7; isize=64 if ext else 32
    mode=u16(off+4); size=u64(off+8) if ext else u32(off+8); xcnt=u16(off+2)
    xsize=0 if xcnt==0 else 12+(xcnt-1)*4
    return dict(nid=nid,off=off,fmt=fmt,layout=layout,isize=isize,mode=mode,size=size,xsize=xsize,word=u32(off+0x10))

def full(x,lclusters):
    h=(x['off']+x['isize']+x['xsize']+7)&~7
    low=u32(h); advice=u16(h+4); alg0=raw[h+6]; alg1=raw[h+7]; lbits=raw[h+8]
    entries=[]; heads=[]; start=h+16
    for lcn in range(lclusters):
        adv,co,word=struct.unpack_from('<HHI',raw,start+lcn*8); kind=adv&3
        entries.append((lcn,kind,co,word,adv))
        if kind in (0,1): heads.append((lcn,kind,co,word,adv))
    return h,low,advice,alg0,alg1,lbits,heads,entries

target=None
for nid in range(int(inos)+32):
    x=inode(nid)
    if stat.S_IFMT(x['mode'])==stat.S_IFREG and x['size']==SIZE and x['layout']==1:
        target=x; break
assert target,target
th,tlow,tadv,ta0,ta1,tlbits,theads,tentries=full(target,24)
assert tadv==0x30 and ta0==0 and ta1==0 and tlbits==0,(tadv,ta0,ta1,tlbits)
frag_head=theads[-1]
fragoff=(frag_head[3]<<32)|tlow
frag_logical=SIZE-frag_head[0]*BS
print(f'target={target}')
print(f'target header={th} fragoff={fragoff} frag_logical={frag_logical} heads={theads}')
print(f'target entries={tentries}')

packed=inode(packed_nid)
assert stat.S_IFMT(packed['mode'])==stat.S_IFREG and packed['layout']==1,packed
plclusters=(packed['size']+BS-1)//BS
ph,plow,padv,pa0,pa1,plbits,pheads,pentries=full(packed,plclusters)
print(f'packed={packed}')
print(f'packed header={ph} low={plow} advice=0x{padv:x} alg=({pa0},{pa1}) lbits={plbits}')
print(f'packed heads={pheads}')
print(f'packed entries={pentries}')
print(f'packed logical clusters={plclusters} eof_mod={packed["size"]%BS}')
print(f'fragment range=[{fragoff},{fragoff+frag_logical}) packed_size={packed["size"]}')
assert fragoff+frag_logical <= packed['size']
PY

echo 'Stage 42 partial-fragment topology probe PASS'
