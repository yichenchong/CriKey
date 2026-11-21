//! The modern SDK worker's *Python-side* behaviour, driven end to end through
//! the Rust [`ModernWorker`] against a real CPython interpreter and the real
//! `sdk/python/_crikey_modern_worker.py` (spec 15; contract §5).
//!
//! These tests are written before the worker shim exists. They pin what the
//! child process must do once the host has spawned it: load a plugin from an
//! entrypoint, honour the host-assembled import path (and *nothing* outside
//! it), stream results while keeping stdout a strict protocol channel, report a
//! plugin's own exception as a structured failure without dying, and run both
//! synchronous and `async def` callbacks on a bounded event loop.
//!
//! # Why a real subprocess and not a double
//!
//! Every contract below is only true of a real child. Isolation under `-S`,
//! `import`-declared dependencies resolving from a managed env directory and
//! *only* from there, `print` never corrupting the JSON stream, an `async def`
//! callback emitting on the worker's asyncio loop, an un-registered background
//! task being cancelled rather than left running — none of these can be
//! observed of an in-process stand-in. These tests prefer the repository
//! virtualenv and otherwise use ordinary interpreter discovery, then assert on
//! what the real interpreter did. If no usable interpreter exists, each test
//! prints a reason and skips rather than using an in-process double.
//!
//! # Time
//!
//! The peer is a real OS process, so its bounds cannot be virtual. Every bound
//! is nevertheless *explicit* — handed to the worker through [`WorkerOptions`],
//! never read from a clock inside library logic — and no test sleeps as a
//! synchronisation primitive. The one fixture that leaves a pending asyncio
//! task `await`s a long sleep it never reaches: the worker cancels it, so the
//! sleep is interrupted immediately and the test is as fast as the machine.
//!
//! # Fixtures
//!
//! Each test writes the smallest plugin package that can express its contract
//! into its own temp directory, removed when the test ends. A fixture that
//! `import`s its managed dependency, or its own sibling module, therefore
//! proves the import path was assembled correctly with no extra assertion.
//!
//! # Surface under test
//!
//! * [`ModernWorker::spawn`] / [`ModernWorker::suggest`] / [`ModernWorker::is_alive`]
//!   / [`ModernWorker::shutdown`]; [`WorkerOptions`] carrying the entrypoint and
//!   the [`ImportPath`]; [`SuggestRequest`] / [`Suggestions`] / [`BatchState`].
//!   `suggest` takes `&mut self` so per-plugin request serialisation is
//!   unrepresentable to get wrong.
//! * The worker frames exercised: `handshake`/`handshake_ack` (during spawn),
//!   `suggest` → zero-or-more `result_batch` `state="partial"` then exactly one
//!   terminal `result_batch` in `{final,failed}`, each carrying `items`, `log`
//!   and (on failure) `error {message, traceback}` surfaced as
//!   `Suggestions::error`.
//! * The SDK context surface: `SuggestContext.emit`, `.log`, `.cancelled` and
//!   the documented `context.spawn(coro)` host-managed background task
//!   registration (§5, §15.8), plus captured `print` landing in the reply
//!   `log`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::{sleep, yield_now};
use std::time::{Duration, Instant};

use crikey_core::{Item, ItemId, PluginId};
use crikey_package_manager::{EnvironmentId, ImportPath, MaterializedEnvironment};
use crikey_plugin_model::ConcurrencySection;
use crikey_plugin_supervisor::shared_budget_from_section;
use crikey_python_host::{
    discover_interpreter, discover_interpreter_in, BatchState, DiscoveryEnvironment, Interpreter,
    ModernWorker, RequiresPython, RuntimeProfile, SuggestRequest, Suggestions, WorkerOptions,
};

// ---------------------------------------------------------------------------
// Bounds
//
// None of these is a performance assertion. Each exists so that a broken
// implementation fails with a message instead of hanging the suite.
// ---------------------------------------------------------------------------

/// Bound on the startup handshake with a correctly behaving child.
const STARTUP_BUDGET_MS: u64 = 30_000;
/// Per-call bound for a worker expected to answer promptly.
const CALL_BUDGET_MS: u64 = 30_000;
/// Bound on a clean shutdown.
const SHUTDOWN_BUDGET_MS: u64 = 5_000;

/// The `requires-python` the modern host resolves against on this repo.
const REQUIRES_PYTHON: &str = ">=3.12";

// ---------------------------------------------------------------------------
// Scratch space
// ---------------------------------------------------------------------------

/// A private directory removed when the test that made it ends.
#[derive(Debug)]
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);

        let path = std::env::temp_dir().join(format!(
            "crikey-worker-python-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // A crashed earlier run must never leak fixtures into this one.
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");

        Self { path }
    }

    fn subdir(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::create_dir_all(&path).expect("scratch subdirectory is creatable");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Fixture authoring
// ---------------------------------------------------------------------------

/// Writes `contents` at `path`, creating parent directories first.
fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture parent directory is creatable");
    }
    fs::write(path, contents).expect("fixture file is writable");
}

/// The repo `sdk/python` directory (dev layout), where `crikey_sdk/` and the
/// worker entry `_crikey_modern_worker.py` live. Mirrors the legacy
/// `shim_path()`: the entry file's presence is asserted so a missing shim is a
/// clear failure rather than an opaque spawn error. The host discovers the same
/// directory through `sdk_root()`'s dev-layout fallback; this test cannot set a
/// process-wide `CRIKEY_MODERN_SDK_DIR` because a Rust test binary is one
/// process running many threads and the mutation would race sibling tests.
fn sdk_python_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("sdk")
        .join("python");
    let dir = fs::canonicalize(&dir).unwrap_or(dir);
    assert!(
        dir.join("_crikey_modern_worker.py").is_file(),
        "the modern worker entry _crikey_modern_worker.py must ship in {}",
        dir.display()
    );
    dir
}

/// Prefer the repository virtual environment, but keep a fresh checkout's
/// Rust tests usable when that ignored directory has not been created.
fn test_interpreter_path() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    #[cfg(windows)]
    let path = root.join(".venv").join("Scripts").join("python.exe");
    #[cfg(not(windows))]
    let path = root.join(".venv").join("bin").join("python");
    path.is_file().then_some(path)
}

fn host_interpreter() -> Option<Interpreter> {
    if let Some(path) = test_interpreter_path() {
        let environment = DiscoveryEnvironment::empty().with_override(path);
        return Some(
            discover_interpreter_in(
                &RuntimeProfile::Bundled,
                &RequiresPython(REQUIRES_PYTHON.to_owned()),
                &environment,
            )
            .unwrap_or_else(|error| panic!("the repository virtualenv is not usable: {error}")),
        );
    }

    match discover_interpreter(
        &RuntimeProfile::Bundled,
        &RequiresPython(REQUIRES_PYTHON.to_owned()),
    ) {
        Ok(interpreter) => Some(interpreter),
        Err(error) => {
            eprintln!("skipping real-interpreter test: no usable CPython was found ({error})");
            None
        }
    }
}

macro_rules! require_host_interpreter {
    () => {
        if host_interpreter().is_none() {
            return;
        }
    };
}

/// Spawns a worker for the plugin at `plugin_source`, exposing `site_dir` as the
/// managed-dependency env directory. The import path is assembled exactly as
/// the host does at run time (spec 15.4): plugin source, then packaged modules
/// (none here), then the managed env, then the CriKey SDK — never global site.
fn spawn_worker(plugin_id: &str, entrypoint: &str, plugin_source: &Path, site_dir: &Path) -> ModernWorker {
    let env = MaterializedEnvironment {
        id: EnvironmentId(format!("env-{plugin_id}")),
        site_dir: site_dir.to_path_buf(),
    };
    let import_path: ImportPath = ImportPath::assemble(plugin_source, &[], &env, &sdk_python_dir());

    let mut options = WorkerOptions::new(PluginId(plugin_id.to_owned()), entrypoint.to_owned(), import_path)
        .with_shared_budget(shared_budget_from_section(&ConcurrencySection::default()));
    options.startup_timeout_ms = STARTUP_BUDGET_MS;
    options.call_timeout_ms = CALL_BUDGET_MS;
    options.shutdown_timeout_ms = SHUTDOWN_BUDGET_MS;

    let interpreter = host_interpreter().expect("host interpreter was checked at test entry");
    ModernWorker::spawn(&interpreter, options)
        .unwrap_or_else(|error| panic!("a loadable plugin spawns a worker, got {error}"))
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// Issues a `suggest` and requires the transport to complete. A plugin raising
/// is *not* an error here — it is an `Ok(Suggestions)` whose `state` is
/// `Failed` — so this only fails on a genuine host/transport fault.
fn suggest(worker: &mut ModernWorker, text: &str) -> Suggestions {
    worker
        .suggest(&SuggestRequest {
            generation: 1,
            text: text.to_owned(),
            normalized: text.to_owned(),
            selected_item_id: None,
        })
        .unwrap_or_else(|error| panic!("suggest {text:?} must complete the transport, got {error}"))
}

/// The value stored under `key` in an item's metadata.
fn meta<'a>(item: &'a Item, key: &str) -> &'a str {
    item.metadata
        .get(key)
        .unwrap_or_else(|| panic!("item metadata must carry {key:?}, got {:?}", item.metadata))
}

/// The joined reply log, for substring assertions.
fn joined_log(reply: &Suggestions) -> String {
    reply.log.join("\n")
}

// ---------------------------------------------------------------------------
// Plugin loading and the import path (spec 15.4)
// ---------------------------------------------------------------------------

#[test]
fn a_plugin_loaded_from_its_entrypoint_imports_its_own_sibling_modules() {
    require_host_interpreter!();
    // The plugin lives in a *sub*package and reaches a sibling module both by
    // absolute (`pkg.sub.helper`) and relative (`.helper`) import. The host
    // only put the plugin source root on the path, so both resolving proves the
    // package hierarchy is intact under the host-assembled `sys.path` (§15.4).
    let scratch = Scratch::new("subpackage");
    let src = scratch.subdir("source");
    write_file(&src.join("pkg").join("__init__.py"), "");
    write_file(&src.join("pkg").join("sub").join("__init__.py"), "");
    write_file(
        &src.join("pkg").join("sub").join("helper.py"),
        "GREETING = \"hello-from-sibling\"\n",
    );
    write_file(
        &src.join("pkg").join("sub").join("entry.py"),
        r#"
from crikey_sdk import Plugin, Item
from pkg.sub.helper import GREETING
from .helper import GREETING as RELATIVE


class Impl(Plugin):
    def suggest(self, query, context):
        assert GREETING == RELATIVE, "absolute and relative sibling imports agree"
        context.emit(Item(stable_id="sibling-1", label=GREETING, target="t"))
"#,
    );

    let site = scratch.subdir("empty-site");
    let mut worker = spawn_worker("modern.subpackage", "pkg.sub.entry:Impl", &src, &site);

    let reply = suggest(&mut worker, "anything");

    assert!(
        matches!(reply.state, BatchState::Final),
        "a clean suggest terminates with state=final, got {:?}",
        reply.state
    );
    assert_eq!(reply.items.len(), 1, "the plugin emitted exactly one item");
    assert_eq!(
        reply.items[0].label, "hello-from-sibling",
        "the value imported from the sibling module reaches the emitted item"
    );
    assert_eq!(
        reply.items[0].stable_id,
        ItemId("sibling-1".to_owned()),
        "a modern plugin supplies its own stable_id and the host keeps it (spec 10.2)"
    );

    worker.shutdown();
}

