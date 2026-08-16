#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
RUNTIME="$ROOT/packaging/android/module/bin/loom-sidecar"
[[ -f "$RUNTIME" ]] || { echo "missing sidecar runtime" >&2; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
STATE="$TMP/state"
BIN="$TMP/bin"
ORIGIN="$TMP/system.erofs"
DM_PATH="$TMP/dm-system"
PROC_MOUNTS="$TMP/proc.mounts"
CMDLOG="$TMP/commands.log"
mkdir -p "$STATE/mnt" "$BIN"
: >"$ORIGIN"
: >"$DM_PATH"
: >"$PROC_MOUNTS"
: >"$CMDLOG"

cat >"$BIN/blockdev" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ "$1" == "--getsz" ]]
echo 8192
EOF

cat >"$BIN/blkid" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
echo erofs
EOF

cat >"$BIN/dmctl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'dmctl %q' "$1" >>"$FAKE_CMDLOG"
shift
printf ' %q' "$@" >>"$FAKE_CMDLOG"
printf '\n' >>"$FAKE_CMDLOG"
case "${1:-}" in
  *) ;;
esac
case "$(head -n 1 <<<"${FUNCNAME[0]:-}")" in *) ;; esac
cmd=$(awk '{print $2}' <<<"$(tail -n 1 "$FAKE_CMDLOG")")
case "$cmd" in
  create) exit 0 ;;
  getpath) printf '%s\n' "$FAKE_DM_PATH" ;;
  delete) exit 0 ;;
  *) exit 2 ;;
esac
EOF

# Replace the dmctl shim with a simpler argv-preserving implementation.
cat >"$BIN/dmctl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
cmd=$1
shift
printf 'dmctl %s' "$cmd" >>"$FAKE_CMDLOG"
printf ' %s' "$@" >>"$FAKE_CMDLOG"
printf '\n' >>"$FAKE_CMDLOG"
case "$cmd" in
  create|delete) exit 0 ;;
  getpath) printf '%s\n' "$FAKE_DM_PATH" ;;
  *) exit 2 ;;
esac
EOF

cat >"$BIN/mount" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'mount' >>"$FAKE_CMDLOG"
printf ' %s' "$@" >>"$FAKE_CMDLOG"
printf '\n' >>"$FAKE_CMDLOG"
if [[ "${FAKE_MOUNT_FAIL:-0}" == 1 ]]; then
  exit 1
fi
prev=
last=
for arg in "$@"; do
  prev=$last
  last=$arg
done
printf '%s %s erofs ro 0 0\n' "$prev" "$last" >>"$FAKE_PROC_MOUNTS"
EOF

cat >"$BIN/umount" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'umount' >>"$FAKE_CMDLOG"
printf ' %s' "$@" >>"$FAKE_CMDLOG"
printf '\n' >>"$FAKE_CMDLOG"
target=${!#}
grep -Fv " $target " "$FAKE_PROC_MOUNTS" >"$FAKE_PROC_MOUNTS.tmp" || true
mv "$FAKE_PROC_MOUNTS.tmp" "$FAKE_PROC_MOUNTS"
EOF

cat >"$BIN/getprop" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ "${1:-}" == ro.boot.slot_suffix ]] && printf '_a\n'
EOF

chmod +x "$BIN"/*

cat >"$STATE/sidecar.conf" <<EOF
LOOM_SIDECAR_ENABLED=1
LOOM_TARGET=system
LOOM_ORIGIN=$ORIGIN
LOOM_DM_NAME=loom-sidecar-system
LOOM_MOUNTPOINT=$STATE/mnt/system
LOOM_FILESYSTEM=auto
EOF

export FAKE_CMDLOG="$CMDLOG"
export FAKE_DM_PATH="$DM_PATH"
export FAKE_PROC_MOUNTS="$PROC_MOUNTS"
export LOOM_STATE_DIR="$STATE"
export LOOM_CONFIG="$STATE/sidecar.conf"
export LOOM_PROC_MOUNTS="$PROC_MOUNTS"
export LOOM_TEST_ALLOW_REGULAR=1
export LOOM_TEST_SKIP_BINARY=1
export LOOM_TEST_UID=0
export LOOM_DMCTL_BIN="$BIN/dmctl"
export LOOM_BLOCKDEV_BIN="$BIN/blockdev"
export LOOM_MOUNT_BIN="$BIN/mount"
export LOOM_UMOUNT_BIN="$BIN/umount"
export LOOM_BLKID_BIN="$BIN/blkid"
export LOOM_GETPROP_BIN="$BIN/getprop"

sh "$RUNTIME" activate
[[ "$(cat "$STATE/status")" == SIDECAR_ACTIVE ]]
grep -Fxq "LOOM_MODE=identity-sidecar" "$STATE/runtime.env"
grep -Fq " $STATE/mnt/system " "$PROC_MOUNTS"
grep -Fq "dmctl create loom-sidecar-system -ro linear 0 8192 $ORIGIN 0" "$CMDLOG"
grep -Fq "mount -t erofs -o ro,nosuid,nodev,noexec $DM_PATH $STATE/mnt/system" "$CMDLOG"
if grep -Eq 'mount .* /system($| )' "$CMDLOG"; then
  echo "sidecar attempted to mount over /system" >&2
  exit 1
fi

sh "$RUNTIME" cleanup
[[ "$(cat "$STATE/status")" == SIDECAR_INACTIVE ]]
[[ ! -s "$PROC_MOUNTS" ]]
grep -Fq "dmctl delete loom-sidecar-system" "$CMDLOG"

: >"$CMDLOG"
: >"$PROC_MOUNTS"
export FAKE_MOUNT_FAIL=1
if sh "$RUNTIME" activate; then
  echo "activation unexpectedly succeeded when mount failed" >&2
  exit 1
fi
[[ "$(cat "$STATE/status")" == SIDECAR_ACTIVATION_FAILED ]]
[[ ! -f "$STATE/runtime.env" ]]
grep -Fq "dmctl delete loom-sidecar-system" "$CMDLOG"
unset FAKE_MOUNT_FAIL

cat >"$STATE/sidecar.conf" <<EOF
LOOM_SIDECAR_ENABLED=1
LOOM_TARGET=system
LOOM_ORIGIN=$ORIGIN
LOOM_DM_NAME=loom-sidecar-system
LOOM_MOUNTPOINT=/system
LOOM_FILESYSTEM=erofs
EOF
: >"$CMDLOG"
if sh "$RUNTIME" preflight; then
  echo "unsafe /system mountpoint configuration was accepted" >&2
  exit 1
fi
if grep -q '^dmctl create ' "$CMDLOG"; then
  echo "dm device was created for unsafe configuration" >&2
  exit 1
fi

printf '%s\n' 'Loom Android sidecar runtime test PASS'
