#!/usr/bin/env bash
set -euo pipefail

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/root"

python3 - "$WORK/root/000payload.bin" <<'PY'
import sys
size=98304
pat=b'LOOM-STAGE38-ZTAILPACKING-'
data=(pat*((size//len(pat))+1))[:size]
assert len(data)==size
open(sys.argv[1],'wb').write(data)
PY
for i in $(seq -w 0 499); do : > "$WORK/root/z_dummy_${i}_for_directory_growth"; done

mkfs.erofs -b 4096 -zlz4 -E legacy-compress,ztailpacking -T 0 \
  --max-extent-bytes 32768 "$WORK/probe.erofs" "$WORK/root" >/dev/null
fsck.erofs "$WORK/probe.erofs" >/dev/null
dump.erofs -e --path=/000payload.bin "$WORK/probe.erofs"

python3 - "$WORK/probe.erofs" <<'PY'
import stat,struct,sys
SIZE=98304
raw=open(sys.argv[1],'rb').read(); sb=1024
assert struct.unpack_from('<I',raw,sb)[0]==0xE0F5E1E2
print('STAGE38_SB incompat=',hex(struct.unpack_from('<I',raw,sb+0x50)[0]))
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
    print(f'STAGE38_TARGET nid={nid} inode_off={off} isize={isize} xsize={xsize} data_word={blocks} header_off={header} header={h.hex()}')
    for lcn in range(24):
        advise,co,word=struct.unpack_from('<HHI',raw,start+lcn*8)
        print(f'STAGE38_IDX lcn={lcn} advise=0x{advise:04x} type={advise&3} clusterofs={co} word=0x{word:08x} delta0=0x{word&0xffff:04x} delta1={word>>16}')
    full_end=start+24*8
    print('STAGE38_AFTER_INDEX offset=',full_end,'bytes=',raw[full_end:full_end+256].hex())
    break
else: raise AssertionError('target not found')
PY
