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
MAPPER="loom-stage5-${RANDOM}-${RANDOM}"

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
ORIGINAL="$WORK/remove.me"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

truncate -s 64M "$STOCK"
mkfs.ext4 -q -F -b 4096 -O metadata_csum,64bit,extent,^bigalloc "$STOCK"
debugfs -w -R 'mkdir /system' "$STOCK" >/dev/null 2>&1
debugfs -w -R 'mkdir /system/etc' "$STOCK" >/dev/null 2>&1

dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'R' > "$ORIGINAL"
printf 'LOOM-STAGE5-STOCK-FILE' | dd of="$ORIGINAL" bs=1 seek=64 conv=notrunc status=none
debugfs -w -R "write $ORIGINAL /system/etc/remove.me" "$STOCK" >/dev/null 2>&1

set +e
e2fsck -fy "$STOCK" >/dev/null
FSCK_RC=$?
set -e
if (( FSCK_RC > 1 )); then
  echo "Stage 5 stock fixture e2fsck failed: rc=$FSCK_RC" >&2
  exit "$FSCK_RC"
fi

TARGET_STAT="$(debugfs -R 'stat /system/etc/remove.me' "$STOCK" 2>/dev/null)"
grep -q 'Type: regular' <<<"$TARGET_STAT"
grep -Eq 'Links:[[:space:]]+1' <<<"$TARGET_STAT"
[[ "$(debugfs -R 'blocks /system/etc/remove.me' "$STOCK" 2>/dev/null | wc -w)" -eq 1 ]]

DUMPED_BEFORE="$WORK/origin-before.bin"
debugfs -R "dump /system/etc/remove.me $DUMPED_BEFORE" "$STOCK" >/dev/null 2>&1
cmp "$DUMPED_BEFORE" "$ORIGINAL"

STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"
STOCK_FREE_BLOCKS="$(dumpe2fs -h "$STOCK" 2>/dev/null | awk -F: '/^Free blocks:/ {gsub(/ /, "", $2); print $2}')"
STOCK_FREE_INODES="$(dumpe2fs -h "$STOCK" 2>/dev/null | awk -F: '/^Free inodes:/ {gsub(/ /, "", $2); print $2}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"

COMPILE_OUTPUT="$(
  "$LOOM" ext4-remove \
    "$STOCK" \
    /system/etc/remove.me \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'shadow_blocks=6'
[[ "$(stat -c %s "$SHADOW")" -eq $((6 * 4096)) ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"

sudo mount -t ext4 -o ro,noload "/dev/mapper/$MAPPER" "$MOUNT_DIR"
[[ -d "$MOUNT_DIR/system/etc" ]]
if sudo test -e "$MOUNT_DIR/system/etc/remove.me"; then
  echo 'effective filesystem still exposes removed path' >&2
  exit 1
fi
sudo umount "$MOUNT_DIR"

# Structural oracle for the final post-eviction state: directory entry, inode bitmap,
# block bitmap, group accounting and primary-superblock accounting/checksums must agree.
sudo e2fsck -fn "/dev/mapper/$MAPPER" >/dev/null

EFFECTIVE_FREE_BLOCKS="$(sudo dumpe2fs -h "/dev/mapper/$MAPPER" 2>/dev/null | awk -F: '/^Free blocks:/ {gsub(/ /, "", $2); print $2}')"
EFFECTIVE_FREE_INODES="$(sudo dumpe2fs -h "/dev/mapper/$MAPPER" 2>/dev/null | awk -F: '/^Free inodes:/ {gsub(/ /, "", $2); print $2}')"
[[ "$EFFECTIVE_FREE_BLOCKS" -eq $((STOCK_FREE_BLOCKS + 1)) ]]
[[ "$EFFECTIVE_FREE_INODES" -eq $((STOCK_FREE_INODES + 1)) ]]

STOCK_HASH_AFTER="$(sha256sum "$STOCK" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]
DUMPED_AFTER="$WORK/origin-after.bin"
debugfs -R "dump /system/etc/remove.me $DUMPED_AFTER" "$STOCK" >/dev/null 2>&1
cmp "$DUMPED_AFTER" "$ORIGINAL"

printf '%s\n' \
  'Stage 5 ext4 remove-file PASS' \
  "  free blocks: $STOCK_FREE_BLOCKS -> $EFFECTIVE_FREE_BLOCKS" \
  "  free inodes: $STOCK_FREE_INODES -> $EFFECTIVE_FREE_INODES" \
  "  shadow bytes: $(stat -c %s "$SHADOW")" \
  "  origin sha256: $STOCK_HASH_AFTER"
