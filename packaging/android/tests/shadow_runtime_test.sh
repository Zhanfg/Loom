#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME="$ROOT/module/bin/loom-shadow"
[[ -f "$RUNTIME" ]] || { echo "missing loom-shadow runtime" >&2; exit 1; }

TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

STATE="$TMP/state"
FAKE="$TMP/fake"
PAYLOAD="$STATE/payload/system"
mkdir -p "$STATE" "$FAKE" "$PAYLOAD/etc" "$PAYLOAD/bin"
: > "$TMP/proc_mounts"
printf 'origin-device\n' > "$TMP/origin.dev"
printf 'alpha\n' > "$PAYLOAD/etc/alpha.conf"
printf 'beta\n' > "$PAYLOAD/bin/beta.bin"

LOG_LOOM="$TMP/loom.log"
LOG_DM="$TMP/dmctl.log"
LOG_LOOP="$TMP/losetup.log"
LOG_MOUNT="$TMP/mount.log"

cat > "$FAKE/loom" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
log=${FAKE_LOOM_LOG:?}
[[ "$1" == erofs-compact-pcluster-swap ]]
[[ "$2" == --multi-encode ]]
origin=$3
target=$4
payload=$5
shadow=$6
origin_device=$7
shadow_token=$8
table=$9
printf 'origin=%s target=%s payload=%s\n' "$origin" "$target" "$payload" >> "$log"
if [[ -n "${FAKE_LOOM_FAIL_TARGET:-}" && "$target" == "$FAKE_LOOM_FAIL_TARGET" ]]; then
  exit 23
fi
python3 - "$shadow" <<'PY'
import pathlib, sys
pathlib.Path(sys.argv[1]).write_bytes(b'S' * 4096)
PY
cat > "$table" <<EOF
0 8 linear $origin_device 0
8 8 linear $shadow_token 0
16 8 linear $origin_device 16
EOF
printf 'compiled target=%s\n' "$target"
SH
chmod +x "$FAKE/loom"

cat > "$FAKE/losetup" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
log=${FAKE_LOOP_LOG:?}
state=${FAKE_LOOP_STATE:?}
mkdir -p "$state"
if [[ "$1" == -f && "$2" == -r && "$3" == --show ]]; then
  n=$(find "$state" -maxdepth 1 -name 'loop*.dev' | wc -l)
  dev="$state/loop${n}.dev"
  : > "$dev"
  printf 'attach %s %s\n' "$dev" "$4" >> "$log"
  printf '%s\n' "$dev"
  exit 0
fi
if [[ "$1" == -d ]]; then
  printf 'detach %s\n' "$2" >> "$log"
  rm -f "$2"
  exit 0
fi
exit 2
SH
chmod +x "$FAKE/losetup"

cat > "$FAKE/dmctl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
log=${FAKE_DM_LOG:?}
state=${FAKE_DM_STATE:?}
mkdir -p "$state"
case "$1" in
  create)
    name=$2
    shift 2
    printf 'create %s' "$name" >> "$log"
    printf ' %q' "$@" >> "$log"
    printf '\n' >> "$log"
    dev="$state/${name}.dev"
    : > "$dev"
    printf '%s\n' "$dev" > "$state/${name}.path"
    ;;
  getpath)
    if [[ "${FAKE_DM_GETPATH_FAIL:-0}" == 1 ]]; then
      exit 1
    fi
    cat "$state/$2.path"
    ;;
  delete)
    printf 'delete %s\n' "$2" >> "$log"
    if [[ -f "$state/$2.path" ]]; then
      dev=$(cat "$state/$2.path")
      rm -f "$dev" "$state/$2.path"
    fi
    ;;
  *) exit 2 ;;
esac
SH
chmod +x "$FAKE/dmctl"

