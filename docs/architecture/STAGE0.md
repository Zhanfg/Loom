# Loom Stage 0: Sparse Effective Block View

## Purpose

Stage 0 exists to answer one question before any Android or filesystem-compiler work begins:

> Can a stock filesystem remain byte-for-byte unchanged while a sparse replacement block is woven into a separate effective block device that the native filesystem driver consumes normally?

If this cannot be demonstrated reliably, the current Loom Fabric direction is rejected.

## Inputs

- A clean ext4 image used strictly as a read-only origin.
- A single regular file occupying one 4096-byte ext4 data block.
- A different 4096-byte replacement payload.

## Generated artifacts

### `shadow.pack`

For Stage 0 this is exactly one block-aligned replacement object. It is deliberately not a filesystem image.

### Loom map

The in-memory map is an ordered, gap-free set of sector extents. A single replacement produces at most three extents:

```text
[origin before replacement]
[shadow replacement]
[origin after replacement]
```

Every extent identifies:

- logical start sector;
- sector count;
- source (`origin` or `shadow`);
- source start sector.

### Device-mapper table

Stage 0 lowers the Loom map to the standard Linux `dm-linear` target. This is a provisional runtime implementation, not a permanent Loom ABI requirement.

## Test procedure

1. Create a 64 MiB ext4 image using 4096-byte blocks.
2. Write `/payload.bin` as exactly one filesystem block.
3. Run `e2fsck` and record the stock image SHA-256.
4. Locate the physical ext4 block backing `/payload.bin` using `debugfs`.
5. Build a 4096-byte `shadow.pack` containing different bytes.
6. Expose the origin and shadow files through read-only loop devices.
7. Compile a three-part Loom map to a `dm-linear` table.
8. Create the effective device-mapper device.
9. Mount the effective device as native ext4 with `ro,noload`.
10. Verify `/payload.bin` matches the replacement block.
11. Unmount and run a read-only ext4 consistency check on the effective device.
12. Verify the stock image SHA-256 is unchanged.
13. Read `/payload.bin` directly from the stock image and verify the original bytes remain.

## PASS criteria

Stage 0 passes only if all conditions hold:

- the effective view exposes replacement bytes;
- the stock SHA-256 is identical before and after the experiment;
- the stock file remains unchanged;
- the effective ext4 filesystem is structurally valid;
- no write-capable origin mapping is required;
- teardown removes the Loom view without modifying the origin.

## FAIL / rejection criteria

The current Fabric direction must be reconsidered if any of the following is required merely to replace a same-size unprotected ext4 data block:

- writing to the origin filesystem;
- changing its inode or extent metadata;
- replaying its journal;
- copying the whole filesystem image;
- implementing a custom VFS filesystem;
- intercepting pathname lookup, `read`, `mmap`, or `exec`;
- patching ext4 itself.

## Out of scope

Stage 0 does not claim to solve:

- file growth or allocation;
- add/remove/rename operations;
- ext4 metadata checksums caused by structural changes;
- fs-verity-protected files;
- EROFS compression/pcluster weaving;
- Android dynamic partitions and AVB integration;
- KernelSU metamodule lifecycle;
- generation switching and automatic rollback.

Those become later stages only after this proof is green.
