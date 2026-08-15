#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

CORE="crates/loom-erofs/src/compact_core.rs"
SINGLE="crates/loom-erofs/src/compact_index.rs"
MULTI="crates/loom-erofs/src/multi_index.rs"
CODEC="crates/loom-erofs/src/multi_lz4.rs"

[[ -f "$CORE" && -f "$SINGLE" && -f "$MULTI" && -f "$CODEC" ]]

grep -q 'fn read_compact_entry' "$CORE"
grep -q 'fn reconstruct_head_pcluster' "$CORE"
grep -q 'fn validate_nonheads' "$CORE"
grep -q 'fn encode_extent' "$CORE"
grep -q 'pub(crate) fn compile_multi_oracle' "$CORE"
grep -q 'pub(crate) fn compile_multi_lz4' "$CORE"
grep -q 'shared_core::compile_multi_oracle' "$MULTI"
grep -q 'shared_core::compile_multi_lz4' "$MULTI"
grep -q 'shared_core::compile_oracle' "$SINGLE"
grep -q 'shared_core::compile_lz4' "$SINGLE"

if grep -q 'fn read_compact_entry' "$SINGLE" "$MULTI"; then
  echo 'Stage 18 regression: compact parser logic leaked back into an API adapter' >&2
  exit 1
fi
if grep -q 'fn encode_extent' "$SINGLE" "$MULTI"; then
  echo 'Stage 18 regression: LZ4 extent encoding leaked back into an API adapter' >&2
  exit 1
fi
if grep -Eq 'shared_core::compile_(oracle|big_oracle)' "$MULTI"; then
  echo 'Stage 18 regression: multi adapter bypassed the unified compact oracle dispatch' >&2
  exit 1
fi
if grep -Eq 'shared_core::compile_(lz4|big_lz4)' "$MULTI"; then
  echo 'Stage 18 regression: multi adapter bypassed the unified compact self-encode dispatch' >&2
  exit 1
fi

# Compatibility adapters may grow as new policy/API surfaces are added, but they must
# remain materially smaller than the unified parser/codec core. A ratio gate preserves
# the architectural invariant without freezing a historical absolute line count.
CORE_LINES="$(wc -l < "$CORE")"
SINGLE_LINES="$(wc -l < "$SINGLE")"
MULTI_LINES="$(wc -l < "$MULTI")"
[[ "$CORE_LINES" -gt 500 ]]
(( SINGLE_LINES * 3 < CORE_LINES ))
(( MULTI_LINES * 3 < CORE_LINES ))

# Unit coverage in the shared core includes both one-head and multi-head topology cases,
# and the workspace gate already runs all loom-erofs unit tests before this structural check.
grep -q 'single_and_multi_topologies_share_compatibility_rules' "$CORE"
grep -q 'later_extent_codec_failure_happens_before_view_construction' "$CORE"

printf '%s\n' \
  'Stage 18 EROFS compact-core unification PASS' \
  "  shared core lines: $CORE_LINES" \
  "  single adapter lines: $SINGLE_LINES" \
  "  multi adapter lines: $MULTI_LINES" \
  '  multi oracle dispatch owner: compact_core' \
  '  multi self-encode dispatch owner: compact_core' \
  '  adapter/core ratio: < 1/3' \
  '  compact parser copies in adapters: 0' \
  '  extent encoder copies in adapters: 0'
