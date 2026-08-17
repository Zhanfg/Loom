# Loom unified kernel runtime architecture

Status: design baseline for `feat/unified-kernel-runtime`.

## 1. Product invariant

Loom must remain one module and one lifecycle. Kernel extensions are not implemented as a second root/module stack mounted beside Loom.

The installed product owns one state machine:

```text
install / update
  -> preflight
  -> optional boot-kernel preparation
  -> reboot boundary when required
  -> kernel-runtime handshake
  -> embedded extension reconciliation
  -> runtime extension reconciliation
  -> filesystem effective-view construction
  -> mount / health verification
  -> steady state
  -> shutdown / rollback
```

A separate `kpatch` module, separate KPM autoload service, or separate rehook state file is not part of the target architecture. Compatibility with legacy KPM binaries is an ABI concern inside Loom, not an external runtime dependency.

## 2. Why the old split model is insufficient

The historical PatchNest family is useful as a failure reference. It separated the module installer/WebUI, userspace `kpatch` client, kernel core, and KPM catalog into independent repositories and dependency chains. That separation made it possible for orchestration features to exist in the shell layer without a matching userspace/kernel ABI.

Concrete examples from the restored tree include:

- module metadata records events and boot scripts try to publish lifecycle events;
- the pinned userspace client does not implement the corresponding event command;
- the kernel `report_user_event()` path only logs an event instead of routing it to subscribed modules;
- file-based KPM loading hard-codes the module init event to `load-file`;
- load/control arguments are string-shaped rather than a versioned typed message;
- rehook is represented as one global enable/disable state instead of hook ownership and dependency state.

Loom must therefore make lifecycle, event delivery, parameter transport and hook ownership kernel-visible protocol concepts rather than shell conventions.

## 3. Runtime layers

### 3.1 Android module layer

The existing KernelSU module remains the only installed control plane. It owns:

- installation and update transactions;
- Android boot-stage entry points;
- boot image preparation when a kernel-runtime injection is required;
- sparse-shadow filesystem construction;
- kernel-runtime handshake and reconciliation;
- extension registry and persistent policy;
- health state, quarantine and rollback;
- WebUI/CLI state exposure.

### 3.2 Userspace runtime

The long-term CLI/runtime is `loom`; no second public `kpatch` command is required.

Userspace responsibilities:

- serialize versioned runtime messages;
- validate extension manifests and compatibility constraints;
- stage runtime or embedded extension content;
- reconcile desired state with kernel state;
- persist policy, but never treat a policy file as proof that kernel state succeeded;
- convert legacy KPM operations into the Loom runtime ABI when compatibility mode is used.

### 3.3 Kernel runtime

The kernel-side runtime is deliberately smaller than a root framework. KernelSU continues to provide root. Loom's kernel runtime provides only the facilities needed by Loom extensions:

- version/capability handshake;
- extension loader and registry;
- typed request/reply transport;
- event subscription and dispatch;
- hook ownership/lease management;
- embedded/runtime extension reconciliation;
- bounded logging/state export;
- fail-closed unload and rollback primitives.

It must not duplicate KernelSU policy, UID authorization or root management.

## 4. One extension model for load and embed

`load` and `embed` are persistence/boot-timing policies for the same logical extension. They must not expose different callback ABIs.

Every extension has one identity and one state record:

```text
id
version
abi
payload digest
desired persistence: runtime | boot-autoload | embedded
desired state: enabled | disabled
actual state: absent | staged | loaded | active | failed | quarantined
subscriptions
hook requirements
dependencies
last transaction / error
```

### Runtime load

A runtime extension is validated in userspace, loaded into the kernel runtime, initialized, subscribed to events and then committed to the registry.

### Boot autoload

Boot autoload reuses the runtime loader after the Loom kernel handshake. Persistence is provided by Loom's registry; no ad-hoc directory scan defines truth.

### Embed

An embedded extension is placed into the boot/kernel payload together with a manifest entry. At boot it is registered through the same extension registry and callback ABI as a dynamically loaded extension. Userspace later reconciles the embedded instance against desired state.

Embedding therefore changes *when and where bytes are supplied*, not the extension programming model.

## 5. Versioned message transport