// ---------------------------------------------------------------------------
// Managed dependencies and isolation (spec 15.3, 15.4; acceptance 31.19)
// ---------------------------------------------------------------------------

/// Writes a fake `acme` "wheel" into `site_dir`: a package that reports a
/// version and a callable, so a plugin that imports it can *use* it in a result.
fn write_acme(site_dir: &Path, version: &str) {
    write_file(
        &site_dir.join("acme").join("__init__.py"),
        &format!("__version__ = {version:?}\n\n\ndef greet():\n    return \"acme-{version}-says-hi\"\n"),
    );
}

const IMPORT_DEP_PLUGIN: &str = r#"
from crikey_sdk import Plugin, Item


class Impl(Plugin):
    def suggest(self, query, context):
        import acme

        context.emit(
            Item(
                stable_id="dep-1",
                label=acme.greet(),
                target="t",
                metadata={"version": acme.__version__},
            )
        )
"#;

#[test]
fn an_import_declared_dependency_resolves_only_when_its_managed_env_is_on_the_path() {
    require_host_interpreter!();
    // §31.19: the plugin declares no dep in Python — it just `import acme`.
    // Whether that import succeeds is decided entirely by whether the managed
    // env's site directory is on the assembled path. Same plugin, two envs.
    let scratch = Scratch::new("dep");
    let src = scratch.subdir("source");
    write_file(&src.join("depplugin.py"), IMPORT_DEP_PLUGIN);

    // With the dependency materialised in the env, the import succeeds and its
    // result proves the dependency's code actually ran.
    let with_dep = scratch.subdir("env-with-dep");
    write_acme(&with_dep, "1.4.2");
    let mut worker = spawn_worker("modern.dep.present", "depplugin:Impl", &src, &with_dep);
    let reply = suggest(&mut worker, "q");
    assert!(
        matches!(reply.state, BatchState::Final),
        "with the dep on the path the callback succeeds, got {:?}",
        reply.state
    );
    assert_eq!(reply.items.len(), 1, "the dep-backed item is emitted");
    assert_eq!(
        reply.items[0].label, "acme-1.4.2-says-hi",
        "the plugin used the managed dependency's own function in its result (§31.19)"
    );
    assert_eq!(
        meta(&reply.items[0], "version"),
        "1.4.2",
        "the version reported is the one materialised in the managed env"
    );
    worker.shutdown();

    // With an empty env the very same `import acme` fails: the dependency is
    // reachable ONLY through the managed env, never through ambient site.
    let empty = scratch.subdir("env-empty");
    let mut worker = spawn_worker("modern.dep.absent", "depplugin:Impl", &src, &empty);
    let reply = suggest(&mut worker, "q");
    assert!(
        matches!(reply.state, BatchState::Failed),
        "without the dep the import raises and the callback fails, got {:?}",
        reply.state
    );
    let error = reply
        .error
        .as_ref()
        .expect("a failed suggest carries the plugin's structured error");
    assert!(
        error.message.contains("acme") || error.traceback.contains("acme"),
        "the failure names the module that could not be imported, got {error:?}"
    );
    assert!(
        error.traceback.contains("ModuleNotFoundError") || error.traceback.contains("ImportError"),
        "the traceback shows the import error, got {:?}",
        error.traceback
    );
    worker.shutdown();
}

const ISOLATION_PLUGIN: &str = r#"
from crikey_sdk import Plugin, Item


class Impl(Plugin):
    def suggest(self, query, context):
        import sys
        import json
        import acme
        import crikey_sdk
        from ownpkg import marker

        context.emit(
            Item(
                stable_id="iso-1",
                label="isolated",
                target="t",
                metadata={
                    # -S was passed: the site module is not auto-initialised, so
                    # a user's global site-packages cannot shadow imports.
                    "no_site": str(sys.flags.no_site),
                    "site_loaded": str(int("site" in sys.modules)),
                    # stdlib, managed dep, the SDK and the plugin's own package
                    # all resolve under the host-assembled path.
                    "stdlib": json.dumps("ok"),
                    "dep": acme.__version__,
                    "sdk": crikey_sdk.__version__,
                    "own": marker.MARKER,
                },
            )
        )
"#;

#[test]
fn the_worker_is_isolated_under_dash_s_yet_reaches_every_intended_import() {
    require_host_interpreter!();
    // Contract §5: under `-S` with a path lacking global site, a plugin can
    // import the stdlib, its own package, its managed deps and `crikey_sdk` —
    // and nothing else. The deterministic isolation signal is `sys.flags.no_site`
    // (1 under `-S`) together with `site` never having been auto-imported; a
    // host that dropped `-S` would fail both while the four intended imports
    // would still pass, so all four are asserted to keep the test honest.
    let scratch = Scratch::new("isolation");
    let src = scratch.subdir("source");
    write_file(&src.join("ownpkg").join("__init__.py"), "");
    write_file(
        &src.join("ownpkg").join("marker.py"),
        "MARKER = \"own-package-ok\"\n",
    );
    write_file(&src.join("isoplugin.py"), ISOLATION_PLUGIN);

    let site = scratch.subdir("env");
    write_acme(&site, "2.0.0");

    let mut worker = spawn_worker("modern.isolation", "isoplugin:Impl", &src, &site);
    let reply = suggest(&mut worker, "q");

    assert!(
        matches!(reply.state, BatchState::Final),
        "every intended import resolves, so the callback succeeds, got {:?}",
        reply.state
    );
    let item = &reply.items[0];
    assert_eq!(
        meta(item, "no_site"),
        "1",
        "the worker runs under -S so site-packages cannot shadow imports"
    );
    assert_eq!(
        meta(item, "site_loaded"),
        "0",
        "the site module is not auto-initialised under -S"
    );
    assert_eq!(meta(item, "stdlib"), "\"ok\"", "the stdlib is importable");
    assert_eq!(
        meta(item, "dep"),
        "2.0.0",
        "the managed dependency is importable from the env site dir"
    );
    assert_eq!(
        meta(item, "own"),
        "own-package-ok",
        "the plugin's own package is importable"
    );
    assert!(
        !meta(item, "sdk").is_empty(),
        "crikey_sdk is importable and reports a version"
    );

    worker.shutdown();
}

// ---------------------------------------------------------------------------
// Streaming, log capture and the strict protocol channel (contract §1, §5)
// ---------------------------------------------------------------------------

const CHATTY_PLUGIN: &str = r#"
import sys
from crikey_sdk import Plugin, Item


class Impl(Plugin):
    def suggest(self, query, context):
        context.emit(Item(stable_id="a", label="first", target="t1"))
        context.emit(Item(stable_id="b", label="second", target="t2"))
        context.log("logged-via-context")
        print("printed-to-stdout")
        sys.stdout.write("printed-without-newline")
"#;

#[test]
fn emit_streams_results_and_log_and_print_land_in_the_reply_log_without_desyncing() {
    require_host_interpreter!();
    // stdout is a strict protocol channel: anything the plugin `print`s must be
    // captured into the reply `log`, never written as a bare line that would
    // split the JSON stream. The reply parsing at all is half the contract; a
    // second successful suggest afterwards proves the channel stayed in sync.
    let scratch = Scratch::new("chatty");
    let src = scratch.subdir("source");
    write_file(&src.join("chatty.py"), CHATTY_PLUGIN);
    let site = scratch.subdir("env");

    let mut worker = spawn_worker("modern.chatty", "chatty:Impl", &src, &site);

    let reply = suggest(&mut worker, "q");
    assert!(
        matches!(reply.state, BatchState::Final),
        "a chatty-but-correct plugin still terminates cleanly, got {:?}",
        reply.state
    );
    assert_eq!(reply.items.len(), 2, "both streamed items survive the transport");
    assert_eq!(reply.items[0].label, "first");
    assert_eq!(reply.items[1].label, "second");

    let log = joined_log(&reply);
    for expected in [
        "logged-via-context",
        "printed-to-stdout",
        "printed-without-newline",
    ] {
        assert!(
            log.contains(expected),
            "plugin output is preserved as log output; {expected:?} missing from {log:?}"
        );
    }

    // If any of the above had leaked onto the protocol channel, the stream
    // would be desynchronised and this next call could not complete.
    let again = suggest(&mut worker, "again");
    assert!(
        matches!(again.state, BatchState::Final),
        "the protocol channel stayed in sync after captured stdout, got {:?}",
        again.state
    );
    assert_eq!(again.items.len(), 2, "the worker keeps answering correctly");

    worker.shutdown();
}

// ---------------------------------------------------------------------------
// Structured failure and survival (spec 24.1; contract §5)
// ---------------------------------------------------------------------------

const RAISING_PLUGIN: &str = r#"
from crikey_sdk import Plugin, Item


class Impl(Plugin):
    def suggest(self, query, context):
        if query.text == "boom":
            raise ValueError("kaboom-from-plugin")
        context.emit(Item(stable_id="ok", label="recovered", target="t"))
"#;

#[test]
fn an_exception_in_a_callback_is_a_structured_failure_and_the_worker_serves_the_next_request() {
    require_host_interpreter!();
    // A plugin raising is the plugin's fault, not the transport's: the worker
    // must report a terminal `failed` with the exception's message and
    // traceback, stay alive, and answer the next request. Conflating this with
    // a transport error would make plugin bugs look like host bugs and lose the
    // diagnostic the developer needs.
    let scratch = Scratch::new("raising");
    let src = scratch.subdir("source");
    write_file(&src.join("raising.py"), RAISING_PLUGIN);
    let site = scratch.subdir("env");

    let mut worker = spawn_worker("modern.raising", "raising:Impl", &src, &site);

    let reply = suggest(&mut worker, "boom");
    assert!(
        matches!(reply.state, BatchState::Failed),
        "a raised exception is a terminal failed state, got {:?}",
        reply.state
    );
    let error = reply
        .error
        .as_ref()
        .expect("a failed suggest carries the plugin's structured error");
    assert!(
        error.message.contains("kaboom-from-plugin"),
        "the failure carries the exception message, got {:?}",
        error.message
    );
    assert!(
        error.traceback.contains("ValueError"),
        "the failure carries a traceback naming the exception type, got {:?}",
        error.traceback
    );

    assert!(
        worker.is_alive(),
        "a plugin exception leaves the worker process alive"
    );

    // The proof the worker survived: it answers the very next request normally.
    let recovered = suggest(&mut worker, "fine");
    assert!(
        matches!(recovered.state, BatchState::Final),
        "the worker serves the next request after a plugin raised, got {:?}",
        recovered.state
    );
    assert_eq!(recovered.items.len(), 1);
    assert_eq!(recovered.items[0].label, "recovered");

    worker.shutdown();
}

