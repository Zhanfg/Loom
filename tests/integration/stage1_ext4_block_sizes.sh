#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"
WORK="$(mktemp -d)"

cleanup_global() {
  rm -rf "$WORK"
}
trap cleanup_global EXIT

run_case() {
  local block_size="$1"
  local case_dir="$WORK/bs-$block_size"
  local stock="$case_dir/stock.ext4"
  local original="$case_dir/original.bin"
  local replacement="$case_dir/replacement.bin"
  local shadow="$case_dir/shadow.pack"
  local table="$case_dir/loom.table"
  local mount_dir="$case_dir/mnt"
  local mapper="loom-bs-${block_size}-${RANDOM}-${RANDOM}"
  local origin_loop=""
  local shadow_loop=""
  local file_size=$((block_size * 2 + 37))
  local expected_shadow=$((block_size * 3))

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

  dd if=/dev/zero bs="$file_size" count=1 status=none | tr '\000' 'A' > "$original"
  printf 'LOOM-BS-%s-ORIGINAL' "$block_size" | dd of="$original" bs=1 seek=3 conv=notrunc status=none
  debugfs -w -R 'mkdir /system' "$stock" >/dev/null 2>&1
  debugfs -w -R 'mkdir /system/etc' "$stock" >/dev/null 2>&1
  debugfs -w -R "write $original /system/etc/block-size.bin" "$stock" >/dev/null 2>&1

  local fixture_blocks
  fixture_blocks="$(debugfs -R 'blocks /system/etc/block-size.bin' "$stock" 2>/dev/null | wc -w)"
  if [[ "$fixture_blocks" -ne 3 ]]; then
    echo "block-size fixture is not dense: block_size=$block_size blocks=$fixture_blocks" >&2
    debugfs -R 'stat /system/etc/block-size.bin' "$stock" >&2 || true
    cleanup_case
    return 1
  fi

  set +e
  e2fsck -fy "$stock" >/dev/null
  local fsck_rc=$?
  set -e
  if (( fsck_rc > 1 )); then
    echo "fixture e2fsck failed: block_size=$block_size rc=$fsck_rc" >&2
    cleanup_case
    return "$fsck_rc"
  fi

  local stock_hash_before
  stock_hash_before="$(sha256sum "$stock" | awk '{print $1}')"

  dd if=/dev/zero bs="$file_size" count=1 status=none | tr '\000' 'B' > "$replacement"
  printf 'LOOM-BS-%s-REPLACED' "$block_size" | dd of="$replacement" bs=1 seek=3 conv=notrunc status=none

  origin_loop="$(sudo losetup --find --show --read-only "$stock")"
  local compile_output
  compile_output="$(
    "$LOOM" ext4-replace \
      "$stock" \
      /system/etc/block-size.bin \
      "$replacement" \
      "$shadow" \
      "$origin_loop" \
      LOOM_SHADOW_PLACEHOLDER \
      "$table"
  )"
  echo "$compile_output" | grep -q "block_size=$block_size"
  echo "$compile_output" | grep -q 'data_blocks=3'
  [[ "$(stat -c %s "$shadow")" -eq "$expected_shadow" ]]

  shadow_loop="$(sudo losetup --find --show --read-only "$shadow")"
  sed -i "s|LOOM_SHADOW_PLACEHOLDER|$shadow_loop|g" "$table"
  sudo dmsetup create "$mapper" < "$table"
  sudo mount -t ext4 -o ro,noload "/dev/mapper/$mapper" "$mount_dir"
  sudo cmp "$mount_dir/system/etc/block-size.bin" "$replacement"
  sudo umount "$mount_dir"
  sudo e2fsck -fn "/dev/mapper/$mapper" >/dev/null

  local stock_hash_after
  stock_hash_after="$(sha256sum "$stock" | awk '{print $1}')"
  [[ "$stock_hash_before" == "$stock_hash_after" ]]

  cleanup_case
  printf 'ext4 block-size %s PASS: file=%s shadow=%s\n' \
    "$block_size" "$file_size" "$expected_shadow"
}

run_case 1024
run_case 2048
run_case 4096

printf '%s\n' 'Stage 1.1 block-size matrix PASS'
