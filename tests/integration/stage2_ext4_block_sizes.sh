#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

run_case() {
  local block_size="$1"
  local original_size="$2"
  local grown_size="$3"
  local case_dir="$WORK/bs-$block_size"
  local stock="$case_dir/stock.ext4"
  local original="$case_dir/original.bin"
  local grown="$case_dir/grown.bin"
  local shadow="$case_dir/shadow.pack"
  local table="$case_dir/loom.table"
  local mount_dir="$case_dir/mnt"
  local mapper="loom-stage2-bs-${block_size}-${RANDOM}-${RANDOM}"
  local origin_loop=""
  local shadow_loop=""

  mkdir -p "$case_dir" "$mount_dir"

  cleanup_case() {
    set +e
    if mountpoint -q "$mount_dir" 2>/dev/null; then
      sudo umount "$mount_dir"
    fi
    if sudo dmsetup info "$mapper" >/dev/null 2>&1; then
      sudo dmsetup remove "$mapper"
    fi
    if [[ -n "$shadow_loop" ]]; then
      sudo losetup -d "$shadow_loop"
    fi
    if [[ -n "$origin_loop" ]]; then
      sudo losetup -d "$origin_loop"
    fi
    set -e
  }

  truncate -s 64M "$stock"
  mkfs.ext4 -q -F -b "$block_size" "$stock"

  dd if=/dev/zero bs="$original_size" count=1 status=none | tr '\000' 'A' > "$original"
  printf 'LOOM-S2-BS-%s-STOCK' "$block_size" | dd of="$original" bs=1 seek=7 conv=notrunc status=none
  debugfs -w -R 'mkdir /system' "$stock" >/dev/null 2>&1
  debugfs -w -R 'mkdir /system/etc' "$stock" >/dev/null 2>&1
  debugfs -w -R "write $original /system/etc/resizable.bin" "$stock" >/dev/null 2>&1

  set +e
  e2fsck -fy "$stock" >/dev/null
  local fsck_rc=$?
  set -e
  if (( fsck_rc > 1 )); then
    echo "Stage 2 block-size fixture e2fsck failed: block_size=$block_size rc=$fsck_rc" >&2
    cleanup_case
    return "$fsck_rc"
  fi

  [[ "$(debugfs -R 'blocks /system/etc/resizable.bin' "$stock" 2>/dev/null | wc -w)" -eq 1 ]]
  dumpe2fs -h "$stock" 2>/dev/null | grep -q 'metadata_csum'
  local stock_hash_before
  stock_hash_before="$(sha256sum "$stock" | awk '{print $1}')"

  dd if=/dev/zero bs="$grown_size" count=1 status=none | tr '\000' 'G' > "$grown"
  printf 'LOOM-S2-BS-%s-GROWN' "$block_size" | dd of="$grown" bs=1 seek=7 conv=notrunc status=none

  origin_loop="$(sudo losetup --find --show --read-only "$stock")"
  local output
  output="$(
    "$LOOM" ext4-resize \
      "$stock" \
      /system/etc/resizable.bin \
      "$grown" \
      "$shadow" \
      "$origin_loop" \
      LOOM_SHADOW_PLACEHOLDER \
      "$table"
  )"

  echo "$output" | grep -q "block_size=$block_size"
  echo "$output" | grep -q 'data_blocks=1'
  echo "$output" | grep -q 'data_shadow_blocks=1'
  echo "$output" | grep -q 'metadata_blocks=1'
  echo "$output" | grep -q 'shadow_blocks=2'
  [[ "$(stat -c %s "$shadow")" -eq $((block_size * 2)) ]]

  shadow_loop="$(sudo losetup --find --show --read-only "$shadow")"
  sed -i "s|LOOM_SHADOW_PLACEHOLDER|$shadow_loop|g" "$table"
  sudo dmsetup create "$mapper" < "$table"
  sudo mount -t ext4 -o ro,noload "/dev/mapper/$mapper" "$mount_dir"
  [[ "$(sudo stat -c %s "$mount_dir/system/etc/resizable.bin")" -eq "$grown_size" ]]
  sudo cmp "$mount_dir/system/etc/resizable.bin" "$grown"
  sudo umount "$mount_dir"
  sudo e2fsck -fn "/dev/mapper/$mapper" >/dev/null

  local stock_hash_after
  stock_hash_after="$(sha256sum "$stock" | awk '{print $1}')"
  [[ "$stock_hash_before" == "$stock_hash_after" ]]

  cleanup_case
  printf 'Stage 2 block-size %s PASS: %s -> %s bytes, shadow=%s\n' \
    "$block_size" "$original_size" "$grown_size" "$((block_size * 2))"
}

run_case 1024 700 900
run_case 2048 1300 1700
run_case 4096 3000 3500

printf '%s\n' 'Stage 2 block-size matrix PASS'
