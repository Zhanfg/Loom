#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
ORD_ROOT="$WORK/ordinary-root"
BIG_ROOT="$WORK/big-root"
MOUNT_DIR="$WORK/mnt"
CURRENT_LOOP=""
CURRENT_SHADOW_LOOP=""
CURRENT_MAPPER=""

cleanup_current() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then sudo umount "$MOUNT_DIR"; fi
  if [[ -n "$CURRENT_MAPPER" ]] && sudo dmsetup info "$CURRENT_MAPPER" >/dev/null 2>&1; then
    sudo dmsetup remove "$CURRENT_MAPPER"
  fi
  if [[ -n "$CURRENT_SHADOW_LOOP" ]]; then sudo losetup -d "$CURRENT_SHADOW_LOOP"; fi
  if [[ -n "$CURRENT_LOOP" ]]; then sudo losetup -d "$CURRENT_LOOP"; fi
  CURRENT_MAPPER=""
  CURRENT_SHADOW_LOOP=""
  CURRENT_LOOP=""
}

cleanup() {
  cleanup_current
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 27 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR

mkdir -p "$ORD_ROOT" "$BIG_ROOT" "$MOUNT_DIR"
FILE_BYTES=$((24 * 4096 - 123))
TAIL_BYTES=$((FILE_BYTES - 23 * 4096))
[[ "$FILE_BYTES" -eq 98181 ]]
[[ "$TAIL_BYTES" -eq 3973 ]]

ORD_ORIGINAL="$WORK/ordinary-original.bin"
ORD_REPLACEMENT="$WORK/ordinary-replacement.bin"
BIG_ORIGINAL="$WORK/big-original.bin"
BIG_REPLACEMENT="$WORK/big-replacement.bin"
BIG_OVERFLOW="$WORK/big-overflow.bin"
ORD_IMG="$WORK/ordinary.erofs"
BIG_IMG="$WORK/big.erofs"

python3 - "$FILE_BYTES" "$ORD_ORIGINAL" "$ORD_REPLACEMENT" "$BIG_ORIGINAL" "$BIG_REPLACEMENT" <<'PY'
import random
import sys

size = int(sys.argv[1])

ordinary_a = bytearray(b'M' * size)
ordinary_b = bytearray(b'N' * size)
ordinary_a[64:64 + len(b'LOOM-STAGE27-ORDINARY-ORIGIN')] = b'LOOM-STAGE27-ORDINARY-ORIGIN'
ordinary_b[64:64 + len(b'LOOM-STAGE27-ORDINARY-REPLACEMENT')] = b'LOOM-STAGE27-ORDINARY-REPLACEMENT'
open(sys.argv[2], 'wb').write(ordinary_a)
open(sys.argv[3], 'wb').write(ordinary_b)

def periodic(seed, marker, period_bytes=10000):
    rng = random.Random(seed)
    period = bytes(rng.randrange(256) for _ in range(period_bytes))
    copies = (size + period_bytes - 1) // period_bytes
    data = bytearray((period * copies)[:size])
    data[64:64 + len(marker)] = marker
    return data

open(sys.argv[4], 'wb').write(periodic(0x270001, b'LOOM-STAGE27-BIG-ORIGIN'))
open(sys.argv[5], 'wb').write(periodic(0x270002, b'LOOM-STAGE27-BIG-REPLACEMENT'))
PY

cp "$ORD_ORIGINAL" "$ORD_ROOT/000payload.bin"
cp "$BIG_ORIGINAL" "$BIG_ROOT/000payload.bin"
for i in $(seq -w 0 499); do
  : > "$ORD_ROOT/z_dummy_${i}_for_directory_growth"
  : > "$BIG_ROOT/z_dummy_${i}_for_directory_growth"
done

mkfs.erofs -b 4096 -C 4096 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 32768 "$ORD_IMG" "$ORD_ROOT" >/dev/null
mkfs.erofs -b 4096 -C 16384 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 32768 "$BIG_IMG" "$BIG_ROOT" >/dev/null
fsck.erofs "$ORD_IMG" >/dev/null
fsck.erofs "$BIG_IMG" >/dev/null

ORD_HASH_BEFORE="$(sha256sum "$ORD_IMG" | awk '{print $1}')"
BIG_HASH_BEFORE="$(sha256sum "$BIG_IMG" | awk '{print $1}')"

# Ordinary compact: multiple one-block pclusters, with the final logical extent ending
# 123 bytes before the 24-lcluster ceiling.
CURRENT_LOOP="$(sudo losetup --find --show --read-only "$ORD_IMG")"
CURRENT_MAPPER="loom-stage27-ordinary-${RANDOM}-${RANDOM}"
ORD_SHADOW="$WORK/ordinary.shadow"
ORD_TABLE="$WORK/ordinary.table"

sudo mount -t erofs -o ro "$CURRENT_LOOP" "$MOUNT_DIR"
[[ "$(stat -c %s "$MOUNT_DIR/000payload.bin")" -eq "$FILE_BYTES" ]]
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORD_ORIGINAL"
sudo umount "$MOUNT_DIR"

ORD_OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --multi-encode \
    "$ORD_IMG" /000payload.bin "$ORD_REPLACEMENT" \
    "$ORD_SHADOW" "$CURRENT_LOOP" LOOM_SHADOW_PLACEHOLDER "$ORD_TABLE"
)"
printf '%s\n' "$ORD_OUTPUT"
echo "$ORD_OUTPUT" | grep -q 'mode=multi-encode'
echo "$ORD_OUTPUT" | grep -q 'logical_lclusters=24'