// ---------------------------------------------------------------------------
// Synchronous and async callbacks (spec 15.8)
// ---------------------------------------------------------------------------

const SYNC_PLUGIN: &str = r#"
from crikey_sdk import Plugin, Item


class Impl(Plugin):
    def suggest(self, query, context):
        context.emit(Item(stable_id="s", label="sync-result", target="t"))
"#;

const ASYNC_PLUGIN: &str = r#"
import asyncio
from crikey_sdk import Plugin, Item


class Impl(Plugin):
    async def suggest(self, query, context):
        # A real await point: this only runs if the worker drives the callback
        # on an asyncio event loop.
        await asyncio.sleep(0)
        on_loop = asyncio.get_running_loop() is not None
        context.emit(
            Item(
                stable_id="a",
                label="async-result",
                target="t",
                metadata={"on_loop": str(on_loop)},
            )
        )
"#;

#[test]
fn both_synchronous_and_async_def_suggest_callbacks_emit_results() {
    require_host_interpreter!();
    // §15.8: the worker supports both. The sync plugin is the baseline; the
    // async one only produces a result if it is awaited on the worker's loop,
    // and its `on_loop` witness proves it ran there rather than being called
    // like a plain coroutine and dropped.
    let scratch = Scratch::new("sync-async");

    let sync_src = scratch.subdir("sync-source");
    write_file(&sync_src.join("syncp.py"), SYNC_PLUGIN);
    let sync_site = scratch.subdir("sync-env");
    let mut sync_worker = spawn_worker("modern.sync", "syncp:Impl", &sync_src, &sync_site);
    let sync_reply = suggest(&mut sync_worker, "q");
    assert!(
        matches!(sync_reply.state, BatchState::Final),
        "the sync callback terminates cleanly, got {:?}",
        sync_reply.state
    );
    assert_eq!(sync_reply.items.len(), 1);
    assert_eq!(sync_reply.items[0].label, "sync-result");
    sync_worker.shutdown();

    let async_src = scratch.subdir("async-source");
    write_file(&async_src.join("asyncp.py"), ASYNC_PLUGIN);
    let async_site = scratch.subdir("async-env");
    let mut async_worker = spawn_worker("modern.async", "asyncp:Impl", &async_src, &async_site);
    let async_reply = suggest(&mut async_worker, "q");
    assert!(
        matches!(async_reply.state, BatchState::Final),
        "the async callback terminates cleanly, got {:?}",
        async_reply.state
    );
    assert_eq!(async_reply.items.len(), 1);
    assert_eq!(async_reply.items[0].label, "async-result");
    assert_eq!(
        meta(&async_reply.items[0], "on_loop"),
        "True",
        "the async callback ran on the worker's asyncio event loop (§15.8)"
    );
    async_worker.shutdown();
}

// ---------------------------------------------------------------------------
// Bounded background tasks (spec 15.8, last sentence)
// ---------------------------------------------------------------------------

const BACKGROUND_PLUGIN: &str = r#"
import asyncio
from crikey_sdk import Plugin, Item


class Impl(Plugin):
    async def _orphan(self, context):
        # If this task were left running it would eventually log; it must be
        # cancelled at callback end before it ever gets past the await.
        await asyncio.sleep(3600)
        context.log("orphan-ran")

    async def _registered(self, context):
        await asyncio.sleep(0)
        context.log("spawned-done")

    async def suggest(self, query, context):
        if query.text == "orphan":
            # A raw, un-registered pending task: the worker must refuse to leave
            # it running and cancel it at callback end (§15.8 last sentence).
            asyncio.ensure_future(self._orphan(context))
            context.emit(Item(stable_id="o", label="made-orphan", target="t"))
        elif query.text == "spawn":
            # The documented registration: awaited to completion cleanly.
            context.spawn(self._registered(context))
            context.emit(Item(stable_id="s", label="made-spawn", target="t"))
        else:
            context.emit(Item(stable_id="p", label="ping", target="t"))
"#;

#[test]
fn an_unregistered_background_task_is_cancelled_and_reported_while_a_spawned_task_completes() {
    require_host_interpreter!();
    // §15.8: unbounded background work is refused. A task created via the
    // documented `context.spawn` is admitted and runs on the host-managed
    // background loop; a raw un-registered pending task is cancelled at
    // callback end and reported in the log, never left running to leak into a
    // later request.
    let scratch = Scratch::new("background");
    let src = scratch.subdir("source");
    write_file(&src.join("bg.py"), BACKGROUND_PLUGIN);
    let site = scratch.subdir("env");

    let mut worker = spawn_worker("modern.background", "bg:Impl", &src, &site);

    // The orphan: cancelled, reported, and its side effect never happens.
    let orphan = suggest(&mut worker, "orphan");
    assert!(
        matches!(orphan.state, BatchState::Final),
        "the callback still terminates cleanly, got {:?}",
        orphan.state
    );
    assert_eq!(orphan.items.len(), 1);
    assert_eq!(orphan.items[0].label, "made-orphan");
    let orphan_log = joined_log(&orphan);
    assert!(
        !orphan_log.contains("orphan-ran"),
        "the orphan task was cancelled before it could run, got log {orphan_log:?}"
    );
    let lowered = orphan_log.to_lowercase();
    assert!(
        lowered.contains("cancel")
            || lowered.contains("background")
            || lowered.contains("unregistered")
            || lowered.contains("pending"),
        "the cancelled background task is reported in the log, got {orphan_log:?}"
    );

    // The registered task is host-managed and therefore is not awaited by the
    // foreground callback. Wait only for its terminal lifecycle frame; this
    // is a bounded observation of the worker protocol, not a callback sleep.
    let spawned = suggest(&mut worker, "spawn");
    assert!(
        matches!(spawned.state, BatchState::Final),
        "the callback with a registered task terminates cleanly, got {:?}",
        spawned.state
    );
    assert_eq!(spawned.items[0].label, "made-spawn");
    let deadline = Instant::now() + Duration::from_secs(2);
    while worker.background_diagnostics().completed < 1 && Instant::now() < deadline {
        yield_now();
        sleep(Duration::from_millis(1));
    }
    let diagnostics = worker.background_diagnostics();
    assert_eq!(diagnostics.registered, 1);
    assert_eq!(diagnostics.admitted, 1);
    assert_eq!(
        diagnostics.completed, 1,
        "the admitted context.spawn task must reach a terminal completion: {diagnostics:?}"
    );

    // The orphan really was killed: a later request never sees its side effect
    // leak in, proving it was not merely detached and left running.
    let ping = suggest(&mut worker, "ping");
    assert!(
        matches!(ping.state, BatchState::Final),
        "the worker keeps serving after cancelling a background task, got {:?}",
        ping.state
    );
    assert!(
        !joined_log(&ping).contains("orphan-ran"),
        "the cancelled orphan never resurfaces in a later request's log"
    );

    worker.shutdown();
}

// ---------------------------------------------------------------------------
// Live configuration delivery (spec 21.4)
// ---------------------------------------------------------------------------

/// A plugin that retains whatever configuration it was last given and reports it
/// back through `suggest`, which is the only way a test can observe a callback
/// that returns nothing and answers nothing.
const CONFIGURATION_PLUGIN: &str = r#"
from crikey_sdk import Plugin, Item


class Impl(Plugin):
    def __init__(self):
        # A dict, not an accumulator: the host publishes the COMPLETE state, so
        # replacing wholesale is the documented correct implementation.
        self.settings = {}
        self.deliveries = 0

    def on_configuration(self, values):
        self.settings = dict(values)
        self.deliveries += 1

    def suggest(self, query, context):
        rendered = ",".join("{}={}".format(k, self.settings[k]) for k in sorted(self.settings))
        context.emit(
            Item(
                stable_id="config-1",
                label=rendered or "<none>",
                target="t",
                metadata={"deliveries": str(self.deliveries)},
            )
        )
"#;

#[test]
fn a_configuration_frame_reaches_the_plugins_on_configuration_hook() {
    require_host_interpreter!();
    let scratch = Scratch::new("configuration");
    let src = scratch.subdir("source");
    write_file(&src.join("cfg.py"), CONFIGURATION_PLUGIN);
    let site = scratch.subdir("empty-site");
    let mut worker = spawn_worker("modern.configuration", "cfg:Impl", &src, &site);

    // Before any publication the plugin holds nothing. Asserted first so a plugin
    // that somehow started with settings could not make the next step pass.
    let before = suggest(&mut worker, "q");
    assert_eq!(before.items[0].label, "<none>");
    assert_eq!(meta(&before.items[0], "deliveries"), "0");

    let values = BTreeMap::from([
        ("theme".to_owned(), "dark".to_owned()),
        ("result-limit".to_owned(), "20".to_owned()),
    ]);
    worker
        .send_configuration(&values, true)
        .expect("a live worker accepts a configuration publication");

    // No sleep: frames are consumed in the order they were written, so the
    // configuration frame is applied before this request is served.
    let after = suggest(&mut worker, "q");
    assert_eq!(
        after.items[0].label, "result-limit=20,theme=dark",
        "the plugin must observe exactly the published state"
    );
    assert_eq!(
        meta(&after.items[0], "deliveries"),
        "1",
        "the hook must run exactly once per publication"
    );

    worker.shutdown();
}

#[test]
fn a_later_publication_replaces_the_whole_state_rather_than_merging_into_it() {
    require_host_interpreter!();
    let scratch = Scratch::new("configuration-replace");
    let src = scratch.subdir("source");
    write_file(&src.join("cfg.py"), CONFIGURATION_PLUGIN);
    let site = scratch.subdir("empty-site");
    let mut worker = spawn_worker("modern.configuration-replace", "cfg:Impl", &src, &site);

    worker
        .send_configuration(
            &BTreeMap::from([
                ("theme".to_owned(), "dark".to_owned()),
                ("removed".to_owned(), "yes".to_owned()),
            ]),
            true,
        )
        .expect("publication accepted");
    worker
        .send_configuration(&BTreeMap::from([("theme".to_owned(), "light".to_owned())]), true)
        .expect("publication accepted");

    let reply = suggest(&mut worker, "q");
    assert_eq!(
        reply.items[0].label, "theme=light",
        "a complete publication replaces the previous state; a merged one would keep `removed`"
    );
    assert_eq!(meta(&reply.items[0], "deliveries"), "2");

    worker.shutdown();
}

