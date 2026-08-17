# Loom

Loom is an experimental systemless meta-module project exploring a single mount mechanism: build an immutable effective block view from a verified/read-only origin plus sparse shadow data, then let the kernel's native filesystem driver consume that view.

## Current status

**Stage 0 / proof of mechanism. Not flashable.**

The current branch proves only the lowest-level fabric invariant:

```text
read-only ext4 origin
        +
4 KiB shadow block
        +
sparse Loom map
        ↓
standard dm-linear table
        ↓
effective block device
        ↓
native ext4 mount
```

Stage 0 intentionally does **not** yet contain:

- an ext4-aware filesystem compiler;
- EROFS compilation;
- KernelSU metamodule integration;
- Android boot integration;
- a custom `dm-loom` kernel target;
- a flashable module package.

## Safety invariant

The origin image/device is an input only. Loom Stage 0 never writes, resizes, discards, repairs, or journals to the origin.

A valid Stage 0 test must demonstrate all of the following:

1. the effective filesystem exposes the shadow content;
2. the stock image SHA-256 is unchanged before and after activation;
3. the stock file still contains its original bytes;
4. the effective ext4 image remains structurally valid;
5. removing the temporary device-mapper view restores the stock path immediately.

## Workspace

```text
crates/loom-types   strong sector/extent types
crates/loom-map     immutable sparse map + dm-linear table compiler
crates/loom-pack    block-aligned shadow pack primitive
crates/loom-cli     Stage 0 command-line frontend
```

The project is Rust-first. C/assembly/eBPF are reserved for narrowly scoped compatibility, architecture-specific, or observability work when there is a demonstrated need.

## Local checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The privileged Linux integration test additionally requires `e2fsprogs`, `dmsetup`, loop devices, and device-mapper:

```sh
bash tests/integration/stage0_ext4_dm_linear.sh
```

GitHub Actions runs the same real ext4/device-mapper experiment on an Ubuntu runner.

## License

GPL-3.0-only.
