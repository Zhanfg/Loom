# CircleCI hard-gate migration

Loom's Linux filesystem integration suite requires host-level loop devices, device-mapper and native filesystem mounts. The CircleCI configuration therefore uses the Linux `machine` executor rather than the Docker executor.

The canonical test entrypoint is:

```bash
bash tests/ci/linux-hard-gate.sh
```

It runs formatting, strict Clippy, workspace/unit tests, the effective-block-store tests, and every filesystem integration stage through Stage 10.

## Executor requirements

The runner must provide:

- Ubuntu Linux with `sudo`;
- loop devices / `losetup`;
- device-mapper / `dmsetup`;
- native ext4 and EROFS mount support;
- `e2fsprogs`, `erofs-utils`, `util-linux`, `kmod`;
- a Rust stable toolchain with rustfmt and Clippy.

The checked-in CircleCI job uses `ubuntu-2404:current` and performs explicit capability checks before running the suite.

## Validation state

The Stage 0-10 suite itself is already proven green on the Stage 10 exact code head before CI migration. The CircleCI wiring must not be called validated until the Loom repository is attached to the intended CircleCI project/runner and the `hard-gate` workflow completes successfully there.

No GitHub Actions fallback is intended after the CircleCI project is attached; `.github/workflows/stage0-linux.yml` is removed in the migration branch.
