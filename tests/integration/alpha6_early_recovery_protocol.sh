#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo build --release -p loom-cli --bin loom-early-state
STATE_BIN="$REPO_ROOT/target/release/loom-early-state"

WORK="$(mktemp -d)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

STATE="$WORK/state"
SNAPSHOTS="$WORK/snapshots"
mkdir -p "$STATE" "$SNAPSHOTS"

make_snapshot() {
  local generation=$1
  local payload=$2
  local dir="$SNAPSHOTS/$generation"
  mkdir -p "$dir"
  printf 'shadow-%s\n' "$payload" >"$dir/shadow.pack"
  printf '0 100 8\n8 300 8\n' >"$dir/shadow.extents"
  cat >"$dir/early.table" <<EOF
0 8 linear __LOOM_ORIGIN__ 0
8 8 linear __LOOM_METADATA_DEVICE__ 100
16 8 linear __LOOM_METADATA_DEVICE__ 300
24 40 linear __LOOM_ORIGIN__ 24
EOF
  local shadow_sha extents_sha table_sha
  shadow_sha="$(sha256sum "$dir/shadow.pack" | awk '{print $1}')"
  extents_sha="$(sha256sum "$dir/shadow.extents" | awk '{print $1}')"
  table_sha="$(sha256sum "$dir/early.table" | awk '{print $1}')"
  cat >"$dir/descriptor.env" <<EOF
LOOM_EARLY_SCHEMA=1
LOOM_GENERATION=$generation
LOOM_STATE=PREPARED_NOT_ACTIVE
LOOM_SHADOW_SHA256=$shadow_sha
LOOM_EXTENTS_SHA256=$extents_sha
LOOM_TABLE_SHA256=$table_sha
LOOM_TAKEOVER=0
EOF
}

assert_file_value() {
  local path=$1
  local expected=$2
  [[ -f "$path" ]]
  [[ "$(cat "$path")" == "$expected" ]]
}

make_snapshot g-a stable-a
make_snapshot g-b candidate-b
make_snapshot g-c candidate-c

# Boot A1: explicitly arm A. No candidate has been attempted yet.
"$STATE_BIN" arm "$STATE" g-a
assert_file_value "$STATE/desired" g-a
[[ ! -e "$STATE/attempted" ]]
[[ ! -e "$STATE/failed" ]]

# Early boot A1: first use of A is a one-shot candidate and MUST durably mark attempted first.
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=candidate generation=g-a reason=first-attempt' ]]
assert_file_value "$STATE/attempted" g-a
[[ ! -e "$STATE/confirmed" ]]

# Userspace reaches boot-completed: A becomes the confirmed last-good generation.
"$STATE_BIN" confirm "$STATE" "$SNAPSHOTS" g-a
assert_file_value "$STATE/confirmed" g-a
[[ ! -e "$STATE/attempted" ]]
[[ ! -e "$STATE/failed" ]]
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=confirmed generation=g-a reason=last-good' ]]

# Upgrade to B. Arm explicitly clears any old failure for B but preserves confirmed A.
"$STATE_BIN" arm "$STATE" g-b
assert_file_value "$STATE/desired" g-b
assert_file_value "$STATE/confirmed" g-a
[[ ! -e "$STATE/attempted" ]]
[[ ! -e "$STATE/failed" ]]

# Boot B1: B gets exactly one early attempt.
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=candidate generation=g-b reason=first-attempt' ]]
assert_file_value "$STATE/attempted" g-b

# Simulate panic/bootloop: no confirm is issued.
# Boot B2: the stale attempted marker must quarantine B BEFORE choosing last-good A.
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=last-good generation=g-a reason=previous-attempt-unconfirmed' ]]
assert_file_value "$STATE/failed" g-b
[[ ! -e "$STATE/attempted" ]]
assert_file_value "$STATE/confirmed" g-a

# Boot B3: B must stay quarantined; it must never receive a hidden automatic retry.
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=last-good generation=g-a reason=candidate-quarantined' ]]
assert_file_value "$STATE/failed" g-b

# Emergency marker always wins, independently of desired/confirmed state.
"$STATE_BIN" force-stock "$STATE" on
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=stock reason=force-stock' ]]
[[ -f "$STATE/force-stock" ]]
"$STATE_BIN" force-stock "$STATE" off
[[ ! -e "$STATE/force-stock" ]]
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=last-good generation=g-a reason=candidate-quarantined' ]]

# A user can deliberately re-arm the quarantined B. That grants exactly one new attempt.
"$STATE_BIN" arm "$STATE" g-b
[[ ! -e "$STATE/failed" ]]
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=candidate generation=g-b reason=first-attempt' ]]
assert_file_value "$STATE/attempted" g-b

# Move to C and corrupt its actual shadow after descriptor creation. Hash verification must reject
# it before writing any attempted marker, then fall back to the still-confirmed A.
"$STATE_BIN" arm "$STATE" g-c
printf 'tampered-after-descriptor\n' >"$SNAPSHOTS/g-c/shadow.pack"
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=last-good generation=g-a reason=desired-snapshot-invalid' ]]
[[ ! -e "$STATE/attempted" ]]
[[ ! -e "$STATE/failed" ]]
assert_file_value "$STATE/confirmed" g-a

# Missing snapshots use the same fail-open-to-last-good path and do not create an attempt marker.
"$STATE_BIN" arm "$STATE" g-missing
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=last-good generation=g-a reason=desired-snapshot-invalid' ]]
[[ ! -e "$STATE/attempted" ]]

# Generation IDs are path-safe identifiers, not arbitrary paths.
if "$STATE_BIN" arm "$STATE" '../escape'; then
  echo 'expected invalid generation id to be rejected' >&2
  exit 1
fi
[[ "$(cat "$STATE/desired")" == g-missing ]]

# Confirmation is also guarded by snapshot integrity and desired-generation identity.
if "$STATE_BIN" confirm "$STATE" "$SNAPSHOTS" g-a; then
  echo 'expected confirm of non-desired generation to fail' >&2
  exit 1
fi
assert_file_value "$STATE/confirmed" g-a

STATUS="$($STATE_BIN status "$STATE")"
grep -Fxq 'desired=g-missing' <<<"$STATUS"
grep -Fxq 'attempted=' <<<"$STATUS"
grep -Fxq 'confirmed=g-a' <<<"$STATUS"
grep -Fxq 'failed=' <<<"$STATUS"
grep -Fxq 'force_stock=0' <<<"$STATUS"

printf '%s\n' \
  'Alpha 6 early recovery protocol PASS' \
  '  candidate attempts: one-shot' \
  '  unconfirmed candidate: quarantined' \
  '  fallback: confirmed last-good, else stock' \
  '  force-stock: dominant' \
  '  snapshot content hashes: verified before attempt'
