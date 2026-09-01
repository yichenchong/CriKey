# Development environment

Practical notes for working in this repository. The design rules live in
[architecture.md](architecture.md) and the decision records; this file is about
the mechanics of building, testing and not running out of disk.

## Toolchain

`rust-toolchain.toml` pins Rust `1.86.0` and requests `rustfmt` and
`clippy`. The root `Cargo.toml` declares the same minimum supported Rust version.

Before proposing a Rust change, run what the Rust job in continuous integration runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo test --workspace --all-targets
cargo check --manifest-path compatibility/native-conformance/Cargo.toml --all-targets
```
Continuous integration sets `RUSTFLAGS: -D warnings`, so a warning fails the
build. Check cargo's own exit status rather than a pipeline's: in a shell,
`cargo test | grep ...` reports grep's status, not cargo's, so a failing test
run can look successful. Use `set -o pipefail`, or redirect to a file and test
`$?` immediately after cargo.

Cross-compilation checks are worth running when touching a platform backend:

```sh
rustup target add x86_64-pc-windows-msvc aarch64-apple-darwin
cargo check -p crikey-platform-windows --target x86_64-pc-windows-msvc --all-targets
cargo check -p crikey-platform-macos --target aarch64-apple-darwin --all-targets
```

Most of those crates are behind conditional compilation, so a normal build on
Linux does not compile them at all and they rot silently. Type-checking them is
not the same as running them; say which you did when reporting results.

## Python

The Python software development kit under `sdk/python/`, the legacy
compatibility shim under `crates/crikey-legacy-compat/python/`, and the
synthetic test plugins under `compatibility/test-plugins/` are compiled by
continuous integration with Python 3.12 and executed by real interpreters during
tests. Use a virtual environment, not a system-wide interpreter:

```sh
python3 -m venv .venv
PYTHONDONTWRITEBYTECODE=1 .venv/bin/python -m compileall -q \
    sdk/python crates/crikey-legacy-compat/python compatibility/test-plugins
```

`.venv/` and `__pycache__/` are ignored by git. The
`PYTHONDONTWRITEBYTECODE=1` setting prevents regular imports from creating
byte-code caches; explicit `compileall` writes its checked bytecode by design,
and those outputs are ignored.

Interpreter discovery order is the `CRIKEY_PYTHON` environment variable, then a
configured external runtime profile, then the runtime bundled with the build,
then `python3` on the search path. Set `CRIKEY_PYTHON` to test against a
specific interpreter. Each rule is decisive: once a rule names an interpreter,
one that will not run or does not satisfy a plugin's `requires-python` is an
error, not a fall-through to the next rule.

## The bundled Python runtime

A released CriKey ships the interpreter it needs (spec §14.11's "current
bundled runtime" profile), so an installed launcher does not depend on
whatever Python the machine happens to have and never imports from a
system-wide `site-packages` (§15.4). A development checkout has no
bundled runtime and never needs one: discovery falls through to `python3` on
the search path exactly as it always did.

Discovery looks for the runtime in two places, in order, relative to the
directory holding the running `crikey` binary:

| Relative location | Install shape |
| --- | --- |
| `python-runtime/` | self-contained directory: portable archive, macOS `Contents/MacOS`, `cargo build` output |
| `../lib/crikey/python-runtime/` | prefix install: `.deb`, `.rpm`, `/usr/local` — `bin/` must not be littered |

Inside either, the interpreter is `bin/python3` on Unix and `python.exe` on
Windows. Both are relative to the binary rather than to an absolute prefix so
an installed tree survives being moved or copied.

To stage one:

```sh
packaging/stage-python-runtime.sh --dest target/debug \
    --archive ~/Downloads/cpython-3.13.1+20241205-x86_64-unknown-linux-gnu-install_only.tar.gz
```

`--dest` is the directory the runtime is staged *beside* — the one holding the
binary for a self-contained tree, or `<prefix>/lib/crikey` for a prefix
install. The archive is a [python-build-standalone][pbs] `install_only` build
for the target triple; those are relocatable, which is what lets the staged
tree be moved. The script never downloads: the runtime that ends up in a
release artefact is a pinned, reviewed input. It fails loudly, and stages
nothing, when the archive is missing, when the tree is not a
python-build-standalone layout, or when the staged interpreter reports a
`sys.prefix` outside the staged directory — a runtime that resolves back into a
system prefix is exactly the dependency bundling exists to remove.

Staging is optional and its absence is honest, not hidden: an installer that
skips it produces an artefact that uses the system interpreter, and must
declare that dependency in its package metadata.

[pbs]: https://github.com/astral-sh/python-build-standalone

## Disk use

This repository is unusually disk-hungry, and it has taken a machine down more
than once. Two separate causes, both worth understanding before changing
anything about the build.

### Debug information

`cargo test --workspace --all-targets` links roughly a hundred test
executables, several of which statically link the graphics, windowing and
clipboard stacks. Under Cargo's default development settings — full debug
information for everything, plus incremental artefacts — that produced **over
45 GB** and exhausted the disk.

The root `Cargo.toml` therefore sets:

```toml
[profile.dev]
debug = "line-tables-only"
incremental = false

