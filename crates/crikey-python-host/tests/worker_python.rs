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
