# CriKey

A fast, keyboard-driven application launcher with a Rust core, isolated plugin
workers, and a Legacy Compatibility Layer that runs existing Keypirinha
plugins. Windows-only parts of that older interface are reported as unavailable
on other platforms rather than emulated.

> CriKey is an independent project and is not affiliated with, endorsed by, or
> sponsored by Keypirinha or its developer. See [NOTICE.md](NOTICE.md).

## Status

Milestones M0 through M5 are complete, and M6 is implemented but not fully
verified. [docs/ROADMAP.md](docs/ROADMAP.md) is the authoritative, measured
status; this is the summary.

- **M0, M1 — core launcher.** Query engine, catalog with a persistent on-disk
  cache, ranking, result aggregation, native launcher runtime, Linux
  application discovery, startup staging and supervisor state.
- **M2 — scheduling and resilience.** Manifest-resolved policy, separate modern
  and legacy scheduling, cancellation, stale-result rejection, bounded and fair
  request and result queues, and deterministic developer traces, all wired
  through the live query pipeline.
- **M3 — Legacy Compatibility Layer.** Loads package directories and
  `.keypirinha-package` archives, runs their plugins in supervised child
  CPython processes under `legacy-strict` scheduling, reproduces the documented
  `keypirinha` module surface, and serves their suggestions through the live
  pipeline without ever blocking the user-interface thread.
- **M4 — modern Python plugins.** Per-plugin interpreter processes with
  isolated import paths, a dependency resolver and lockfile pinning each
  package by digest, and the published Python SDK.
- **M5 — native plugins.** The versioned inter-process schema, the supervised
  native host, the published Rust SDK, and an out-of-tree conformance fixture
  built the way a third party builds a plugin.
- **M6 — additional platforms.** Windows, macOS and Linux backends behind
  common interfaces with per-desktop capability reporting. Implemented, but see
  the known gaps below.

### Known gaps

These are stated here because they are easy to assume working:

- **Windows and macOS are not runtime-verified.** Their backends type-check for
  their targets and are covered by portable tests, but no part of this
  repository's verification executes them on Windows or macOS. Warm activation
  on Windows from hotkey to presented frame is unmeasured.
- **The Linux launcher registers no global shortcut.** The Linux hotkey backend
  works and is proven by synthetic key presses against a real X server, but the
  `crikey run` entry point registers the live shortcut only under its Windows
  configuration.
- **Plugin permissions are recorded, not enforced.** A manifest declares
  permissions and the loader stores them, but of the twelve only the network
  permission has a consumer. Do not read a manifest as a statement of what a
  plugin is confined to.
- **Some other manifest fields are declaration-only too.** The performance
  hints (startup, soft and hard suggestion timeouts, maximum results per query
  and per batch) and the supervisor deadline profiles are parsed and validated
  but do not yet affect runtime behaviour. The concurrency budgets, by
  contrast, *are* enforced, at real dispatch sites.
- **Composition input is unsupported on Linux.** The pinned windowing
  integration deliberately discards those events, so input methods that need
  composition do not work there.
- **Ranking does not learn.** History, context and preference inputs exist and
  are scored, but nothing supplies them, so ranking is match quality only.

See [docs/architecture.md](docs/architecture.md) for the component map and
[docs/development.md](docs/development.md) for how to build and test the tree.

## Layout

```text
crates/        Rust workspace: core, scheduler, hosts, platform backends, CLI
sdk/rust       Official Rust SDK for native plugins
sdk/python     Official Python SDK for modern plugins
sdk/protocol   Versioned IPC schema for native plugins (not modern Python)
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

It empties only `target/debug`, so the cost is one rebuild. Release output,
cross-compilation directories and the out-of-tree plugin fixture are left
alone, because they are expensive to rebuild and are not rewritten on every
edit. It takes Cargo's own build lock and holds it while it works, so it will
not delete artefacts out from under a running build.

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
7. A capability is never *reported* as available, and a declared value is never
   *implied* to be enforced, without a working implementation behind it. Some
   manifest fields are deliberately accepted but not yet consumed — several
   permissions, the performance hints, the deadline profiles — and those are
   documented as declaration-only, both here under Known gaps and at their
   definitions. Parsing a field ahead of its consumer is allowed; letting a
   reader believe it does something is not. Advertising substance that does not
   exist is the single most common defect this codebase has produced.
8. Anything arriving from a plugin — archive entries, wire frames, interpreter
   output, package digests — is validated before use, with bounded reads and
   allocations, and rejected rather than partially applied.

## Licence

Apache-2.0. See [LICENSE](LICENSE).