ORD_HEADS="$(printf '%s\n' "$ORD_OUTPUT" | sed -n 's/.*head_lclusters=\(\[[^]]*\]\).*/\1/p')"
ORD_ENCODED="$(printf '%s\n' "$ORD_OUTPUT" | sed -n 's/.*encoded_bytes=\(\[[^]]*\]\).*/\1/p')"
ORD_PCLUSTERS="$(printf '%s\n' "$ORD_OUTPUT" | sed -n 's/.*physical_pclusters=\([0-9][0-9]*\).*/\1/p')"
ORD_SHADOW_BLOCKS="$(printf '%s\n' "$ORD_OUTPUT" | sed -n 's/.*shadow_blocks=\([0-9][0-9]*\).*/\1/p')"
[[ -n "$ORD_HEADS" && -n "$ORD_ENCODED" && -n "$ORD_PCLUSTERS" && -n "$ORD_SHADOW_BLOCKS" ]]

python3 - "$FILE_BYTES" "$ORD_HEADS" "$ORD_ENCODED" "$ORD_PCLUSTERS" "$ORD_SHADOW_BLOCKS" <<'PY'
import ast
import sys
size = int(sys.argv[1])
heads = ast.literal_eval(sys.argv[2])
encoded = ast.literal_eval(sys.argv[3])
pclusters = int(sys.argv[4])
shadow_blocks = int(sys.argv[5])
assert len(heads) >= 3
assert heads[0] == 0
assert heads == sorted(heads)
assert len(encoded) == len(heads) == pclusters == shadow_blocks
last_start = heads[-1] * 4096
last_len = size - last_start
assert 0 < last_len <= 32768
assert last_len % 4096 == 3973
assert all(0 < n <= 4096 for n in encoded)
PY
[[ "$(stat -c %s "$ORD_SHADOW")" -eq $((ORD_SHADOW_BLOCKS * 4096)) ]]

CURRENT_SHADOW_LOOP="$(sudo losetup --find --show --read-only "$ORD_SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$CURRENT_SHADOW_LOOP|g" "$ORD_TABLE"
sudo dmsetup create "$CURRENT_MAPPER" < "$ORD_TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$CURRENT_MAPPER" "$MOUNT_DIR"
[[ "$(stat -c %s "$MOUNT_DIR/000payload.bin")" -eq "$FILE_BYTES" ]]
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORD_REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo fsck.erofs "/dev/mapper/$CURRENT_MAPPER" >/dev/null

ORD_HASH_AFTER="$(sha256sum "$ORD_IMG" | awk '{print $1}')"
[[ "$ORD_HASH_BEFORE" == "$ORD_HASH_AFTER" ]]
cleanup_current
printf '%s\n' 'Stage 27 ordinary compact partial-tail mount/fsck PASS'

# Big-pcluster: the same non-block-aligned logical EOF must truncate only the final
# logical extent while preserving its recovered physical CBLKCNT capacity.
CURRENT_LOOP="$(sudo losetup --find --show --read-only "$BIG_IMG")"
CURRENT_MAPPER="loom-stage27-big-${RANDOM}-${RANDOM}"
BIG_SHADOW="$WORK/big.shadow"
BIG_TABLE="$WORK/big.table"

sudo mount -t erofs -o ro "$CURRENT_LOOP" "$MOUNT_DIR"
[[ "$(stat -c %s "$MOUNT_DIR/000payload.bin")" -eq "$FILE_BYTES" ]]
sudo cmp "$MOUNT_DIR/000payload.bin" "$BIG_ORIGINAL"
sudo umount "$MOUNT_DIR"

BIG_OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --multi-encode \
    "$BIG_IMG" /000payload.bin "$BIG_REPLACEMENT" \
    "$BIG_SHADOW" "$CURRENT_LOOP" LOOM_SHADOW_PLACEHOLDER "$BIG_TABLE"
)"
printf '%s\n' "$BIG_OUTPUT"
echo "$BIG_OUTPUT" | grep -q 'mode=multi-encode'
echo "$BIG_OUTPUT" | grep -q 'logical_lclusters=24'

