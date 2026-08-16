#!/usr/bin/env bash
set -euo pipefail

WORK="$(mktemp -d)"
ROOT="$WORK/root"
IMG="$WORK/fragments.erofs"
MNT="$WORK/mnt"
cleanup() {
  set +e
  mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT"
  rm -rf "$WORK"
}
trap cleanup EXIT
mkdir -p "$ROOT" "$MNT"

python3 - "$ROOT/000payload.bin" <<'PY'
from pathlib import Path
SIZE=98304
pat=b'LOOM-STAGE39-FRAGMENTS-'
Path(__import__('sys').argv[1]).write_bytes((pat*((SIZE//len(pat))+1))[:SIZE])
PY
for i in $(seq -w 0 499); do : > "$ROOT/z_dummy_${i}_for_directory_growth"; done

mkfs.erofs -b 4096 -zlz4 -E legacy-compress,fragments -T 0 \
  --max-extent-bytes 32768 "$IMG" "$ROOT" >/dev/null
fsck.erofs "$IMG"
printf '%s\n' '--- target dump ---'
dump.erofs -e --path=/000payload.bin "$IMG"

sudo mount -t erofs -o ro "$IMG" "$MNT"
cmp "$ROOT/000payload.bin" "$MNT/000payload.bin"
sudo umount "$MNT"

python3 - "$IMG" <<'PY'
import stat,struct,sys
raw=open(sys.argv[1],'rb').read(); sb=1024; BS=4096; SIZE=98304
u16=lambda o: struct.unpack_from('<H',raw,o)[0]
u32=lambda o: struct.unpack_from('<I',raw,o)[0]
u64=lambda o: struct.unpack_from('<Q',raw,o)[0]
assert u32(sb)==0xE0F5E1E2
compat=u32(sb+0x08); incompat=u32(sb+0x50); meta=u32(sb+0x28); inos=u64(sb+0x10); packed=u64(sb+0x60)
print(f'super compat=0x{compat:x} incompat=0x{incompat:x} meta={meta} inos={inos} packed_nid={packed}')
assert incompat & 0x20,hex(incompat)
assert packed != 0

def inode(nid):
    off=meta*BS+nid*32
    fmt=u16(off); ext=fmt&1; layout=(fmt>>1)&7; isize=64 if ext else 32
    mode=u16(off+4); size=u64(off+8) if ext else u32(off+8); xcnt=u16(off+2)
    xsize=0 if xcnt==0 else 12+(xcnt-1)*4
    word=u32(off+0x10)
    return dict(nid=nid,off=off,fmt=fmt,ext=ext,layout=layout,isize=isize,mode=mode,size=size,xcnt=xcnt,xsize=xsize,word=word)

target=None
for nid in range(int(inos)+32):
    x=inode(nid)
    if stat.S_IFMT(x['mode'])==stat.S_IFREG and x['size']==SIZE and x['layout'] in (1,3):
        target=x; break
assert target,target
pack=inode(packed)
print('target',target)
print('packed',pack)
assert stat.S_IFMT(pack['mode'])==stat.S_IFREG

# Legacy full map header is 8-byte aligned after inode+xattrs, then 16 bytes.
t=target
header=(t['off']+t['isize']+t['xsize']+7)&~7
h=raw[header:header+16]
fragoff=u32(header); advise=u16(header+4); alg0=raw[header+6]; alg1=raw[header+7]; lbits=raw[header+8]
print(f'target_map header={header} fragoff_low={fragoff} advise=0x{advise:x} alg=({alg0},{alg1}) lbits={lbits} data_word={t["word"]}')
assert advise & 0x20,hex(advise)
start=header+16
entries=[]; heads=[]
for lcn in range(24):
    adv,co,word=struct.unpack_from('<HHI',raw,start+lcn*8); kind=adv&3
    entries.append((lcn,kind,co,word,adv))
    if kind in (0,1): heads.append((lcn,kind,co,word,adv))
print('target_heads',heads)
print('target_entries',entries)

# For full-index fragment heads, upper 32 bits live in the head word.
frag_heads=[x for x in heads if x[3]==0 or x[0]==heads[-1][0]]
last=heads[-1]
fragmentoff=(last[3]<<32)|fragoff
print(f'target_fragment candidate_head={last} fragmentoff={fragmentoff}')

# Parse packed inode map header/index region according to its own layout.
p=pack
pbase=(p['off']+p['isize']+p['xsize']+7)&~7
ph=raw[pbase:pbase+16]
print(f'packed_map header={pbase} raw={ph.hex()} advise=0x{u16(pbase+4):x} data_word={p["word"]}')
print(f'packed first128={raw[pbase:pbase+128].hex()}')
print(f'packed_fragment_span start={fragmentoff} end={fragmentoff + (SIZE-65536)} packed_size={p["size"]}')
assert fragmentoff < p['size']
PY

echo 'Stage 39 fragment topology probe PASS'
