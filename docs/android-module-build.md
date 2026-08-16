# Android module build pipeline

The Android packaging workflow produces an arm64 KernelSU/Magisk-style module ZIP from a selected Loom source ref.

## Manual build from GitHub Actions

1. Open **Actions**.
2. Select **Android flashable module**.
3. Choose **Run workflow**.
4. Set `source_ref` to the Loom branch, tag, or commit SHA to package. The default points at the latest verified Android-capable Loom branch until the runtime stack reaches `main`.
5. Download the `Loom-Android-*` artifact after the job completes.

The artifact contains the module ZIP and its SHA-256 file.

## Packaging-alpha boundary

The generated ZIP is structurally installable by KernelSU/Magisk-compatible module managers and contains the Android arm64 Loom binary plus boot hooks. Automatic effective-view activation is intentionally fail-closed until the Android runtime mount bridge is implemented. The service hook leaves stock mounts untouched rather than attempting an unverified mount takeover.

The packaging format and CI workflow are designed to remain stable when runtime activation is added; only the module runtime scripts need to gain activation logic.