#[test]
fn a_plugin_that_implements_no_configuration_hook_is_unaffected_by_a_publication() {
    require_host_interpreter!();
    // Every SDK callback is optional. A publication to a plugin that ignores
    // configuration must be a no-op, not a protocol fault that stops the worker.
    let scratch = Scratch::new("configuration-absent");
    let src = scratch.subdir("source");
    write_file(
        &src.join("plain.py"),
        r#"
from crikey_sdk import Plugin, Item


class Impl(Plugin):
    def suggest(self, query, context):
        context.emit(Item(stable_id="plain-1", label="still-here", target="t"))
"#,
    );
    let site = scratch.subdir("empty-site");
    let mut worker = spawn_worker("modern.configuration-absent", "plain:Impl", &src, &site);

    worker
        .send_configuration(&BTreeMap::from([("theme".to_owned(), "dark".to_owned())]), true)
        .expect("publication accepted");

    let reply = suggest(&mut worker, "q");
    assert!(matches!(reply.state, BatchState::Final), "{:?}", reply.state);
    assert_eq!(reply.items[0].label, "still-here");
    assert!(
        worker.is_alive(),
        "an ignored publication must not stop the worker"
    );

    worker.shutdown();
}

#[test]
fn a_raising_configuration_hook_leaves_the_worker_serving() {
    require_host_interpreter!();
    // Configuration delivery is not a request anyone awaits, so a plugin bug in
    // `on_configuration` cannot be reported as a failed reply. It must instead
    // leave the worker usable, which is the only observable guarantee available.
    let scratch = Scratch::new("configuration-raises");
    let src = scratch.subdir("source");
    write_file(
        &src.join("bad.py"),
        r#"
from crikey_sdk import Plugin, Item


class Impl(Plugin):
    def on_configuration(self, values):
        raise RuntimeError("configuration hook is broken")

    def suggest(self, query, context):
        context.emit(Item(stable_id="bad-1", label="survived", target="t"))
"#,
    );
    let site = scratch.subdir("empty-site");
    let mut worker = spawn_worker("modern.configuration-raises", "bad:Impl", &src, &site);

    worker
        .send_configuration(&BTreeMap::from([("theme".to_owned(), "dark".to_owned())]), true)
        .expect("publication accepted");

    let reply = suggest(&mut worker, "q");
    assert!(
        matches!(reply.state, BatchState::Final),
        "a raising configuration hook must not poison the next request, got {:?}",
        reply.state
    );
    assert_eq!(reply.items[0].label, "survived");
    assert!(worker.is_alive());

    worker.shutdown();
}

#[test]
fn publishing_configuration_to_a_dead_worker_is_an_error_rather_than_a_silent_loss() {
    require_host_interpreter!();
    // A publication that vanished into a dead worker would leave the launcher
    // believing a plugin holds a state it never received.
    let scratch = Scratch::new("configuration-dead");
    let src = scratch.subdir("source");
    write_file(
        &src.join("crasher.py"),
        r#"
import os

from crikey_sdk import Plugin


class Impl(Plugin):
    def suggest(self, query, context):
        os._exit(9)
"#,
    );
    let site = scratch.subdir("empty-site");
    let mut worker = spawn_worker("modern.configuration-dead", "crasher:Impl", &src, &site);

    let crash = worker
        .suggest(&SuggestRequest {
            generation: 1,
            text: "boom".to_owned(),
            normalized: "boom".to_owned(),
            selected_item_id: None,
        })
        .expect_err("an interpreter that exits mid-call is a transport fault");
    assert!(
        matches!(crash, crikey_python_host::HostError::Crashed { .. }),
        "expected a crashed-worker error, got {crash}"
    );
    assert!(!worker.is_alive());

    let error = worker
        .send_configuration(&BTreeMap::from([("theme".to_owned(), "dark".to_owned())]), true)
        .expect_err("the caller must learn the plugin did not receive the state");
    assert!(
        matches!(error, crikey_python_host::HostError::Crashed { .. }),
        "expected a crashed-worker error, got {error}"
    );
}

// ---------------------------------------------------------------------------
// The `environment` permission (spec 20)
// ---------------------------------------------------------------------------

/// `permissions.environment` has to decide the shape of the child, or it is a
/// manifest field that reads like a confinement and confines nothing.
///
/// Both halves are the test. Stripped-by-default is what makes the grant worth
/// declaring; inherited-when-granted is what makes it honest. A build where
/// both settings produced the same interpreter would pass any test that only
/// checked one of them.
#[test]
fn the_environment_grant_decides_whether_the_child_sees_the_hosts_own_variables() {
    require_host_interpreter!();
    let scratch = Scratch::new("environment-grant");
    let src = scratch.subdir("source");
    write_file(
        &src.join("entry.py"),
        r#"
import os

from crikey_sdk import Plugin, Item


class Impl(Plugin):
    def suggest(self, query, context):
        context.emit(
            Item(
                stable_id="env-1",
                label=os.environ.get(query.text, "absent"),
                target="t",
            )
        )
"#,
    );
    let site = scratch.subdir("empty-site");
    let env = MaterializedEnvironment {
        id: EnvironmentId("env-environment-grant".to_owned()),
        site_dir: site.clone(),
    };
    let import_path = ImportPath::assemble(&src, &[], &env, &sdk_python_dir());
    let interpreter = host_interpreter().expect("host interpreter was checked at test entry");

    // A name no other process can be holding, so "absent" can only mean the
    // host stripped it rather than that the host never had it.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let key = format!(
        "CRIKEY_HOST_ONLY_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    std::env::set_var(&key, "host-only-value");

    let label = |inherited: bool| {
        let options = WorkerOptions::new(
            PluginId("modern.environment".to_owned()),
            "entry:Impl".to_owned(),
            import_path.clone(),
        )
        .with_shared_budget(shared_budget_from_section(&ConcurrencySection::default()))
        .with_startup_timeout_ms(STARTUP_BUDGET_MS)
        .with_call_timeout_ms(CALL_BUDGET_MS)
        .with_shutdown_timeout_ms(SHUTDOWN_BUDGET_MS)
        .with_environment_inheritance(inherited);
        let mut worker = ModernWorker::spawn(&interpreter, options)
            .unwrap_or_else(|error| panic!("the env fixture spawns a worker, got {error}"));
        let reply = suggest(&mut worker, &key);
        assert_eq!(
            reply.state,
            BatchState::Final,
            "environment fixture must return a final batch: {reply:?}"
        );
        let label = reply.items[0].label.clone();
        worker.shutdown();
        label
    };

    assert_eq!(
        label(false),
        "absent",
        "a plugin that never declared `permissions.environment` must not inherit the host's variables"
    );
    assert_eq!(
        label(true),
        "host-only-value",
        "the grant must actually pass the ambient environment through"
    );

    std::env::remove_var(&key);
}

fn command_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../compatibility/test-plugins/modern-commands")
}

#[test]
fn commands_use_catalog_and_suggest_instance_and_preserve_exact_targets() {
    require_host_interpreter!();
    use crikey_core::command::{CommandAvailability, ExecutableTarget, PluginCommands};
    for entrypoint in ["commands_plugin:CommandsPlugin", "commands_plugin:AsyncCommands"] {
        let scratch = Scratch::new("commands-origin");
        let site = scratch.subdir("site");
        let mut worker = spawn_worker("exact.owner", entrypoint, &command_fixture(), &site);
        assert!(worker.supports_commands());
        let incarnation = worker.incarnation();
        // A fresh callback cannot fabricate the catalog state needed by commands.
        assert!(matches!(
            worker.commands(),
            Err(crikey_python_host::HostError::CommandsFailed(_))
        ));
        let catalog = worker.build_catalog().expect("real catalog callback");
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].plugin_id, PluginId("exact.owner".into()));
        assert_eq!(suggest(&mut worker, "valid").items.len(), 1);
        let PluginCommands::Declarations(commands) = worker.commands().expect("declarations") else {
            panic!("commands must be explicitly supported")
        };
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].command_id, "durable-primary");
        assert_eq!(commands[0].label, "Declared primary");
        assert_eq!(commands[0].item_id, catalog[0].stable_id);
        assert_eq!(commands[0].action_id, None);
        assert_eq!(commands[0].argument_policy, crikey_core::ArgumentPolicy::Optional);
        assert_eq!(
            commands[1].action_id,
            Some(crikey_core::ActionId("alternate".into()))
        );
        assert_eq!(commands[1].argument_policy, crikey_core::ArgumentPolicy::Required);
        // Binding is host-controlled and does not claim catalog validation or
        // authorize execution. Provider integration validates before Available.
        let bound = commands[1].clone().bind(
            PluginId("host-selected.owner".into()),
            CommandAvailability::Unavailable,
        );
        assert_eq!(
            bound.reference.provider_id,
            PluginId("host-selected.owner".into())
        );
        assert_eq!(bound.availability, CommandAvailability::Unavailable);
        assert_eq!(
            bound.executable_target,
            ExecutableTarget::Item {
                item_id: catalog[0].stable_id.clone(),
                action_id: Some(crikey_core::ActionId("alternate".into())),
            }
        );
        assert_eq!(worker.incarnation(), incarnation);
        assert!(worker.poll_alive());
        worker.shutdown();
    }
}

#[test]
fn commands_refuse_malformed_and_oversized_callbacks_without_losing_instance() {
    require_host_interpreter!();
    use crikey_core::command::PluginCommands;
    let scratch = Scratch::new("commands-bounds");
    let site = scratch.subdir("site");
    let mut worker = spawn_worker(
        "commands.bounds",
        "commands_plugin:CommandsPlugin",
        &command_fixture(),
        &site,
    );
    worker.build_catalog().expect("catalog primes this instance");
    let incarnation = worker.incarnation();
    for mode in [
        "malformed",
        "duplicate",
        "control",
        "unicode-id",
        "policy",
        "label",
        "count",
        "aggregate",
        "escaped-frame",
        "raises",
    ] {
        assert!(matches!(suggest(&mut worker, mode).state, BatchState::Final));
        let error = worker.commands().expect_err(mode);
        assert!(
            matches!(error, crikey_python_host::HostError::CommandsFailed(_)),
            "{mode}: {error}"
        );
        assert!(
            error.to_string().len() < 256,
            "failure must not echo huge callback values"
        );
        assert!(worker.poll_alive(), "{mode} must not corrupt the protocol");
        suggest(&mut worker, "valid");
        assert!(
            matches!(&worker.commands().expect("recovery"), PluginCommands::Declarations(v) if v.len() == 2)
        );
        assert_eq!(worker.incarnation(), incarnation);
    }
    for (mode, count) in [
        ("empty", 0),
        ("limits", 1),
        ("max-count", 10000),
        ("exact-aggregate", 511),
    ] {
        suggest(&mut worker, mode);
        let PluginCommands::Declarations(commands) = worker.commands().expect(mode) else {
            panic!("{mode} should be accepted")
        };
        assert_eq!(commands.len(), count);
    }
    suggest(&mut worker, "valid");
    worker.cancel_handle().cancel();
    assert_eq!(
        worker.commands().expect("latched cancellation"),
        PluginCommands::Cancelled
    );
    worker.cancel_handle().reset();
    assert!(matches!(
        worker.commands().expect("after cancellation"),
        PluginCommands::Declarations(_)
    ));
    worker.shutdown();
}

