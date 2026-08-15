#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

CORE="crates/loom-erofs/src/compact_core.rs"
ADAPTER="crates/loom-erofs/src/compact_index.rs"
OLD_BIG="crates/loom-erofs/src/big_pcluster.rs"

[[ -f "$CORE" && -f "$ADAPTER" ]]
[[ ! -e "$OLD_BIG" ]]

grep -q 'pub(crate) fn compile_big_oracle' "$CORE"
grep -q 'pub(crate) fn compile_big_lz4' "$CORE"
grep -q 'fn read_big_topology' "$CORE"
grep -q 'fn validate_big_single_extent' "$CORE"
grep -q 'D0_CBLKCNT' "$CORE"

grep -q 'shared_core::compile_big_oracle' "$ADAPTER"
grep -q 'shared_core::compile_big_lz4' "$ADAPTER"

if grep -Eq 'fn (read_superblock|read_inode|read_compact_entry|compact_regions|compact_entry_position|validate_big_single_extent)' "$ADAPTER"; then
  echo 'Stage 21 regression: compact/big parser logic leaked into the compatibility adapter' >&2
  exit 1
fi
if grep -q 'big_pcluster.rs' "$ADAPTER"; then
  echo 'Stage 21 regression: legacy big-pcluster parser module was reintroduced' >&2
  exit 1
fi

grep -q 'cblkcnt_marker_is_bit_11_of_compact_low_field' "$CORE"
grep -q 'one_head_two_block_extent_accepts_cblkcnt' "$CORE"
grep -q 'eight_kib_0padding_span_round_trips' "$CORE"

printf '%s\n' \
  'Stage 21 big-pcluster compact-core unification PASS' \
  '  legacy standalone big parser: removed' \
  '  CBLKCNT topology owner: compact_core' \
  '  big oracle owner: compact_core' \
  '  big self-encoder owner: compact_core' \
  '  compatibility adapter parser copies: 0'
