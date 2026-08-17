# Loom Android Alpha 3 — flashable block generation

Status: first flashable development boundary.

## What this build is

Alpha 3 packages the proven Loom block-view compiler into an installable KernelSU/Magisk-style module and adds a multi-module generation layer.

The primary filesystem path remains:

```text
read-only stock block device
        +
ordinary module system/ payloads
        ↓
Loom filesystem compiler
        ↓
sparse shadow blocks + Loom map
        ↓
read-only dm effective device
        ↓
native EROFS driver
```

OverlayFS, Magic Mount and per-file bind mounts are not LoomFS backends and are not used to construct this view.

## Module composition

The first composer scans `/data/adb/modules` and considers only ordinary enabled modules that:

- have `module.prop`;
- have a `system/` tree;
- do not contain `disable`, `remove` or `skip_mount`;
- are not Loom itself;
- are not another metamodule.

For the first deterministic contract, module ids are processed lexically and a later module wins when two modules provide the same pathname.

The resulting final regular-file tree is compiled against the current effective block origin. Unsupported semantics such as symlinks, device nodes, fifos, sockets and whiteout-style objects fail the whole generation closed instead of silently falling back to a VFS overlay.

## Generation transaction

A generated view has one id and one boot transaction:

```text
PREPARED
  -> ACTIVE_PENDING_BOOT
  -> COMMITTED (boot-completed)
```

`pending-generation` is written before block-view activation. If that marker survives into a later boot, Loom enters `recovery-hold`, tears down its own validation view, and refuses automatic reactivation until explicitly resumed.

This establishes the recovery contract needed by later first-stage takeover work without making the current validation build capable of replacing `/system`.

## Current mount boundary

Alpha 3 still mounts the final effective EROFS only below:

```text
/data/adb/loom/mnt/system-generation
```

`LOOM_TAKEOVER=0` is mandatory and any other value is rejected.

This is deliberate. On modern Android, the real system/vendor code partitions are mounted by first-stage init before normal second-stage `init.rc` processing. Therefore a KernelSU module script or ordinary `initrc/` injection is not a valid implementation of the final LoomFS takeover invariant.

The final path must instead establish or substitute the Loom effective block device before the native filesystem driver performs the first system mount. That future bootstrap is a separate device-proven gate.

## Why this is flashable but not called takeover-ready

The ZIP is a real module package: it can be installed, booted, execute preflight/service/boot-completed hooks, discover modules, compile an effective block view, mount the validation view and recover its own resources.

It does **not** claim that Android itself is already booting from that view. Enabling a fake VFS overmount merely to claim takeover would violate the LoomFS architecture.

## Source profiles

The default release profile uses the already validated Android direct-block source branch:

```text
feat/android-block-origin-alpha2
```

The packaging workflow can also build the Stage 43 compiler and applies the narrowly scoped Android block-device geometry patch before compilation. The patch is checked with `git apply --check`; a mismatch fails the build rather than producing an ambiguous binary.

## Next gates

1. Produce and validate the Alpha 3 arm64 module ZIP.
2. Run the composed-generation runtime on a real KernelSU device with takeover disabled.
3. Collapse per-file layered dm-linear materialization into a single `dm-loom`-style map target or equivalent single-device implementation.
4. Implement a first-stage block handoff that runs before Android mounts the system filesystem.
5. Integrate the early safety gate and kernel payload runtime only after the block takeover path is recoverable.
6. Promote Loom to the active KernelSU metamodule only when it can faithfully replace ordinary module mounting rather than disabling users' existing mount semantics.
