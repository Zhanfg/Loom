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
MAPPER="loom-stage6-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if mountpoint -q "$MOUNT_DIR" 2>/dev/null; then
    sudo umount "$MOUNT_DIR"
  fi
  if sudo dmsetup info "$MAPPER" >/dev/null 2>&1; then
    sudo dmsetup remove "$MAPPER"
  fi
  if [[ -n "$SHADOW_LOOP" ]]; then
    sudo losetup -d "$SHADOW_LOOP"
  fi
  if [[ -n "$ORIGIN_LOOP" ]]; then
    sudo losetup -d "$ORIGIN_LOOP"
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$MOUNT_DIR"
STOCK="$WORK/stock.ext4"
ORIGINAL="$WORK/target.bin"
CONTEXT="$WORK/context.bin"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"
EFFECTIVE_CONTEXT="$WORK/effective-context.bin"

truncate -s 64M "$STOCK"
mkfs.ext4 -q -F -b 4096 -I 256 -O metadata_csum,64bit,extent,^bigalloc "$STOCK"
debugfs -w -R 'mkdir /system' "$STOCK" >/dev/null 2>&1
debugfs -w -R 'mkdir /system/etc' "$STOCK" >/dev/null 2>&1

dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'S' > "$ORIGINAL"
printf 'LOOM-STAGE6-XATTR' | dd of="$ORIGINAL" bs=1 seek=32 conv=notrunc status=none
debugfs -w -R "write $ORIGINAL /system/etc/target.bin" "$STOCK" >/dev/null 2>&1

set +e
e2fsck -fy "$STOCK" >/dev/null
FSCK_RC=$?
set -e
if (( FSCK_RC > 1 )); then
  echo "Stage 6 stock fixture e2fsck failed: rc=$FSCK_RC" >&2
  exit "$FSCK_RC"
fi

# Include the Android-style trailing NUL explicitly; Loom treats xattr values as raw bytes.
printf 'u:object_r:system_file:s0\0' > "$CONTEXT"
[[ "$(stat -c %s "$CONTEXT")" -eq 26 ]]

STOCK_EA="$(debugfs -R 'ea_list /system/etc/target.bin' "$STOCK" 2>/dev/null || true)"
if grep -q 'security.selinux' <<<"$STOCK_EA"; then
  echo 'stock fixture unexpectedly already contains security.selinux' >&2
  exit 1
fi

STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"

COMPILE_OUTPUT="$(
  "$LOOM" ext4-selinux \
    "$STOCK" \
    /system/etc/target.bin \
    "$CONTEXT" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'value_bytes=26'
echo "$COMPILE_OUTPUT" | grep -q 'shadow_blocks=1'
[[ "$(stat -c %s "$SHADOW")" -eq 4096 ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"

sudo mount -t ext4 -o ro,noload "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/system/etc/target.bin" "$ORIGINAL"
sudo python3 - "$MOUNT_DIR/system/etc/target.bin" "$EFFECTIVE_CONTEXT" <<'PY'
import os
import sys
value = os.getxattr(sys.argv[1], b"security.selinux")
with open(sys.argv[2], "wb") as stream:
    stream.write(value)
PY
cmp "$EFFECTIVE_CONTEXT" "$CONTEXT"
sudo umount "$MOUNT_DIR"

# The xattr must be structurally valid to both the native ext4 driver and e2fsck.
sudo e2fsck -fn "/dev/mapper/$MAPPER" >/dev/null

STOCK_HASH_AFTER="$(sha256sum "$STOCK" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]
STOCK_EA_AFTER="$(debugfs -R 'ea_list /system/etc/target.bin' "$STOCK" 2>/dev/null || true)"
! grep -q 'security.selinux' <<<"$STOCK_EA_AFTER"

printf '%s\n' \
  'Stage 6 ext4 security.selinux PASS' \
  "  value bytes: $(stat -c %s "$CONTEXT")" \
  "  shadow bytes: $(stat -c %s "$SHADOW")" \
  "  origin sha256: $STOCK_HASH_AFTER"
