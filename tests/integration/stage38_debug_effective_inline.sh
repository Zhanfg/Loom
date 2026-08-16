#!/usr/bin/env bash
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"
cargo build --release -p loom-cli
LOOM="$REPO_ROOT/target/release/loom"
WORK="$(mktemp -d)"; ROOT="$WORK/root"; MNT="$WORK/mnt"
IMG="$WORK/origin.erofs"; REPL="$WORK/replacement.bin"; SHADOW="$WORK/shadow.pack"; TABLE="$WORK/table"
ORIGIN_LOOP=""; SHADOW_LOOP=""; MAPPER="loom-stage38-debug-${RANDOM}-${RANDOM}"
cleanup() {
  set +e
  mountpoint -q "$MNT" 2>/dev/null && sudo umount "$MNT"
  sudo dmsetup info "$MAPPER" >/dev/null 2>&1 && sudo dmsetup remove "$MAPPER"
  [[ -n "$SHADOW_LOOP" ]] && sudo losetup -d "$SHADOW_LOOP"
  [[ -n "$ORIGIN_LOOP" ]] && sudo losetup -d "$ORIGIN_LOOP"
  rm -rf "$WORK"
}
trap cleanup EXIT
mkdir -p "$ROOT" "$MNT"
python3 - "$ROOT/000payload.bin" "$REPL" <<'PY'
import sys
SIZE=98304; EXTENT=32768; pat=b'LOOM-STAGE38-ZTAILPACKING-'
open(sys.argv[1],'wb').write((pat*((SIZE//len(pat))+1))[:SIZE])
open(sys.argv[2],'wb').write(b'A'*EXTENT+b'B'*EXTENT+b'Z'*EXTENT)
PY
for i in $(seq -w 0 499); do : > "$ROOT/z_dummy_${i}_for_directory_growth"; done
mkfs.erofs -b 4096 -zlz4 -E legacy-compress,ztailpacking -T 0 --max-extent-bytes 32768 "$IMG" "$ROOT" >/dev/null
fsck.erofs "$IMG"
ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$IMG")"
OUTPUT="$("$LOOM" erofs-compact-pcluster-swap --multi-encode "$IMG" /000payload.bin "$REPL" "$SHADOW" "$ORIGIN_LOOP" LOOM_SHADOW_PLACEHOLDER "$TABLE")"
printf '%s\n' "$OUTPUT"
SHADOW_LOOP="$(sudo losetup --find --show --read-only "$SHADOW")"
sed -i "s|LOOM_SHADOW_PLACEHOLDER|$SHADOW_LOOP|g" "$TABLE"
printf '%s\n' '--- dm table ---'; cat "$TABLE"
sudo dmsetup create "$MAPPER" < "$TABLE"
printf '%s\n' '--- origin/effective metadata ---'
sudo python3 - "$IMG" "/dev/mapper/$MAPPER" <<'PY'
import struct,sys
for label,path in [('origin',sys.argv[1]),('effective',sys.argv[2])]:
    with open(path,'rb',buffering=0) as f:
        f.seek(2848); h=f.read(16)
        f.seek(3056); tail=f.read(165)
    print(label,'header',h.hex(),'idata',struct.unpack_from('<H',h,2)[0],'advise',hex(struct.unpack_from('<H',h,4)[0]))
    print(label,'tail-first64',tail[:64].hex(),'tail-nonzero',sum(bool(x) for x in tail),'tail-last32',tail[-32:].hex())
PY
printf '%s\n' '--- effective fsck ---'
set +e
sudo fsck.erofs "/dev/mapper/$MAPPER" 2>&1
FSCK_RC=$?
printf 'fsck_rc=%s\n' "$FSCK_RC"
printf '%s\n' '--- effective dump ---'
sudo dump.erofs -e --path=/000payload.bin "/dev/mapper/$MAPPER" 2>&1
DUMP_RC=$?
printf 'dump_rc=%s\n' "$DUMP_RC"
printf '%s\n' '--- effective mount ---'
sudo mount -t erofs -o ro "/dev/mapper/$MAPPER" "$MNT" 2>&1
MOUNT_RC=$?
printf 'mount_rc=%s\n' "$MOUNT_RC"
if [[ "$MOUNT_RC" -eq 0 ]]; then
  sudo cmp "$MNT/000payload.bin" "$REPL"; CMP_RC=$?; printf 'cmp_rc=%s\n' "$CMP_RC"
  sudo umount "$MNT"
fi
printf '%s\n' '--- dmesg tail ---'
sudo dmesg | tail -n 200 2>&1 || true
set -e
exit 1
