# CriKey

A fast, keyboard-driven application launcher with a Rust core, isolated plugin
workers, and a Legacy Compatibility Layer that runs existing Keypirinha plugins
on Windows.

> CriKey is an independent project and is not affiliated with, endorsed by, or
> sponsored by Keypirinha or its developer. See [NOTICE.md](NOTICE.md).

## Status

M0 skeleton complete. M1 core search, catalog persistence, launcher view model,
Linux discovery, startup staging, and supervisor state are implemented; native
window integration and Windows platform backends remain. See
[docs/ROADMAP.md](docs/ROADMAP.md) for measured status and
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
