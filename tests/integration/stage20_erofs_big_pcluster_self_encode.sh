#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
ORIGIN_ROOT="$WORK/origin-root"
MOUNT_DIR="$WORK/mnt"
ORIGIN_LOOP=""
SHADOW_LOOP=""
MAPPER="loom-stage20-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then sudo umount "$MOUNT_DIR"; fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then sudo dmsetup remove "$MAPPER"; fi
  if [[ -n "$SHADOW_LOOP" ]]; then sudo losetup -d "$SHADOW_LOOP"; fi
  if [[ -n "$ORIGIN_LOOP" ]]; then sudo losetup -d "$ORIGIN_LOOP"; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$ORIGIN_ROOT" "$MOUNT_DIR"
ORIGINAL="$WORK/original.bin"
REPLACEMENT="$WORK/replacement.bin"
OVERFLOW="$WORK/overflow.bin"
ORIGIN_IMG="$WORK/origin.erofs"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

# The origin and positive replacement use different deterministic 6000-byte periods.
# For a 32 KiB logical payload this produces a raw LZ4 stream larger than 4 KiB but
# smaller than 8 KiB, so the Stage 20 stream necessarily crosses the physical block
# boundary inside the existing two-block big pcluster.
# The overflow payload is deterministic pseudo-random data and is expected to encode
# above 8 KiB, exercising fail-closed capacity handling before EffectiveBlockStore opens.
python3 - "$ORIGINAL" "$REPLACEMENT" "$OVERFLOW" <<'PY'
import random
import sys

def periodic(seed, marker):
    rng = random.Random(seed)
    period = bytes(rng.randrange(256) for _ in range(6000))
    data = bytearray((period * 6)[:32768])
    data[64:64 + len(marker)] = marker
    return data

def xorshift_payload():
    state = 0x5354_3230
    out = bytearray(32768)
    for i in range(len(out)):
        state ^= (state << 13) & 0xffffffff
        state ^= state >> 17
        state ^= (state << 5) & 0xffffffff
        state &= 0xffffffff
        out[i] = state & 0xff
    return out

open(sys.argv[1], 'wb').write(periodic(0x200001, b'LOOM-STAGE20-ORIGIN'))
open(sys.argv[2], 'wb').write(periodic(0x200002, b'LOOM-STAGE20-REPLACEMENT'))
open(sys.argv[3], 'wb').write(xorshift_payload())
PY

cp "$ORIGINAL" "$ORIGIN_ROOT/000payload.bin"
mkfs.erofs -b 4096 -C 8192 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 32768 "$ORIGIN_IMG" "$ORIGIN_ROOT" >/dev/null
fsck.erofs "$ORIGIN_IMG" >/dev/null

STOCK_HASH_BEFORE="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$ORIGIN_IMG")"

COMPILE_OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --big-encode \
    "$ORIGIN_IMG" \
    /000payload.bin \
    "$REPLACEMENT" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'mode=big-encode'
echo "$COMPILE_OUTPUT" | grep -q 'logical_lclusters=8'
echo "$COMPILE_OUTPUT" | grep -q 'shadow_blocks=2'
[[ "$(stat -c %s "$SHADOW")" -eq 8192 ]]

ENCODED_BYTES="$(echo "$COMPILE_OUTPUT" | sed -n 's/.*encoded_bytes=\([0-9][0-9]*\).*/\1/p')"
[[ -n "$ENCODED_BYTES" ]]
[[ "$ENCODED_BYTES" -gt 4096 ]]
[[ "$ENCODED_BYTES" -le 8192 ]]

# Verify the physical layout itself, not only the reported size: the prefix is zero,
# the first non-zero byte begins in physical block 0, and the encoded stream continues
# through physical block 1. This proves the 0padding span crosses the 4 KiB boundary.
python3 - "$SHADOW" "$ENCODED_BYTES" <<'PY'
import sys
span = open(sys.argv[1], 'rb').read()
encoded = int(sys.argv[2])
assert len(span) == 8192
start = len(span) - encoded
assert 0 < start < 4096, (start, encoded)
assert span[:start] == b'\x00' * start
assert span[start] != 0
assert any(span[4096:])
PY

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"

sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null

STOCK_HASH_AFTER="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]

sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"
SHADOW_LOOP=""

sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORIGINAL"
sudo umount "$MOUNT_DIR"

# A same-size payload whose raw LZ4 stream exceeds the existing 8192-byte CBLKCNT
# footprint must fail before the CLI writes either artifact.
rm -f "$WORK/reject.shadow" "$WORK/reject.table" "$WORK/reject.out" "$WORK/reject.err"
if "$LOOM" erofs-compact-pcluster-swap --big-encode \
  "$ORIGIN_IMG" /000payload.bin "$OVERFLOW" \
  "$WORK/reject.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/reject.table" \
  >"$WORK/reject.out" 2>"$WORK/reject.err"; then
  echo 'Stage 20 expected >8192-byte encoded payload rejection' >&2
  exit 1
fi
grep -Eq 'does not fit existing big pcluster|does not fit existing pcluster|encoded .*capacity 8192' "$WORK/reject.err"
[[ ! -e "$WORK/reject.shadow" ]]
[[ ! -e "$WORK/reject.table" ]]

printf '%s\n' \
  'Stage 20 compact big-pcluster self-encode PASS' \
  '  logical bytes: 32768' \
  '  CBLKCNT physical capacity: 8192 bytes' \
  "  Loom raw-LZ4 bytes: $ENCODED_BYTES" \
  '  encoded stream crosses 4 KiB boundary: yes' \
  '  overflow side effects: none' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $STOCK_HASH_AFTER"