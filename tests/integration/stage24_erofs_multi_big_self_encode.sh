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
MAPPER="loom-stage24-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then sudo umount "$MOUNT_DIR"; fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then sudo dmsetup remove "$MAPPER"; fi
  if [[ -n "$SHADOW_LOOP" ]]; then sudo losetup -d "$SHADOW_LOOP"; fi
  if [[ -n "$ORIGIN_LOOP" ]]; then sudo losetup -d "$ORIGIN_LOOP"; fi
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 24 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR

mkdir -p "$ORIGIN_ROOT" "$MOUNT_DIR"
ORIGINAL="$WORK/original.bin"
REPLACEMENT="$WORK/replacement.bin"
OVERFLOW="$WORK/overflow.bin"
ORIGIN_IMG="$WORK/origin.erofs"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

python3 - "$ORIGINAL" "$REPLACEMENT" "$OVERFLOW" <<'PY'
import random
import sys

EXTENT = 32768
PERIOD = 10000

def periodic(seed, marker):
    rng = random.Random(seed)
    period = bytes(rng.randrange(256) for _ in range(PERIOD))
    part = bytearray((period * 4)[:EXTENT])
    part[64:64 + len(marker)] = marker
    return part

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

origin = bytearray()
replacement = bytearray()
overflow = bytearray()
for extent in range(3):
    origin.extend(periodic(0x240100 + extent, f'LOOM-STAGE24-ORIGIN-{extent}'.encode()))
    replacement.extend(periodic(0x240200 + extent, f'LOOM-STAGE24-SELF-{extent}'.encode()))
    if extent == 1:
        overflow.extend(incompressible(0x240300 + extent))
    else:
        overflow.extend(periodic(0x240400 + extent, f'LOOM-STAGE24-FIT-{extent}'.encode()))

open(sys.argv[1], 'wb').write(origin)
open(sys.argv[2], 'wb').write(replacement)
open(sys.argv[3], 'wb').write(overflow)
PY

cp "$ORIGINAL" "$ORIGIN_ROOT/000payload.bin"
for i in $(seq -w 0 499); do
  : > "$ORIGIN_ROOT/z_dummy_${i}_for_directory_growth"
done

mkfs.erofs -b 4096 -C 16384 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 32768 "$ORIGIN_IMG" "$ORIGIN_ROOT" >/dev/null
fsck.erofs "$ORIGIN_IMG" >/dev/null

STOCK_HASH_BEFORE="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$ORIGIN_IMG")"

OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --multi-encode \
    "$ORIGIN_IMG" /000payload.bin "$REPLACEMENT" \
    "$SHADOW" "$ORIGIN_LOOP" LOOM_SHADOW_PLACEHOLDER "$TABLE"
)"
printf '%s\n' "$OUTPUT"

echo "$OUTPUT" | grep -q 'mode=multi-encode'
echo "$OUTPUT" | grep -q 'physical_pclusters=3'
echo "$OUTPUT" | grep -q 'logical_lclusters=24'
echo "$OUTPUT" | grep -q 'head_lclusters=\[0, 8, 16\]'
echo "$OUTPUT" | grep -q 'shadow_blocks=9'
[[ "$(stat -c %s "$SHADOW")" -eq 36864 ]]

ENCODED_LIST="$(echo "$OUTPUT" | sed -n 's/.*encoded_bytes=\[\([^]]*\)\].*/\1/p')"
[[ -n "$ENCODED_LIST" ]]
IFS=',' read -r E0 E1 E2 <<< "$ENCODED_LIST"
E0="${E0// /}"
E1="${E1// /}"
E2="${E2// /}"
for encoded in "$E0" "$E1" "$E2"; do
  [[ "$encoded" -gt 8192 ]]
  [[ "$encoded" -le 12288 ]]
done

python3 - "$SHADOW" "$E0" "$E1" "$E2" <<'PY'
import sys
span = open(sys.argv[1], 'rb').read()
encoded = [int(v) for v in sys.argv[2:]]
assert len(span) == 3 * 12288
for i, n in enumerate(encoded):
    extent = span[i * 12288:(i + 1) * 12288]
    start = len(extent) - n
    assert 0 < start < 4096, (i, start, n)
    assert extent[:start] == b'\x00' * start
    assert extent[start] != 0
    assert any(extent[4096:8192])
    assert any(extent[8192:])
PY

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo umount "$MOUNT_DIR"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null
sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"
SHADOW_LOOP=""

STOCK_HASH_MID="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_MID" ]]

# The middle logical extent is intentionally incompressible. The encoder must finish
# validating extent 0, reject HEAD lcluster 8, and still write neither artifact.
rm -f "$WORK/reject.shadow" "$WORK/reject.table" "$WORK/reject.out" "$WORK/reject.err"
if "$LOOM" erofs-compact-pcluster-swap --multi-encode \
  "$ORIGIN_IMG" /000payload.bin "$OVERFLOW" \
  "$WORK/reject.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/reject.table" \
  >"$WORK/reject.out" 2>"$WORK/reject.err"; then
  echo 'Stage 24 expected later big extent footprint rejection' >&2
  exit 1
fi
grep -q 'HEAD lcluster 8' "$WORK/reject.err"
grep -q 'capacity 12288' "$WORK/reject.err"
[[ ! -e "$WORK/reject.shadow" ]]
[[ ! -e "$WORK/reject.table" ]]

# Scalar big-encode stays a compatibility surface for exactly one big extent.
rm -f "$WORK/scalar.shadow" "$WORK/scalar.table" "$WORK/scalar.out" "$WORK/scalar.err"
if "$LOOM" erofs-compact-pcluster-swap --big-encode \
  "$ORIGIN_IMG" /000payload.bin "$REPLACEMENT" \
  "$WORK/scalar.shadow" "$ORIGIN_LOOP" UNUSED "$WORK/scalar.table" \
  >"$WORK/scalar.out" 2>"$WORK/scalar.err"; then
  echo 'Stage 24 expected scalar big-encode rejection for multi-extent topology' >&2
  exit 1
fi
grep -q 'unexpected single-extent topology' "$WORK/scalar.err"
[[ ! -e "$WORK/scalar.shadow" ]]
[[ ! -e "$WORK/scalar.table" ]]

STOCK_HASH_AFTER="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]

sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORIGINAL"
sudo umount "$MOUNT_DIR"

printf '%s\n' \
  'Stage 24 multi big-pcluster self-encode PASS' \
  '  logical bytes: 98304' \
  '  logical lclusters: 24' \
  '  HEAD lclusters: [0, 8, 16]' \
  '  recovered CBLKCNT blocks: [3, 3, 3]' \
  "  Loom raw-LZ4 bytes: [$E0, $E1, $E2]" \
  '  every stream crosses two physical boundaries: yes' \
  '  physical shadow blocks: 9' \
  '  later-extent overflow side effects: none' \
  '  scalar big-encode multi-extent rejection: PASS' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $STOCK_HASH_AFTER"
