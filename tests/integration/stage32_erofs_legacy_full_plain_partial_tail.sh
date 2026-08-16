#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"
cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
ROOT="$WORK/root"; MNT="$WORK/mnt"
IMG="$WORK/origin.erofs"; BAD_IMG="$WORK/bad.erofs"
ORIGINAL="$WORK/original.bin"; REPLACEMENT="$WORK/replacement.bin"
SHADOW="$WORK/shadow.pack"; TABLE="$WORK/loom.table"
BAD_SHADOW="$WORK/bad.shadow"; BAD_TABLE="$WORK/bad.table"
ORIGIN_LOOP=""; SHADOW_LOOP=""; MAPPER="loom-stage32-${RANDOM}-${RANDOM}"
cleanup() {
  set +e
  mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT"
  sudo dmsetup info "$MAPPER" >/dev/null 2>&1 && sudo dmsetup remove "$MAPPER"
  [[ -n "$SHADOW_LOOP" ]] && sudo losetup -d "$SHADOW_LOOP"
  [[ -n "$ORIGIN_LOOP" ]] && sudo losetup -d "$ORIGIN_LOOP"
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 32 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR
mkdir -p "$ROOT" "$MNT"

python3 - "$ORIGINAL" "$REPLACEMENT" <<'PY'
import random, sys
SIZE=98304-123; PREFIX=65536
def build(seed, tag):
    rng=random.Random(seed)
    pat=tag+b'-COMPRESSIBLE-'
    prefix=(pat*((PREFIX//len(pat))+1))[:PREFIX]
    tail=bytes(rng.randrange(256) for _ in range(SIZE-PREFIX))
    return prefix+tail
open(sys.argv[1],'wb').write(build(0x320001,b'LOOM-STAGE32-ORIGIN'))
open(sys.argv[2],'wb').write(build(0x320002,b'LOOM-STAGE32-REPLACEMENT'))
PY
cp "$ORIGINAL" "$ROOT/000payload.bin"
for i in $(seq -w 0 499); do : > "$ROOT/z_dummy_${i}_for_directory_growth"; done
mkfs.erofs -b 4096 -C 4096 -zlz4 -E legacy-compress,noinline_data -T 0 \
  --max-extent-bytes 32768 "$IMG" "$ROOT" >/dev/null
fsck.erofs "$IMG" >/dev/null
DUMP="$(dump.erofs -e --path=/000payload.bin "$IMG")"
printf '%s\n' "$DUMP"
echo "$DUMP" | grep -q 'Size: 98181'
echo "$DUMP" | grep -q 'On-disk size: 40960'
echo "$DUMP" | grep -q 'Layout: 1'
echo "$DUMP" | grep -q '/000payload.bin: 10 extents found'

python3 - "$IMG" <<'PY'
import stat, struct, sys
SIZE=98304-123
raw=open(sys.argv[1],'rb').read(); sb=1024
assert struct.unpack_from('<I',raw,sb)[0]==0xE0F5E1E2
assert struct.unpack_from('<I',raw,sb+0x50)[0]==0
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
    start=((off+isize+xsize+7)&~7)+16
    break
else: raise AssertionError('target not found')
heads=[]; plain=[]
for lcn in range(24):
    p=start+lcn*8; advise,cofs,word=struct.unpack_from('<HHI',raw,p); kind=advise&3
    assert advise & ~3 == 0
    if kind in (0,1): heads.append((lcn,kind,cofs,word))
    if kind==0: plain.append((lcn,cofs,word))
expected=[(0,1,0,1),(8,1,0,2)]+[(lcn,0,0,lcn-13) for lcn in range(16,24)]
assert heads==expected, heads
assert plain==[(lcn,0,lcn-13) for lcn in range(16,24)], plain
assert blocks==10
assert plain[-1]==(23,0,10)
print(f'Stage 32 raw partial PLAIN topology PASS heads={heads} data_word={blocks}')
PY

HASH_BEFORE="$(sha256sum "$IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$IMG")"
sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MNT"
sudo cmp "$MNT/000payload.bin" "$ORIGINAL"
sudo umount "$MNT"

OUTPUT="$("$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$IMG" /000payload.bin "$REPLACEMENT" "$SHADOW" "$ORIGIN_LOOP" \
  LOOM_SHADOW_PLACEHOLDER "$TABLE")"
printf '%s\n' "$OUTPUT"
echo "$OUTPUT" | grep -q 'physical_pclusters=10'
echo "$OUTPUT" | grep -q 'logical_lclusters=24'
echo "$OUTPUT" | grep -q 'head_lclusters=\[0, 8, 16, 17, 18, 19, 20, 21, 22, 23\]'
echo "$OUTPUT" | grep -q 'origin_pclusters=\[1, 2, 3, 4, 5, 6, 7, 8, 9, 10\]'
echo "$OUTPUT" | grep -q 'shadow_blocks=10'
[[ "$(stat -c %s "$SHADOW")" -eq 40960 ]]
ENCODED="$(printf '%s\n' "$OUTPUT" | sed -n 's/.*encoded_bytes=\(\[[^]]*\]\).*/\1/p')"
python3 - "$ENCODED" <<'PY'
import ast,sys
v=ast.literal_eval(sys.argv[1]); assert len(v)==10,v
assert 0<v[0]<4096 and 0<v[1]<4096,v
assert v[2:9]==[4096]*7,v
assert v[9]==3973,v
print(f'Stage 32 encoded partial raw tail PASS encoded={v}')
PY

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MNT"
sudo cmp "$MNT/000payload.bin" "$REPLACEMENT"
sudo umount "$MNT"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null
HASH_AFTER="$(sha256sum "$IMG" | awk '{print $1}')"; [[ "$HASH_BEFORE" == "$HASH_AFTER" ]]

# Corrupt the final raw PLAIN tail so it is neither a valid raw head nor a
# valid Stage 30 zero-block EOF sentinel. Reject before output materialization.
cp "$IMG" "$BAD_IMG"
python3 - "$BAD_IMG" <<'PY'
import stat,struct,sys
SIZE=98304-123; path=sys.argv[1]; raw=bytearray(open(path,'rb').read()); sb=1024
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
    xsize=0 if xcnt==0 else 12+(xcnt-1)*4
    p=((off+isize+xsize+7)&~7)+16+23*8
    assert struct.unpack_from('<HHI',raw,p)==(0,0,10)
    struct.pack_into('<H',raw,p+2,1)
    open(path,'wb').write(raw); break
else: raise AssertionError('target not found')
PY
rm -f "$BAD_SHADOW" "$BAD_TABLE"
if BAD_OUTPUT="$("$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$BAD_IMG" /000payload.bin "$REPLACEMENT" "$BAD_SHADOW" "$ORIGIN_LOOP" \
  UNUSED "$BAD_TABLE" 2>&1)"; then BAD_STATUS=0; else BAD_STATUS=$?; fi
printf '%s\n' "$BAD_OUTPUT"
[[ "$BAD_STATUS" -ne 0 ]]
echo "$BAD_OUTPUT" | grep -q 'partial full-index PLAIN tail is neither a zero-block EOF sentinel nor an aligned raw data head'
[[ ! -e "$BAD_SHADOW" && ! -e "$BAD_TABLE" ]]

printf '%s\n' \
  'Stage 32 legacy full-index partial PLAIN raw tail PASS' \
  '  logical bytes: 98181' \
  '  HEAD1 lclusters: [0, 8]' \
  '  PLAIN data lclusters: [16, 17, 18, 19, 20, 21, 22, 23]' \
  '  final PLAIN logical bytes: 3973; physical bytes: 4096' \
  '  physical pclusters: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]' \
  '  effective partial raw-tail replacement: PASS' \
  '  effective fsck.erofs: PASS' \
  '  malformed final PLAIN classification rejection before materialization: PASS' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $HASH_AFTER"
