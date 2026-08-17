#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli --bin loom-early-map
EARLY_MAP="$REPO_ROOT/target/release/loom-early-map"
FIEMAP="$REPO_ROOT/target/loom-fiemap-host"

gcc -std=c11 -O2 -Wall -Wextra -Werror \
  tools/loom-fiemap.c -o "$FIEMAP"

WORK="$(mktemp -d)"
MOUNTPOINT="$WORK/metadata-mnt"
ORIGIN_LOOP=""
SHADOW_LOOP=""
METADATA_LOOP=""
DM_REFERENCE="loom-alpha5-reference-${RANDOM}-${RANDOM}"
DM_EARLY="loom-alpha5-early-${RANDOM}-${RANDOM}"
MOUNTED=0

cleanup() {
  set +e
  for mapper in "$DM_EARLY" "$DM_REFERENCE"; do
    if sudo dmsetup info "$mapper" >/dev/null 2>&1; then
      sudo dmsetup remove "$mapper"
    fi
  done
  if [[ "$MOUNTED" == 1 ]]; then
    sudo umount "$MOUNTPOINT" >/dev/null 2>&1 || true
  fi
  for loop in "$METADATA_LOOP" "$SHADOW_LOOP" "$ORIGIN_LOOP"; do
    if [[ -n "$loop" ]]; then
      sudo losetup -d "$loop" >/dev/null 2>&1 || true
    fi
  done
  rm -rf "$WORK"
}
trap cleanup EXIT

ORIGIN="$WORK/origin.bin"
AGGREGATE="$WORK/aggregate-shadow.pack"
METADATA_IMAGE="$WORK/metadata.img"
FLAT_TABLE="$WORK/flat.table"
EXTENT_MAP="$WORK/shadow.extents"
EARLY_RAW="$WORK/early.raw.table"
EARLY_TABLE="$WORK/early.table"
REFERENCE_TABLE="$WORK/reference.table"
mkdir -p "$MOUNTPOINT"

# 64 sectors of deterministic authoritative origin and a 12-sector aggregate shadow.
python3 - "$ORIGIN" "$AGGREGATE" <<'PY'
import pathlib
import sys
origin, shadow = map(pathlib.Path, sys.argv[1:])
origin.write_bytes(bytes(((i * 29 + 11) & 0xff) for i in range(64 * 512)))
shadow.write_bytes(bytes([0xA5]) * (12 * 512))
PY
ORIGIN_SHA_BEFORE="$(sha256sum "$ORIGIN" | awk '{print $1}')"
SHADOW_SHA="$(sha256sum "$AGGREGATE" | awk '{print $1}')"

ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$ORIGIN")"
SHADOW_LOOP="$(sudo losetup --find --show --read-only "$AGGREGATE")"

# Reference flattened view: sectors [8,20) come from the aggregate shadow file.
cat >"$FLAT_TABLE" <<EOF
0 8 linear $ORIGIN_LOOP 0
8 12 linear $SHADOW_LOOP 0
20 44 linear $ORIGIN_LOOP 20
EOF
cp "$FLAT_TABLE" "$REFERENCE_TABLE"
sudo dmsetup create "$DM_REFERENCE" <"$REFERENCE_TABLE"
REFERENCE_PATH="/dev/mapper/$DM_REFERENCE"

# Build a stand-in ext4 /metadata partition and persist the same aggregate shadow into it.
truncate -s 64M "$METADATA_IMAGE"
mkfs.ext4 -q -F -m 0 "$METADATA_IMAGE"
METADATA_LOOP="$(sudo losetup --find --show "$METADATA_IMAGE")"
sudo mount -t ext4 "$METADATA_LOOP" "$MOUNTPOINT"
MOUNTED=1
sudo mkdir -p "$MOUNTPOINT/loom/early/g-test"
sudo cp "$AGGREGATE" "$MOUNTPOINT/loom/early/g-test/shadow.pack"
sudo chmod 0400 "$MOUNTPOINT/loom/early/g-test/shadow.pack"
sudo sync

# Capture the real physical extents while ext4 owns the file. The helper itself requires
# uninterrupted, sector-aligned, non-delalloc extents and rejects unsupported FIEMAP flags.
sudo "$FIEMAP" \
  "$MOUNTPOINT/loom/early/g-test/shadow.pack" \
  "$MOUNTPOINT/loom/early/g-test/shadow.extents"
sudo cp "$MOUNTPOINT/loom/early/g-test/shadow.extents" "$EXTENT_MAP"
sudo chown "$(id -u):$(id -g)" "$EXTENT_MAP"
[[ -s "$EXTENT_MAP" ]]

# Confirm that the persisted bytes are exact before leaving the filesystem view.
sudo sha256sum "$MOUNTPOINT/loom/early/g-test/shadow.pack" | grep -Fq "$SHADOW_SHA"
sudo umount "$MOUNTPOINT"
MOUNTED=0

# Convert the normal flat map from file-backed shadow offsets to raw /metadata sectors.
"$EARLY_MAP" \
  "$FLAT_TABLE" \
  "$ORIGIN_LOOP" \
  "$SHADOW_LOOP" \
  "$EXTENT_MAP" \
  "$EARLY_RAW"

# The prepared first-stage table must not retain the transient aggregate-shadow loop or any file path.
! grep -Fq "$SHADOW_LOOP" "$EARLY_RAW"
! grep -Fq 'shadow.pack' "$EARLY_RAW"
grep -Fq '__LOOM_ORIGIN__' "$EARLY_RAW"
grep -Fq '__LOOM_METADATA_DEVICE__' "$EARLY_RAW"

sed \
  -e "s|__LOOM_ORIGIN__|$ORIGIN_LOOP|g" \
  -e "s|__LOOM_METADATA_DEVICE__|$METADATA_LOOP|g" \
  "$EARLY_RAW" >"$EARLY_TABLE"
! grep -q '__LOOM_' "$EARLY_TABLE"

sudo dmsetup create "$DM_EARLY" <"$EARLY_TABLE"
EARLY_PATH="/dev/mapper/$DM_EARLY"

# Core proof: using only the verified-style origin device and raw metadata sectors must produce
# the same bytes as the normal flattened view that still references an aggregate-shadow loop.
sudo cmp "$REFERENCE_PATH" "$EARLY_PATH"

# Independent content assertions on the early raw-backed view.
sudo python3 - "$EARLY_PATH" <<'PY'
import pathlib
import sys
raw = pathlib.Path(sys.argv[1]).read_bytes()
assert len(raw) == 64 * 512
assert raw[8*512:20*512] == bytes([0xA5]) * (12*512)
PY

ORIGIN_SHA_AFTER="$(sha256sum "$ORIGIN" | awk '{print $1}')"
[[ "$ORIGIN_SHA_AFTER" == "$ORIGIN_SHA_BEFORE" ]]

printf '%s\n' \
  'Alpha 5 metadata raw-shadow PASS' \
  "  shadow bytes: $(stat -c %s "$AGGREGATE")" \
  "  metadata extents: $(wc -l <"$EXTENT_MAP" | tr -d ' ')" \
  "  early table extents: $(wc -l <"$EARLY_RAW" | tr -d ' ')" \
  '  first-stage shadow loop required: no' \
  '  VFS overlay required: no'
