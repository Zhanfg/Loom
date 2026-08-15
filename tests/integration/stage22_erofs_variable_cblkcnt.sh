#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
ORIGIN_ROOT="$WORK/origin-root"
REPLACEMENT_ROOT="$WORK/replacement-root"
MOUNT_DIR="$WORK/mnt"
ORIGIN_LOOP=""
SHADOW_LOOP=""
MAPPER="loom-stage22-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then sudo umount "$MOUNT_DIR"; fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then sudo dmsetup remove "$MAPPER"; fi
  if [[ -n "$SHADOW_LOOP" ]]; then sudo losetup -d "$SHADOW_LOOP"; fi
  if [[ -n "$ORIGIN_LOOP" ]]; then sudo losetup -d "$ORIGIN_LOOP"; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$ORIGIN_ROOT" "$REPLACEMENT_ROOT" "$MOUNT_DIR"
ORIGINAL="$WORK/original.bin"
ORACLE_REPLACEMENT="$WORK/oracle-replacement.bin"
SELF_REPLACEMENT="$WORK/self-replacement.bin"
OVERFLOW="$WORK/overflow.bin"
ORIGIN_IMG="$WORK/origin.erofs"
REPLACEMENT_IMG="$WORK/replacement.erofs"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

# A repeated deterministic 10,000-byte random period yields a raw LZ4 stream above
# 8 KiB but below 12 KiB for this 32 KiB logical payload. With -C 16384, mkfs.erofs
# therefore emits one big extent whose CBLKCNT is 3 instead of the Stage 19/20 value 2.
python3 - "$ORIGINAL" "$ORACLE_REPLACEMENT" "$SELF_REPLACEMENT" "$OVERFLOW" <<'PY'
import random
import sys

def periodic(seed, marker):
    rng = random.Random(seed)
    period = bytes(rng.randrange(256) for _ in range(10000))
    data = bytearray((period * 4)[:32768])
    data[64:64 + len(marker)] = marker
    return data

def xorshift_payload():
    state = 0x5354_3232
    out = bytearray(32768)
    for i in range(len(out)):
        state ^= (state << 13) & 0xffffffff
        state ^= state >> 17
        state ^= (state << 5) & 0xffffffff
        state &= 0xffffffff
        out[i] = state & 0xff
    return out

open(sys.argv[1], 'wb').write(periodic(0x220001, b'LOOM-STAGE22-ORIGIN'))
open(sys.argv[2], 'wb').write(periodic(0x220002, b'LOOM-STAGE22-ORACLE'))
open(sys.argv[3], 'wb').write(periodic(0x220003, b'LOOM-STAGE22-SELF'))
open(sys.argv[4], 'wb').write(xorshift_payload())
PY

cp "$ORIGINAL" "$ORIGIN_ROOT/000payload.bin"
cp "$ORACLE_REPLACEMENT" "$REPLACEMENT_ROOT/000payload.bin"

# Force root directories onto ordinary data blocks so path traversal remains orthogonal
# to this proof.
for i in $(seq -w 0 499); do
  : > "$ORIGIN_ROOT/z_dummy_${i}_for_directory_growth"
  : > "$REPLACEMENT_ROOT/z_dummy_${i}_for_directory_growth"
done

mkfs.erofs -b 4096 -C 16384 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 32768 "$ORIGIN_IMG" "$ORIGIN_ROOT" >/dev/null
mkfs.erofs -b 4096 -C 16384 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 32768 "$REPLACEMENT_IMG" "$REPLACEMENT_ROOT" >/dev/null

fsck.erofs "$ORIGIN_IMG" >/dev/null
fsck.erofs "$REPLACEMENT_IMG" >/dev/null

# Independently prove the replacement image before Loom sees it.
REPLACEMENT_LOOP="$(sudo losetup --find --show --read-only "$REPLACEMENT_IMG")"
sudo mount -t erofs -o ro "$REPLACEMENT_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORACLE_REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo losetup -d "$REPLACEMENT_LOOP"

STOCK_HASH_BEFORE="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$ORIGIN_IMG")"

