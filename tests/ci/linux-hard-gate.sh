#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p loom-view --test effective_store

bash tests/integration/stage0_ext4_dm_linear.sh
bash tests/integration/stage1_ext4_compiler.sh
bash tests/integration/stage1_ext4_partial_block.sh
bash tests/integration/stage1_ext4_block_sizes.sh
bash tests/integration/stage1_ext4_block_delta.sh
bash tests/integration/stage1_ext4_sparse_reject.sh
bash tests/integration/stage2_ext4_inode_resize.sh
bash tests/integration/stage2_ext4_resize_reject.sh
bash tests/integration/stage2_ext4_block_sizes.sh
bash tests/integration/stage3_ext4_block_allocation.sh
bash tests/integration/stage4_ext4_create_file.sh
bash tests/integration/stage5_ext4_remove_file.sh
bash tests/integration/stage6_ext4_selinux_xattr.sh
bash tests/integration/stage7_ext4_transaction_view.sh
bash tests/integration/stage9_erofs_flat_plain.sh
bash tests/integration/stage10_erofs_compressed_pcluster.sh
bash tests/integration/stage11_erofs_lz4_encoder.sh
bash tests/integration/stage12_erofs_compact_pcluster.sh
bash tests/integration/stage13_erofs_compact_lz4_encoder.sh
bash tests/integration/stage14_erofs_compact_2b_pack.sh
bash tests/integration/stage15_erofs_compact_2b_self_encode.sh
bash tests/integration/stage16_erofs_multi_pcluster_oracle.sh
bash tests/integration/stage17_erofs_multi_pcluster_self_encode.sh
bash tests/integration/stage18_erofs_compact_core_unification.sh
bash tests/integration/stage19_erofs_big_pcluster_oracle.sh
bash tests/integration/stage20_erofs_big_pcluster_self_encode.sh
bash tests/integration/stage21_erofs_big_pcluster_core_unification.sh
bash tests/integration/stage22_erofs_variable_cblkcnt.sh
bash tests/integration/stage23_erofs_multi_big_oracle.sh
bash tests/integration/stage24_erofs_multi_big_self_encode.sh
bash tests/integration/stage25_erofs_mixed_cblkcnt.sh
bash tests/integration/stage26_erofs_xattr_bearing_paths.sh
bash tests/integration/stage27_erofs_partial_tail.sh
bash tests/integration/stage28_erofs_inline_directory_tail.sh
bash tests/integration/stage29_erofs_legacy_full_index.sh
bash tests/integration/stage30_erofs_legacy_full_partial_tail.sh
bash tests/integration/stage31_erofs_legacy_full_plain_data.sh
bash tests/integration/stage32_erofs_legacy_full_plain_partial_tail.sh
bash tests/integration/stage33_erofs_legacy_full_big_pcluster.sh
bash tests/integration/stage34_erofs_legacy_full_big_partial_tail.sh

printf '%s\n' 'Loom Linux filesystem hard gate PASS'
