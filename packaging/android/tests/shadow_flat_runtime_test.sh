#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME="$ROOT/module/bin/loom-shadow-flat"
[[ -f "$RUNTIME" ]] || { echo "missing loom-shadow-flat runtime" >&2; exit 1; }

TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

STATE="$TMP/state"
FAKE="$TMP/fake"
mkdir -p "$STATE" "$FAKE" "$TMP/dm" "$TMP/loops"
: >"$TMP/proc_mounts"
printf 'origin\n' >"$TMP/origin.dev"

LOG_DM="$TMP/dm.log"
LOG_LOOP="$TMP/loop.log"
LOG_FLATTEN="$TMP/flatten.log"

cat >"$STATE/shadow.conf" <<EOF
LOOM_DM_PREFIX=loom-shadow-flat-test
EOF

cat >"$FAKE/layered" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
state=${LOOM_STATE_DIR:?}
proc=${FAKE_PROC_MOUNTS:?}
tmp=${FAKE_TMP:?}
runtime="$state/shadow-runtime"
case "${1:-status}" in
  preflight) exit 0 ;;
  activate)
    rm -rf "$runtime"
    mkdir -p "$runtime/layers/1" "$runtime/layers/2"
    printf 'S1' >"$runtime/layers/1/shadow.pack"
    printf 'S2' >"$runtime/layers/2/shadow.pack"
    : >"$tmp/loops/layer1.dev"; : >"$tmp/loops/layer2.dev"
    : >"$tmp/dm/layer1.dev"; : >"$tmp/dm/layer2.dev"
    printf '%s\n' "$tmp/loops/layer1.dev" >"$runtime/layers/1/loop"
    printf '%s\n' "$tmp/loops/layer2.dev" >"$runtime/layers/2/loop"
    printf '%s\n' 'loom-shadow-flat-test-1' >"$runtime/layers/1/dm_name"
    printf '%s\n' 'loom-shadow-flat-test-2' >"$runtime/layers/2/dm_name"
    printf '%s\n' "$tmp/dm/layer1.dev" >"$runtime/layers/1/dm_path"
    printf '%s\n' "$tmp/dm/layer2.dev" >"$runtime/layers/2/dm_path"
    cat >"$runtime/layers/1/table" <<EOF
0 8 linear $tmp/origin.dev 0
8 8 linear $tmp/loops/layer1.dev 0
16 8 linear $tmp/origin.dev 16
EOF
    cat >"$runtime/layers/2/table" <<EOF
0 8 linear $tmp/dm/layer1.dev 0
8 8 linear $tmp/loops/layer2.dev 0
16 8 linear $tmp/dm/layer1.dev 16
EOF
    printf '2\n' >"$runtime/layer_count"
    cat >"$runtime/runtime.env" <<EOF
LOOM_MODE=sparse-shadow-sidecar
LOOM_ORIGIN=$tmp/origin.dev
LOOM_EFFECTIVE_DEVICE=$tmp/dm/layer2.dev
LOOM_MOUNTPOINT=$state/mnt/system-shadow
LOOM_PAYLOAD_ROOT=$state/payload/system
LOOM_SOURCE_MODULE_ID=
LOOM_LAYER_COUNT=2
LOOM_TAKEOVER=0
EOF
    mkdir -p "$state/mnt/system-shadow"
    printf '%s %s erofs ro 0 0\n' "$tmp/dm/layer2.dev" "$state/mnt/system-shadow" >"$proc"
    printf 'SHADOW_ACTIVE\n' >"$state/status"
    ;;
  cleanup)
    rm -rf "$runtime"
    : >"$proc"
    printf 'SHADOW_INACTIVE\n' >"$state/status"
    ;;
  status) printf 'layered-status\n' ;;
  *) exit 2 ;;
esac
SH
chmod +x "$FAKE/layered"

cat >"$FAKE/flatten" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
log=${FAKE_FLATTEN_LOG:?}
manifest=$1
aggregate=$2
origin=$3
token=$4
table=$5
printf 'manifest=%s origin=%s token=%s\n' "$manifest" "$origin" "$token" >>"$log"
[[ "$(wc -l <"$manifest" | tr -d ' ')" == 2 ]]
if [[ "${FAKE_FLATTEN_FAIL:-0}" == 1 ]]; then
  exit 23
fi
python3 - "$aggregate" <<'PY'
import pathlib, sys
pathlib.Path(sys.argv[1]).write_bytes(b'F' * 6144)
PY
cat >"$table" <<EOF
0 8 linear $origin 0
8 12 linear $token 0
20 4 linear $origin 20
EOF
SH
chmod +x "$FAKE/flatten"

cat >"$FAKE/losetup" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
log=${FAKE_LOOP_LOG:?}
tmp=${FAKE_TMP:?}
if [[ "$1" == -f && "$2" == -r && "$3" == --show ]]; then
  dev="$tmp/loops/aggregate.dev"
  : >"$dev"
  printf 'attach %s %s\n' "$dev" "$4" >>"$log"
  printf '%s\n' "$dev"
  exit 0
fi
if [[ "$1" == -d ]]; then
  printf 'detach %s\n' "$2" >>"$log"
  rm -f "$2"
  exit 0