#[test]
fn commands_without_override_are_explicitly_unsupported_not_metadata_inferred() {
    require_host_interpreter!();
    use crikey_core::command::PluginCommands;
    let scratch = Scratch::new("commands-unsupported");
    let site = scratch.subdir("site");
    let mut worker = spawn_worker(
        "commands.unsupported",
        "commands_plugin:WithoutCommands",
        &command_fixture(),
        &site,
    );
    assert!(!worker.supports_commands());
    assert_eq!(worker.build_catalog().expect("ordinary catalog").len(), 1);
    assert_eq!(
        worker.commands().expect("unsupported is not an empty catalog"),
        PluginCommands::Unsupported
    );
    assert!(worker.poll_alive());
    worker.shutdown();
}

fn typed_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../compatibility/test-plugins/modern-typed")
}

fn typed_values(name: &str, password: &str) -> crikey_core::structured_arguments::ArgumentValues {
    use crikey_core::structured_arguments::*;
    ArgumentValues(BTreeMap::from([
        (
            FieldId("name".into()),
            StructuredValue::NonSecret(NonSecretValue::Text(name.into())),
        ),
        (
            FieldId("password".into()),
            StructuredValue::Password(Password::new(password.into()).unwrap()),
        ),
        (
            FieldId("region".into()),
            StructuredValue::NonSecret(NonSecretValue::Dropdown(OptionId("local".into()))),
        ),
    ]))
}

#[test]
fn typed_real_worker_actionless_async_fixed_outcomes_and_healthy_reacquire() {
    require_host_interpreter!();
    use crikey_core::structured_arguments::SubmissionOutcome;
    let scratch = Scratch::new("typed-real");
    let site = scratch.subdir("site");
    let mut worker = spawn_worker(
        "typed-fixture",
        "typed_plugin:TypedPlugin",
        &typed_fixture(),
        &site,
    );
    assert!(worker.supports_typed_arguments());
    assert!(worker.supports_result_presentation());
    let catalog = worker.build_catalog().expect("typed catalog");
    let schema = Arc::clone(&worker.presentation().schemas[0].schema);
    assert!(matches!(
        worker
            .typed_execute(
                &catalog[0],
                None,
                &schema,
                typed_values("valid", "typed-password"),
                None,
            )
            .unwrap(),
        crikey_core::structured_view::TypedExecutionOutcome::Submission(SubmissionOutcome::Completed)
    ));
    assert!(matches!(
        worker
            .typed_execute(
                &catalog[0],
                None,
                &schema,
                typed_values("reject", "typed-password"),
                None,
            )
            .unwrap(),
        crikey_core::structured_view::TypedExecutionOutcome::Submission(SubmissionOutcome::InvalidFields(_))
    ));
    let action_schema = Arc::clone(&worker.presentation().schemas[1].schema);
    assert!(matches!(
        worker
            .typed_execute(
                &catalog[1],
                Some("submit"),
                &action_schema,
                typed_values("valid", "action-password"),
                None,
            )
            .unwrap(),
        crikey_core::structured_view::TypedExecutionOutcome::Submission(SubmissionOutcome::Completed)
    ));
    let incarnation = worker.incarnation();
    worker.shutdown();
    let mut replacement = spawn_worker(
        "typed-fixture",
        "typed_plugin:TypedPlugin",
        &typed_fixture(),
        &site,
    );
    assert_ne!(replacement.incarnation(), incarnation);
    assert!(replacement
        .confirm_typed_schema(&catalog[0], None, &schema)
        .unwrap());
    assert!(matches!(
        replacement
            .typed_execute(
                &catalog[0],
                None,
                &schema,
                typed_values("valid", "new-password"),
                None
            )
            .unwrap(),
        crikey_core::structured_view::TypedExecutionOutcome::Submission(SubmissionOutcome::Completed)
    ));
    let mut changed = (*schema).clone();
    changed.fields[0].label.push_str(" changed");
    assert!(!replacement
        .confirm_typed_schema(&catalog[0], None, &changed)
        .unwrap());
    replacement.shutdown();
}

#[test]
fn every_typed_submission_suppresses_remaining_incarnation_diagnostics() {
    require_host_interpreter!();
    use crikey_core::structured_arguments::SubmissionOutcome;
    let scratch = Scratch::new("typed-privacy");
    let site = scratch.subdir("site");
    let mut worker = spawn_worker(
        "typed-fixture",
        "typed_plugin:TypedPlugin",
        &typed_fixture(),
        &site,
    );
    let catalog = worker.build_catalog().unwrap();
    let schema = Arc::clone(&worker.presentation().schemas[0].schema);
    let password = "PASSWORD_MUST_NOT_REACH_DIAGNOSTICS_47";
    let result = worker
        .typed_execute(&catalog[0], None, &schema, typed_values("leak", password), None)
        .unwrap();
    assert!(matches!(
        result,
        crikey_core::structured_view::TypedExecutionOutcome::Submission(SubmissionOutcome::Failed(_))
    ));
    assert!(!format!("{result:?}").contains(password));
    let later = suggest(&mut worker, "typed-leak-after");
    assert!(later.log.is_empty());
    assert!(!format!("{later:?}").contains(password));
    worker.send_configuration(&BTreeMap::new(), true).unwrap();
    assert!(!format!("{worker:?}").contains(password));
    let crash = worker
        .suggest(&SuggestRequest {
            generation: 2,
            text: "typed-crash-after".into(),
            normalized: "typed-crash-after".into(),
            selected_item_id: None,
        })
        .unwrap_err();
    assert!(!format!("{crash:?}").contains(password));
    worker.shutdown();
}

#[test]
fn typed_scopes_are_explicit_and_preserve_query_generation() {
    require_host_interpreter!();
    let scratch = Scratch::new("typed-scopes");
    let site = scratch.subdir("site");
    let mut worker = spawn_worker(
        "typed-fixture",
        "typed_plugin:TypedPlugin",
        &typed_fixture(),
        &site,
    );
    for (generation, scope, item) in [(41, "local", "typed-actionless"), (42, "remote", "typed-action")] {
        let reply = worker
            .suggest_scoped(
                &SuggestRequest {
                    generation,
                    text: "typed".into(),
                    normalized: "typed".into(),
                    selected_item_id: None,
                },
                Some(scope),
            )
            .unwrap();
        assert_eq!(reply.state, BatchState::Final);
        assert_eq!(reply.items.len(), 1);
        assert_eq!(reply.items[0].stable_id.0, item);
        assert_eq!(reply.presentation.schemas.len(), 1);
    }
    worker.shutdown();
}

#[test]
fn typed_sync_callback_and_sdk_independent_fingerprint_refusal() {
    require_host_interpreter!();
    use crikey_core::structured_arguments::*;
    let scratch = Scratch::new("typed-sdk-binding");
    let src = scratch.subdir("source");
    write_file(
        &src.join("binding.py"),
        r#"
from crikey_sdk import Plugin, Item, ArgumentSchema, TextField, SchemaBinding, ResultPresentation
SCHEMA = ArgumentSchema((TextField(id="text", label="Text", required=True),))
class Sync(Plugin):
    drift = False
    def catalog_presentation(self):
        return ResultPresentation(schemas=(SchemaBinding("sync", SCHEMA),))
    def build_catalog(self):
        if self.drift:
            object.__setattr__(SCHEMA.fields[0], "label", "Changed after publication")
        return [Item("sync", "Sync typed", "sync")]
    def execute_typed(self, item, action_id, values):
        assert values.get("text").value == "sentinel"
        return None
    def execute(self, *args):
        raise AssertionError("legacy callback must not run")
class Drift(Sync):
    drift = True
"#,
    );
    let site = scratch.subdir("site");
    for (class, expected) in [
        ("Sync", SubmissionOutcome::Completed),
        (
            "Drift",
            SubmissionOutcome::Failed(SubmissionFailureCode::Unavailable),
        ),
    ] {
        let mut worker = spawn_worker("binding", &format!("binding:{class}"), &src, &site);
        let items = worker.build_catalog().unwrap();
        let schema = Arc::clone(&worker.presentation().schemas[0].schema);
        if class == "Sync" {
            let mut suggestion_first = spawn_worker("binding", "binding:Sync", &src, &site);
            let empty = suggest(&mut suggestion_first, "schema-free-suggestion");
            assert!(empty.presentation.schemas.is_empty());
            assert!(suggestion_first
                .confirm_typed_schema(&items[0], None, &schema)
                .unwrap());
            suggestion_first.shutdown();
        }
        let values = ArgumentValues(BTreeMap::from([(
            FieldId("text".into()),
            StructuredValue::NonSecret(NonSecretValue::Text("sentinel".into())),
        )]));
        assert_eq!(
            worker
                .typed_execute(&items[0], None, &schema, values, None)
                .unwrap(),
            crikey_core::structured_view::TypedExecutionOutcome::Submission(expected)
        );
        worker.shutdown();
    }
}

#[test]
fn real_worker_malformed_typed_metadata_fails_before_admission() {
    require_host_interpreter!();
    let scratch = Scratch::new("typed-malformed");
    let site = scratch.subdir("site");
    let cases = [
        serde_json::json!({"fields": (0..65).map(|n| serde_json::json!({"id":format!("f{n}"),"label":"Field","kind":"text"})).collect::<Vec<_>>()}),
        serde_json::json!({"fields":[{"id":"p","label":"Password","kind":"password","default":null}]}),
        serde_json::json!({"fields":[{"id":"x","label":"First","kind":"text"},{"id":"x","label":"Duplicate","kind":"text"}]}),
        serde_json::json!({"fields":[{"id":"x".repeat(257),"label":"Long ID","kind":"text"}]}),
        serde_json::json!({"fields":[{"id":"x","label":"x".repeat(8193),"kind":"text"}]}),
        serde_json::json!({"fields":[{"id":"x","label":"Options","kind":"dropdown","options":(0..257).map(|n| serde_json::json!({"id":format!("o{n}"),"label":"Option"})).collect::<Vec<_>>()}]}),
    ];
    for (n, schema) in cases.into_iter().enumerate() {
        let src = scratch.subdir(&format!("case-{n}"));
        let frame = serde_json::json!({"id":2,"kind":"result_batch","generation":1,"state":"final","items":[],"log":[],"presentation":{"schemas":[{"item_id":"x","action_id":null,"schema":schema}]}});
        let encoded = serde_json::to_string(&frame).unwrap() + "\n";
        let literal = serde_json::to_string(&encoded).unwrap();
        write_file(&src.join("malformed.py"), &format!("import __main__,json\nfrom crikey_sdk import Plugin\nclass Impl(Plugin):\n def suggest(self,q,c):\n  __main__._PROTOCOL.write({literal})\n  __main__._PROTOCOL.flush()\n"));
        let mut worker = spawn_worker("malformed", "malformed:Impl", &src, &site);
        let result = worker.suggest(&SuggestRequest {
            generation: 1,
            text: "x".into(),
            normalized: "x".into(),
            selected_item_id: None,
        });
        assert!(matches!(result, Err(crikey_python_host::HostError::Protocol(_))));
        assert!(!worker.is_alive());
        worker.shutdown();
    }
}

