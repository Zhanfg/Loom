#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"

WORK="$(mktemp -d)"
MOUNT_DIR="$WORK/mnt"
ORIGIN_LOOP=""
SHADOW_LOOP=""
MAPPER="loom-stage7-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then sudo umount "$MOUNT_DIR"; fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then sudo dmsetup remove "$MAPPER"; fi
  if [[ -n "$SHADOW_LOOP" ]]; then sudo losetup -d "$SHADOW_LOOP"; fi
  if [[ -n "$ORIGIN_LOOP" ]]; then sudo losetup -d "$ORIGIN_LOOP"; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$MOUNT_DIR"
STOCK="$WORK/stock.ext4"
PAYLOAD="$WORK/loom-created.bin"
CONTEXT="$WORK/context.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"
EFFECTIVE_CONTEXT="$WORK/effective-context.bin"

truncate -s 64M "$STOCK"
mkfs.ext4 -q -F -b 4096 -I 256 -O metadata_csum,64bit,extent,^bigalloc "$STOCK"
debugfs -w -R 'mkdir /system' "$STOCK" >/dev/null 2>&1
debugfs -w -R 'mkdir /system/etc' "$STOCK" >/dev/null 2>&1

set +e
e2fsck -fy "$STOCK" >/dev/null
FSCK_RC=$?
set -e
if (( FSCK_RC > 1 )); then
  echo "Stage 7 stock fixture e2fsck failed: rc=$FSCK_RC" >&2
  exit "$FSCK_RC"
fi

printf 'stage7-created-and-labeled\n' > "$PAYLOAD"
for i in $(seq 1 30); do printf 'txn_%02d=value_%02d\n' "$i" "$i" >> "$PAYLOAD"; done
[[ "$(stat -c %s "$PAYLOAD")" -lt 4096 ]]
printf 'u:object_r:system_file:s0\0' > "$CONTEXT"
[[ "$(stat -c %s "$CONTEXT")" -eq 26 ]]

STOCK_TARGET="$(debugfs -R 'stat /system/etc/txn.bin' "$STOCK" 2>&1 || true)"
grep -q 'File not found' <<<"$STOCK_TARGET"
STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"

COMPILE_OUTPUT="$(
  "$LOOM" ext4-create-selinux \
    "$STOCK" \
    /system/etc/txn.bin \
    "$PAYLOAD" \
    "$CONTEXT" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'value_bytes=26'
echo "$COMPILE_OUTPUT" | grep -q 'shadow_blocks=7'
[[ "$(stat -c %s "$SHADOW")" -eq $((7 * 4096)) ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"

sudo mount -t ext4 -o ro,noload "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/system/etc/txn.bin" "$PAYLOAD"
sudo python3 - "$MOUNT_DIR/system/etc/txn.bin" "$EFFECTIVE_CONTEXT" <<'PY'
import os
import sys
with open(sys.argv[2], "wb") as stream:
    stream.write(os.getxattr(sys.argv[1], b"security.selinux"))
PY
cmp "$EFFECTIVE_CONTEXT" "$CONTEXT"
sudo umount "$MOUNT_DIR"

sudo e2fsck -fn "/dev/mapper/$MAPPER" >/dev/null

STOCK_HASH_AFTER="$(sha256sum "$STOCK" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]
STOCK_TARGET_AFTER="$(debugfs -R 'stat /system/etc/txn.bin' "$STOCK" 2>&1 || true)"
grep -q 'File not found' <<<"$STOCK_TARGET_AFTER"

printf '%s\n' \
  'Stage 7 effective-view transaction PASS' \
  '  operations: CREATE + security.selinux' \
  '  shadow blocks: 7 (inode metadata collision coalesced)' \
  "  shadow bytes: $(stat -c %s "$SHADOW")" \
  "  origin sha256: $STOCK_HASH_AFTER"
