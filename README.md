# CriKey

A fast, keyboard-driven application launcher with a Rust core, isolated plugin
workers, and a Legacy Compatibility Layer that runs existing Keypirinha
plugins. Windows-only parts of that older interface are reported as unavailable
on other platforms rather than emulated.

> CriKey is an independent project and is not affiliated with, endorsed by, or
> sponsored by Keypirinha or its developer. See [NOTICE.md](NOTICE.md).

## Status

Milestones M0 through M7.5 are implemented, which covers the launcher, all
four plugin runtimes, the ecosystem work (signed packages, the plugin index,
WebAssembly, the restricted C ABI, Wayland shortcuts, remote catalogs) and the
per-platform packaging. M8, native Windows and macOS runtime verification, is
planned and is the reason those two platforms are still listed as unverified
below. [docs/ROADMAP.md](docs/ROADMAP.md)
is the authoritative, measured status.

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
- **Six of twelve manifest permissions are reported, not enforced.** Five reach
  a real gate: `network` (scheduling), `process` (host-mediated launch, per
  owner, including legacy packages), `background-execution` (modern Python
  worker admission), `filesystem` at the one read the host performs for a
  plugin — its own package — and `environment`, which decides whether a spawned
  native or modern-Python child inherits the ambient environment or the
  stripped one. `network-listener` is refused at parse time, because nothing in
  this host can grant an inbound socket. The remaining six —`clipboard`,
  `window-enumeration`, `window-control`, `notifications`, `secrets` and
  `native-library-loading` — describe operations the plugin's own process
  performs, so the host has nothing to decline; they are named per plugin, with
  a reason, by `crikey plugin doctor`. Read those six as a request, never as a
  confinement.
- **The Linux sandbox confines writes, and nothing else.** On Linux every
  supervised plugin child — native, WASM, C-ABI, modern Python and legacy — is
  restricted with Landlock before it executes: it may create, modify, rename,
  truncate or delete only beneath the directories the host named for it — its
  scratch space, and for a legacy package the one cache directory the
  compatibility API tells it to write to — and a
  manifest that did not ask for `network` has TCP `bind` and `connect` refused
  by the kernel. Reads and execution are **not** restricted, there is no
  seccomp policy, and UDP and Unix sockets are outside what Landlock governs.
  Windows and macOS install no equivalent: the Windows job object is a
  resource limit, not a sandbox. `crikey plugin doctor` prints the posture per
  plugin, and `CRIKEY_PLUGIN_SANDBOX=off` disables it for a whole process
  (§20.2, ADR-0019).
- **Legacy packages have no declarations at all.** A Keypirinha package ships no
  `crikey.toml`, so the host applies an explicit compatibility baseline —
  host-mediated process launch and package-file reads, nothing else — and prints
  it as `legacy_permission_posture=compatibility-baseline` in
  `crikey plugin doctor`. Beyond the Linux write confinement above, the CPython
  child is unrestricted: it reaches the clipboard, the network and every
  readable file through its own interpreter.
- **Composition input on Linux commits, but never shows a preedit.** The
  renderer tracks preedit state, withholds launcher keys while an input method
  owns the keyboard, and forwards a commit into egui — which `egui-winit` 0.29
  discards on Linux, so composed characters used to vanish. A live test types a
  real `Multi_key a e` through XTEST against a private Xvfb and asserts the `æ`
  reaches the query. Xlib's built-in input method sends only
  `Ime::Preedit("", None)` before that commit, so a non-empty preedit has never
  been observed on this host and is covered at unit level only; nothing renders
  an in-progress composition on Linux today.
- **Ranking context is X11-only.** `crikey run` advances the recency clock from
  the system clock, restores and saves selection history under
  `$XDG_STATE_HOME/crikey/selection-history.json`, and matches the foreground
  window against the application catalog to supply the context signal. The last
  of those needs `WindowService::foreground_window`, which only the X11 backend
  implements: under Wayland, and on Windows and macOS, the context term stays
  off rather than being guessed at.
- **Distribution recipes are checked in, but release builds are environment-bound.**
  `packaging/{windows,macos,linux}` now contain the installer, bundle and
  system-package definitions, and Linux's recipe has been exercised locally.
  No prebuilt installer, certificate or signing key is committed; Windows and
  macOS release builds still require their native toolchains. The optional
  bundled Python profile is staged by
  [`packaging/stage-python-runtime.sh`](packaging/stage-python-runtime.sh)
  when a python-build-standalone archive is supplied.
- **The compatibility matrix has no planned entries.** All 115 documented
  legacy API rows are classified as full, behavioural-difference, partial,
  Windows-only or unsupported. Partial and unsupported rows retain their
  explicit caveats in `compatibility/api-matrix/matrix.toml`; they are not
  silently presented as full compatibility.

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
# The package ships two binaries, so Cargo needs to be told which one.
cargo run -p crikey-cli --bin crikey -- version
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
[docs/development.md](docs/development.md). To tie the clearing to a build
instead of to the clock, `scripts/dev-rebuild.sh test --workspace` prunes and
then forwards to Cargo; it costs a cold build every time, so it is a separate
command rather than a change to `cargo`.

### Python

The Python software development kit, legacy compatibility shim, and synthetic
test plugins are driven by real interpreters during tests. Use a virtual
environment rather than a system-wide interpreter:

```sh
python3 -m venv .venv
PYTHONDONTWRITEBYTECODE=1 .venv/bin/python -m compileall -q \
    sdk/python crates/crikey-legacy-compat/python compatibility/test-plugins
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
   *implied* to be enforced, without a working implementation behind it. Six of
   the twelve manifest permissions are accepted and enforce nothing — the host
   never performs those operations for a plugin, so it has nothing to decline —
   and those are documented as declaration-only, both here under Known gaps and
   at their definitions, and named per plugin with a reason by
   `crikey plugin doctor`. Parsing a field ahead of its consumer is allowed;
   letting a reader believe it does something is not. Advertising substance that
   does not exist is the single most common defect this codebase has produced.
8. Anything arriving from a plugin — archive entries, wire frames, interpreter
   output, package digests — is validated before use, with bounded reads and
   allocations, and rejected rather than partially applied.

## Licence

Apache-2.0. See [LICENSE](LICENSE).