[profile.dev.package."*"]
debug = 0
```

Workspace crates keep line tables, so a panic still names a source location.
Dependencies, which dominate the byte count and are not the code under test,
get no debug information. `[profile.test]` inherits this. The result is about
**2 GB** for the whole test set. If you need full debug information for a
specific investigation, ask for it on the command line for that run rather than
widening the profile for everyone:

```sh
CARGO_PROFILE_TEST_DEBUG=2 cargo test -p crikey-core
```

### Accumulated stale artefacts

Cargo never removes superseded output. When a source file changes, rustc writes
a new set of artefacts under a new content hash and the previous set remains.
Nothing collects them. During a long editing session this accumulates quickly:
one session left **306 test executables** in `target/debug/deps` for a
workspace that has about 98, including eight stale copies of the main binary
totalling 459 MB, and grew `target/debug` from 2.0 GB to 5.6 GB.

There is no supported way to make Cargo garbage-collect a project's build
directory on the stable toolchain, so the repository ships a script:

```sh
scripts/prune-build-cache.sh            # prune if target/debug exceeds the threshold
scripts/prune-build-cache.sh --dry-run  # report what it would do
scripts/prune-build-cache.sh --force    # prune regardless of size
THRESHOLD_GB=8 scripts/prune-build-cache.sh
```

The threshold defaults to 5 GB and is measured against `target/debug` **alone**,
not the whole build directory. That distinction is load-bearing. The
cross-compilation directories, release output and the out-of-tree fixture
together occupy several gigabytes by themselves, so a threshold applied to the
whole of `target` would be permanently exceeded and would delete the
development profile after every single rebuild, which is worse than doing
nothing. Measuring the directory that actually gets deleted also removes any
need for hysteresis: a healthy full build is about 2 GB, so the default only
fires once stale artefacts have roughly doubled it.

It clears everything inside `target/debug` when over the threshold. That is
deliberately blunt: removing individual files by age risks deleting something
Cargo still considers current, whereas emptying the profile is always safe and
costs only a rebuild. It leaves alone `target/release`, the cross-compilation
directories such as `target/x86_64-pc-windows-msvc`, and
`target/native-conformance` (the out-of-tree plugin fixture), because those are
expensive to regenerate and are not rewritten on every edit.

To avoid deleting artefacts out from under a live compiler, it takes Cargo's own
exclusive build lock on `target/debug/.cargo-lock` and holds it across the
removal, creating that file first if it does not exist, which is exactly what
Cargo itself does. That is genuine mutual exclusion, not a guess: a build either
completed before the script started, or it waits for the lock. Checking for
running `cargo` processes instead would be a race, because a build can start
between the check and the removal.

One consequence is worth knowing if you ever modify the script: it must not
delete `.cargo-lock` itself. Doing so would leave it holding a lock on an
unlinked file while a newly started Cargo creates a fresh lock file and builds
happily alongside the deletion. That is why the removal skips that one entry
rather than deleting the directory outright. The only remaining caveat is that
if `flock` is not installed the script falls back to a process check, which is
advisory only.

A dry run only measures and reports, so it does not create or lock
`.cargo-lock`.

### Running it automatically

To stop having to think about it, install a user timer. This needs no root
access and touches nothing outside your own home directory.

`~/.config/systemd/user/crikey-prune-build-cache.service`:

```ini
[Unit]
Description=Reclaim disk from the CriKey Cargo build directory

[Service]
Type=oneshot
Nice=19
IOSchedulingClass=idle
ExecStart=%h/projects/crikey/scripts/prune-build-cache.sh
```

`~/.config/systemd/user/crikey-prune-build-cache.timer`:

```ini
[Unit]
Description=Periodically reclaim disk from the CriKey Cargo build directory

[Timer]
OnCalendar=hourly
Persistent=true
RandomizedDelaySec=5min
AccuracySec=1min

