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
MAPPER="loom-stage4-${RANDOM}-${RANDOM}"

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
PAYLOAD="$WORK/loom.conf"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"

truncate -s 64M "$STOCK"
mkfs.ext4 -q -F -b 4096 -O metadata_csum,64bit,extent,^bigalloc "$STOCK"
debugfs -w -R 'mkdir /system' "$STOCK" >/dev/null 2>&1
debugfs -w -R 'mkdir /system/etc' "$STOCK" >/dev/null 2>&1

set +e
e2fsck -fy "$STOCK" >/dev/null
FSCK_RC=$?
set -e
if (( FSCK_RC > 1 )); then
  echo "Stage 4 stock fixture e2fsck failed: rc=$FSCK_RC" >&2
  exit "$FSCK_RC"
fi

printf 'loom-stage4-created\n' > "$PAYLOAD"
for i in $(seq 1 40); do printf 'key_%02d=value_%02d\n' "$i" "$i" >> "$PAYLOAD"; done
[[ "$(stat -c %s "$PAYLOAD")" -lt 4096 ]]

debugfs -R 'stat /system/etc' "$STOCK" 2>/dev/null | grep -q 'Type: directory'
STOCK_TARGET_STAT="$(debugfs -R 'stat /system/etc/loom.conf' "$STOCK" 2>&1 || true)"
if ! grep -q 'File not found' <<<"$STOCK_TARGET_STAT"; then
  echo 'stock unexpectedly already contains target path' >&2
  printf '%s\n' "$STOCK_TARGET_STAT" >&2
  exit 1
fi

STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"
STOCK_FREE_BLOCKS="$(dumpe2fs -h "$STOCK" 2>/dev/null | awk -F: '/^Free blocks:/ {gsub(/ /, "", $2); print $2}')"
STOCK_FREE_INODES="$(dumpe2fs -h "$STOCK" 2>/dev/null | awk -F: '/^Free inodes:/ {gsub(/ /, "", $2); print $2}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"

COMPILE_OUTPUT="$(
  "$LOOM" ext4-create \
    "$STOCK" \
    /system/etc/loom.conf \
    "$PAYLOAD" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'shadow_blocks=7'
[[ "$(stat -c %s "$SHADOW")" -eq $((7 * 4096)) ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"

sudo mount -t ext4 -o ro,noload "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/system/etc/loom.conf" "$PAYLOAD"
[[ "$(sudo stat -c %s "$MOUNT_DIR/system/etc/loom.conf")" -eq "$(stat -c %s "$PAYLOAD")" ]]
[[ "$(sudo stat -c %a "$MOUNT_DIR/system/etc/loom.conf")" == 644 ]]
sudo umount "$MOUNT_DIR"

sudo e2fsck -fn "/dev/mapper/$MAPPER" >/dev/null

EFFECTIVE_FREE_BLOCKS="$(sudo dumpe2fs -h "/dev/mapper/$MAPPER" 2>/dev/null | awk -F: '/^Free blocks:/ {gsub(/ /, "", $2); print $2}')"
EFFECTIVE_FREE_INODES="$(sudo dumpe2fs -h "/dev/mapper/$MAPPER" 2>/dev/null | awk -F: '/^Free inodes:/ {gsub(/ /, "", $2); print $2}')"
[[ "$EFFECTIVE_FREE_BLOCKS" -eq $((STOCK_FREE_BLOCKS - 1)) ]]
[[ "$EFFECTIVE_FREE_INODES" -eq $((STOCK_FREE_INODES - 1)) ]]

STOCK_HASH_AFTER="$(sha256sum "$STOCK" | awk '{print $1}')"
[[ "$STOCK_HASH_BEFORE" == "$STOCK_HASH_AFTER" ]]
STOCK_TARGET_AFTER="$(debugfs -R 'stat /system/etc/loom.conf' "$STOCK" 2>&1 || true)"
grep -q 'File not found' <<<"$STOCK_TARGET_AFTER"

printf '%s\n' \
  'Stage 4 ext4 create-file PASS' \
  "  payload bytes: $(stat -c %s "$PAYLOAD")" \
  "  free blocks: $STOCK_FREE_BLOCKS -> $EFFECTIVE_FREE_BLOCKS" \
  "  free inodes: $STOCK_FREE_INODES -> $EFFECTIVE_FREE_INODES" \
  "  shadow bytes: $(stat -c %s "$SHADOW")" \
  "  origin sha256: $STOCK_HASH_AFTER"
