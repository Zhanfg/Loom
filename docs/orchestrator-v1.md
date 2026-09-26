# Loom Orchestrator v1

This branch starts the provider-neutral mount orchestration layer on top of the existing Stage43 filesystem fabric.

## Policy

- OverlayFS is the default stable backend for ordinary module paths.
- NoMount is the preferred mountless backend when mountless visibility, UID isolation, or hot reload is required.
- Kasumi is experimental and is never auto-selected unless experimental providers are explicitly enabled.
- Partial success is not a commit. A failed apply or verification rolls back the whole applied batch.
- Existing EROFS/ext4/block-origin work remains independent and is not replaced by this layer.

## Layers

1. capabilities: backend-neutral feature contract.
2. providers: descriptors for OverlayFS, NoMount, and Kasumi.
3. planner: per-path provider selection with stability gating.
4. transaction: apply -> verify -> commit semantics with reverse rollback.

## Next integration steps

- Add a runtime OverlayFS adapter over the existing Stage43 mount fabric.
- Add a NoMount keyring adapter and protocol/version probe.
- Add backend read-back verification before commit.
- Add a cross-provider transaction coordinator.
- Keep Kasumi behind an explicit experimental gate until device stability improves.
- Add Magic Mount as a compatibility fallback using MMRS/Hybrid semantics.
