# Loom Android Alpha 4 — single-DM generation

Status: flashable validation boundary after Alpha 3.

## Objective

Alpha 3 proved that Loom can compile enabled ordinary module `system/` trees into one transactional block-level generation. Its steady-state implementation still retained one device-mapper layer per materialized replacement.

Alpha 4 removes that persistent N-layer graph without changing the filesystem compiler.

The compile path is:

```text
read-only Android origin
        +
module replacement 1
        -> temporary dm layer 1
        +
module replacement 2
        -> temporary dm layer 2
        +
...
```

Later replacements must see earlier effective changes, so these layers are allowed during the compile transaction.

Before the generation is accepted, `loom-flatten` recursively composes every complete dm-linear table and sparse shadow pack into:

```text
read-only authoritative origin
        +
one compact aggregate shadow pack
        ↓
one complete dm-linear table
        ↓
one stable read-only effective DM device
        ↓
native EROFS
```

All transient layer DM objects and loop devices are removed only after the flat device has been created and mounted successfully.

## Why this precedes `dm-loom`

A custom `dm-loom` target is not required to prove the mapping semantics. The existing Linux `linear` target can express the final two-source map exactly.

Alpha 4 therefore proves and hard-gates the userspace mapping algebra first. A future kernel target can consume the same LoomMap-style mapping while reducing table/setup overhead, without also changing filesystem compilation semantics at the same time.

## Failure model

Flattening is a commit boundary, not a best-effort optimization.

If aggregate-shadow creation, table validation, loop attachment, final DM creation, unmount, or final mount fails:

1. the flat resources are removed;
2. the transient layered validation view is removed;
3. no layered fallback remains mounted silently;
4. the specific `SHADOW_FLATTEN_*` failure status is retained for diagnosis.

The normal Alpha 3 generation pending/boot-completed/recovery-hold transaction remains unchanged above this layer.

## Safety boundary

Alpha 4 still has:

```text
LOOM_TAKEOVER=0
```

The final device is mounted only below Loom's validation mountpoint. Android first-stage `/system` source replacement is not claimed here.

No OverlayFS, Magic Mount, or per-file bind mount is introduced.

## Validation

The source hard gate contains a privileged loop/device-mapper equivalence proof. It creates a real two-layer chain with overlapping replacements, flattens it, and byte-compares the complete flat device against the original chain.

The Android packaging gate separately checks wrapper transaction behavior, intermediate-resource teardown, failure rollback, AArch64 `loom`/`loom-flatten` binaries, and archive metadata.

## Next gate

After Alpha 4 is green on host and device validation, the next architectural step is pre-first-mount block handoff. That work must make Android's native first EROFS mount consume the Loom effective block device; it must not simulate takeover with a VFS overmount.
