#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
ORIGIN_ROOT="$WORK/origin-root"
REPLACEMENT_ROOT="$WORK/replacement-root"
MISMATCH_ROOT="$WORK/mismatch-root"
MOUNT_DIR="$WORK/mnt"
ORIGIN_LOOP=""
SHADOW_LOOP=""
MAPPER="loom-stage25-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then sudo umount "$MOUNT_DIR"; fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then sudo dmsetup remove "$MAPPER"; fi
  if [[ -n "$SHADOW_LOOP" ]]; then sudo losetup -d "$SHADOW_LOOP"; fi
  if [[ -n "$ORIGIN_LOOP" ]]; then sudo losetup -d "$ORIGIN_LOOP"; fi
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 25 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR

mkdir -p "$ORIGIN_ROOT" "$REPLACEMENT_ROOT" "$MISMATCH_ROOT" "$MOUNT_DIR"
ORIGINAL="$WORK/original.bin"
REPLACEMENT="$WORK/replacement.bin"
MISMATCH="$WORK/mismatch.bin"
OVERFLOW="$WORK/overflow.bin"
ORIGIN_IMG="$WORK/origin.erofs"
REPLACEMENT_IMG="$WORK/replacement.erofs"
MISMATCH_IMG="$WORK/mismatch.erofs"
ORACLE_SHADOW="$WORK/oracle.shadow"
ORACLE_TABLE="$WORK/oracle.table"
SELF_SHADOW="$WORK/self.shadow"
SELF_TABLE="$WORK/self.table"

python3 - "$ORIGINAL" "$REPLACEMENT" "$MISMATCH" "$OVERFLOW" <<'PY'
import random
import sys

EXTENT = 32768
PERIODS = [6000, 10000, 14000]
MISMATCH_PERIODS = [6000, 6000, 14000]

def periodic(seed, marker, period_bytes):
    rng = random.Random(seed)
    period = bytes(rng.randrange(256) for _ in range(period_bytes))
    copies = (EXTENT + period_bytes - 1) // period_bytes
    part = bytearray((period * copies)[:EXTENT])
    part[64:64 + len(marker)] = marker
    return part

def build(seed_base, marker, periods):
    data = bytearray()
    for extent, period_bytes in enumerate(periods):
        data.extend(periodic(
            seed_base + extent,
            marker + str(extent).encode(),
            period_bytes,
        ))
    return data

def incompressible(seed):
    state = seed & 0xffffffff
    out = bytearray(EXTENT)
    for i in range(EXTENT):
        state ^= (state << 13) & 0xffffffff
        state ^= state >> 17
        state ^= (state << 5) & 0xffffffff
        state &= 0xffffffff
        out[i] = state & 0xff
    return out

open(sys.argv[1], 'wb').write(build(0x250100, b'LOOM-STAGE25-ORIGIN-', PERIODS))
open(sys.argv[2], 'wb').write(build(0x250200, b'LOOM-STAGE25-REPLACEMENT-', PERIODS))
open(sys.argv[3], 'wb').write(build(0x250300, b'LOOM-STAGE25-MISMATCH-', MISMATCH_PERIODS))
overflow = bytearray()
overflow.extend(periodic(0x250401, b'LOOM-STAGE25-FIT-0', PERIODS[0]))
overflow.extend(periodic(0x250402, b'LOOM-STAGE25-FIT-1', PERIODS[1]))
overflow.extend(incompressible(0x250403))
open(sys.argv[4], 'wb').write(overflow)
PY

cp "$ORIGINAL" "$ORIGIN_ROOT/000payload.bin"
cp "$REPLACEMENT" "$REPLACEMENT_ROOT/000payload.bin"
cp "$MISMATCH" "$MISMATCH_ROOT/000payload.bin"
for i in $(seq -w 0 499); do
  : > "$ORIGIN_ROOT/z_dummy_${i}_for_directory_growth"
  : > "$REPLACEMENT_ROOT/z_dummy_${i}_for_directory_growth"
  : > "$MISMATCH_ROOT/z_dummy_${i}_for_directory_growth"
