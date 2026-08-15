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
MAPPER="loom-stage26-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then sudo umount "$MOUNT_DIR"; fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then sudo dmsetup remove "$MAPPER"; fi
  if [[ -n "$SHADOW_LOOP" ]]; then sudo losetup -d "$SHADOW_LOOP"; fi
  if [[ -n "$ORIGIN_LOOP" ]]; then sudo losetup -d "$ORIGIN_LOOP"; fi
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'rc=$?; printf "Stage 26 FAIL line=%s status=%s command=%s\n" "$LINENO" "$rc" "$BASH_COMMAND" >&2; exit "$rc"' ERR

mkdir -p "$ORIGIN_ROOT" "$MOUNT_DIR"
ORIGINAL="$WORK/original.bin"
REPLACEMENT="$WORK/replacement.bin"
ORIGIN_IMG="$WORK/origin.erofs"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

python3 - "$ORIGINAL" "$REPLACEMENT" <<'PY'
import random
import sys

SIZE = 32768
PERIOD = 10000

def periodic(seed, marker):
    rng = random.Random(seed)
    period = bytes(rng.randrange(256) for _ in range(PERIOD))
    copies = (SIZE + PERIOD - 1) // PERIOD
    data = bytearray((period * copies)[:SIZE])
    data[64:64 + len(marker)] = marker
    return data

open(sys.argv[1], 'wb').write(periodic(0x260001, b'LOOM-STAGE26-ORIGIN'))
open(sys.argv[2], 'wb').write(periodic(0x260002, b'LOOM-STAGE26-REPLACEMENT'))
PY

cp "$ORIGINAL" "$ORIGIN_ROOT/000payload.bin"
for i in $(seq -w 0 499); do
  : > "$ORIGIN_ROOT/z_dummy_${i}_for_directory_growth"
done

# Both the traversed directory inode and the compressed target inode carry real xattrs.
# The values are checked on the stock mount and again after Loom overlays only data pclusters.
python3 - "$ORIGIN_ROOT" <<'PY'
import os
import sys

root = sys.argv[1]
os.setxattr(root, b'user.loom.stage26.root', b'root-xattr-preserved')
os.setxattr(
    os.path.join(root, '000payload.bin'),
    b'user.loom.stage26.target',
    b'target-xattr-preserved',
)
assert os.getxattr(root, b'user.loom.stage26.root') == b'root-xattr-preserved'
assert os.getxattr(
    os.path.join(root, '000payload.bin'),
    b'user.loom.stage26.target',
) == b'target-xattr-preserved'
PY

mkfs.erofs -b 4096 -C 16384 -zlz4 -E noinline_data -T 0 \
  --max-extent-bytes 32768 "$ORIGIN_IMG" "$ORIGIN_ROOT" >/dev/null
fsck.erofs "$ORIGIN_IMG" >/dev/null

STOCK_HASH_BEFORE="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$ORIGIN_IMG")"

# Prove mkfs actually materialized both xattrs before using the image as Stage 26 evidence.
sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORIGINAL"
sudo python3 - "$MOUNT_DIR" <<'PY'
import os
import sys

root = sys.argv[1]
assert os.getxattr(root, b'user.loom.stage26.root') == b'root-xattr-preserved'
assert os.getxattr(
    os.path.join(root, '000payload.bin'),
    b'user.loom.stage26.target',
) == b'target-xattr-preserved'
PY
sudo umount "$MOUNT_DIR"
printf '%s\n' 'Stage 26 stock xattr materialization PASS'

OUTPUT="$(
  "$LOOM" erofs-compact-pcluster-swap --big-encode \
    "$ORIGIN_IMG" /000payload.bin "$REPLACEMENT" \
    "$SHADOW" "$ORIGIN_LOOP" LOOM_SHADOW_PLACEHOLDER "$TABLE"
)"
printf '%s\n' "$OUTPUT"

echo "$OUTPUT" | grep -q 'mode=big-encode'
echo "$OUTPUT" | grep -q 'logical_lclusters=8'
echo "$OUTPUT" | grep -q 'shadow_blocks=3'
[[ "$(stat -c %s "$SHADOW")" -eq 12288 ]]
ENCODED_BYTES="$(echo "$OUTPUT" | sed -n 's/.*encoded_bytes=\([0-9][0-9]*\).*/\1/p')"
[[ -n "$ENCODED_BYTES" ]]
[[ "$ENCODED_BYTES" -gt 8192 ]]
[[ "$ENCODED_BYTES" -le 12288 ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$REPLACEMENT"
sudo python3 - "$MOUNT_DIR" <<'PY'
import os
import sys

root = sys.argv[1]
assert os.getxattr(root, b'user.loom.stage26.root') == b'root-xattr-preserved'
assert os.getxattr(
    os.path.join(root, '000payload.bin'),
    b'user.loom.stage26.target',
) == b'target-xattr-preserved'
PY
sudo umount "$MOUNT_DIR"
sudo fsck.erofs "/dev/mapper/$MAPPER" >/dev/null
printf '%s\n' 'Stage 26 effective data/xattr mount/fsck PASS'

STOCK_HASH_AFTER="$(sha256sum "$ORIGIN_IMG" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]

sudo dmsetup remove "$MAPPER"
sudo losetup -d "$SHADOW_LOOP"
SHADOW_LOOP=""

# Re-check the authoritative origin after the effective-view proof.
sudo mount -t erofs -o ro "$ORIGIN_LOOP" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/000payload.bin" "$ORIGINAL"
sudo python3 - "$MOUNT_DIR" <<'PY'
import os
import sys

root = sys.argv[1]
assert os.getxattr(root, b'user.loom.stage26.root') == b'root-xattr-preserved'
assert os.getxattr(
    os.path.join(root, '000payload.bin'),
    b'user.loom.stage26.target',
) == b'target-xattr-preserved'
PY
sudo umount "$MOUNT_DIR"

printf '%s\n' \
  'Stage 26 xattr-bearing compact EROFS PASS' \
  '  traversed directory xattr: preserved' \
  '  compressed target xattr: preserved' \
  '  logical bytes: 32768' \
  '  CBLKCNT physical blocks: 3' \
  "  Loom raw-LZ4 bytes: $ENCODED_BYTES" \
  '  effective data replacement: PASS' \
  '  effective xattr preservation: PASS' \
  '  effective fsck.erofs: PASS' \
  '  authoritative origin: unchanged' \
  "  origin sha256: $STOCK_HASH_AFTER"