BIG_HEADS="$(printf '%s\n' "$BIG_OUTPUT" | sed -n 's/.*head_lclusters=\(\[[^]]*\]\).*/\1/p')"
BIG_ENCODED="$(printf '%s\n' "$BIG_OUTPUT" | sed -n 's/.*encoded_bytes=\(\[[^]]*\]\).*/\1/p')"
BIG_ORIGIN_PCLUSTERS="$(printf '%s\n' "$BIG_OUTPUT" | sed -n 's/.*origin_pclusters=\(\[[^]]*\]\).*/\1/p')"
BIG_SHADOW_BLOCKS="$(printf '%s\n' "$BIG_OUTPUT" | sed -n 's/.*shadow_blocks=\([0-9][0-9]*\).*/\1/p')"
[[ -n "$BIG_HEADS" && -n "$BIG_ENCODED" && -n "$BIG_ORIGIN_PCLUSTERS" && -n "$BIG_SHADOW_BLOCKS" ]]

BIG_LAST_HEAD="$(python3 - "$FILE_BYTES" "$BIG_HEADS" "$BIG_ENCODED" <<'PY'
import ast
import sys
size = int(sys.argv[1])
heads = ast.literal_eval(sys.argv[2])
encoded = ast.literal_eval(sys.argv[3])
assert len(heads) >= 3
assert heads[0] == 0
assert heads == sorted(heads)
assert len(encoded) == len(heads)
last_start = heads[-1] * 4096
last_len = size - last_start
assert 0 < last_len <= 32768
assert last_len % 4096 == 3973
assert all(n > 4096 for n in encoded)
print(heads[-1])
PY
)"
[[ -n "$BIG_LAST_HEAD" ]]
[[ "$BIG_SHADOW_BLOCKS" -ge 6 ]]
[[ "$(stat -c %s "$BIG_SHADOW")" -eq $((BIG_SHADOW_BLOCKS * 4096)) ]]

CURRENT_SHADOW_LOOP="$(sudo losetup --find --show --read-only "$BIG_SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$CURRENT_SHADOW_LOOP|g" "$BIG_TABLE"
sudo dmsetup create "$CURRENT_MAPPER" < "$BIG_TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$CURRENT_MAPPER" "$MOUNT_DIR"
[[ "$(stat -c %s "$MOUNT_DIR/000payload.bin")" -eq "$FILE_BYTES" ]]
sudo cmp "$MOUNT_DIR/000payload.bin" "$BIG_REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo fsck.erofs "/dev/mapper/$CURRENT_MAPPER" >/dev/null

BIG_HASH_AFTER="$(sha256sum "$BIG_IMG" | awk '{print $1}')"
[[ "$BIG_HASH_BEFORE" == "$BIG_HASH_AFTER" ]]
cleanup_current
printf '%s\n' 'Stage 27 big-pcluster partial-tail mount/fsck PASS'

# Final partial extent overflow remains transactional: all earlier extents fit, but the
# actual EOF-bounded tail is made incompressible and must fail before any artifact exists.
cp "$BIG_REPLACEMENT" "$BIG_OVERFLOW"
python3 - "$BIG_OVERFLOW" "$BIG_LAST_HEAD" <<'PY'
import sys
path = sys.argv[1]
start = int(sys.argv[2]) * 4096
data = bytearray(open(path, 'rb').read())
state = 0x2717A11
for i in range(start, len(data)):
    state ^= (state << 13) & 0xffffffff
    state ^= state >> 17
    state ^= (state << 5) & 0xffffffff
    state &= 0xffffffff
    data[i] = state & 0xff
open(path, 'wb').write(data)
PY

CURRENT_LOOP="$(sudo losetup --find --show --read-only "$BIG_IMG")"
rm -f "$WORK/overflow.shadow" "$WORK/overflow.table" "$WORK/overflow.err" "$WORK/overflow.out"
if "$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$BIG_IMG" /000payload.bin "$BIG_OVERFLOW" \
  "$WORK/overflow.shadow" "$CURRENT_LOOP" UNUSED "$WORK/overflow.table" \
  >"$WORK/overflow.out" 2>"$WORK/overflow.err"; then
  echo 'Stage 27 expected final partial-extent footprint rejection' >&2
  exit 1
fi
grep -q "HEAD lcluster $BIG_LAST_HEAD" "$WORK/overflow.err"
grep -q 'does not fit existing pcluster' "$WORK/overflow.err"
[[ ! -e "$WORK/overflow.shadow" ]]
[[ ! -e "$WORK/overflow.table" ]]
cleanup_current

printf '%s\n' \
  'Stage 27 partial final EROFS lcluster PASS' \
  "  logical bytes: $FILE_BYTES" \
  '  logical lclusters: 24 (ceil)' \
  "  final lcluster bytes: $TAIL_BYTES" \
  "  ordinary HEAD lclusters: $ORD_HEADS" \
  "  ordinary encoded bytes: $ORD_ENCODED" \
  "  big HEAD lclusters: $BIG_HEADS" \
  "  big origin pclusters: $BIG_ORIGIN_PCLUSTERS" \
  "  big encoded bytes: $BIG_ENCODED" \
  '  ordinary partial-tail mount/fsck: PASS' \
  '  big partial-tail mount/fsck: PASS' \
  '  final partial-extent overflow side effects: none' \
  '  authoritative origins: unchanged' \
  "  ordinary origin sha256: $ORD_HASH_AFTER" \
  "  big origin sha256: $BIG_HASH_AFTER"
