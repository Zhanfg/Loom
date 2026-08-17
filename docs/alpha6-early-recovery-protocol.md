# Loom Alpha 6 — one-shot early recovery protocol

Status: source-side safety gate before any first-stage takeover implementation.

## Problem

Alpha 5 proves that a LoomFS aggregate shadow can be represented before `/system` is mounted using only:

```text
verified system origin
+
raw ext4 /metadata sectors
→ effective DM view
```

That makes a true first-stage handoff technically possible, but it is not yet safe enough to enable. KernelSU's ordinary module safe mode is reached later in boot and cannot rescue arbitrary code or a broken filesystem view that prevents the system from reaching that stage.

Alpha 6 therefore defines an independent early recovery protocol stored alongside the early snapshots.

## Persistent state

The protocol owns five small durable state files:

```text
desired
attempted
confirmed
failed
force-stock
```

They have distinct meanings:

- `desired`: generation explicitly selected for early use;
- `attempted`: candidate whose first-stage redirect was durably started but has not yet been confirmed by userspace;
- `confirmed`: last generation known to have reached the confirmation point;
- `failed`: desired generation quarantined after an unconfirmed attempt;
- `force-stock`: emergency override that forbids every Loom early generation.

State changes use write + file sync + rename + directory sync. A future first-stage host must persist `attempted` **before** redirecting the system mount source.

## Decision algorithm

At early boot:

1. `force-stock` present → use stock immediately.
2. no `desired` → use stock.
3. desired snapshot fails structural or content-integrity validation → use confirmed last-good if valid, otherwise stock.
4. `desired == confirmed` → use the confirmed generation normally.
5. `failed == desired` → never retry automatically; use last-good or stock.
6. `attempted == desired` while it is not confirmed → the previous boot did not finish confirmation:
   - mark the generation `failed`;
   - clear `attempted`;
   - use last-good or stock.
7. otherwise this is the first attempt:
   - durably write `attempted=desired`;
   - only then return the candidate generation to the future handoff code.

This makes every newly armed generation one-shot by default.

## Confirmation

Confirmation is a userspace action after the actual early generation has survived the required boot health boundary.

It is accepted only when:

- the snapshot is still valid;
- its real `shadow.pack`, `shadow.extents` and `early.table` SHA-256 values match `descriptor.env`;
- the confirmed generation is still the current `desired` generation.

Confirmation writes `confirmed=<generation>` and clears `attempted` and `failed`.

Alpha 6 source does not pretend an Alpha 5 sidecar generation has already executed in first stage, so no Android packaging path auto-confirms an early generation yet.

## Snapshot verification

Before a generation can be returned from `decide`, Alpha 6 requires:

- path-safe generation id;
- regular `descriptor.env`, `shadow.pack`, `shadow.extents`, `early.table` files;
- matching descriptor generation;
- descriptor state `PREPARED_NOT_ACTIVE` or `CONFIRMED`;
- SHA-256 fields for shadow, extents and table;
- actual dependency-free safe-Rust SHA-256 recomputation of all three files;
- exact digest equality.

A modified payload is rejected before an `attempted` marker is created.

## Explicit re-arm

A quarantined generation is not permanently banned. An explicit `arm <generation>` operation:

- sets `desired`;
- clears that candidate's current attempted/failed state;
- preserves the previously confirmed last-good generation.

The candidate then receives exactly one new attempt.

## Force stock

`force-stock on` is dominant over all desired/confirmed state. This is the primitive a future early key/safe-mode bridge can set without deleting snapshots or rewriting boot.

## Boundary

Alpha 6 still does not patch first-stage init, create a first-stage Loom DM device, replace `/system`, or run embedded KPM code.

Its purpose is to make the recovery decision deterministic and durable **before** those capabilities are introduced.

The next takeover implementation must consume this protocol rather than inventing a second boot counter or auto-retry mechanism.
