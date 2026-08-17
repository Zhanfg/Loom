# Loom Alpha 8 — boot-scoped early activation attestation

Status: source-side confirmation-safety gate before AOSP first-stage takeover.

## Why Alpha 6 is not enough by itself

Alpha 6 prevents a new early generation from receiving more than one unconfirmed attempt. Alpha 7 proves the attempted marker is durable before DM creation and that a failed upgrade can rematerialize the last-good filesystem view.

A remaining confirmation hazard is a stale activation record. If an early candidate created an `active-generation` marker and then failed before userspace confirmation, a later stock or last-good boot must not be able to mistake that old marker for proof that the candidate survived the current boot.

## Boot epoch

Alpha 8 binds activation to the kernel boot id.

A future first-stage host performs:

```text
begin(state, /proc/.../boot_id)
    ↓
write current-boot=<boot_id>
    ↓
clear stale active record
    ↓
recovery decide
    ↓
create effective DM
    ↓
activate(state, boot_id, generation, action)
```

`begin` commits the new boot id before removing stale `active`. Even if stale-record cleanup fails, an old active record cannot match the new boot epoch.

## Active record

After the effective DM has been created successfully, but before the native filesystem mount is attempted, the host may write:

```text
boot_id=<kernel boot id>
generation=<generation>
action=candidate|confirmed|last-good
```

Authorization is checked against Alpha 6 state:

- `candidate` requires `attempted=<generation>`;
- `confirmed` requires `confirmed=<generation>`;
- `last-good` requires `confirmed=<generation>`.

An arbitrary generation therefore cannot be declared active by the attestation helper.

## Userspace verification

At the later health/boot-completed boundary, userspace must first read the current kernel boot id and call `verify`.

Verification succeeds only when:

- supplied boot id is syntactically valid;
- `current-boot` matches it;
- `active.boot_id` matches it;
- generation/action are structurally valid;
- the Alpha 6 state still authorizes the action/generation pair.

Only an `action=candidate` result for the same boot may proceed to Alpha 6 `confirm`. A last-good fallback must never confirm the still-desired failed upgrade.

## Stale marker safety

A previous boot's active record is invalid in a new kernel boot even if the file physically survives. This property remains true if cleanup was interrupted because verification compares both current and active boot ids with the actual current boot id.

## Boundary

Alpha 8 still does not patch Android first-stage init or replace `/system`. It closes the final userspace-confirmation ambiguity so the subsequent AOSP integration can report the generation actually used by the current boot without inventing a second confirmation protocol.
