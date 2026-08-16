#!/usr/bin/env bash
set -euo pipefail

WORK="$(mktemp -d)"
ROOT="$WORK/root"; IMG="$WORK/shared-partial.erofs"; MNT="$WORK/mnt"
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
SIZE=98181
for path,pat in [(sys.argv[1],b'STAGE43-ALPHA-PARTIAL-'),(sys.argv[2],b'STAGE43-BETA-PARTIAL-')]:
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
[[ -n "$NID_A" && -n "$NID_B" ]]

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
assert u32(sb)==0xE0F5E1E2 and u32(sb+0x50)==0x20
meta=u32(sb+0x28); packed_nid=u64(sb+0x60); assert packed_nid

def inode(nid):
    off=meta*BS+nid*32
    fmt=u16(off); ext=fmt&1; layout=(fmt>>1)&7; isize=64 if ext else 32
    mode=u16(off+4); size=u64(off+8) if ext else u32(off+8); xcnt=u16(off+2)
    xsize=0 if xcnt==0 else 12+(xcnt-1)*4
    return dict(nid=nid,off=off,layout=layout,isize=isize,mode=mode,size=size,xsize=xsize,word=u32(off+0x10))

def parse_full(x,lclusters):
    h=(x['off']+x['isize']+x['xsize']+7)&~7
    low=u32(h); advice=u16(h+4); a0=raw[h+6]; a1=raw[h+7]; lbits=raw[h+8]
    entries=[]; heads=[]; start=h+16
    for lcn in range(lclusters):
        adv,co,word=struct.unpack_from('<HHI',raw,start+lcn*8); kind=adv&3
        assert adv & ~3 == 0,(x['nid'],lcn,adv)
        entries.append((lcn,kind,co,word))
        if kind in (0,1): heads.append((lcn,kind,co,word))
    return h,low,advice,a0,a1,lbits,heads,entries

frags=[]
for nid in target_nids:
    x=inode(nid); assert stat.S_IFMT(x['mode'])==stat.S_IFREG and x['size']==98181 and x['layout']==1,x
    h,low,adv,a0,a1,lbits,heads,entries=parse_full(x,24)
    assert adv==0x30 and a0==0 and a1==0 and lbits==0 and x['word']==2
    # Final PLAIN is EOF sentinel, fragment HEAD is final LZ4 HEAD at LCN16.
    assert entries[23]==(23,0,3973,0),entries[23]
    lz4_heads=[v for v in heads if v[1]==1]
    assert [v[0] for v in lz4_heads]==[0,8,16],lz4_heads
    frag=lz4_heads[-1]
    fragoff=(frag[3]<<32)|low
    fraglen=x['size']-frag[0]*BS
    frags.append((nid,fragoff,fraglen))
    print(f'target nid={nid} header={h} fragmentoff={fragoff} fragment_len={fraglen} lz4_heads={lz4_heads} eof={entries[23]}')

packed=inode(packed_nid); assert stat.S_IFMT(packed['mode'])==stat.S_IFREG and packed['layout']==1,packed
plclusters=(packed['size']+BS-1)//BS
ph,plow,padv,pa0,pa1,plbits,pheads,pentries=parse_full(packed,plclusters)
assert plow==0 and padv==0x10 and pa0==0 and pa1==0 and plbits==0
# Packed EOF sentinel may be present on the final partial logical cluster.
print(f'packed nid={packed_nid} size={packed["size"]} data_word={packed["word"]} header={ph} lclusters={plclusters}')
print(f'packed heads={pheads}')
print(f'packed entries={pentries}')
print(f'fragments={frags}')

# Report overlap of each fragment range against packed logical HEAD intervals.
data_heads=[v for v in pheads if v[1]==1]
for i,h in enumerate(data_heads):
    start=h[0]*BS
    end=(data_heads[i+1][0]*BS if i+1<len(data_heads) else packed['size'])
    print(f'packed_extent index={i} logical=[{start},{end}) pblk={h[3]}')
    for nid,fo,fl in frags:
        ov=max(0,min(end,fo+fl)-max(start,fo))
        if ov:
            print(f'overlap target_nid={nid} packed_extent={i} bytes={ov} extent_range=[{start},{end}) fragment_range=[{fo},{fo+fl})')

assert sorted((fo,fl) for _,fo,fl in frags)==[(0,32645),(32645,32645)]
assert packed['size']==65290,packed
# The boundary 32645 is intentionally not a 4-KiB boundary.
assert 32645 % BS != 0
PY

echo 'Stage 43 shared-partial fragment overlap probe PASS'