#[test]
fn retired_source_gate_never_sends_typed_callback_and_worker_remains_usable() {
    require_host_interpreter!();
    use crikey_core::structured_arguments::*;
    let scratch = Scratch::new("typed-admission");
    let site = scratch.subdir("site");
    let mut worker = spawn_worker(
        "typed-fixture",
        "typed_plugin:TypedPlugin",
        &typed_fixture(),
        &site,
    );
    let items = worker.build_catalog().unwrap();
    let schema = Arc::clone(&worker.presentation().schemas[0].schema);
    assert_eq!(
        worker
            .typed_execute_with_admission(
                &items[0],
                None,
                &schema,
                typed_values("leak", "retired-secret"),
                None,
                |_send| false
            )
            .unwrap(),
        crikey_core::structured_view::TypedExecutionOutcome::Submission(SubmissionOutcome::Failed(
            SubmissionFailureCode::Unavailable
        ))
    );
    assert!(worker.is_alive());
    assert!(matches!(
        worker
            .typed_execute(
                &items[0],
                None,
                &schema,
                typed_values("valid", "new-secret"),
                None
            )
            .unwrap(),
        crikey_core::structured_view::TypedExecutionOutcome::Submission(SubmissionOutcome::Completed)
    ));
    worker.shutdown();
}

#[test]
fn sdk_process_latch_blocks_mirroring_formatting_threads_and_queued_logs() {
    require_host_interpreter!();
    use crikey_core::structured_arguments::*;
    let scratch = Scratch::new("typed-sdk-privacy");
    let src = scratch.subdir("source");
    let site = scratch.subdir("site");
    write_file(
        &src.join("privacy.py"),
        r#"
import __main__, logging, sys, threading
from crikey_sdk import Plugin, Item, ArgumentSchema, TextField, SchemaBinding, ResultPresentation
formats = 0
mirrors = 0
SCHEMA = ArgumentSchema((TextField(id="text", label="Text"),))
class Probe:
    def __str__(self):
        global formats
        formats += 1
        return "SECRET_FORMAT_PROBE"
class ProbeException(RuntimeError):
    def __str__(self):
        global formats
        formats += 1
        return "SECRET_EXCEPTION_PROBE"
class Mirror:
    encoding = "utf-8"
    errors = "strict"
    def write(self, text):
        global mirrors
        mirrors += 1
        return len(text)
    def flush(self):
        pass
class Impl(Plugin):
    def build_catalog(self):
        return [Item("typed", "Typed", "typed")]
    def catalog_presentation(self):
        return ResultPresentation(schemas=(SchemaBinding("typed", SCHEMA),))
    def execute_typed(self, item, action, values):
        __main__._CAPTURE._mirror = Mirror()
        sys.stdout.write(Probe())
        __main__._CAPTURE.line(Probe())
        handler = logging.StreamHandler(sys.stderr)
        handler.handle(logging.LogRecord("probe", logging.ERROR, "probe.py", 1, Probe(), (), None))
        def fail():
            raise ProbeException()
        thread = threading.Thread(target=fail)
        thread.start()
        thread.join()
        class BrokenFinalizer:
            def __del__(self):
                raise ProbeException()
        finalizer = BrokenFinalizer()
        del finalizer
        return None
    def suggest(self, query, context):
        if query.text == "report":
            context.emit(Item("report", "Report", "report", metadata={"formats": str(formats), "mirrors": str(mirrors)}))
        else:
            raise ProbeException()
    def on_configuration(self, values):
        raise ProbeException()
    def stop(self):
        raise ProbeException()
"#,
    );
    let mut worker = spawn_worker("privacy", "privacy:Impl", &src, &site);
    let items = worker.build_catalog().unwrap();
    assert!(worker.supports_typed_arguments());
    assert!(!worker.supports_result_presentation());
    let schema = Arc::clone(&worker.presentation().schemas[0].schema);
    let values = ArgumentValues(BTreeMap::from([(
        FieldId("text".into()),
        StructuredValue::NonSecret(NonSecretValue::Text("not-a-password-field".into())),
    )]));
    assert_eq!(
        worker
            .typed_execute(&items[0], None, &schema, values, None)
            .unwrap(),
        crikey_core::structured_view::TypedExecutionOutcome::Submission(SubmissionOutcome::Completed)
    );
    let failed = suggest(&mut worker, "fail");
    assert_eq!(failed.state, BatchState::Failed);
    assert!(failed.log.is_empty());
    worker.send_configuration(&BTreeMap::new(), true).unwrap();
    let report = suggest(&mut worker, "report");
    assert_eq!(meta(&report.items[0], "formats"), "0");
    assert_eq!(meta(&report.items[0], "mirrors"), "0");
    worker.shutdown();
}

#[test]
fn old_scope_cancel_race_cannot_reset_the_next_scoped_request() {
    require_host_interpreter!();
    let scratch = Scratch::new("typed-scope-race");
    let site = scratch.subdir("site");
    let mut worker = spawn_worker(
        "typed-fixture",
        "typed_plugin:TypedPlugin",
        &typed_fixture(),
        &site,
    );
    let cancel = worker.cancel_handle();
    let old = std::thread::spawn(move || {
        let reply = worker.suggest_scoped(
            &SuggestRequest {
                generation: 61,
                text: "typed-scope-wait".into(),
                normalized: "typed-scope-wait".into(),
                selected_item_id: None,
            },
            Some("local"),
        );
        (worker, reply)
    });
    cancel.cancel();
    let (mut worker, old) = old.join().unwrap();
    let old = old.unwrap();
    assert_eq!(old.state, BatchState::Cancelled);
    assert!(old.presentation.schemas.is_empty());
    cancel.reset();
    let current = worker
        .suggest_scoped(
            &SuggestRequest {
                generation: 62,
                text: "typed".into(),
                normalized: "typed".into(),
                selected_item_id: None,
            },
            Some("remote"),
        )
        .unwrap();
    assert_eq!(current.state, BatchState::Final);
    assert_eq!(current.items.len(), 1);
    assert_eq!(current.items[0].stable_id.0, "typed-action");
    worker.shutdown();
}

#[test]
fn presentation_only_negotiation_grants_no_typed_execution() {
    require_host_interpreter!();
    use crikey_core::structured_arguments::*;
    let scratch = Scratch::new("presentation-only");
    let src = scratch.subdir("source");
    let site = scratch.subdir("site");
    write_file(
        &src.join("visual.py"),
        r#"
from crikey_sdk import Plugin, Item, ResultPresentation, Section, Scope, ItemPresentation
class Impl(Plugin):
    def supports_result_presentation(self):
        return True
    def build_catalog(self):
        return [Item("ordinary", "Ordinary", "ordinary")]
    def catalog_presentation(self):
        return ResultPresentation(sections=(Section("group", "Group"),),
                                  scopes=(Scope("scope", "Scope"),),
                                  items=(ItemPresentation("ordinary", "group", ("scope",)),))
"#,
    );
    let mut worker = spawn_worker("visual", "visual:Impl", &src, &site);
    assert!(!worker.supports_typed_arguments());
    assert!(worker.supports_result_presentation());
    let items = worker.build_catalog().unwrap();
    assert_eq!(worker.presentation().sections.len(), 1);
    assert!(worker.presentation().schemas.is_empty());
    assert_eq!(
        worker
            .typed_execute(
                &items[0],
                None,
                &ArgumentSchema::default(),
                ArgumentValues::default(),
                None,
            )
            .unwrap(),
        crikey_core::structured_view::TypedExecutionOutcome::Submission(SubmissionOutcome::Failed(
            SubmissionFailureCode::Unsupported
        ))
    );
    assert!(matches!(
        worker.execute(&items[0], None, None, None).unwrap(),
        crikey_python_host::ExecuteOutcome::Ok
    ));
    worker.shutdown();
}

#[test]
fn failed_and_cancelled_queries_retire_previous_schema_authority() {
    require_host_interpreter!();
    use crikey_core::structured_arguments::*;
    let scratch = Scratch::new("query-schema-retirement");
    let src = scratch.subdir("source");
    let site = scratch.subdir("site");
    write_file(
        &src.join("retirement.py"),
        r#"
from crikey_sdk import Plugin, Item, ArgumentSchema, TextField, SchemaBinding, ResultPresentation, TypedOutcome
SCHEMA = ArgumentSchema((TextField(id="text", label="Text"),))
class Impl(Plugin):
    def build_catalog(self):
        return []
    def suggestion_presentation(self, query):
        return ResultPresentation(schemas=(SchemaBinding("dynamic", SCHEMA),))
    def suggest(self, query, context):
        if query.text == "fail":
            raise RuntimeError("fixed fixture failure")
        if query.text == "cancel":
            context._is_cancelled.__self__.set()
            return
        context.emit(Item("dynamic", "Dynamic", "dynamic"))
    def execute_typed(self, item, action, values):
        return TypedOutcome(code="denied")
    def execute(self, *args):
        return None
"#,
    );
    let mut worker = spawn_worker("retirement", "retirement:Impl", &src, &site);
    for (mode, state) in [("fail", BatchState::Failed), ("cancel", BatchState::Cancelled)] {
        let ready = suggest(&mut worker, "ready");
        let schema = Arc::clone(&ready.presentation.schemas[0].schema);
        assert!(worker
            .confirm_typed_schema(&ready.items[0], None, &schema)
            .unwrap());
        assert_eq!(suggest(&mut worker, mode).state, state);
        worker.cancel_handle().reset();
        assert!(!worker
            .confirm_typed_schema(&ready.items[0], None, &schema)
            .unwrap());
        let values = ArgumentValues(BTreeMap::from([(
            FieldId("text".into()),
            StructuredValue::NonSecret(NonSecretValue::Text("sentinel".into())),
        )]));
        assert_eq!(
            worker
                .typed_execute(&ready.items[0], None, &schema, values, None)
                .unwrap(),
            crikey_core::structured_view::TypedExecutionOutcome::Submission(SubmissionOutcome::Failed(
                SubmissionFailureCode::Unavailable
            ))
        );
        // Retiring execution authority must not turn an old schema-only target
        // into a legacy callback under hosts that cannot consume its sidecar.
        assert!(matches!(
            worker.execute(&ready.items[0], None, None, None).unwrap(),
            crikey_python_host::ExecuteOutcome::Failed(_)
        ));
    }
    worker.shutdown();
}

