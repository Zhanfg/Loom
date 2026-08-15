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
ORIGINAL_REPLACE="$WORK/original-replace.bin"
REPLACEMENT="$WORK/replacement.bin"
REMOVE_FILE="$WORK/remove.me"
PAYLOAD="$WORK/payload.bin"
CONTEXT="$WORK/context.bin"
PLAN="$WORK/plan.tsv"
SHADOW="$WORK/shadow.pack"
TABLE="$WORK/loom.table"
EFFECTIVE_CONTEXT="$WORK/effective-context.bin"

truncate -s 64M "$STOCK"
mkfs.ext4 -q -F -b 4096 -I 256 -O metadata_csum,64bit,extent,^bigalloc "$STOCK"
debugfs -w -R 'mkdir /system' "$STOCK" >/dev/null 2>&1
debugfs -w -R 'mkdir /system/etc' "$STOCK" >/dev/null 2>&1

dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'O' > "$ORIGINAL_REPLACE"
cp "$ORIGINAL_REPLACE" "$REPLACEMENT"
printf 'LOOM-STAGE7-REPLACED' | dd of="$REPLACEMENT" bs=1 seek=128 conv=notrunc status=none

dd if=/dev/zero bs=4096 count=1 status=none | tr '\000' 'D' > "$REMOVE_FILE"
printf 'LOOM-STAGE7-REMOVE' | dd of="$REMOVE_FILE" bs=1 seek=64 conv=notrunc status=none
printf 'loom-stage7-created\n' > "$PAYLOAD"
for i in $(seq 1 20); do printf 'txn_%02d=value_%02d\n' "$i" "$i" >> "$PAYLOAD"; done
printf 'u:object_r:system_file:s0\0' > "$CONTEXT"

debugfs -w -R "write $ORIGINAL_REPLACE /system/etc/replace.bin" "$STOCK" >/dev/null 2>&1
debugfs -w -R "write $REMOVE_FILE /system/etc/remove.me" "$STOCK" >/dev/null 2>&1

set +e
e2fsck -fy "$STOCK" >/dev/null
FSCK_RC=$?
set -e
if (( FSCK_RC > 1 )); then
  echo "Stage 7 stock fixture e2fsck failed: rc=$FSCK_RC" >&2
  exit "$FSCK_RC"
fi

cat > "$PLAN" <<'EOF'
CREATE	/system/etc/loom.conf	payload.bin
SELINUX	/system/etc/loom.conf	context.bin
REPLACE	/system/etc/replace.bin	replacement.bin
REMOVE	/system/etc/remove.me
EOF

STOCK_HASH_BEFORE="$(sha256sum "$STOCK" | awk '{print $1}')"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$STOCK")"

COMPILE_OUTPUT="$(
  "$LOOM" ext4-transaction \
    "$STOCK" \
    "$PLAN" \
    "$SHADOW" \
    "$ORIGIN_LOOP" \
    LOOM_SHADOW_PLACEHOLDER \
    "$TABLE"
)"

echo "$COMPILE_OUTPUT" | grep -q 'operations=4'
CHANGED_SECTORS="$(sed -n 's/.*changed_sectors=\([0-9][0-9]*\).*/\1/p' <<<"$COMPILE_OUTPUT")"
[[ -n "$CHANGED_SECTORS" && "$CHANGED_SECTORS" -gt 0 ]]
[[ "$(stat -c %s "$SHADOW")" -eq $((CHANGED_SECTORS * 512)) ]]

SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"

sudo mount -t ext4 -o ro,noload "/dev/mapper/$MAPPER" "$MOUNT_DIR"
sudo cmp "$MOUNT_DIR/system/etc/loom.conf" "$PAYLOAD"
sudo cmp "$MOUNT_DIR/system/etc/replace.bin" "$REPLACEMENT"
if sudo test -e "$MOUNT_DIR/system/etc/remove.me"; then
  echo 'effective transaction still exposes removed file' >&2
  exit 1
fi
sudo python3 - "$MOUNT_DIR/system/etc/loom.conf" "$EFFECTIVE_CONTEXT" <<'PY'
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

debugfs -R 'stat /system/etc/loom.conf' "$STOCK" 2>&1 | grep -q 'File not found'
DUMP_REMOVE="$WORK/stock-remove.bin"
DUMP_REPLACE="$WORK/stock-replace.bin"
debugfs -R "dump /system/etc/remove.me $DUMP_REMOVE" "$STOCK" >/dev/null 2>&1
debugfs -R "dump /system/etc/replace.bin $DUMP_REPLACE" "$STOCK" >/dev/null 2>&1
cmp "$DUMP_REMOVE" "$REMOVE_FILE"
cmp "$DUMP_REPLACE" "$ORIGINAL_REPLACE"

printf '%s\n' \
  'Stage 7 ext4 transaction PASS' \
  '  operations: 4' \
  "  changed sectors: $CHANGED_SECTORS" \
  "  shadow bytes: $(stat -c %s "$SHADOW")" \
  "  origin sha256: $STOCK_HASH_AFTER"
