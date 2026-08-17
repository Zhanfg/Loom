# Loom Alpha 8 — boot-scoped early activation attestation

Status: source-side confirmation-safety gate before AOSP first-stage takeover.

## Problem

Alpha 6 gives new early generations one-shot recovery and Alpha 7 proves mapper creation happens only after `attempted` is durable. A stale activation marker from a failed previous boot could still be dangerous if later userspace mistook it for proof that the current boot used that generation.

Alpha 8 binds activation to the current kernel boot id.

## Boot sequence contract

A future first-stage host must execute:

```text
previous userspace: arm desired generation

new boot:
  begin(state, current kernel boot_id)
      -> commit current-boot first
      -> clear stale active
  recovery decide
      -> candidate writes attempted before return
  build effective DM
  activate(state, boot_id, generation, action)
  native filesystem mount

later userspace:
  verify(state, current kernel boot_id)
  only same-boot candidate may be confirmed
```

Committing the new boot id before removing old `active` means an interrupted cleanup cannot make an old record valid in the new boot.

## Active authorization

`activate` accepts only:

- `candidate` when `attempted=<generation>`;
- `confirmed` when `confirmed=<generation>`;
- `last-good` when `confirmed=<generation>`.

The active record contains:

```text
boot_id=<uuid>
generation=<generation>
action=candidate|confirmed|last-good
```

## Verification

`verify` requires all of the following:

- valid supplied kernel boot id;
- matching `current-boot`;
- a regular, parseable `active` record;
- matching active boot id;
- valid generation/action;
- recovery state still authorizing the action/generation pair.

A copied or stale previous-boot `active` file therefore cannot authorize confirmation.

Only `action=candidate` should lead to a new Alpha 6 confirmation. `confirmed` is already last-good; `last-good` is explicitly a fallback for a different desired generation and must never confirm that failed upgrade.

## Boundary

Alpha 8 still does not modify AOSP first-stage init, AVB, fstab or `/system`. It closes the boot-scoped confirmation gap before the first actual takeover patch is introduced.