# Oracle path: a real CBLKCNT=3 replacement must compile to three shadow blocks.
ORACLE_OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --big-oracle \
    "$ORIGIN_IMG" /000payload.bin "$REPLACEMENT_IMG" \
    "$SHADOW" "$ORIGIN_LOOP" LOOM_SHADOW_PLACEHOLDER "$TABLE"
)"

echo "$ORACLE_OUTPUT" | grep -q 'mode=big-oracle'
echo "$ORACLE_OUTPUT" | grep -q 'encoded_bytes=12288'
echo "$ORACLE_OUTPUT" | grep -q 'logical_lclusters=8'
echo "$ORACLE_OUTPUT" | grep -q 'shadow_blocks=3'
[[ "$(stat -c %s "$SHADOW")" -eq 12288 ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORACLE_REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null
sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"
SHADOW_LOOP=""

STOCK_HASH_MID="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_MID" ]]

# Self-encode path: the Loom-owned raw LZ4 stream must occupy more than two blocks but
# fit inside the recovered three-block CBLKCNT footprint.
rm -f "$SHADOW" "$TABLE"
SELF_OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --big-encode \
    "$ORIGIN_IMG" /000payload.bin "$SELF_REPLACEMENT" \
    "$SHADOW" "$ORIGIN_LOOP" LOOM_SHADOW_PLACEHOLDER "$TABLE"
)"

echo "$SELF_OUTPUT" | grep -q 'mode=big-encode'
echo "$SELF_OUTPUT" | grep -q 'logical_lclusters=8'
echo "$SELF_OUTPUT" | grep -q 'shadow_blocks=3'
[[ "$(stat -c %s "$SHADOW")" -eq 12288 ]]

ENCODED_BYTES="$(echo "$SELF_OUTPUT" | sed -n 's/.*encoded_bytes=\([0-9][0-9]*\).*/\1/p')"
[[ -n "$ENCODED_BYTES" ]]
[[ "$ENCODED_BYTES" -gt 8192 ]]
[[ "$ENCODED_BYTES" -le 12288 ]]

python3 - "$SHADOW" "$ENCODED_BYTES" <<'PY'
import sys
span = open(sys.argv[1], 'rb').read()
encoded = int(sys.argv[2])
assert len(span) == 12288
start = len(span) - encoded
assert 0 < start < 4096, (start, encoded)
assert span[:start] == b'\x00' * start
assert span[start] != 0
assert any(span[4096:8192])
assert any(span[8192:])
PY

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$SELF_REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null
sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"
SHADOW_LOOP=""

STOCK_HASH_AFTER="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]

# Same-size incompressible data must still fail before either output artifact is written.
rm -f "$WORK/reject.shadow" "$WORK/reject.table" "$WORK/reject.out" "$WORK/reject.err"
if "$LOOM" erofs-compact-pcluster-swap --big-encode \
  "$ORIGIN_IMG" /000payload.bin "$OVERFLOW" \
  "$WORK/reject.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/reject.table" \
  >"$WORK/reject.out" 2>"$WORK/reject.err"; then
  echo 'Stage 22 expected >12288-byte encoded payload rejection' >&2
  exit 1
fi
grep -Eq 'does not fit existing pcluster|encoded .*capacity 12288' "$WORK/reject.err"
[[ ! -e "$WORK/reject.shadow" ]]
[[ ! -e "$WORK/reject.table" ]]

sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORIGINAL"
sudo umount "$MOUNT_DIR"

printf '%s\n' \
  'Stage 22 variable-CBLKCNT big-pcluster PASS' \
  '  logical bytes: 32768' \
  '  recovered CBLKCNT physical blocks: 3' \
  '  physical capacity: 12288 bytes' \
  "  Loom raw-LZ4 bytes: $ENCODED_BYTES" \
  '  encoded stream crosses two physical boundaries: yes' \
  '  oracle shadow blocks: 3' \
  '  self-encode shadow blocks: 3' \
  '  overflow side effects: none' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $STOCK_HASH_AFTER"
