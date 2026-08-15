# Stage 4 — bounded contiguous ext4 growth

Stage 4 generalizes the Stage 3 allocator from exactly one new data block to a bounded contiguous run while preserving the same read-only origin and metadata closure.

## Contract

- source file: one dense, single-link regular file data block;
- existing source data remains byte-for-byte stock;
- replacement is filesystem-block aligned;
- new data blocks: `1..=64`;
- all new blocks come from one contiguous free run in the source block's group;
- no cross-group allocation;
- no external extent-tree blocks;
- no inode allocation or directory mutation;
- origin is never written.

## Expected effective closure

For `N` new blocks, Loom shadows exactly:

- `N` new data blocks;
- one inode-table block;
- one block-bitmap block;
- one group-descriptor-containing block;
- one primary-superblock-containing block.

Therefore `shadow_blocks = N + 4`.

Stage 3 remains the `ExactlyOne` policy over the same generalized implementation and is a mandatory regression gate.
