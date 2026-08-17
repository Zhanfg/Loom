#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

STATE_BIN="$REPO_ROOT/target/loom-early-state-alpha7"
TABLE_BIN="$REPO_ROOT/target/loom-early-table-alpha7"
gcc -std=c11 -O2 -Wall -Wextra -Werror tools/loom-early-state.c -o "$STATE_BIN"
gcc -std=c11 -O2 -Wall -Wextra -Werror tools/loom-early-table.c -o "$TABLE_BIN"

WORK="$(mktemp -d)"
STATE="$WORK/state"
SNAPSHOTS="$WORK/snapshots"
ORIGIN_LOOP=""
METADATA_LOOP=""
DM_NAME="loom-alpha7-handoff-${RANDOM}-${RANDOM}"

cleanup() {
  set +e
  if sudo dmsetup info "$DM_NAME" >/dev/null 2>&1; then
    sudo dmsetup remove "$DM_NAME"
  fi
  [[ -n "$METADATA_LOOP" ]] && sudo losetup -d "$METADATA_LOOP" >/dev/null 2>&1 || true
  [[ -n "$ORIGIN_LOOP" ]] && sudo losetup -d "$ORIGIN_LOOP" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$STATE" "$SNAPSHOTS/g-candidate"
ORIGIN="$WORK/origin.bin"
METADATA="$WORK/metadata.bin"
EXPECTED="$WORK/expected.bin"
CONCRETE="$WORK/concrete.table"

python3 - "$ORIGIN" "$METADATA" "$EXPECTED" "$SNAPSHOTS/g-candidate" <<'PY'
import hashlib
import pathlib
import sys
origin_path, metadata_path, expected_path, snap_path = map(pathlib.Path, sys.argv[1:])
origin = bytearray(((i * 31 + 7) & 0xff) for i in range(64 * 512))
metadata = bytearray(512 * 512)
a = bytes([0xA7]) * (6 * 512)
b = bytes([0xB7]) * (6 * 512)
metadata[100*512:106*512] = a
metadata[300*512:306*512] = b
expected = bytearray(origin)
expected[8*512:14*512] = a
expected[14*512:20*512] = b
origin_path.write_bytes(origin)
metadata_path.write_bytes(metadata)
expected_path.write_bytes(expected)
shadow = a + b
extents = b"0 100 6\n6 300 6\n"
table = (
    b"0 8 linear __LOOM_ORIGIN__ 0\n"
    b"8 6 linear __LOOM_METADATA_DEVICE__ 100\n"
    b"14 6 linear __LOOM_METADATA_DEVICE__ 300\n"
    b"20 44 linear __LOOM_ORIGIN__ 20\n"
)
(snap_path / "shadow.pack").write_bytes(shadow)
(snap_path / "shadow.extents").write_bytes(extents)
(snap_path / "early.table").write_bytes(table)
def h(data):
    return hashlib.sha256(data).hexdigest()
(snap_path / "descriptor.env").write_text(
    "LOOM_EARLY_SCHEMA=1\n"
    "LOOM_GENERATION=g-candidate\n"
    "LOOM_STATE=PREPARED_NOT_ACTIVE\n"
    f"LOOM_SHADOW_SHA256={h(shadow)}\n"
    f"LOOM_EXTENTS_SHA256={h(extents)}\n"
    f"LOOM_TABLE_SHA256={h(table)}\n"
    "LOOM_TAKEOVER=0\n"
)
PY

ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$ORIGIN")"
METADATA_LOOP="$(sudo losetup --find --show --read-only "$METADATA")"

"$STATE_BIN" arm "$STATE" g-candidate
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=candidate generation=g-candidate reason=first-attempt' ]]

# This is the critical ordering assertion: attempted must already be durable before
# any concrete table is materialized or device-mapper object is created.
[[ "$(cat "$STATE/attempted")" == g-candidate ]]
[[ ! -e "$CONCRETE" ]]
! sudo dmsetup info "$DM_NAME" >/dev/null 2>&1

"$TABLE_BIN" \
  "$SNAPSHOTS/g-candidate/early.table" \
  "$ORIGIN_LOOP" \
  "$METADATA_LOOP" \
  "$CONCRETE"
[[ -s "$CONCRETE" ]]
! grep -q '__LOOM_' "$CONCRETE"
grep -Fq "$ORIGIN_LOOP" "$CONCRETE"
grep -Fq "$METADATA_LOOP" "$CONCRETE"

sudo dmsetup create "$DM_NAME" <"$CONCRETE"
DM_PATH="/dev/mapper/$DM_NAME"
sudo cmp "$DM_PATH" "$EXPECTED"

# Independent spot checks prove both fragmented metadata ranges were wired correctly.
sudo python3 - "$DM_PATH" <<'PY'
import pathlib, sys
raw = pathlib.Path(sys.argv[1]).read_bytes()
assert raw[8*512:14*512] == bytes([0xA7]) * (6*512)
assert raw[14*512:20*512] == bytes([0xB7]) * (6*512)
PY

# A malformed table can never smuggle an arbitrary block device into first stage.
BAD="$WORK/bad.table"
BAD_OUT="$WORK/bad.out"
cat >"$BAD" <<'EOF'
0 64 linear /dev/forbidden 0
EOF
if "$TABLE_BIN" "$BAD" "$ORIGIN_LOOP" "$METADATA_LOOP" "$BAD_OUT"; then
  echo 'expected forbidden early-table backing to be rejected' >&2
  exit 1
fi
[[ ! -e "$BAD_OUT" ]]

# Simulate a boot that redirected but never reached confirmation. The following
# early decision must quarantine the candidate and choose stock because there is no last-good yet.
sudo dmsetup remove "$DM_NAME"
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=stock reason=previous-attempt-unconfirmed' ]]
[[ "$(cat "$STATE/failed")" == g-candidate ]]
[[ ! -e "$STATE/attempted" ]]

# The candidate remains quarantined on later boots and receives no hidden retry.
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=stock reason=candidate-quarantined' ]]

printf '%s\n' \
  'Alpha 7 first-stage handoff proof PASS' \
  '  attempted marker persisted before DM creation: yes' \
  '  early table allowed sources: origin + metadata only' \
  '  raw metadata-backed effective DM matches expected bytes: yes' \
  '  unconfirmed redirect next boot: stock' \
  '  hidden automatic retry: no'
