# Loom Alpha 7 — first-stage handoff proof

Status: source-side integration proof only. No Android first-stage patch is activated by this stage.

## Objective

Alpha 6 established a durable one-shot recovery protocol. Alpha 7 proves the ordering and mapping contract that a future first-stage host must follow before it is allowed to redirect the first system filesystem mount.

The required sequence is:

```text
read desired/attempted/confirmed/failed/force-stock
        ↓
verify snapshot bytes
        ↓
choose stock / last-good / candidate
        ↓
if candidate: persist attempted first
        ↓
materialize early.table tokens
        ↓
create one read-only effective DM device
        ↓
hand that device to the native filesystem mount
```

The critical safety invariant is that a candidate mapper must never be created before the attempted marker is durable.

## Strict table materializer

`tools/loom-early-table.c` accepts only complete `linear` mappings whose backing source is one of:

```text
__LOOM_ORIGIN__
__LOOM_METADATA_DEVICE__
```

It rejects:

- arbitrary device names embedded in the prepared table;
- zero-length extents;
- non-contiguous logical coverage;
- numeric/range overflow;
- malformed or overlong rows;
- identical/invalid concrete origin and metadata device paths;
- output overwrite and symlink output paths.

The output is fsynced before it can be consumed by a host.

## Real device-mapper proof

`alpha7_first_stage_handoff_proof.sh` creates:

- a deterministic 64-sector stock origin;
- a metadata block device containing two fragmented shadow ranges;
- a valid Alpha 5-style early snapshot and descriptor;
- an Alpha 6 recovery state directory.

The proof then:

1. arms the generation;
2. calls `decide` and requires a candidate result;
3. verifies `attempted=<generation>` already exists while no concrete table/DM device exists;
4. materializes the two tokens to the real origin and metadata loop devices;
5. creates one DM device;
6. byte-compares the complete effective device with independently built expected bytes;
7. rejects a table that attempts to name an arbitrary block device;
8. withholds confirmation to simulate an early boot failure;
9. on the next decision requires candidate quarantine and stock fallback;
10. requires all later decisions to remain stock until explicit re-arm.

## Boundary

This stage still does not modify Android first-stage init, boot images, fstab, AVB setup, `/system`, or KernelSU. It deliberately proves the host-independent contract before an AOSP integration patch is introduced.

The next stage may integrate with AOSP only after Alpha 6 recovery and Alpha 7 handoff proofs are green. Every AOSP setup failure must keep the original verified block source and continue stock mounting.