done

for pair in \
  "$ORIGIN_IMG:$ORIGIN_ROOT" \
  "$REPLACEMENT_IMG:$REPLACEMENT_ROOT" \
  "$MISMATCH_IMG:$MISMATCH_ROOT"; do
  image="${pair%%:*}"
  root="${pair#*:}"
  mkfs.erofs -b 4096 -C 16384 -zlz4 -E noinline_data -T 0 \
    --max-extent-bytes 32768 "$image" "$root" >/dev/null
  fsck.erofs "$image" >/dev/null
done

STOCK_HASH_BEFORE="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$ORIGIN_IMG")"

# Oracle proof: mixed compressibility must recover one 2-block, one 3-block and one
# 4-block big extent. The later HEAD pclusters must therefore advance by +2 then +3.
ORACLE_OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --multi \
    "$ORIGIN_IMG" /000payload.bin "$REPLACEMENT_IMG" \
    "$ORACLE_SHADOW" "$ORIGIN_LOOP" LOOM_SHADOW_PLACEHOLDER "$ORACLE_TABLE"
)"
printf '%s\n' "$ORACLE_OUTPUT"

echo "$ORACLE_OUTPUT" | grep -q 'mode=multi'
echo "$ORACLE_OUTPUT" | grep -q 'physical_pclusters=3'
echo "$ORACLE_OUTPUT" | grep -q 'logical_lclusters=24'
echo "$ORACLE_OUTPUT" | grep -q 'head_lclusters=\[0, 8, 16\]'
echo "$ORACLE_OUTPUT" | grep -q 'origin_pclusters=\[1, 3, 6\]'
echo "$ORACLE_OUTPUT" | grep -q 'replacement_pclusters=\[1, 3, 6\]'
echo "$ORACLE_OUTPUT" | grep -q 'encoded_bytes=\[8192, 12288, 16384\]'
echo "$ORACLE_OUTPUT" | grep -q 'shadow_blocks=9'
[[ "$(stat -c %s "$ORACLE_SHADOW")" -eq 36864 ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$ORACLE_SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$ORACLE_TABLE"
sudo dmsetup create "$MAPPER" < "$ORACLE_TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null
sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"
SHADOW_LOOP=""
printf '%s\n' 'Stage 25 mixed-CBLKCNT oracle mount/fsck PASS'

# Self-encode proof over the same heterogeneous capacities.
SELF_OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --multi-encode \
    "$ORIGIN_IMG" /000payload.bin "$REPLACEMENT" \
    "$SELF_SHADOW" "$ORIGIN_LOOP" LOOM_SHADOW_PLACEHOLDER "$SELF_TABLE"
)"
printf '%s\n' "$SELF_OUTPUT"

echo "$SELF_OUTPUT" | grep -q 'mode=multi-encode'
echo "$SELF_OUTPUT" | grep -q 'physical_pclusters=3'
echo "$SELF_OUTPUT" | grep -q 'head_lclusters=\[0, 8, 16\]'
echo "$SELF_OUTPUT" | grep -q 'origin_pclusters=\[1, 3, 6\]'
echo "$SELF_OUTPUT" | grep -q 'shadow_blocks=9'
[[ "$(stat -c %s "$SELF_SHADOW")" -eq 36864 ]]

ENCODED_LIST="$(echo "$SELF_OUTPUT" | sed -n 's/.*encoded_bytes=\[\([^]]*\)\].*/\1/p')"
[[ -n "$ENCODED_LIST" ]]
IFS=',' read -r E0 E1 E2 <<< "$ENCODED_LIST"
E0="${E0// /}"
E1="${E1// /}"
E2="${E2// /}"
[[ "$E0" -gt 4096 && "$E0" -le 8192 ]]
[[ "$E1" -gt 8192 && "$E1" -le 12288 ]]
[[ "$E2" -gt 12288 && "$E2" -le 16384 ]]

python3 - "$SELF_SHADOW" "$E0" "$E1" "$E2" <<'PY'
import sys

span = open(sys.argv[1], 'rb').read()
encoded = [int(v) for v in sys.argv[2:]]
capacities = [8192, 12288, 16384]
offset = 0
for i, (n, capacity) in enumerate(zip(encoded, capacities)):
    extent = span[offset:offset + capacity]
    assert len(extent) == capacity
    start = capacity - n
    assert 0 < start < 4096, (i, start, n, capacity)
    assert extent[:start] == b'\x00' * start
    assert extent[start] != 0
    for boundary in range(4096, capacity, 4096):
        assert any(extent[boundary:boundary + 4096]), (i, boundary)
    offset += capacity
assert offset == len(span) == 36864
PY

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SELF_SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$SELF_TABLE"
sudo dmsetup create "$MAPPER" < "$SELF_TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null
sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"
SHADOW_LOOP=""
printf '%s\n' 'Stage 25 mixed-CBLKCNT self-encode mount/fsck PASS'

# Same HEAD topology but only the middle extent changes from 3 blocks to 2. The oracle
# must reject the per-extent footprint vector, not merely compare total physical blocks.
rm -f "$WORK/mismatch.shadow" "$WORK/mismatch.table" "$WORK/mismatch.out" "$WORK/mismatch.err"
if "$LOOM" erofs-compact-pcluster-swap --multi \
  "$ORIGIN_IMG" /000payload.bin "$MISMATCH_IMG" \
  "$WORK/mismatch.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/mismatch.table" \
  >"$WORK/mismatch.out" 2>"$WORK/mismatch.err"; then
  echo 'Stage 25 expected isolated middle-extent CBLKCNT mismatch rejection' >&2
  exit 1
fi
grep -Eq 'incompatible compact replacement: .*big-pcluster|big-pcluster .* differs' "$WORK/mismatch.err"
[[ ! -e "$WORK/mismatch.shadow" ]]
[[ ! -e "$WORK/mismatch.table" ]]

# The first two extents fit their 8 KiB and 12 KiB capacities. The final extent is
# incompressible and must fail at HEAD lcluster 16 before EffectiveBlockStore materializes.
rm -f "$WORK/overflow.shadow" "$WORK/overflow.table" "$WORK/overflow.out" "$WORK/overflow.err"
if "$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$ORIGIN_IMG" /000payload.bin "$OVERFLOW" \
  "$WORK/overflow.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/overflow.table" \
  >"$WORK/overflow.out" 2>"$WORK/overflow.err"; then
  echo 'Stage 25 expected final mixed-CBLKCNT extent overflow rejection' >&2
  exit 1
fi
grep -q 'HEAD lcluster 16' "$WORK/overflow.err"
grep -q 'capacity 16384' "$WORK/overflow.err"
[[ ! -e "$WORK/overflow.shadow" ]]
[[ ! -e "$WORK/overflow.table" ]]

STOCK_HASH_AFTER="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]
sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORIGINAL"
sudo umount "$MOUNT_DIR"

printf '%s\n' \
  'Stage 25 heterogeneous CBLKCNT multi-big PASS' \
  '  logical bytes: 98304' \
  '  HEAD lclusters: [0, 8, 16]' \
  '  recovered CBLKCNT blocks: [2, 3, 4]' \
  '  reconstructed pclusters: [1, 3, 6]' \
  '  physical capacities: [8192, 12288, 16384]' \
  "  Loom raw-LZ4 bytes: [$E0, $E1, $E2]" \
  '  oracle effective mount/fsck: PASS' \
  '  self-encode effective mount/fsck: PASS' \
  '  isolated middle-footprint mismatch rejection: PASS' \
  '  final-extent overflow side effects: none' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $STOCK_HASH_AFTER"