#[test]
fn complete_escaped_catalog_frame_is_refused_without_killing_the_worker() {
    require_host_interpreter!();
    let scratch = Scratch::new("complete-presentation-frame");
    let src = scratch.subdir("source");
    let site = scratch.subdir("site");
    write_file(
        &src.join("complete_frame.py"),
        r#"
from crikey_sdk import Plugin, Item, ResultPresentation, Section, Scope, ItemPresentation
LABEL = "\u0080" * 4096
SCOPES = tuple("scope" + str(n) for n in range(128))
class Impl(Plugin):
    def __init__(self):
        self.large = True
    def supports_result_presentation(self):
        return True
    def catalog_presentation(self):
        if not self.large:
            return ResultPresentation()
        return ResultPresentation(
            sections=tuple(Section("s" + str(n), LABEL) for n in range(128)),
            scopes=tuple(Scope(id, LABEL) for id in SCOPES),
            items=tuple(ItemPresentation("item" + str(n), "s0", SCOPES) for n in range(1024)))
    def build_catalog(self):
        if not self.large:
            return [Item("recovered", "Recovered", "recovered")]
        self.large = False
        for _ in range(512):
            print("L" * 4096)
        return [Item("item" + str(n), "Item", "item") for n in range(1024)]
"#,
    );
    let mut worker = spawn_worker("complete-frame", "complete_frame:Impl", &src, &site);
    let incarnation = worker.incarnation();
    assert!(matches!(
        worker.build_catalog(),
        Err(crikey_python_host::HostError::PluginFailed { .. })
    ));
    assert!(worker.poll_alive());
    let recovered = worker.build_catalog().unwrap();
    assert_eq!(worker.incarnation(), incarnation);
    assert_eq!(recovered[0].stable_id.0, "recovered");
    worker.shutdown();
}

#[test]
fn standard_routes_preserve_navigation_frozen_definitions_and_typed_outcomes() {
    require_host_interpreter!();
    use crikey_core::structured_arguments::SubmissionOutcome;
    use crikey_core::structured_view::*;
    use std::num::NonZeroU64;
    let scratch = Scratch::new("standard-views");
    let site = scratch.subdir("site");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../compatibility/test-plugins/modern-views");
    let mut worker = spawn_worker("views-fixture", "views_plugin:ViewsPlugin", &fixture, &site);
    assert!(worker.supports_structured_views());
    assert!(worker.supports_typed_arguments());
    assert!(!worker.supports_result_presentation());
    let root = worker.build_catalog().unwrap();
    assert!(matches!(worker.execute(&root[0], None, None, None).unwrap(),
        crikey_python_host::ExecuteOutcome::Navigate(StructuredRouteOutcome::Push(RouteId(route))) if route=="gallery"));
    let root_schema = Arc::clone(&worker.presentation().schemas[0].schema);
    assert!(
        matches!(worker.typed_execute(&root[1],None,&root_schema,typed_values("valid","root-secret"),None).unwrap(),
        TypedExecutionOutcome::Navigate(StructuredRouteOutcome::Push(RouteId(route))) if route=="gallery")
    );
    let session = ViewSessionIdentity {
        logical_view: NonZeroU64::new(1).unwrap(),
        surface: NonZeroU64::new(2).unwrap(),
    };
    let mut request = ViewRequest {
        route: RouteId("gallery".into()),
        session,
        mode: ViewRequestMode::Open,
        previous_revision: None,
        state: None,
        query: String::new(),
        scroll: None,
        reason: ViewRefreshReason::Explicit,
    };
    let gallery = worker.structured_view(&request).unwrap();
    assert!(matches!(gallery.descriptor.view, StandardView::Grid(_)));
    let item = &gallery.descriptor.definitions[0].item;
    assert!(matches!(
        worker
            .execute(&root[0], Some("open"), None, Some(&gallery.context))
            .unwrap(),
        crikey_python_host::ExecuteOutcome::Failed(_)
    ));
    assert!(
        matches!(worker.execute(item,Some("open"),None,Some(&gallery.context)).unwrap(),
        crikey_python_host::ExecuteOutcome::Navigate(StructuredRouteOutcome::Push(RouteId(route))) if route=="detail")
    );
    request.mode = ViewRequestMode::Refresh;
    request.previous_revision = Some(gallery.context.revision);
    let admission = RouteAdmission::worker(
        RouteIdentity {
            owner: item.plugin_id.clone(),
            route: gallery.context.route.clone(),
            incarnation: NonZeroU64::new(worker.incarnation()).unwrap(),
        },
        gallery.context.surface(),
    )
    .unwrap();
    request.state = Some(gallery.descriptor.initial_state(&admission).unwrap());
    let refreshed = worker.structured_view(&request).unwrap();
    assert_ne!(gallery.context.revision, refreshed.context.revision);
    assert!(matches!(
        worker
            .execute(item, Some("open"), None, Some(&gallery.context))
            .unwrap(),
        crikey_python_host::ExecuteOutcome::Failed(_)
    ));
    request.route = RouteId("form".into());
    request.mode = ViewRequestMode::Open;
    request.previous_revision = None;
    request.state = None;
    let form = worker.structured_view(&request).unwrap();
    let item = &form.descriptor.definitions[0].item;
    let StandardView::Form(view) = &form.descriptor.view else {
        panic!("form descriptor")
    };
    let schema = form.descriptor.schema_for(&view.submit).unwrap();
    assert!(matches!(
        worker
            .typed_execute(
                item,
                None,
                schema,
                typed_values("reject", "route-password"),
                Some(&form.context)
            )
            .unwrap(),
        TypedExecutionOutcome::Submission(SubmissionOutcome::InvalidFields(_))
    ));
    assert!(
        matches!(worker.typed_execute(item,None,schema,typed_values("valid","route-password"),Some(&form.context)).unwrap(),
        TypedExecutionOutcome::Navigate(StructuredRouteOutcome::Replace(RouteId(route))) if route=="done")
    );
    worker.retire_view();
    assert!(matches!(
        worker.execute(item, None, None, Some(&form.context)).unwrap(),
        crikey_python_host::ExecuteOutcome::Failed(_)
    ));
    worker.shutdown();
}

const VIEW_ADMISSION_SOURCE: &str = r#"
import __main__ as worker
import copy
from crikey_sdk import (
    Plugin, Item, Action, Push, Replace, ViewDescriptor, RouteItemDefinition,
    ActionSection, ActionSubmenu, ExecutablePresentation, ListView, DetailView,
    Preview, ItemPresentation, Section, Scope, ArgumentSchema, TextField,
)
SCHEMA = ArgumentSchema((TextField(id="name", label="Name", required=True),))
METADATA = {"tag": "application", "prefix": "plugin-defined:application"}
class Impl(Plugin):
    def __init__(self):
        self.corruption = None
        original = worker._emit
        def emit(frame):
            if frame.get("kind") == "structured_view_result" and frame.get("status") == "content" and frame["descriptor"]["definitions"]:
                descriptor = frame["descriptor"]
                view = descriptor["view"]
                definition = descriptor["definitions"][0]
                mode = self.corruption
                if mode == "duplicate-definitions":
                    descriptor["definitions"].append(copy.deepcopy(definition))
                elif mode == "duplicate-memberships":
                    view["items"].append(copy.deepcopy(view["items"][0]))
                elif mode == "duplicate-sections":
                    view["sections"].append(copy.deepcopy(view["sections"][0]))
                elif mode == "duplicate-scopes":
                    view["scopes"].append(copy.deepcopy(view["scopes"][0]))
                elif mode == "duplicate-item-scopes":
                    view["items"][0]["scopes"].append("scope")
                elif mode == "duplicate-actions":
                    definition["item"]["actions"].append(copy.deepcopy(definition["item"]["actions"][0]))
                elif mode == "duplicate-action-sections":
                    definition["action_sections"].append(dict(id="main", label="Other", entries=[]))
                elif mode == "duplicate-submenus":
                    definition["action_sections"].append(dict(id="other", label="Other",
                        entries=[dict(kind="submenu", id="menu", label="Other", entries=[])]))
                elif mode == "duplicate-menu-actions":
                    entries = definition["action_sections"][0]["entries"][0]["entries"]
                    entries.append(copy.deepcopy(entries[0]))
                elif mode in ("image-product", "image-u8", "image-float", "image-negative"):
                    pixels = ([0] * (1024 * 1024) if mode == "image-product" else
                              [256, 0, 0, 0] if mode == "image-u8" else
                              [0.0, 0, 0, 0] if mode == "image-float" else [-1, 0, 0, 0])
                    # Dimensions deliberately follow pixels, as JSON order grants no authority.
                    view["items"][0]["image"] = dict(rgba=pixels, height=1, width=1)
            return original(frame)
        worker._emit = emit
    def structured_view(self, request, context):
        if request.route == "unsafe":
            return ViewDescriptor("Unsafe", (), DetailView(
                Preview(text="[bad](javascript:alert(1))", markdown=True)))
        self.corruption = request.route
        category = {"shadow": "plugin-defined:application", "builtin": "application",
                    "custom": "custom"}.get(request.route, "plugin-defined")
        kwargs = {} if request.route == "default" else {"category": category}
        if request.query == "remove-selected":
            assert request.mode == "refresh" and request.state.selected_item_id == "same"
            assert request.anchor.item_id == "same" and request.anchor.row_offset == 7
        item_id = "replacement" if request.query == "remove-selected" else "same"
        item = Item(item_id, "Local", request.route, metadata=dict(METADATA),
            actions=[Action("ordinary", "Ordinary", applicable_categories=(category, "application", "plugin-defined:application"), execution_policy="plugin"),
                     Action("typed", "Typed", applicable_categories=(category, "application", "plugin-defined:application"), execution_policy="plugin")],
            **kwargs)
        definition = RouteItemDefinition(item, action_sections=(
            ActionSection("main", "Main", (ActionSubmenu("menu", "Menu", (
                ExecutablePresentation("ordinary"), ExecutablePresentation("typed", SCHEMA))),)),),
            primary_action="ordinary")
        return ViewDescriptor("Valid", (definition,), ListView(
            items=(ItemPresentation(item_id, "entries", () if request.query == "remove-scope" else ("scope",)),),
            sections=(Section("entries", "Entries"),),
            scopes=() if request.query == "remove-scope" else (Scope("scope", "Scope"),),
            initial_selection=item_id))
    def check(self, route, item):
        canonical = {"shadow": "plugin-defined:application", "builtin": "application",
                     "custom": "plugin-defined:custom"}.get(route, "plugin-defined:plugin-defined")
        assert item.target == route and item.category == canonical
        assert item.metadata == METADATA
        for action in item.actions:
            assert action.applicable_categories == (canonical, "application", "plugin-defined:application")
    def execute_view(self, route, item, action, argument):
        self.check(route, item)
        assert action == "ordinary" and argument is None
        return Push("done")
    async def execute_view_typed(self, route, item, action, values):
        self.check(route, item)
        assert action == "typed" and values.get("name").value == "typed"
        return Replace("done")
