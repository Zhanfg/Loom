#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME="$ROOT/module/bin/loom-early-prepare"
[[ -f "$RUNTIME" ]] || { echo 'missing loom-early-prepare runtime' >&2; exit 1; }

TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

STATE="$TMP/state"
METADATA="$TMP/metadata"
FAKE="$TMP/fake"
METADEV="$TMP/metadata.dev"
mkdir -p "$STATE/shadow-runtime/flat" "$METADATA" "$FAKE"
: >"$METADEV"
printf 'MAIN_ACTIVE\n' >"$STATE/status"
printf 'g-alpha5-test\n' >"$STATE/current-generation"
printf 'S%.0s' {1..6144} >"$STATE/shadow-runtime/flat/shadow.pack"
cat >"$STATE/shadow-runtime/flat/table.raw" <<'EOF'
0 8 linear /dev/fake-origin 0
8 12 linear __LOOM_AGGREGATE_SHADOW__ 0
20 44 linear /dev/fake-origin 20
EOF
cat >"$STATE/shadow-runtime/runtime.env" <<'EOF'
LOOM_MODE=sparse-shadow-flat-generation
LOOM_ORIGIN=/dev/fake-origin
LOOM_EFFECTIVE_DEVICE=/dev/fake-flat
LOOM_MOUNTPOINT=/data/adb/loom/mnt/system-generation
LOOM_LAYER_COUNT=2
LOOM_TRANSIENT_LAYER_COUNT=2
LOOM_STABLE_DM_DEPTH=1
LOOM_TAKEOVER=0
EOF

cat >"$TMP/proc_mounts" <<EOF
$METADEV $METADATA ext4 rw 0 0
EOF

cat >"$FAKE/fiemap" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
input=$1
output=$2
[[ -s "$input" ]]
[[ ! -e "$output" ]]
# 6144 bytes = 12 sectors, physically represented here by two fragmented ranges.
printf '%s\n' '0 100 6' '6 300 6' >"$output"
SH
chmod +x "$FAKE/fiemap"

cat >"$FAKE/early-map" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
flat=$1
origin=$2
shadow=$3
extents=$4
output=$5
[[ -s "$flat" && -s "$extents" ]]
[[ "$origin" == /dev/fake-origin ]]
[[ "$shadow" == __LOOM_AGGREGATE_SHADOW__ ]]
[[ ! -e "$output" ]]
cat >"$output" <<'EOF'
0 8 linear __LOOM_ORIGIN__ 0
8 6 linear __LOOM_METADATA_DEVICE__ 100
14 6 linear __LOOM_METADATA_DEVICE__ 300
20 44 linear __LOOM_ORIGIN__ 20
EOF
SH
chmod +x "$FAKE/early-map"

CONF="$STATE/early.conf"
cat >"$CONF" <<EOF
LOOM_EARLY_PREPARE_ENABLED=0
LOOM_METADATA_MOUNT=$METADATA
LOOM_TAKEOVER=0
EOF

run_early() {
  LOOM_MODDIR="$ROOT/module" \
  LOOM_STATE_DIR="$STATE" \
  LOOM_EARLY_CONFIG="$CONF" \
  LOOM_METADATA_MOUNT_OVERRIDE="$METADATA" \
  LOOM_PROC_MOUNTS="$TMP/proc_mounts" \
  LOOM_FIEMAP_BIN_OVERRIDE="$FAKE/fiemap" \
  LOOM_EARLY_MAP_BIN_OVERRIDE="$FAKE/early-map" \
  LOOM_TEST_ALLOW_REGULAR=1 \
  LOOM_TEST_UID=0 \
    bash "$RUNTIME" "$@"
}

# Prepare is opt-in and must not touch the main LoomFS runtime status.
run_early prepare
grep -Fxq EARLY_PREPARE_DISABLED "$STATE/early-status"
grep -Fxq MAIN_ACTIVE "$STATE/status"
[[ ! -e "$METADATA/loom" ]]

# Enable prepare-only mode. No takeover flag is permitted.
sed -i 's/^LOOM_EARLY_PREPARE_ENABLED=0$/LOOM_EARLY_PREPARE_ENABLED=1/' "$CONF"
run_early prepare
grep -Fxq EARLY_SNAPSHOT_READY "$STATE/early-status"
grep -Fxq MAIN_ACTIVE "$STATE/status"
grep -Fxq g-alpha5-test "$STATE/early-prepared-generation"
SNAP="$METADATA/loom/early/g-alpha5-test"
[[ -f "$SNAP/shadow.pack" ]]
[[ -f "$SNAP/shadow.extents" ]]
[[ -f "$SNAP/early.table" ]]
[[ -f "$SNAP/descriptor.env" ]]
grep -Fxq 'LOOM_EARLY_SCHEMA=1' "$SNAP/descriptor.env"
grep -Fxq 'LOOM_GENERATION=g-alpha5-test' "$SNAP/descriptor.env"
grep -Fxq 'LOOM_STATE=PREPARED_NOT_ACTIVE' "$SNAP/descriptor.env"
grep -Fxq "LOOM_METADATA_DEVICE=$METADEV" "$SNAP/descriptor.env"
grep -Fxq 'LOOM_TAKEOVER=0' "$SNAP/descriptor.env"
grep -Fxq 'LOOM_ORIGIN_TOKEN=__LOOM_ORIGIN__' "$SNAP/descriptor.env"
grep -Fxq 'LOOM_METADATA_TOKEN=__LOOM_METADATA_DEVICE__' "$SNAP/descriptor.env"
grep -Fq '__LOOM_METADATA_DEVICE__' "$SNAP/early.table"
! grep -Fq '__LOOM_AGGREGATE_SHADOW__' "$SNAP/early.table"

# Re-running the same committed generation is idempotent.
FIRST_DESCRIPTOR_SHA="$(sha256sum "$SNAP/descriptor.env" | awk '{print $1}')"
run_early prepare
grep -Fxq EARLY_SNAPSHOT_READY "$STATE/early-status"
[[ "$(sha256sum "$SNAP/descriptor.env" | awk '{print $1}')" == "$FIRST_DESCRIPTOR_SHA" ]]

# A conflicting existing generation is fail-closed rather than overwritten.
printf 'different-shadow\n' >"$STATE/shadow-runtime/flat/shadow.pack"
if run_early prepare; then
  echo 'expected conflicting generation snapshot to fail closed' >&2
  exit 1
fi
grep -Fxq EARLY_SNAPSHOT_CONFLICT "$STATE/early-status"
grep -Fxq MAIN_ACTIVE "$STATE/status"
[[ "$(sha256sum "$SNAP/descriptor.env" | awk '{print $1}')" == "$FIRST_DESCRIPTOR_SHA" ]]

# Enabling takeover in the prepare-only Alpha 5 profile is rejected at config load.
sed -i 's/^LOOM_TAKEOVER=0$/LOOM_TAKEOVER=1/' "$CONF"
if run_early prepare; then
  echo 'expected LOOM_TAKEOVER=1 to be rejected' >&2
  exit 1
fi
grep -Fxq EARLY_CONFIG_INVALID "$STATE/early-status"
grep -Fxq MAIN_ACTIVE "$STATE/status"

printf '%s\n' 'Loom Android early snapshot prepare runtime test PASS'
