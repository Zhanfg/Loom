# Loom Android Alpha 5 — prepare-only early snapshot

Status: flashable preparation boundary for the future pre-first-mount LoomFS handoff.

## What Alpha 5 adds

Alpha 4 already leaves one stable LoomFS device:

```text
authoritative system origin
        +
one aggregate sparse shadow loop
        ↓
one read-only dm-linear device
        ↓
native EROFS
```

That is suitable after Android userspace has started, but a true first-stage handoff cannot depend on `/data` or on setting up a loop device for the shadow file.

Alpha 5 therefore adds an optional, prepare-only snapshot transaction after the current generation reaches `boot-completed` and is committed.

When explicitly enabled, the runtime:

1. takes the committed Alpha 4 aggregate shadow;
2. copies it to `/metadata/loom/early/<generation>/shadow.pack`;
3. syncs the filesystem;
4. captures the ext4 file's physical extents through a strict FIEMAP helper;
5. rewrites the flat Loom map so shadow ranges reference raw physical sectors of the metadata block device;
6. writes `early.table`, `shadow.extents`, hashes and `descriptor.env` next to the sealed snapshot;
7. marks the snapshot `PREPARED_NOT_ACTIVE`.

The resulting early table contains only two abstract sources:

```text
__LOOM_ORIGIN__
__LOOM_METADATA_DEVICE__
```

It contains no `/data` path and no shadow loop dependency.

## Default and safety boundary

Preparation is disabled by default:

```text
LOOM_EARLY_PREPARE_ENABLED=0
LOOM_TAKEOVER=0
```

Enabling preparation does not activate the snapshot during the next boot. Alpha 5 does not modify boot images, first-stage init, the `/system` source device, or KernelSU safe-mode behavior.

A prepare failure is isolated from the active LoomFS generation. It writes only `/data/adb/loom/early-status`; it does not replace the main runtime status or roll back a successfully committed Alpha 4 generation.

Existing snapshot directories are immutable by policy at the generation level: if a directory with the same generation id exists but its recorded shadow digest differs, preparation fails with `EARLY_SNAPSHOT_CONFLICT` instead of overwriting it.

## Why ext4 only for this proof

The raw-sector mapping assumes the prepared file's physical extents remain valid after capture. The first implementation therefore requires an ext4 `/metadata` backing filesystem and rejects FIEMAP states that imply unstable or unsupported mappings, including delayed allocation, unwritten, encoded, shared, inline, merged or unaligned extents.

F2FS is not claimed by this Alpha because background relocation/GC needs a separate stability design before file physical extents can safely become an early-boot ABI.

## Recovery direction

The future first-stage host will consume only a validated descriptor and raw early table. Before that is enabled, the recovery contract must add an early pending/last-good generation marker under `/metadata` so an interrupted early boot automatically bypasses Loom and returns to the original verified system source on the next boot.

Normal KernelSU post-fs-data safe mode remains useful later in boot, but Alpha 5 does not pretend that it can protect code which would execute before that stage.

## Next gate

The next implementation stage is the first-stage handoff itself:

```text
first-stage resolves verified system source
        ↓
validate Loom early descriptor from /metadata
        ↓
substitute __LOOM_ORIGIN__ + __LOOM_METADATA_DEVICE__
        ↓
create Loom effective DM device
        ↓
native first EROFS mount consumes Loom device
```

That stage must retain a fail-open-to-stock path and an automatic last-good recovery path before takeover is allowed on a real device.
