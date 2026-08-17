#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

STATE_BIN="$REPO_ROOT/target/loom-early-state-host"
gcc -std=c11 -O2 -Wall -Wextra -Werror \
  tools/loom-early-state.c -o "$STATE_BIN"

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

# A is first armed but cannot be confirmed until it has actually been returned
# as an early candidate and the attempted marker is durable.
"$STATE_BIN" arm "$STATE" g-a
if "$STATE_BIN" confirm "$STATE" "$SNAPSHOTS" g-a; then
  echo 'expected confirmation before an early attempt to be rejected' >&2
  exit 1
fi
assert_file_value "$STATE/desired" g-a
[[ ! -e "$STATE/attempted" ]]

DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=candidate generation=g-a reason=first-attempt' ]]
assert_file_value "$STATE/attempted" g-a

"$STATE_BIN" confirm "$STATE" "$SNAPSHOTS" g-a
assert_file_value "$STATE/confirmed" g-a
[[ ! -e "$STATE/attempted" ]]
[[ ! -e "$STATE/failed" ]]
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=confirmed generation=g-a reason=last-good' ]]

# Upgrade to B: first boot gets exactly one candidate attempt.
"$STATE_BIN" arm "$STATE" g-b
assert_file_value "$STATE/desired" g-b
assert_file_value "$STATE/confirmed" g-a
[[ ! -e "$STATE/failed" ]]
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=candidate generation=g-b reason=first-attempt' ]]
assert_file_value "$STATE/attempted" g-b

# No confirmation simulates a panic before the userspace health boundary.
# The next boot must quarantine B before returning last-good A.
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=last-good generation=g-a reason=previous-attempt-unconfirmed' ]]
assert_file_value "$STATE/failed" g-b
[[ ! -e "$STATE/attempted" ]]
assert_file_value "$STATE/confirmed" g-a

# Repeated boots never silently retry quarantined B.
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=last-good generation=g-a reason=candidate-quarantined' ]]

# Emergency force-stock always wins and does not destroy the last-good state.
"$STATE_BIN" force-stock "$STATE" on
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=stock reason=force-stock' ]]
assert_file_value "$STATE/confirmed" g-a
"$STATE_BIN" force-stock "$STATE" off
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=last-good generation=g-a reason=candidate-quarantined' ]]

# Explicit re-arm is the only path that grants failed B another attempt.
"$STATE_BIN" arm "$STATE" g-b
[[ ! -e "$STATE/failed" ]]
[[ ! -e "$STATE/attempted" ]]
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=candidate generation=g-b reason=first-attempt' ]]
assert_file_value "$STATE/attempted" g-b

# Move to C and alter real bytes after its descriptor was generated. The helper's
# built-in SHA-256 must agree with sha256sum and reject the candidate before attempt.
"$STATE_BIN" arm "$STATE" g-c
printf 'tampered-after-descriptor\n' >"$SNAPSHOTS/g-c/shadow.pack"
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=last-good generation=g-a reason=desired-snapshot-invalid' ]]
[[ ! -e "$STATE/attempted" ]]
[[ ! -e "$STATE/failed" ]]
assert_file_value "$STATE/confirmed" g-a

# A missing desired snapshot has the same fail-open-to-last-good semantics.
"$STATE_BIN" arm "$STATE" g-missing
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=last-good generation=g-a reason=desired-snapshot-invalid' ]]
[[ ! -e "$STATE/attempted" ]]

# Corrupt recovery state itself must not become a boot blocker. It returns stock.
printf '../bad-state\n' >"$STATE/attempted"
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=stock reason=state-invalid' ]]
assert_file_value "$STATE/desired" g-missing
assert_file_value "$STATE/confirmed" g-a
rm -f "$STATE/attempted"

# Path-like generation ids are always rejected and must not mutate desired state.
if "$STATE_BIN" arm "$STATE" '../escape'; then
  echo 'expected invalid generation id to be rejected' >&2
  exit 1
fi
assert_file_value "$STATE/desired" g-missing

# Confirming a different generation than desired is forbidden even if it is last-good.
if "$STATE_BIN" confirm "$STATE" "$SNAPSHOTS" g-a; then
  echo 'expected mismatched confirmation to be rejected' >&2
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
  'Alpha 6 C early recovery protocol PASS' \
  '  candidate attempt: durable + one-shot' \
  '  unconfirmed candidate: quarantined before fallback' \
  '  last-good: retained across failed upgrades' \
  '  state corruption: fail-open to stock' \
  '  snapshot bytes: SHA-256 verified before attempt' \
  '  false confirmation without attempt: rejected'
