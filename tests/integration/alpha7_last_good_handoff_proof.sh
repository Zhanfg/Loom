#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

STATE_BIN="$REPO_ROOT/target/loom-early-state-alpha7-last-good"
TABLE_BIN="$REPO_ROOT/target/loom-early-table-alpha7-last-good"
gcc -std=c11 -O2 -Wall -Wextra -Werror tools/loom-early-state.c -o "$STATE_BIN"
gcc -std=c11 -O2 -Wall -Wextra -Werror tools/loom-early-table.c -o "$TABLE_BIN"

WORK="$(mktemp -d)"
STATE="$WORK/state"
SNAPSHOTS="$WORK/snapshots"
ORIGIN_LOOP=""
METADATA_LOOP=""
DM_NAME="loom-alpha7-last-good-${RANDOM}-${RANDOM}"

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

mkdir -p "$STATE" "$SNAPSHOTS/g-a" "$SNAPSHOTS/g-b"
ORIGIN="$WORK/origin.bin"
METADATA="$WORK/metadata.bin"
EXPECTED_A="$WORK/expected-a.bin"
EXPECTED_B="$WORK/expected-b.bin"

python3 - "$ORIGIN" "$METADATA" "$EXPECTED_A" "$EXPECTED_B" "$SNAPSHOTS" <<'PY'
import hashlib
import pathlib
import sys
origin_path, metadata_path, expected_a_path, expected_b_path, snapshots = [pathlib.Path(x) for x in sys.argv[1:]]
origin = bytearray(((i * 13 + 17) & 0xff) for i in range(64 * 512))
metadata = bytearray(1024 * 512)

def write_snapshot(name, byte, first_sector):
    payload = bytes([byte]) * (12 * 512)
    metadata[first_sector*512:(first_sector+12)*512] = payload
    expected = bytearray(origin)
    expected[8*512:20*512] = payload
    directory = snapshots / name
    shadow = payload
    extents = f"0 {first_sector} 12\n".encode()
    table = (
        b"0 8 linear __LOOM_ORIGIN__ 0\n" +
        f"8 12 linear __LOOM_METADATA_DEVICE__ {first_sector}\n".encode() +
        b"20 44 linear __LOOM_ORIGIN__ 20\n"
    )
    (directory / "shadow.pack").write_bytes(shadow)
    (directory / "shadow.extents").write_bytes(extents)
    (directory / "early.table").write_bytes(table)
    h = lambda data: hashlib.sha256(data).hexdigest()
    (directory / "descriptor.env").write_text(
        "LOOM_EARLY_SCHEMA=1\n"
        f"LOOM_GENERATION={name}\n"
        "LOOM_STATE=PREPARED_NOT_ACTIVE\n"
        f"LOOM_SHADOW_SHA256={h(shadow)}\n"
        f"LOOM_EXTENTS_SHA256={h(extents)}\n"
        f"LOOM_TABLE_SHA256={h(table)}\n"
        "LOOM_TAKEOVER=0\n"
    )
    return expected

expected_a = write_snapshot("g-a", 0xA6, 100)
expected_b = write_snapshot("g-b", 0xB6, 300)
origin_path.write_bytes(origin)
metadata_path.write_bytes(metadata)
expected_a_path.write_bytes(expected_a)
expected_b_path.write_bytes(expected_b)
PY

ORIGIN_LOOP="$(sudo losetup --find --show --read-only "$ORIGIN")"
METADATA_LOOP="$(sudo losetup --find --show --read-only "$METADATA")"

materialize_and_check() {
  local generation=$1
  local expected=$2
  local concrete="$WORK/${generation}.table"
  rm -f "$concrete"
  "$TABLE_BIN" "$SNAPSHOTS/$generation/early.table" "$ORIGIN_LOOP" "$METADATA_LOOP" "$concrete"
  sudo dmsetup create "$DM_NAME" <"$concrete"
  sudo cmp "/dev/mapper/$DM_NAME" "$expected"
  sudo dmsetup remove "$DM_NAME"
}

# Establish A as a genuinely attempted and confirmed last-good generation.
"$STATE_BIN" arm "$STATE" g-a
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=candidate generation=g-a reason=first-attempt' ]]
[[ "$(cat "$STATE/attempted")" == g-a ]]
materialize_and_check g-a "$EXPECTED_A"
"$STATE_BIN" confirm "$STATE" "$SNAPSHOTS" g-a
[[ "$(cat "$STATE/confirmed")" == g-a ]]
[[ ! -e "$STATE/attempted" ]]

# B is a valid new generation and its first early view is also materializable.
"$STATE_BIN" arm "$STATE" g-b
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=candidate generation=g-b reason=first-attempt' ]]
[[ "$(cat "$STATE/attempted")" == g-b ]]
materialize_and_check g-b "$EXPECTED_B"

# Simulate B failing before userspace confirmation. On the next boot the state
# helper must quarantine B and return A, and A must still produce the exact old view.
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=last-good generation=g-a reason=previous-attempt-unconfirmed' ]]
[[ "$(cat "$STATE/failed")" == g-b ]]
[[ "$(cat "$STATE/confirmed")" == g-a ]]
[[ ! -e "$STATE/attempted" ]]
materialize_and_check g-a "$EXPECTED_A"

# The following boot remains on A and never retries B unless a user re-arms it.
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=last-good generation=g-a reason=candidate-quarantined' ]]
materialize_and_check g-a "$EXPECTED_A"

printf '%s\n' \
  'Alpha 7 last-good handoff PASS' \
  '  generation A: attempted + materialized + confirmed' \
  '  generation B: attempted + materialized + unconfirmed' \
  '  next boot: B quarantined' \
  '  fallback generation: A' \
  '  fallback A device bytes: exact'
