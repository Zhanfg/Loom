# Loom Alpha 6 — one-shot early recovery protocol

Status: source-side safety gate before any first-stage takeover.

## Why this exists

Alpha 5 proves that Loom can reconstruct an early effective filesystem from the verified system origin plus raw ext4 `/metadata` sectors, without `/data`, a shadow loop, OverlayFS or Magic Mount.

That makes a true pre-first-mount handoff possible, but KernelSU's ordinary safe mode occurs too late to rescue a filesystem or kernel failure that prevents Android from reaching normal module userspace. Alpha 6 therefore adds an independent, durable recovery protocol that a future first-stage host must obey.

The implementation is a narrow C helper rather than another general runtime. Early recovery is one of the explicitly permitted low-level compatibility surfaces in Loom: it must remain usable before the normal Rust userspace and `/system` are available.

## Durable state

The protocol owns:

```text
desired
attempted
confirmed
failed
force-stock
```

- `desired` — generation explicitly selected for early activation.
- `attempted` — candidate whose first-stage activation was durably started but not yet confirmed.
- `confirmed` — last generation known to have survived the userspace confirmation boundary.
- `failed` — desired generation quarantined after an unconfirmed attempt.
- `force-stock` — emergency override that forbids every Loom early generation.

State writes use create-new temp file, write, `fsync`, rename and directory `fsync`.

## One-shot algorithm

Before any future first-stage redirect:

1. `force-stock` present → stock.
2. missing desired state → stock.
3. corrupt recovery state → stock.
4. invalid/tampered desired snapshot → confirmed last-good if valid, otherwise stock.
5. desired already confirmed → confirmed generation.
6. desired already failed → last-good or stock; never automatic retry.
7. desired still has `attempted` but is not confirmed → persist `failed`, clear attempted, then use last-good or stock.
8. otherwise persist `attempted=desired` first, then return the candidate.

If the attempt marker cannot be written, the decision is stock. If quarantine state cannot be written, the decision is stock. Recovery metadata failure itself therefore does not authorize a risky early redirect.

## Confirmation

`confirm` is accepted only when:

- generation id is valid;
- the snapshot passes content verification;
- `desired` exactly matches the generation;
- the generation has a matching `attempted` marker, or is already the same confirmed generation for idempotence.

This deliberately prevents Alpha 5 sidecar success from being misreported as an early-boot success.

## Snapshot integrity

Before a snapshot can be attempted or confirmed, the C helper reopens regular files with `O_NOFOLLOW` and checks:

- descriptor generation;
- descriptor state `PREPARED_NOT_ACTIVE` or `CONFIRMED`;
- `LOOM_SHADOW_SHA256`;
- `LOOM_EXTENTS_SHA256`;
- `LOOM_TABLE_SHA256`;
- actual SHA-256 of `shadow.pack`;
- actual SHA-256 of `shadow.extents`;
- actual SHA-256 of `early.table`.

The SHA-256 implementation is dependency-free and embedded in the helper so this integrity check does not depend on `/system` utilities.

## Explicit re-arm

`arm <generation>` sets the new desired generation and clears its current attempted/failed state while preserving the previously confirmed last-good generation. A quarantined generation therefore receives another attempt only after explicit re-arm.

## Force stock

`force-stock on` is dominant over desired, attempted, confirmed and failed state. This is the primitive a future key/safe-mode bridge can set without deleting snapshots or reflashing boot.

## Boundary

Alpha 6 still does not patch first-stage init, redirect `/system`, create the real first-stage Loom DM device, or run embedded KPM code. It establishes the recovery protocol those features must consume.

No later takeover implementation is allowed to add a separate hidden retry counter or bypass the durable `attempted` marker.
