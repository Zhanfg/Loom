#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli --bin loom --bin loom-flatten
FLATTEN="$REPO_ROOT/target/release/loom-flatten"

WORK="$(mktemp -d)"
DM1="loom-alpha4-layer1-${RANDOM}-${RANDOM}"
DM2="loom-alpha4-layer2-${RANDOM}-${RANDOM}"
DM_FLAT="loom-alpha4-flat-${RANDOM}-${RANDOM}"
ORIGIN_LOOP=""
SHADOW1_LOOP=""
SHADOW2_LOOP=""
AGG_LOOP=""

cleanup() {
  set +e
  for mapper in "$DM_FLAT" "$DM2" "$DM1"; do
    if sudo dmsetup info "$mapper" >/dev/null 2>&1; then
      sudo dmsetup remove "$mapper"
    fi
  done
  for loop in "$AGG_LOOP" "$SHADOW2_LOOP" "$SHADOW1_LOOP" "$ORIGIN_LOOP"; do
    if [[ -n "$loop" ]]; then
      sudo losetup -d "$loop" >/dev/null 2>&1 || true
    fi
  done
  rm -rf "$WORK"
}
trap cleanup EXIT

ORIGIN="$WORK/origin.bin"
SHADOW1="$WORK/shadow1.pack"
SHADOW2="$WORK/shadow2.pack"
TABLE1="$WORK/layer1.table"
TABLE2="$WORK/layer2.table"
MANIFEST="$WORK/layers.tsv"
AGGREGATE="$WORK/aggregate.pack"
FLAT_RAW="$WORK/flat.raw.table"
FLAT_TABLE="$WORK/flat.table"

python3 - "$ORIGIN" "$SHADOW1" "$SHADOW2" <<'PY'
import pathlib
import sys
origin, shadow1, shadow2 = map(pathlib.Path, sys.argv[1:])
origin.write_bytes(bytes(((i * 17 + 3) & 0xFF) for i in range(32768)))
shadow1.write_bytes(bytes([0xA1]) * 4096)
shadow2.write_bytes(bytes([0xB2]) * 4096)
PY

ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$ORIGIN")"
SHADOW1_LOOP="$(sudo losetup --find --show --read-only "$SHADOW1")"
SHADOW2_LOOP="$(sudo losetup --find --show --read-only "$SHADOW2")"
TOTAL_SECTORS="$(sudo blockdev --getsz "$ORIGIN_LOOP")"
[[ "$TOTAL_SECTORS" == 64 ]]

# Layer 1 replaces sectors [8,16) with shadow1.
cat >"$TABLE1" <<EOF
0 8 linear $ORIGIN_LOOP 0
8 8 linear $SHADOW1_LOOP 0
16 48 linear $ORIGIN_LOOP 16
EOF
sudo dmsetup create "$DM1" <"$TABLE1"
DM1_PATH="/dev/mapper/$DM1"

# Layer 2 replaces sectors [12,20), overlapping the latter half of layer 1.
cat >"$TABLE2" <<EOF
0 12 linear $DM1_PATH 0
12 8 linear $SHADOW2_LOOP 0
20 44 linear $DM1_PATH 20
EOF
sudo dmsetup create "$DM2" <"$TABLE2"
DM2_PATH="/dev/mapper/$DM2"

printf '%s\t%s\t%s\t%s\n' \
  "$TABLE1" "$ORIGIN_LOOP" "$SHADOW1_LOOP" "$SHADOW1" \
  >"$MANIFEST"
printf '%s\t%s\t%s\t%s\n' \
  "$TABLE2" "$DM1_PATH" "$SHADOW2_LOOP" "$SHADOW2" \
  >>"$MANIFEST"

"$FLATTEN" \
  "$MANIFEST" \
  "$AGGREGATE" \
  "$ORIGIN_LOOP" \
  __LOOM_AGGREGATE_SHADOW__ \
  "$FLAT_RAW"

# The first layer contributed 8 sectors and the second contributed another 8,
# but four sectors from layer 1 are hidden by layer 2 and must be compacted away.
[[ "$(stat -c %s "$AGGREGATE")" == 6144 ]]

if grep -Fq "$DM1_PATH" "$FLAT_RAW" || grep -Fq "$SHADOW1_LOOP" "$FLAT_RAW" || grep -Fq "$SHADOW2_LOOP" "$FLAT_RAW"; then
  echo 'flattened table still references an intermediate layer device' >&2
  exit 1
fi

grep -Fq "$ORIGIN_LOOP" "$FLAT_RAW"
grep -Fq '__LOOM_AGGREGATE_SHADOW__' "$FLAT_RAW"

AGG_LOOP="$(sudo losetup --find --show --read-only "$AGGREGATE")"
sed "s|__LOOM_AGGREGATE_SHADOW__|$AGG_LOOP|g" "$FLAT_RAW" >"$FLAT_TABLE"
sudo dmsetup create "$DM_FLAT" <"$FLAT_TABLE"
DM_FLAT_PATH="/dev/mapper/$DM_FLAT"

# The single flattened device must be byte-identical to the original two-layer chain.
sudo cmp "$DM2_PATH" "$DM_FLAT_PATH"

# Spot-check the overlap semantics as an independent assertion.
python3 - "$DM_FLAT_PATH" <<'PY'
import pathlib
import sys
raw = pathlib.Path(sys.argv[1]).read_bytes()
assert raw[8*512:12*512] == bytes([0xA1]) * (4*512)
assert raw[12*512:20*512] == bytes([0xB2]) * (8*512)
PY

printf '%s\n' \
  'Alpha 4 dm flatten PASS' \
  "  original layered devices: 2" \
  "  stable effective devices: 1" \
  "  aggregate shadow bytes: $(stat -c %s "$AGGREGATE")" \
  "  flattened table extents: $(wc -l <"$FLAT_RAW" | tr -d ' ')"