"#;

fn view_admission_source(scratch: &Scratch) -> PathBuf {
    let src = scratch.subdir("src");
    write_file(&src.join("admission.py"), VIEW_ADMISSION_SOURCE);
    src
}

fn view_admission_request(route: &str) -> crikey_core::structured_view::ViewRequest {
    use crikey_core::structured_view::*;
    use std::num::NonZeroU64;
    ViewRequest {
        mode: ViewRequestMode::Open,
        route: RouteId(route.into()),
        session: ViewSessionIdentity {
            logical_view: NonZeroU64::new(7).unwrap(),
            surface: NonZeroU64::new(9).unwrap(),
        },
        previous_revision: None,
        state: None,
        query: String::new(),
        scroll: None,
        reason: ViewRefreshReason::Explicit,
    }
}

#[test]
fn route_categories_round_trip_real_rust_host_for_ordinary_and_typed_callbacks() {
    require_host_interpreter!();
    use crikey_core::structured_arguments::*;
    use crikey_core::structured_view::*;
    use crikey_core::{ActionId, Category};
    let scratch = Scratch::new("view-category-roundtrip");
    let src = view_admission_source(&scratch);
    let site = scratch.subdir("site");
    let mut worker = spawn_worker("admission", "admission:Impl", &src, &site);
    for (route, expected) in [
        ("default", Category::PluginDefined("plugin-defined".into())),
        ("shadow", Category::PluginDefined("application".into())),
        ("builtin", Category::Application),
        ("custom", Category::PluginDefined("custom".into())),
    ] {
        let reply = worker.structured_view(&view_admission_request(route)).unwrap();
        let item = &reply.descriptor.definitions[0].item;
        assert_eq!(item.category, expected);
        assert_eq!(
            item.actions[0].applicable_categories,
            vec![
                expected,
                Category::Application,
                Category::PluginDefined("application".into())
            ]
        );
        assert_eq!(item.metadata.get("tag").map(String::as_str), Some("application"));
        assert!(
            matches!(worker.execute(item,Some("ordinary"),None,Some(&reply.context)).unwrap(),
            crikey_python_host::ExecuteOutcome::Navigate(StructuredRouteOutcome::Push(RouteId(route))) if route=="done")
        );
        let schema = reply.descriptor.definitions[0]
            .presentation(Some(&ActionId("typed".into())))
            .unwrap()
            .schema
            .as_ref()
            .unwrap();
        let values = ArgumentValues(BTreeMap::from([(
            FieldId("name".into()),
            StructuredValue::NonSecret(NonSecretValue::Text("typed".into())),
        )]));
        assert!(
            matches!(worker.typed_execute(item,Some("typed"),schema,values,Some(&reply.context)).unwrap(),
            TypedExecutionOutcome::Navigate(StructuredRouteOutcome::Replace(RouteId(route))) if route=="done")
        );
    }
    worker.shutdown();
}

#[test]
fn unsafe_public_sdk_detail_is_refused_without_killing_producer_or_retaining_old_authority() {
    require_host_interpreter!();
    let scratch = Scratch::new("view-safe-content-refusal");
    let src = view_admission_source(&scratch);
    let site = scratch.subdir("site");
    let mut worker = spawn_worker("admission", "admission:Impl", &src, &site);
    let old = worker
        .structured_view(&view_admission_request("default"))
        .unwrap();
    let incarnation = worker.incarnation();
    let error = worker
        .structured_view(&view_admission_request("unsafe"))
        .unwrap_err();
    assert!(format!("{error:?}").contains("structured view content rejected"));
    assert!(worker.poll_alive());
    assert_eq!(worker.incarnation(), incarnation);
    assert!(matches!(
        worker
            .execute(
                &old.descriptor.definitions[0].item,
                Some("ordinary"),
                None,
                Some(&old.context)
            )
            .unwrap(),
        crikey_python_host::ExecuteOutcome::Failed(_)
    ));
    let fresh = worker
        .structured_view(&view_admission_request("default"))
        .unwrap();
    assert!(matches!(
        worker
            .execute(
                &fresh.descriptor.definitions[0].item,
                Some("ordinary"),
                None,
                Some(&fresh.context)
            )
            .unwrap(),
        crikey_python_host::ExecuteOutcome::Navigate(_)
    ));
    worker.shutdown();
}

#[test]
fn malformed_route_images_and_namespace_duplicates_fail_at_real_host_boundary() {
    require_host_interpreter!();
    let scratch = Scratch::new("view-preflight-consumer");
    let src = view_admission_source(&scratch);
    let site = scratch.subdir("site");
    for route in [
        "image-product",
        "image-u8",
        "image-float",
        "image-negative",
        "duplicate-definitions",
        "duplicate-memberships",
        "duplicate-sections",
        "duplicate-scopes",
        "duplicate-item-scopes",
        "duplicate-actions",
        "duplicate-action-sections",
        "duplicate-submenus",
        "duplicate-menu-actions",
    ] {
        let mut worker = spawn_worker("admission", "admission:Impl", &src, &site);
        assert!(
            worker.structured_view(&view_admission_request(route)).is_err(),
            "{route}"
        );
        assert!(
            !worker.is_alive(),
            "{route} must fail allocation-free wire preflight"
        );
    }
}

#[test]
fn retired_view_writer_admission_never_calls_the_structured_callback() {
    require_host_interpreter!();
    let scratch = Scratch::new("view-final-writer-gate");
    let site = scratch.subdir("site");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../compatibility/test-plugins/modern-views");
    let mut worker = spawn_worker("views-fixture", "views_plugin:ViewsPlugin", &fixture, &site);
    let mut request = view_admission_request("gallery");
    // A callback that would terminate the physical source must never run.
    request.query = "source-death".into();
    assert!(worker
        .structured_view_with_admission(&request, |_send| false)
        .is_err());
    assert!(worker.poll_alive());
    request.query.clear();
    assert!(worker.structured_view(&request).is_ok());
    worker.shutdown();
}

#[test]
fn query_refresh_treats_old_selection_input_and_scroll_as_intent_not_new_authority() {
    require_host_interpreter!();
    use crikey_core::result_presentation::ScopeId;
    use crikey_core::structured_arguments::*;
    use crikey_core::structured_view::*;
    use crikey_core::ActionId;
    use std::num::NonZeroU64;
    let scratch = Scratch::new("view-refresh-retired-selection");
    let src = view_admission_source(&scratch);
    let site = scratch.subdir("site");
    let mut worker = spawn_worker("admission", "admission:Impl", &src, &site);
    let old = worker
        .structured_view(&view_admission_request("default"))
        .unwrap();
    let old_item = &old.descriptor.definitions[0].item;
    let target = OwnedItem {
        owner: old_item.plugin_id.clone(),
        item: old_item.stable_id.clone(),
    };
    let executable = ExecutableKey {
        target: target.clone(),
        action: Some(ActionId("typed".into())),
    };
    let old_schema = old.descriptor.schema_for(&executable).unwrap();
    let mut request = view_admission_request("default");
    request.mode = ViewRequestMode::Refresh;
    request.previous_revision = Some(old.context.revision);
    request.reason = ViewRefreshReason::Query;
    request.query = "remove-selected".into();
    request.state = Some(HostViewState {
        selected: Some(target.clone()),
        scope: Some(ScopeId("scope".into())),
        input: Some(executable),
    });
    request.scroll = Some(ScrollAnchor {
        item: target.clone(),
        row_offset: 7,
    });
    let refreshed = worker.structured_view(&request).unwrap();
    let admission = RouteAdmission::worker(
        RouteIdentity {
            owner: target.owner.clone(),
            route: request.route.clone(),
            incarnation: NonZeroU64::new(worker.incarnation()).unwrap(),
        },
        refreshed.context.surface(),
    )
    .unwrap();
    refreshed
        .validate(&request, refreshed.context.revision, &admission)
        .unwrap();
    assert!(refreshed.descriptor.definition(&target).is_none());
    assert!(matches!(
        worker
            .execute(old_item, Some("ordinary"), None, Some(&old.context))
            .unwrap(),
        crikey_python_host::ExecuteOutcome::Failed(_)
    ));
    let values = ArgumentValues(BTreeMap::from([(
        FieldId("name".into()),
        StructuredValue::NonSecret(NonSecretValue::Text("typed".into())),
    )]));
    assert!(matches!(
        worker
            .typed_execute(old_item, Some("typed"), old_schema, values, Some(&old.context))
            .unwrap(),
        TypedExecutionOutcome::Submission(SubmissionOutcome::Failed(SubmissionFailureCode::Unavailable))
    ));
    // P3 can retain the same bounded selection/input/anchor intent across a
    // second query even though the intermediate descriptor removed that item.
    let previous_revision = refreshed.context.revision;
    request.previous_revision = Some(previous_revision);
    let refreshed = worker.structured_view(&request).unwrap();
    assert_ne!(refreshed.context.revision, previous_revision);
    assert!(refreshed.descriptor.definition(&target).is_none());
    let item = &refreshed.descriptor.definitions[0].item;
    assert_eq!(item.stable_id.0, "replacement");
    assert!(matches!(
        worker
            .execute(item, Some("ordinary"), None, Some(&refreshed.context))
            .unwrap(),
        crikey_python_host::ExecuteOutcome::Navigate(_)
    ));

    // A removed scope is explicit descriptor state, not a transport rewrite to All.
    request.previous_revision = Some(refreshed.context.revision);
    request.query = "remove-scope".into();
    let selected = OwnedItem {
        owner: item.plugin_id.clone(),
        item: item.stable_id.clone(),
    };
    request.state = Some(HostViewState {
        selected: Some(selected.clone()),
        scope: Some(ScopeId("scope".into())),
        input: None,
    });
    request.scroll = Some(ScrollAnchor {
        item: selected,
        row_offset: 3,
    });
    let no_scope = worker.structured_view(&request).unwrap();
    let admission = RouteAdmission::worker(
        RouteIdentity {
            owner: target.owner,
            route: request.route.clone(),
            incarnation: NonZeroU64::new(worker.incarnation()).unwrap(),
        },
        no_scope.context.surface(),
    )
    .unwrap();
    no_scope
        .validate(&request, no_scope.context.revision, &admission)
        .unwrap();
    let StandardView::List(collection) = &no_scope.descriptor.view else {
        panic!("list response")
    };
    assert!(collection.scopes.is_empty());
    assert!(collection.items.iter().all(|item| item.scopes.is_empty()));
    assert_eq!(
        request.state.as_ref().unwrap().scope,
        Some(ScopeId("scope".into()))
    );
    assert!(matches!(
        worker
            .execute(item, Some("ordinary"), None, Some(&refreshed.context))
            .unwrap(),
        crikey_python_host::ExecuteOutcome::Failed(_)
    ));
    worker.shutdown();
}
