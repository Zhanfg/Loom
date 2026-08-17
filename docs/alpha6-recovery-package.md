# Loom Android Alpha 6 — recovery protocol package

Status: flashable recovery-protocol validation build; first-stage takeover remains disabled.

## Included capabilities

Alpha 6 packages the source-side one-shot recovery helper beside the already validated Alpha 5 snapshot pipeline.

The module contains:

```text
loom
loom-flatten
loom-early-map
loom-fiemap
loom-early-state
```

and retains the Alpha 4/5 Android runtimes for generation compilation, single-DM commit and optional raw `/metadata` snapshot preparation.

## Recovery protocol location

The intended future paths are fixed:

```text
snapshots: /metadata/loom/early/<generation>/
state:     /metadata/loom/state/
```

The state helper implements:

```text
desired
attempted
confirmed
failed
force-stock
```

with durable one-shot candidate semantics and content-hash verification.

## No fake activation

This build explicitly does not connect normal Android boot scripts to `arm` or `confirm`.

`recovery.conf` is packaged with:

```text
LOOM_EARLY_AUTO_ARM=0
LOOM_EARLY_AUTO_CONFIRM=0
LOOM_TAKEOVER=0
```

The package builder and archive gate both reject automatic `loom-early-state arm` or `loom-early-state confirm` calls from `post-fs-data.sh`, `service.sh` or `boot-completed.sh`.

This matters because a successful Alpha 5 sidecar boot does not prove an early generation was used. The first real confirmation must come only after a first-stage host reports that the selected generation actually supplied the first system filesystem view and the device survived the required health boundary.

## What can be tested now

The installed `loom-early-state` binary can be exercised manually against test snapshot/state directories to validate storage and syscall compatibility on a device. The default package does not arm a generation and therefore cannot change the device's boot source.

## Next gate

Once source and Android Alpha 6 validation are green, the next implementation is the smallest possible first-stage host:

1. read `/metadata/loom/state`;
2. run the same decision semantics;
3. validate the chosen snapshot;
4. persist `attempted` before redirect;
5. replace early-table tokens with the resolved verified system and metadata devices;
6. create one Loom effective DM device;
7. hand that device to the native first EROFS mount;
8. fall back to stock on every validation/setup failure.

No KPM activation is added until this filesystem takeover and automatic rollback path are device-proven.