The old single-string `args`/`ctl_args` model is not sufficient. Loom uses one versioned message envelope followed by zero or more aligned typed attributes.

The initial UAPI is declared in `kernel/uapi/loom_runtime.h`.

A request carries at minimum:

- ABI major/minor;
- opcode;
- request sequence;
- extension handle;
- event id when applicable;
- bounded payload length;
- typed attributes.

Attributes can be repeated, so an operation can carry multiple parameters. Standard keys occupy the low key range and extension-private keys use a separate range. The kernel copies the whole request into kernel-owned memory before parsing it; user pointers are never retained by an extension.

This transport is used for load, unload, control, events, capability negotiation and hook management. A legacy KPM compatibility shim may expose the historical string callbacks, but the Loom control plane does not reduce its own ABI to that format.

## 6. Event model

Events are first-class routed messages, not strings passed only to module init.

The core provides a registry:

```text
(event_id, priority) -> subscriber handles
```

An extension declares subscriptions during registration. Dispatch semantics are deterministic:

1. snapshot the subscriber set;
2. order by priority and stable registration order;
3. deliver a read-only event message to each subscriber;
4. collect result/status without allowing one extension to corrupt the next subscriber's payload;
5. record failures against the owning extension;
6. apply event-specific failure policy.

Initial lifecycle events include core-ready, post-fs-data, filesystem-view-ready, boot-completed, suspend, resume and shutdown. Custom event ids are namespaced and versioned.

Kernel-originated events may use the same internal envelope. A bounded queue/poll opcode can later expose asynchronous notifications to userspace without requiring another privileged daemon protocol.

## 7. Hook and rehook model

A single global `rehook_enabled` boolean is not sufficient once multiple extensions depend on hooks.

Loom models hooks as owned resources:

```text
hook identity
hook method
target
owner extension(s)
reference count
priority / ordering
conflict policy
installed generation
health state
```

Extensions request hook leases. The core installs a physical hook only after validating all requested leases. Removing or disabling one extension releases only that extension's lease; the hook remains while another owner still requires it.

`rehook` becomes reconciliation, not a toggle:

```text
expected hook graph
  vs
observed hook graph
  -> repair only missing/stale generations
  -> never tear down unrelated owners
```

Global emergency disable remains possible but is a recovery operation, not normal module control.

## 8. Transaction and rollback boundary

Kernel and filesystem work share one Loom transaction id.

A transaction records each reversible action, for example:

```text
boot payload staged
kernel handshake established
extension A loaded
hook lease X acquired
shadow layer 1 created
shadow layer 2 created
mount committed
```

Failure unwinds only actions owned by that transaction in reverse order. Boot-image activation is a separate explicit commit boundary and must preserve slot/original-image rollback metadata.

An extension that fails validation or init is retained for diagnosis and may be quarantined; failure must not silently delete its payload.

## 9. Legacy KPM compatibility

Compatibility is useful, but it is not the architecture.

A legacy loader may recognize existing KPM sections such as `.kpm.init`, `.kpm.ctl0`, `.kpm.ctl1` and `.kpm.exit` and expose them through an adapter:

- legacy init receives a synthesized compatibility event;
- legacy `ctl0` receives one serialized string only when the caller explicitly chooses legacy control;
- legacy `ctl1` remains isolated from the native typed message ABI;
- legacy modules cannot claim unsupported native capabilities.

New Loom-aware extensions use a descriptor/event ABI rather than relying on the legacy init-event/string-control contract.

## 10. Implementation order

1. Freeze and test the versioned userspace/kernel message envelope.
2. Add userspace encoder/decoder and a fake kernel transport for CI.
3. Add kernel handshake/capability table.
4. Add extension registry with legacy KPM loader compatibility.
5. Replace string-only control with native typed control messages.
6. Add event subscription/dispatch and lifecycle events.
7. Add hook lease graph and reconciliation-based rehook.
8. Unify runtime/autoload/embedded state records.
9. Integrate Android module lifecycle and sparse-shadow transaction ids.
10. Only after the above passes CI and device probes, enable an explicit boot/kernel injection path.

Until step 10 is verified on-device, the current filesystem Alpha remains independently usable and kernel-runtime activation must fail closed.
