# CriKey

A fast, keyboard-driven application launcher with a Rust core, isolated plugin
workers, and a Legacy Compatibility Layer that runs existing Keypirinha plugins
on Windows.

> CriKey is an independent project and is not affiliated with, endorsed by, or
> sponsored by Keypirinha or its developer. See [NOTICE.md](NOTICE.md).

## Status

M0 and M1 are complete: core search, catalog persistence, native launcher
runtime, Linux discovery, startup staging, supervisor state, and the Windows
hotkey/application backends are implemented. Two M1 items are carried to M6
rather than claimed here — executing the Win32 backends in an interactive
Windows session, and measuring warm activation end to end on Win32 from hotkey
delivery to the presented frame on hardware with a real GPU and compositor.
M2 scheduling and resilience is complete:
manifest-resolved policy, modern and legacy scheduling, cancellation, stale
rejection, bounded/fair request and result queues, and deterministic developer
traces are wired through the live query pipeline.

M3 is complete: the Legacy Compatibility Layer loads Keypirinha package
directories and `.keypirinha-package` archives, runs their plugins in supervised
child CPython processes under `legacy-strict` scheduling, reproduces the
documented `keypirinha` module surface, publishes a tested compatibility matrix
and referenced plugin corpus, and serves legacy suggestions through the live
query pipeline without ever blocking the UI thread. Windows execution of the
Win32-only surface is reported honestly as unavailable rather than simulated.

See [docs/ROADMAP.md](docs/ROADMAP.md) for measured status and
[docs/architecture.md](docs/architecture.md) for the component map.

## Layout

```text
crates/        Rust workspace: core, scheduler, hosts, platform backends, CLI
sdk/rust       Official Rust SDK for native plugins
sdk/python     Official Python SDK for modern plugins
sdk/protocol   Versioned IPC schema shared by all out-of-process plugins
compatibility/ Legacy API matrix, synthetic test plugins, real-plugin corpus
plugins/       First-party built-in plugins
benchmarks/    Synthetic workloads and performance harnesses
docs/          Specification, architecture, ADRs, roadmap
packaging/     Per-platform distribution artefacts
```

## Build

```sh
cargo test --workspace --all-targets
cargo run -p crikey-cli -- version
```

## Design invariants

These hold everywhere in the tree; violating one is a bug, not a tradeoff.

1. Third-party plugin code never runs in the CriKey UI process.
2. The UI thread never blocks on plugin work.
3. Every query state has a monotonically increasing generation; stale results
   are never displayed.
4. `legacy-strict` plugins are never time-debounced, never host-gated, and
   their callbacks never overlap.
5. Every queue is bounded and has an explicit overflow policy.
6. Platform-independent crates never call a desktop API directly.

## Licence

Apache-2.0. See [LICENSE](LICENSE).
