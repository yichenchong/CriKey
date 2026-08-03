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
scripts/       Developer maintenance scripts
docs/          Specification, architecture, ADRs, roadmap
packaging/     Per-platform distribution artefacts
```

## Build

```sh
cargo test --workspace --all-targets
cargo run -p crikey-cli -- version
```

### Disk use, and why it matters here

`cargo test --workspace --all-targets` links roughly a hundred separate test
executables, several of which statically link the graphics, windowing and
clipboard libraries. Two things follow, and both have bitten this project.

First, the development profile in the root `Cargo.toml` deliberately keeps
debug information small: workspace crates get line tables only, and
dependencies get none. Under Cargo's defaults the same command produced over
45 GB and exhausted a development machine's disk. With these settings the whole
test set is about 2 GB. Do not "restore" full debug information across the
workspace without measuring what it costs.

Second, Cargo never removes superseded build output. Each time a source file
changes, the previous artefacts stay on disk under their old content hash, so a
long editing session accumulates them. One session left 306 test executables in
`target/debug/deps` for a workspace that has about 98, including eight stale
copies of the main binary. To reclaim that:

```sh
scripts/prune-build-cache.sh            # prune if target/debug exceeds 5 GB
scripts/prune-build-cache.sh --dry-run  # report only
scripts/prune-build-cache.sh --force    # prune regardless of size
```

It deletes only `target/debug`, so the cost is one rebuild. Release output,
cross-compilation directories and the out-of-tree plugin fixture are left
alone, because they are expensive to rebuild and are not rewritten on every
edit. It refuses to run while a build is in progress.

To have it happen without being asked, install the timer described in
[docs/development.md](docs/development.md).

### Python

The Python software development kit and the legacy compatibility shim are
driven by real interpreters during tests. Use a virtual environment rather than
a system-wide interpreter:

```sh
python3 -m venv .venv
.venv/bin/python -m compileall -q sdk/python crates/crikey-legacy-compat/python
```

`.venv/` is ignored by git. Set `CRIKEY_PYTHON` to point the interpreter
discovery at a specific binary.

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