fi
exit 2
SH
chmod +x "$FAKE/losetup"

cat >"$FAKE/dmctl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
log=${FAKE_DM_LOG:?}
tmp=${FAKE_TMP:?}
case "$1" in
  create)
    name=$2
    printf 'create %s\n' "$name" >>"$log"
    dev="$tmp/dm/${name}.dev"
    : >"$dev"
    printf '%s\n' "$dev" >"$tmp/dm/${name}.path"
    ;;
  getpath) cat "$tmp/dm/$2.path" ;;
  delete)
    printf 'delete %s\n' "$2" >>"$log"
    rm -f "$tmp/dm/$2.dev" "$tmp/dm/$2.path"
    ;;
  *) exit 2 ;;
esac
SH
chmod +x "$FAKE/dmctl"

cat >"$FAKE/mount" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
proc=${FAKE_PROC_MOUNTS:?}
args=("$@")
count=${#args[@]}
device=${args[$((count-2))]}
mountpoint=${args[$((count-1))]}
printf '%s %s erofs ro 0 0\n' "$device" "$mountpoint" >>"$proc"
SH
chmod +x "$FAKE/mount"

cat >"$FAKE/umount" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
proc=${FAKE_PROC_MOUNTS:?}
target=${@: -1}
tmp="$proc.tmp"
grep -Fv " $target " "$proc" >"$tmp" || true
mv "$tmp" "$proc"
SH
chmod +x "$FAKE/umount"

export LOOM_STATE_DIR="$STATE"
export LOOM_MODDIR="$ROOT/module"
export LOOM_SHADOW_CONFIG="$STATE/shadow.conf"
export LOOM_PROC_MOUNTS="$TMP/proc_mounts"
export LOOM_TEST_ALLOW_REGULAR=1
export LOOM_LAYERED_RUNTIME_OVERRIDE="$FAKE/layered"
export LOOM_FLATTEN_BIN_OVERRIDE="$FAKE/flatten"
export LOOM_LOSETUP_BIN="$FAKE/losetup"
export LOOM_DMCTL_BIN="$FAKE/dmctl"
export LOOM_DMSETUP_BIN=
export LOOM_MOUNT_BIN="$FAKE/mount"
export LOOM_UMOUNT_BIN="$FAKE/umount"
export FAKE_TMP="$TMP"
export FAKE_PROC_MOUNTS="$TMP/proc_mounts"
export FAKE_DM_LOG="$LOG_DM"
export FAKE_LOOP_LOG="$LOG_LOOP"
export FAKE_FLATTEN_LOG="$LOG_FLATTEN"

bash "$RUNTIME" preflight
bash "$RUNTIME" activate
[[ "$(cat "$STATE/status")" == SHADOW_ACTIVE_FLAT ]]
grep -Fxq 'LOOM_MODE=sparse-shadow-flat-generation' "$STATE/shadow-runtime/runtime.env"
grep -Fxq 'LOOM_STABLE_DM_DEPTH=1' "$STATE/shadow-runtime/runtime.env"
grep -Fxq 'LOOM_TRANSIENT_LAYER_COUNT=2' "$STATE/shadow-runtime/runtime.env"
grep -Fxq 'LOOM_FLAT_STATE=ACTIVE' "$STATE/shadow-runtime/flat/flat.env"
flat_dev="$TMP/dm/loom-shadow-flat-test-flat.dev"
grep -Fq "$flat_dev $STATE/mnt/system-shadow " "$TMP/proc_mounts"
grep -Fq 'delete loom-shadow-flat-test-2' "$LOG_DM"
grep -Fq 'delete loom-shadow-flat-test-1' "$LOG_DM"
grep -Fq "detach $TMP/loops/layer2.dev" "$LOG_LOOP"
grep -Fq "detach $TMP/loops/layer1.dev" "$LOG_LOOP"
[[ -f "$STATE/shadow-runtime/flat/shadow.pack" ]]
[[ "$(stat -c %s "$STATE/shadow-runtime/flat/shadow.pack")" == 6144 ]]

bash "$RUNTIME" cleanup
[[ "$(cat "$STATE/status")" == SHADOW_INACTIVE ]]
[[ ! -d "$STATE/shadow-runtime" ]]
[[ ! -s "$TMP/proc_mounts" ]]
grep -Fq 'delete loom-shadow-flat-test-flat' "$LOG_DM"
grep -Fq "detach $TMP/loops/aggregate.dev" "$LOG_LOOP"

# A flatten failure must tear down the layered validation view instead of using
# it as an implicit fallback.
: >"$LOG_DM"; : >"$LOG_LOOP"; : >"$TMP/proc_mounts"
export FAKE_FLATTEN_FAIL=1
if bash "$RUNTIME" activate; then
  echo 'expected flatten failure to fail closed' >&2
  exit 1
fi
[[ "$(cat "$STATE/status")" == SHADOW_INACTIVE ]]
[[ ! -d "$STATE/shadow-runtime" ]]
[[ ! -s "$TMP/proc_mounts" ]]
unset FAKE_FLATTEN_FAIL

printf '%s\n' 'Loom Android single-dm shadow runtime test PASS'
