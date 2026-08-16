#!/usr/bin/env bash
set -euo pipefail
WORK="$(mktemp -d)"; ROOT="$WORK/root"; IMG="$WORK/mixed.erofs"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$ROOT"
python3 - "$ROOT/000payload.bin" <<'PY'
import random,sys
CHUNK=32768
rng=random.Random(0x350001)
first=(b'LOOM-STAGE35-COMPRESS-A-'*((CHUNK//24)+2))[:CHUNK]
middle=bytes(rng.randrange(256) for _ in range(CHUNK))
last=(b'LOOM-STAGE35-COMPRESS-B-'*((CHUNK//24)+2))[:CHUNK]
data=first+middle+last
assert len(data)==98304
open(sys.argv[1],'wb').write(data)
PY
for i in $(seq -w 0 499); do : > "$ROOT/z_dummy_${i}"; done
mkfs.erofs -b 4096 -C 16384 -zlz4 -E legacy-compress,noinline_data -T 0 --max-extent-bytes 32768 "$IMG" "$ROOT" >/dev/null
fsck.erofs "$IMG" >/dev/null
dump.erofs -e --path=/000payload.bin "$IMG"
python3 - "$IMG" <<'PY'
import stat,struct,sys
SIZE=98304; raw=open(sys.argv[1],'rb').read(); sb=1024
print('STAGE35_SB incompat=',hex(struct.unpack_from('<I',raw,sb+0x50)[0]))
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
 h=((off+isize+xsize+7)&~7); header=raw[h:h+16]; start=h+16
 print(f'STAGE35_TARGET nid={nid} data_word={blocks} header={header.hex()}')
 for lcn in range(24):
  p=start+lcn*8; adv,cofs,word=struct.unpack_from('<HHI',raw,p); d0,d1=struct.unpack_from('<HH',raw,p+4)
  print(f'STAGE35_IDX lcn={lcn} advise=0x{adv:04x} type={adv&3} clusterofs={cofs} word=0x{word:08x} delta0=0x{d0:04x} delta1={d1}')
 break
else: raise AssertionError('target not found')
PY