[Install]
WantedBy=timers.target
```

The timer is deliberately **calendar-based rather than monotonic**. An earlier
version of this recipe used `OnBootSec=10min` with `OnUnitActiveSec=1h`, and it
silently stopped working. A monotonic timer's elapse points are relative to
boot and to the service's own last activation, so when the user manager
restarts — a logout, a session change — both points are already in the past,
nothing re-arms them, and the timer parks in `active (elapsed)` with
`Trigger: n/a`, meaning it will never fire again. `Persistent=true` does not
rescue it, because that key applies to calendar timers only and is ignored for
a monotonic-only timer. On one machine this left the timer dead for 13 days
while `target/debug` grew from 3.3 GB to 32 GB and nearly filled the disk.

`OnCalendar=hourly` has an absolute next elapse point that survives a manager
restart, and with it `Persistent=true` becomes meaningful: a run missed while
logged out or suspended fires once the manager is back.

Then:

```sh
systemctl --user daemon-reload
systemctl --user enable --now crikey-prune-build-cache.timer
systemctl --user list-timers crikey-prune-build-cache.timer
journalctl --user -u crikey-prune-build-cache.service -n 20
```

After installing it, confirm the timer is actually armed rather than merely
enabled — `enabled` and `active` are both true of the dead state above:

```sh
systemctl --user list-timers crikey-prune-build-cache.timer
```

`NEXT` and `LEFT` must name a future time. If `NEXT` is `n/a`, the timer will
never fire.

Adjust `ExecStart` if your checkout is not at `~/projects/crikey`. The service
runs at lowest priority and idle input/output scheduling, so it will not
compete with a build.

To check what it has been doing, or to turn it off again:

```sh
journalctl --user -u crikey-prune-build-cache.service -n 50   # what it pruned
systemctl --user disable --now crikey-prune-build-cache.timer # stop it
rm ~/.config/systemd/user/crikey-prune-build-cache.{service,timer}
systemctl --user daemon-reload
```

Nothing about this is required to build the project; it is a convenience, and
removing it only means going back to running the script by hand.

By default a user timer only runs while you have a login session. To let it run
regardless, enable lingering once (this is the only step needing root):

```sh
sudo loginctl enable-linger "$USER"
```

### Clearing on every rebuild

The timer above is periodic, so between two of its runs a heavy session still
accumulates. When you want the clearing tied to the build itself rather than to
the clock, use the wrapper:

```sh
scripts/dev-rebuild.sh                     # prune, then `cargo build`
scripts/dev-rebuild.sh test --workspace    # prune, then `cargo test --workspace`
scripts/dev-rebuild.sh clippy --all-targets
```

It runs `prune-build-cache.sh --force` and then execs Cargo with every argument
forwarded unchanged, so the exit status is Cargo's own and Ctrl-C reaches the
compiler. The prune stage reports on stderr, leaving stdout clean for commands
with machine-readable output such as `metadata` or `--message-format json`.

Two things about it are deliberate. It is a separate entry point rather than
anything that alters `cargo`, because Cargo has no post-build hook on the stable
toolchain: aliases in `.cargo/config.toml` may only name Cargo subcommands, and
a `build.rs` cannot delete the profile the compiler invoking it is writing to.
And the cost is opt-in, because it is real — every invocation is a cold build of
whatever the command needs, since the artefacts a previous build left are
exactly the incremental cache it deletes first. Plain `cargo build`, `cargo
test` and `cargo clippy` are unaffected by the script existing; reach for the
wrapper when disk matters more than latency, and use Cargo directly otherwise.

## Commit identity

Commits are authored by the repository owner, `Yi Chen Chong
<yichenchong@yahoo.com>`. That is set in this clone's `.git/config`, so an
ordinary `git commit` is already correct and nothing needs passing on the
command line.

Do not override it — not with `git -c user.name=...`, not with `GIT_AUTHOR_*`
or `GIT_COMMITTER_*` in the environment, and not by editing `.git/config`. An
earlier agent set this clone to a placeholder `CriKey <dev@crikey.invalid>`;
because that address belongs to no GitHub account, sixty-nine commits and six
release tags were attributed to nobody, and putting it right meant rewriting
every commit and force-pushing every tag. Check with:

```console
$ git config user.name && git config user.email
Yi Chen Chong
yichenchong@yahoo.com
```

## Working in parallel

Several people or agents editing this workspace at once share one build
directory, which has two consequences worth knowing.

A crate that fails to parse blocks everyone downstream of it, and this
dependency graph funnels almost everything through a few low-level crates. So
make each edit take a file from one compiling state to another, and run
`cargo check -p crikey-core` (using the affected package name) immediately
after a structural change rather than batching several and checking at the end.

If a build fails inside a crate you are not editing, it is very likely someone
else's in-flight edit. Tell the owner and wait rather than fixing it yourself:
two people fixing the same line simultaneously has produced a compile error
that neither would have caused alone.

Tests share the machine too. Prefer an injectable clock or explicit
synchronisation over sleeping, and avoid asserting wall-clock thresholds: a
timing bound that passes on an idle machine fails when the suite runs in
parallel.

Parallelism also has a subtler consequence worth knowing if you write a test
that creates an executable and then runs it. Test binaries run their cases in
parallel threads of one process, so when any thread spawns a child the fork
inherits every open descriptor, including another thread's write handle to a
file it has just created. If that file is a script another case is about to
execute, the kernel refuses to run it with `ETXTBSY` ("Text file busy") because
a writer still holds it open. The symptom is maddening: the test passes in
isolation and fails only under full parallel load, and the error looks like the
program under test rejecting a perfectly valid fixture. Write the file to a
sibling temporary name, make it executable, and rename it into place, so it is
published under a name no writer ever held. The interpreter shim helpers in
`crates/crikey-legacy-compat/tests/worker_runtime.rs` and
`crates/crikey-python-host/tests/` do this and are worth copying.
