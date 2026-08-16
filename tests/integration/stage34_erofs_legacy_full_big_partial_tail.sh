#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"
cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"; ROOT="$WORK/root"; MNT="$WORK/mnt"
IMG="$WORK/origin.erofs"; BAD_IMG="$WORK/bad.erofs"
ORIGINAL="$WORK/original.bin"; REPLACEMENT="$WORK/replacement.bin"
SHADOW="$WORK/shadow.pack"; TABLE="$WORK/loom.table"
BAD_SHADOW="$WORK/bad.shadow"; BAD_TABLE="$WORK/bad.table"
ORIGIN_LOOP=""; SHADOW_LOOP=""; MAPPER="loom-stage34-${RANDOM}-${RANDOM}"
cleanup() {
  set +e
  mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT"
  sudo dmsetup info "$MAPPER" >/dev/null 2>&1 && sudo dmsetup remove "$MAPPER"
  [[ -n "$SHADOW_LOOP" ]] && sudo losetup -d "$SHADOW_LOOP"
  [[ -n "$ORIGIN_LOOP" ]] && sudo losetup -d "$ORIGIN_LOOP"
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 34 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR
mkdir -p "$ROOT" "$MNT"

python3 - "$ORIGINAL" "$REPLACEMENT" <<'PY'
import random,sys
SIZE=98304-123; PERIOD=10000
def make(seed,marker):
    rng=random.Random(seed); p=bytes(rng.randrange(256) for _ in range(PERIOD))
    data=bytearray((p*((SIZE+PERIOD-1)//PERIOD))[:SIZE])
    data[64:64+len(marker)]=marker
    assert len(data)==SIZE
    return data
open(sys.argv[1],'wb').write(make(0x340001,b'LOOM-STAGE34-ORIGIN'))
open(sys.argv[2],'wb').write(make(0x340002,b'LOOM-STAGE34-REPLACEMENT'))
PY
cp "$ORIGINAL" "$ROOT/000payload.bin"
for i in $(seq -w 0 499); do : > "$ROOT/z_dummy_${i}"; done
mkfs.erofs -b 4096 -C 16384 -zlz4 -E legacy-compress,noinline_data -T 0 \
  --max-extent-bytes 32768 "$IMG" "$ROOT" >/dev/null
fsck.erofs "$IMG" >/dev/null
DUMP="$(dump.erofs -e --path=/000payload.bin "$IMG")"
printf '%s\n' "$DUMP"
echo "$DUMP" | grep -q 'Size: 98181'
echo "$DUMP" | grep -q 'On-disk size: 36864'
echo "$DUMP" | grep -q 'Layout: 1'
echo "$DUMP" | grep -q '/000payload.bin: 3 extents found'

python3 - "$IMG" <<'PY'
import stat,struct,sys
SIZE=98304-123; raw=open(sys.argv[1],'rb').read(); sb=1024
assert struct.unpack_from('<I',raw,sb)[0]==0xE0F5E1E2
assert struct.unpack_from('<I',raw,sb+0x50)[0]==0x2
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
    break
else: raise AssertionError('target not found')
assert struct.unpack_from('<H',header,4)[0]==0x2 and header[6]==0 and header[7]==0
assert blocks==9
heads=[]
for lcn in range(24):
    p=start+lcn*8; adv,cofs,word=struct.unpack_from('<HHI',raw,p); kind=adv&3
    assert adv & ~3 == 0
    if lcn==23:
        assert (kind,cofs,word)==(0,3973,0), (kind,cofs,word)
        continue
    assert cofs==0
    group=(lcn//8)*8; end=23 if group==16 else group+8
    if lcn in (0,8,16):
        assert kind==1; heads.append((lcn,word))
    elif lcn in (1,9,17):
        assert kind==2
        d0,d1=struct.unpack_from('<HH',raw,p+4)
        assert d0==0x0803
        assert d1==end-lcn, (lcn,d1,end-lcn)
    else:
        assert kind==2
        d0,d1=struct.unpack_from('<HH',raw,p+4)
        assert d0==lcn-group and d1==end-lcn, (lcn,d0,d1)
assert heads==[(0,2),(8,5),(16,8)], heads
print(f'Stage 34 raw full-big partial PASS heads={heads} CBLKCNT=[3,3,3] sentinel=(23,3973,0) data_word={blocks}')
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
echo "$OUTPUT" | grep -q 'physical_pclusters=3'
echo "$OUTPUT" | grep -q 'logical_lclusters=24'
echo "$OUTPUT" | grep -q 'head_lclusters=\[0, 8, 16\]'
echo "$OUTPUT" | grep -q 'origin_pclusters=\[2, 5, 8\]'
echo "$OUTPUT" | grep -q 'shadow_blocks=9'
[[ "$(stat -c %s "$SHADOW")" -eq 36864 ]]
ENCODED="$(echo "$OUTPUT" | sed -n 's/.*encoded_bytes=\[\([^]]*\)\].*/\1/p')"
IFS=',' read -r E0 E1 E2 <<< "$ENCODED"; E0="${E0// /}"; E1="${E1// /}"; E2="${E2// /}"
for n in "$E0" "$E1" "$E2"; do [[ "$n" -gt 8192 && "$n" -le 12288 ]]; done
python3 - "$SHADOW" "$E0" "$E1" "$E2" <<'PY'
import sys
raw=open(sys.argv[1],'rb').read(); sizes=[int(x) for x in sys.argv[2:]]
assert len(raw)==36864
for i,n in enumerate(sizes):
    span=raw[i*12288:(i+1)*12288]
    assert span[0]!=0 and span[n:]==b'\0'*(12288-n)
    assert any(span[4096:8192]) and any(span[8192:n])
print(f'Stage 34 LegacyStart partial spans PASS encoded={sizes}')
PY

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MNT"
sudo cmp "$MNT/000payload.bin" "$REPLACEMENT"
sudo umount "$MNT"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null
sudo dmsetup remove "$MAPPER"; sudo losetup -d "$SHADOW_LOOP"; SHADOW_LOOP=""
[[ "$HASH_BEFORE" == "$(sha256sum "$IMG" | awk '{print $1}')" ]]

# Corrupt the verified EOF sentinel offset. The parser must reject before outputs exist.
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
    assert struct.unpack_from('<HHI',raw,p)==(0,3973,0)
    struct.pack_into('<H',raw,p+2,3972); open(path,'wb').write(raw); break
else: raise AssertionError('target not found')
PY
rm -f "$BAD_SHADOW" "$BAD_TABLE"
if BAD_OUTPUT="$("$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$BAD_IMG" /000payload.bin "$REPLACEMENT" "$BAD_SHADOW" "$ORIGIN_LOOP" UNUSED "$BAD_TABLE" 2>&1)"; then BAD_STATUS=0; else BAD_STATUS=$?; fi
printf '%s\n' "$BAD_OUTPUT"
[[ "$BAD_STATUS" -ne 0 ]]
echo "$BAD_OUTPUT" | grep -q 'partial full-index file lacks the expected zero-block PLAIN EOF sentinel'
[[ ! -e "$BAD_SHADOW" && ! -e "$BAD_TABLE" ]]
HASH_AFTER="$(sha256sum "$IMG" | awk '{print $1}')"; [[ "$HASH_BEFORE" == "$HASH_AFTER" ]]

printf '%s\n' \
  'Stage 34 legacy full-index big-pcluster partial EOF PASS' \
  '  logical bytes: 98181' \
  '  HEAD lclusters: [0, 8, 16]' \
  '  physical pcluster starts: [2, 5, 8]' \
  '  CBLKCNT: [3, 3, 3]' \
  '  EOF sentinel: lcn=23 clusterofs=3973 blkaddr=0' \
  '  final D0_CBLKCNT delta1: 6' \
  "  Loom raw-LZ4 bytes: [$E0, $E1, $E2]" \
  '  shadow blocks: 9' \
  '  effective replacement + fsck.erofs: PASS' \
  '  malformed sentinel rejection before materialization: PASS' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $HASH_AFTER"
