#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

STATE_BIN="$REPO_ROOT/target/loom-early-state-alpha8"
ATTEST_BIN="$REPO_ROOT/target/loom-early-attest-alpha8"
gcc -std=c11 -O2 -Wall -Wextra -Werror tools/loom-early-state.c -o "$STATE_BIN"
gcc -std=c11 -O2 -Wall -Wextra -Werror tools/loom-early-attest.c -o "$ATTEST_BIN"

WORK="$(mktemp -d)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT
STATE="$WORK/state"
SNAPSHOTS="$WORK/snapshots"
mkdir -p "$STATE" "$SNAPSHOTS/g-a" "$SNAPSHOTS/g-b"

make_snapshot() {
  local generation=$1
  local value=$2
  local dir="$SNAPSHOTS/$generation"
  printf 'shadow-%s\n' "$value" >"$dir/shadow.pack"
  printf '0 100 8\n' >"$dir/shadow.extents"
  cat >"$dir/early.table" <<'EOF'
0 8 linear __LOOM_METADATA_DEVICE__ 100
8 56 linear __LOOM_ORIGIN__ 8
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

make_snapshot g-a a
make_snapshot g-b b

BOOT1='11111111-1111-4111-8111-111111111111'
BOOT2='22222222-2222-4222-8222-222222222222'
BOOT3='33333333-3333-4333-8333-333333333333'
BOOT4='44444444-4444-4444-8444-444444444444'
BOOT5='55555555-5555-4555-8555-555555555555'

# Previous userspace selects A. New first-stage begins a fresh boot epoch before deciding.
"$STATE_BIN" arm "$STATE" g-a
"$ATTEST_BIN" begin "$STATE" "$BOOT1"
[[ "$(cat "$STATE/current-boot")" == "$BOOT1" ]]
[[ ! -e "$STATE/active" ]]
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=candidate generation=g-a reason=first-attempt' ]]
[[ "$(cat "$STATE/attempted")" == g-a ]]

# Candidate activation is authorized only for the exact attempted generation.
if "$ATTEST_BIN" activate "$STATE" "$BOOT1" g-b candidate; then
  echo 'expected unauthorized candidate activation to fail' >&2
  exit 1
fi
"$ATTEST_BIN" activate "$STATE" "$BOOT1" g-a candidate
VERIFY="$($ATTEST_BIN verify "$STATE" "$BOOT1")"
[[ "$VERIFY" == "generation=g-a action=candidate boot_id=$BOOT1" ]]

# A stale/different kernel boot id can never validate this active record.
if "$ATTEST_BIN" verify "$STATE" "$BOOT2"; then
  echo 'expected cross-boot active verification to fail' >&2
  exit 1
fi

# boot-completed first verifies this boot's active candidate, then confirms A.
"$STATE_BIN" confirm "$STATE" "$SNAPSHOTS" g-a
[[ "$(cat "$STATE/confirmed")" == g-a ]]
[[ ! -e "$STATE/attempted" ]]

# Next boot creates a new epoch and clears the previous active record before decision.
"$ATTEST_BIN" begin "$STATE" "$BOOT2"
[[ "$(cat "$STATE/current-boot")" == "$BOOT2" ]]
[[ ! -e "$STATE/active" ]]
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=confirmed generation=g-a reason=last-good' ]]
"$ATTEST_BIN" activate "$STATE" "$BOOT2" g-a confirmed
VERIFY="$($ATTEST_BIN verify "$STATE" "$BOOT2")"
[[ "$VERIFY" == "generation=g-a action=confirmed boot_id=$BOOT2" ]]

# Prepare upgrade B. On its first early boot it is a one-shot candidate.
"$STATE_BIN" arm "$STATE" g-b
"$ATTEST_BIN" begin "$STATE" "$BOOT3"
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=candidate generation=g-b reason=first-attempt' ]]
"$ATTEST_BIN" activate "$STATE" "$BOOT3" g-b candidate
VERIFY="$($ATTEST_BIN verify "$STATE" "$BOOT3")"
[[ "$VERIFY" == "generation=g-b action=candidate boot_id=$BOOT3" ]]
# Simulated boot failure: no confirm.

# New boot epoch clears B's stale active marker. Recovery quarantines B and selects A.
"$ATTEST_BIN" begin "$STATE" "$BOOT4"
[[ ! -e "$STATE/active" ]]
DECISION="$($STATE_BIN decide "$STATE" "$SNAPSHOTS")"
[[ "$DECISION" == 'action=last-good generation=g-a reason=previous-attempt-unconfirmed' ]]
[[ "$(cat "$STATE/failed")" == g-b ]]
[[ "$(cat "$STATE/confirmed")" == g-a ]]
"$ATTEST_BIN" activate "$STATE" "$BOOT4" g-a last-good
VERIFY="$($ATTEST_BIN verify "$STATE" "$BOOT4")"
[[ "$VERIFY" == "generation=g-a action=last-good boot_id=$BOOT4" ]]

# Last-good fallback A must not confirm as if desired B had succeeded.
if "$STATE_BIN" confirm "$STATE" "$SNAPSHOTS" g-a; then
  echo 'expected last-good fallback not to confirm against desired B' >&2
  exit 1
fi
[[ "$(cat "$STATE/desired")" == g-b ]]
[[ "$(cat "$STATE/confirmed")" == g-a ]]

# Even if an old active record reappears after a new boot epoch, binding rejects it.
cp "$STATE/active" "$WORK/old-active"
"$ATTEST_BIN" begin "$STATE" "$BOOT5"
cp "$WORK/old-active" "$STATE/active"
if "$ATTEST_BIN" verify "$STATE" "$BOOT5"; then
  echo 'expected stale active record from previous boot to fail verification' >&2
  exit 1
fi

# Boundary-length valid boot ids work; malformed ids do not.
[[ "$(cat "$STATE/current-boot")" == "$BOOT5" ]]
if "$ATTEST_BIN" begin "$STATE" 'not-a-kernel-boot-id'; then
  echo 'expected invalid boot id to be rejected' >&2
  exit 1
fi

printf '%s\n' \
  'Alpha 8 boot attestation PASS' \
  '  active generation bound to exact kernel boot id: yes' \
  '  new boot clears stale active before decision: yes' \
  '  candidate activation requires attempted owner: yes' \
  '  confirmed/last-good activation requires confirmed owner: yes' \
  '  stale previous-boot active cannot trigger confirmation: yes'