cat > "$FAKE/mount" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
log=${FAKE_MOUNT_LOG:?}
proc=${FAKE_PROC_MOUNTS:?}
printf '%q ' "$@" >> "$log"; printf '\n' >> "$log"
args=("$@")
count=${#args[@]}
device=${args[$((count-2))]}
mountpoint=${args[$((count-1))]}
printf '%s %s erofs ro 0 0\n' "$device" "$mountpoint" >> "$proc"
SH
chmod +x "$FAKE/mount"

cat > "$FAKE/umount" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
proc=${FAKE_PROC_MOUNTS:?}
target=${@: -1}
tmp="$proc.tmp"
grep -Fv " $target " "$proc" > "$tmp" || true
mv "$tmp" "$proc"
SH
chmod +x "$FAKE/umount"

cat > "$STATE/shadow.conf" <<EOF
LOOM_SHADOW_ENABLED=1
LOOM_SOURCE_MODULE_ID=
LOOM_PAYLOAD_ROOT=$PAYLOAD
LOOM_TARGET=system
LOOM_ORIGIN=$TMP/origin.dev
LOOM_DM_PREFIX=loom-shadow-test
LOOM_MOUNTPOINT=$STATE/mnt/system-shadow
LOOM_MAX_LAYERS=8
LOOM_TAKEOVER=0
EOF

export LOOM_STATE_DIR="$STATE"
export LOOM_MODDIR="$ROOT/module"
export LOOM_SHADOW_CONFIG="$STATE/shadow.conf"
export LOOM_PROC_MOUNTS="$TMP/proc_mounts"
export LOOM_TEST_UID=0
export LOOM_TEST_ALLOW_REGULAR=1
export LOOM_BIN_OVERRIDE="$FAKE/loom"
export LOOM_LOSETUP_BIN="$FAKE/losetup"
export LOOM_DMCTL_BIN="$FAKE/dmctl"
export LOOM_DMSETUP_BIN=
export LOOM_MOUNT_BIN="$FAKE/mount"
export LOOM_UMOUNT_BIN="$FAKE/umount"
export LOOM_FIND_BIN="$(command -v find)"
export LOOM_SORT_BIN="$(command -v sort)"
export FAKE_LOOM_LOG="$LOG_LOOM"
export FAKE_DM_LOG="$LOG_DM"
export FAKE_LOOP_LOG="$LOG_LOOP"
export FAKE_MOUNT_LOG="$LOG_MOUNT"
export FAKE_PROC_MOUNTS="$TMP/proc_mounts"
export FAKE_DM_STATE="$TMP/dm"
export FAKE_LOOP_STATE="$TMP/loops"

bash "$RUNTIME" preflight
[[ "$(cat "$STATE/status")" == SHADOW_PREFLIGHT_OK ]]
bash "$RUNTIME" activate
[[ "$(cat "$STATE/status")" == SHADOW_ACTIVE ]]
grep -Fxq 'LOOM_MODE=sparse-shadow-sidecar' "$STATE/shadow-runtime/runtime.env"
grep -Fxq 'LOOM_LAYER_COUNT=2' "$STATE/shadow-runtime/runtime.env"
grep -Fxq 'LOOM_TAKEOVER=0' "$STATE/shadow-runtime/runtime.env"
grep -Fq " $STATE/mnt/system-shadow " "$TMP/proc_mounts"

first_dm="$TMP/dm/loom-shadow-test-1.dev"
grep -Fq "origin=$first_dm target=/etc/alpha.conf" "$LOG_LOOM" || \
  grep -Fq "origin=$first_dm target=/bin/beta.bin" "$LOG_LOOM"

grep -Fq 'create loom-shadow-test-1 -ro linear 0 8' "$LOG_DM"
grep -Fq ' linear 8 8 ' "$LOG_DM"
grep -Fq 'create loom-shadow-test-2 -ro linear 0 8' "$LOG_DM"

if grep -Eq 'mount[^\n]*(/system|/vendor|/product)([[:space:]]|$)' "$RUNTIME"; then
  echo 'unsafe system mount target found in loom-shadow runtime' >&2
  exit 1
fi

grep -Fq "$STATE/mnt/system-shadow" "$LOG_MOUNT"

bash "$RUNTIME" cleanup
[[ "$(cat "$STATE/status")" == SHADOW_INACTIVE ]]
[[ ! -d "$STATE/shadow-runtime" ]]
[[ ! -s "$TMP/proc_mounts" ]]
grep -Fq 'delete loom-shadow-test-2' "$LOG_DM"
grep -Fq 'delete loom-shadow-test-1' "$LOG_DM"
[[ "$(grep -c '^detach ' "$LOG_LOOP")" -ge 2 ]]

: > "$LOG_LOOM"; : > "$LOG_DM"; : > "$LOG_LOOP"; : > "$TMP/proc_mounts"
export FAKE_LOOM_FAIL_TARGET=/etc/alpha.conf
if bash "$RUNTIME" activate; then
  echo 'expected sparse-shadow activation failure' >&2
  exit 1
fi
[[ "$(cat "$STATE/status")" == SHADOW_COMPILE_FAILED ]]
[[ ! -d "$STATE/shadow-runtime" ]]
[[ ! -s "$TMP/proc_mounts" ]]
grep -q '^delete loom-shadow-test-' "$LOG_DM"
grep -q '^detach ' "$LOG_LOOP"
unset FAKE_LOOM_FAIL_TARGET

# A dm object created successfully but lacking a usable getpath must be deleted
# immediately; its loop must also be detached.
: > "$LOG_DM"; : > "$LOG_LOOP"; : > "$TMP/proc_mounts"
export FAKE_DM_GETPATH_FAIL=1
if bash "$RUNTIME" activate; then
  echo 'expected dm getpath failure to abort activation' >&2
  exit 1
fi
[[ "$(cat "$STATE/status")" == SHADOW_COMPILE_FAILED ]]
[[ ! -d "$STATE/shadow-runtime" ]]
[[ ! -s "$TMP/proc_mounts" ]]
grep -Fq 'delete loom-shadow-test-1' "$LOG_DM"
grep -q '^detach ' "$LOG_LOOP"
unset FAKE_DM_GETPATH_FAIL

sed -i 's/^LOOM_TAKEOVER=0$/LOOM_TAKEOVER=1/' "$STATE/shadow.conf"
if bash "$RUNTIME" preflight; then
  echo 'takeover=1 must be rejected in Alpha 2' >&2
  exit 1
fi
[[ "$(cat "$STATE/status")" == SHADOW_CONFIG_INVALID ]]
sed -i 's/^LOOM_TAKEOVER=1$/LOOM_TAKEOVER=0/' "$STATE/shadow.conf"

ln -s alpha.conf "$PAYLOAD/etc/link.conf"
if bash "$RUNTIME" preflight; then
  echo 'symlink payload must be rejected in Alpha 2' >&2
  exit 1
fi
[[ "$(cat "$STATE/status")" == SHADOW_UNSUPPORTED_PAYLOAD ]]

printf '%s\n' 'Loom Android sparse-shadow runtime test PASS'
