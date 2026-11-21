//! The modern CPython worker boundary, driven against a *real* interpreter and
//! the real `_crikey_modern_worker.py` shim (spec 15.6, 15.7, 4.2; acceptance
//! 31.10). These tests are written before the implementation: they pin the
//! contract of [`ModernWorker`] — how a plugin's suggestions, catalog and
//! executions cross the process boundary, and what happens when the plugin on
//! the far side raises, spins, crashes, or floods the channel.
//!
//! # Why a real subprocess
//!
//! Every contract below is only true of a real child. An in-process double
//! could not have a distinct pid, could not `os._exit` mid-call without taking
//! the host with it, and could not be observed to survive an interpreter crash
//! — which is precisely spec 4.2 ("Python shall not run in the UI process") and
//! acceptance 31.10 ("an interpreter crash shall not terminate CriKey"). So
//! these tests prefer the repository virtualenv and otherwise use ordinary
//! interpreter discovery, while the failure tests provoke real failures. If no
//! usable interpreter exists, each real-Python test prints a reason and skips;
//! there is no silent fallback to an in-process double.
//!
//! # Fixtures
//!
//! Each test writes the smallest modern plugin that can express its contract
//! into its own temp directory (a `plugin.py` exposing a `Plugin` subclass),
//! removed when the test ends. The real SDK reaches the child through
//! [`sdk_root`] on the import path, so a fixture that says
//! `from crikey_sdk.plugin import Plugin` proves the import path was assembled
//! with no extra assertion.
//!
//! # Time
//!
//! The peer is a real OS process, so a deadline against it cannot be virtual.
//! Every bound is nevertheless *explicit* (passed through [`WorkerOptions`]) and
//! no test sleeps as a synchronisation primitive: cross-thread ordering is by
//! bounded polling of an observable the plugin creates, against a deadline, the
//! way the legacy worker does. The liveness ceilings turn a regression that
//! would hang the run into a named failure rather than a stall.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crikey_core::{
    Action, ActionId, ArgumentPolicy, Category, ExecutionPolicy, HitPolicy, Item, ItemId, PluginId,
};
use crikey_package_manager::ImportPath;
use crikey_plugin_model::ConcurrencySection;
use crikey_plugin_supervisor::{shared_budget_from_section, BudgetKind};
#[cfg(unix)]
use crikey_python_host::discover_interpreter_in;
use crikey_python_host::{
    discover_interpreter, sdk_root, BatchState, CancelHandle, DiscoveryEnvironment, ExecuteOutcome,
    HostError, Interpreter, ModernWorker, PluginError, RequiresPython, RuntimeProfile, SuggestRequest,
    Suggestions, WorkerExit, WorkerOptions, MAX_FRAME_BYTES, MAX_LOG_LINE_BYTES, WORKER_ENTRY_FILE,
};

// ---------------------------------------------------------------------------
// Bounds
//
// None of these is a performance assertion. Each exists so that a broken
// implementation fails with a message instead of hanging the suite.
// ---------------------------------------------------------------------------

/// Bound on the startup handshake.
const STARTUP_BUDGET_MS: u64 = 30_000;

/// Per-call bound for workers expected to answer promptly.
const CALL_BUDGET_MS: u64 = 30_000;

/// Bound on an orderly shutdown.
const SHUTDOWN_BUDGET_MS: u64 = 5_000;

/// Ceiling on any rendezvous with a correctly behaving child.
const RESPONSE_LIMIT: Duration = Duration::from_secs(60);

/// Interval between polls while waiting on a real-process observable.
const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Minimum CPython this host must provide for the modern worker.
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
            "crikey-modern-worker-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // A crashed earlier run must never leak fixtures into this one.
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");

        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

#[cfg(unix)]
fn generation_mismatch_shim(scratch: &Scratch) -> PathBuf {
    // Written to a sibling temporary name and renamed into place. Writing the
    // final path directly is racy: these tests run in parallel threads in one
    // process, so when any thread spawns a child the fork inherits every open
    // descriptor, including another thread's write handle to a shim it has just
    // created. The kernel then refuses to execute that shim with `ETXTBSY`
    // ("Text file busy") because a writer still holds it open. A rename
    // publishes the finished file under a name no writer ever held.
    let path = scratch.join("generation-mismatch-python");
    let staging = path.with_extension("staging");
    fs::write(
        &staging,
        r#"#!/bin/sh
if [ "$1" = "-c" ]; then
    printf '3.12.0\n'
    exit 0
fi
while IFS= read -r line; do
    case "$line" in
        *'"kind":"handshake"'*) printf '%s\n' '{"id":1,"kind":"handshake_ack","protocol_version":1}' ;;
        *'"kind":"suggest"'*) printf '%s\n' '{"id":2,"kind":"result_batch","generation":999,"state":"final","items":[],"log":[],"error":null}' ;;
        *'"kind":"shutdown"'*) exit 0 ;;
    esac
done
"#,
    )
    .expect("generation mismatch shim is writable");
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))
        .expect("generation mismatch shim is executable");
    fs::rename(&staging, &path).expect("generation mismatch shim is published atomically");
    path
}

#[cfg(unix)]
fn invalid_utf8_shim(scratch: &Scratch) -> PathBuf {
    let path = scratch.join("invalid-utf8-python");
    let staging = path.with_extension("staging");
    fs::write(
        &staging,
        r#"#!/bin/sh
if [ "$1" = "-c" ]; then
    printf '3.12.0\n'
    exit 0
fi
while IFS= read -r line; do
    case "$line" in
        *'"kind":"handshake"'*) printf '%s\n' '{"id":1,"kind":"handshake_ack","protocol_version":1}' ;;
        *'"kind":"suggest"'*)
            printf '%s' '{"id":2,"kind":"result_batch","generation":1,"state":"final","items":[],"log":["'
            printf '\377'
            printf '%s\n' '"],"error":null}'
            ;;
        *'"kind":"shutdown"'*) exit 0 ;;
    esac
done
"#,
    )
    .expect("invalid UTF-8 shim is writable");
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))
        .expect("invalid UTF-8 shim is executable");
    fs::rename(&staging, &path).expect("invalid UTF-8 shim is published atomically");
    path
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Fixtures and options
// ---------------------------------------------------------------------------

/// Writes `<scratch>/<name>/plugin.py` and returns the directory to import from.
///
/// The module is named `plugin` and the class `Fixture`, so every fixture's
/// entrypoint is the same `"plugin:Fixture"`; what differs is the body.
fn write_plugin(scratch: &Scratch, name: &str, source: &str) -> PathBuf {
    let dir = scratch.join(name);
    fs::create_dir_all(&dir).expect("plugin directory is creatable");
    fs::write(dir.join("plugin.py"), source).expect("fixture plugin is writable");
    dir
}

/// The import path handed to a worker: the plugin's own source first, then the
/// real SDK so `import crikey_sdk` resolves. System site-packages is never on
/// it — the worker runs under `-S`, and this path is all it gets.
fn import_path(plugin_dir: &Path) -> ImportPath {
    ImportPath {
        entries: vec![plugin_dir.to_path_buf(), sdk_root()],
    }
}

fn options(plugin: &str, plugin_dir: &Path) -> WorkerOptions {
    WorkerOptions::new(
        PluginId(plugin.to_owned()),
        "plugin:Fixture",
        import_path(plugin_dir),
    )
    .with_startup_timeout_ms(STARTUP_BUDGET_MS)
    .with_call_timeout_ms(CALL_BUDGET_MS)
    .with_shutdown_timeout_ms(SHUTDOWN_BUDGET_MS)
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
            crikey_python_host::discover_interpreter_in(
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

/// Spawns a worker for `plugin` whose source is `source`, borrowing `scratch`
/// for the fixture tree.
fn spawn(scratch: &Scratch, plugin: &str, source: &str) -> ModernWorker {
    let dir = write_plugin(scratch, plugin, source);
    let interpreter = host_interpreter().expect("host interpreter was checked at test entry");
    ModernWorker::spawn(&interpreter, options(plugin, &dir))
        .expect("a loadable modern plugin spawns a worker")
}

fn suggest_request(text: &str) -> SuggestRequest {
    SuggestRequest {
        generation: 1,
        text: text.to_owned(),
        normalized: text.to_lowercase(),
        selected_item_id: None,
    }
}

/// A core item to hand back to a plugin's `execute`.
fn core_item(plugin: &str, stable: &str) -> Item {
    Item {
        stable_id: ItemId(stable.to_owned()),
        plugin_id: PluginId(plugin.to_owned()),
        category: Category::PluginDefined("plugin-defined".to_owned()),
        label: "run".to_owned(),
        description: String::new(),
        target: "target".to_owned(),
        search_terms: Vec::new(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::default(),
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: vec![Action {
            action_id: ActionId("open".to_owned()),
            label: "Open".to_owned(),
            description: "Open from the host".to_owned(),
            applicable_categories: vec![
                Category::PluginDefined("application".to_owned()),
                Category::Application,
            ],
            icon_reference: Some("icon-open".to_owned()),
            execution_policy: ExecutionPolicy::Plugin,
        }],
    }
}

/// Polls `cond` against a deadline. Returns whether it became true in time.
///
/// This is the "bounded polling against a deadline" the contract sanctions for
/// real-process liveness: it synchronises on an observable the child creates,
/// never on an elapsed duration, so it is as fast as the machine and cannot
/// race, yet a plugin that never reaches the observable fails with a message.
fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Whether the operating system still holds a table entry for `pid`.
///
/// `None` means this platform cannot be asked without a new dependency. On
/// Linux a child that exited but was never waited on remains a visible zombie,
/// so this distinguishes *reaped* from merely *dead* — the difference between a
/// clean teardown and an orphan.
fn process_table_contains(pid: u32) -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        Some(Path::new(&format!("/proc/{pid}")).exists())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

// ---------------------------------------------------------------------------
// Fixture plugin sources
// ---------------------------------------------------------------------------

/// Emits an ordered run of suggestions, returns a two-item catalog, and either
/// succeeds or raises in `execute` depending on the action id.
const WELL_BEHAVED: &str = r#"
from crikey_sdk.plugin import Plugin, Item, Action, plugin_defined_category


class Fixture(Plugin):
    def build_catalog(self):
        return [
            Item(stable_id="cat-1", label="Catalog One", target="t1",
                 actions=[Action(
                     action_id="open", label="Open",
                     icon_reference="icon-open",
                     applicable_categories=(
                         plugin_defined_category("application"), "application"
                     ),
                     execution_policy="plugin",
                 )]),
            Item(stable_id="cat-2", label="Catalog Two", target="t2",
                 description="second"),
        ]

    def suggest(self, query, context):
        for index in range(20):
            context.emit(Item(stable_id="s-%02d" % index,
                              label="Item %02d" % index,
                              target="target-%02d" % index))

    def execute(self, item, action_id, argument):
        if action_id == "open":
            action = item.actions[0]
            if (
                action.applicable_categories
                != ("plugin-defined:application", "application")
                or action.execution_policy != "plugin"
            ):
                raise RuntimeError("action fields were rewritten before execute")
        if action_id == "raise":
            raise RuntimeError("execute boom for %s" % item.stable_id)
"#;

/// Raises inside `suggest`; its catalog callback does not, and is what proves
/// the worker outlived the raise.
const RAISES_IN_SUGGEST: &str = r#"
from crikey_sdk.plugin import Plugin, Item


class Fixture(Plugin):
    def build_catalog(self):
        return [Item(stable_id="alive", label="alive", target="alive")]

    def suggest(self, query, context):
        raise ValueError("suggest kaboom")
"#;

/// Kills its own interpreter mid-`suggest` after announcing on stderr, so the
/// host observes both the death and a non-empty diagnostic tail.
const CRASHES_IN_SUGGEST: &str = r#"
import os
import sys
from crikey_sdk.plugin import Plugin


class Fixture(Plugin):
    def suggest(self, query, context):
        sys.stderr.write("modern worker deliberately aborting\n")
        sys.stderr.flush()
        os._exit(1)
"#;

/// On its FIRST suggest, announces it is running by creating the file named in
/// the query text, then spins until the request is cancelled and returns
/// cooperatively (a Cancelled batch). On EVERY LATER suggest it emits one item
/// and returns normally (a Final batch) — so a worker reused after a cancel is
/// observably not stuck Cancelled, and its own code runs the follow-up.
const CANCEL_THEN_FINISH: &str = r#"
from crikey_sdk.plugin import Plugin, Item


class Fixture(Plugin):
    def __init__(self):
        self._served = 0

    def suggest(self, query, context):
        self._served += 1
        if self._served == 1:
            with open(query.text, "w") as handle:
                handle.write("running")
            while not context.cancelled:
                pass
            return
        context.emit(Item(stable_id="after-cancel", label="after", target="after"))
"#;
/// Registers three independent coroutines from one synchronous suggestion.
/// The host budget admits one and refuses two; the admitted task sleeps long
/// enough that a second suggestion proves it does not hold the foreground call.
const BACKGROUND_TASKS: &str = r#"
import asyncio

from crikey_sdk.plugin import Item, Plugin


class Fixture(Plugin):
    def suggest(self, query, context):
        async def background(index):
            with open(query.text + ".start-" + str(index), "w") as handle:
                handle.write("started")
            await asyncio.sleep(30)
            with open(query.text + ".done-" + str(index), "w") as handle:
                handle.write("done")

        for index in range(3):
            context.spawn(background(index))
        context.emit(Item(stable_id="foreground", label="foreground", target="foreground"))
"#;

/// Registers exactly one coroutine whose only effect is a file the test can
/// see. Whether that file ever appears is the whole observation: a coroutine
/// the host refused is closed unrun, so the marker's absence is proof the
/// plugin got no background execution rather than proof it was merely slow.
const BACKGROUND_MARKER: &str = r#"
from crikey_sdk.plugin import Item, Plugin


class Fixture(Plugin):
    def suggest(self, query, context):
        marker = query.text

        async def background():
            with open(marker, "w") as handle:
                handle.write("ran")

        context.spawn(background())
        context.emit(Item(stable_id="foreground", label="foreground", target="foreground"))
"#;

/// Streams partial suggestion batches without end and never returns a terminal
/// frame. A correct host bounds the whole call (one aggregate deadline plus a
/// total-item cap); a host that gives each frame a fresh timeout would loop
/// here forever.
const STREAMS_FOREVER: &str = r#"
from crikey_sdk.plugin import Plugin, Item


class Fixture(Plugin):
    def suggest(self, query, context):
        index = 0
        while True:
            context.emit(Item(stable_id=str(index), label="x", target="t"))
            index += 1
"#;

/// Raises inside `build_catalog`; its `suggest` succeeds, proving the worker
/// outlives a catalog fault (a catalog raise is the plugin's, not the
/// transport's).
const RAISES_IN_CATALOG: &str = r#"
from crikey_sdk.plugin import Plugin, Item


class Fixture(Plugin):
    def build_catalog(self):
        raise RuntimeError("catalog boom")

    def suggest(self, query, context):
        context.emit(Item(stable_id="ok", label="ok", target="ok"))
"#;

/// Deliberately bypasses the SDK producer and writes an oversized raw JSON
/// protocol line. This tests the host's hostile-peer framing boundary.
const OVERSIZED_FRAME: &str = r#"
import json
import os
from crikey_sdk.plugin import Plugin


class Fixture(Plugin):
    def suggest(self, query, context):
        frame = {"id": 2, "kind": "result_batch", "generation": query.generation,
                 "state": "final", "log": [],
                 "items": [{"stable_id": "big", "label": "X" * __N__, "target": "t"}]}
        raw = memoryview((json.dumps(frame, separators=(",", ":")) + "\n").encode("utf-8"))
        while raw:
            raw = raw[os.write(1, raw):]
"#;

/// Logs a single line far longer than one retained log line may be, then emits
/// one small item, so the reply frame stays valid while the log must be clamped.
const OVERSIZED_LOG: &str = r#"
from crikey_sdk.plugin import Plugin, Item


class Fixture(Plugin):
    def suggest(self, query, context):
        context.log("L" * __N__)
        context.emit(Item(stable_id="ok", label="ok", target="ok"))
"#;

/// Reports, as an item's target, the pid the interpreter itself runs under.
const PID_WITNESS: &str = r#"
import os
from crikey_sdk.plugin import Plugin, Item


class Fixture(Plugin):
    def suggest(self, query, context):
        context.emit(Item(stable_id="pid", label="pid", target=str(os.getpid())))
"#;

// ---------------------------------------------------------------------------
// The SDK ships with the worker entry the host launches.
// ---------------------------------------------------------------------------

#[test]
fn the_sdk_root_ships_the_modern_worker_entry() {
    let entry = sdk_root().join(WORKER_ENTRY_FILE);
    assert!(
        entry.is_file(),
        "the modern worker entry {WORKER_ENTRY_FILE} must ship in {}",
        sdk_root().display()
    );
}

#[cfg(unix)]
#[test]
fn a_present_mismatched_result_generation_is_rejected_as_protocol_error() {
    let scratch = Scratch::new("generation-mismatch");
    let shim = generation_mismatch_shim(&scratch);
    let interpreter = discover_interpreter_in(
        &RuntimeProfile::External(shim),
        &RequiresPython(">=3.8".to_owned()),
        &DiscoveryEnvironment::empty(),
    )
    .expect("the protocol shim reports a supported interpreter");
    let options = WorkerOptions::new(
        PluginId("modern.generation-mismatch".to_owned()),
        "unused:Fixture",
        ImportPath {
            entries: vec![sdk_root()],
        },
    )
    .with_startup_timeout_ms(1_000)
    .with_call_timeout_ms(1_000)
    .with_shutdown_timeout_ms(1_000);
    let mut worker = ModernWorker::spawn(&interpreter, options).expect("the protocol shim handshakes");

    let error = worker
        .suggest(&suggest_request("query"))
        .expect_err("a result for another generation is a protocol error");
    assert!(
        matches!(error, HostError::Protocol(_)),
        "a mismatched generation is rejected as HostError::Protocol, got {error:?}"
    );
    assert!(!worker.is_alive(), "a mismatched generation stops the worker");
    worker.shutdown();
}

#[cfg(unix)]
#[test]
fn invalid_utf8_on_the_protocol_channel_is_rejected() {
    let scratch = Scratch::new("invalid-utf8");
    let shim = invalid_utf8_shim(&scratch);
    let interpreter = discover_interpreter_in(
        &RuntimeProfile::External(shim),
        &RequiresPython(">=3.8".to_owned()),
        &DiscoveryEnvironment::empty(),
    )
    .expect("the protocol shim reports a supported interpreter");
    let options = WorkerOptions::new(
        PluginId("modern.invalid-utf8".to_owned()),
        "unused:Fixture",
        ImportPath {
            entries: vec![sdk_root()],
        },
    )
    .with_startup_timeout_ms(1_000)
    .with_call_timeout_ms(1_000)
    .with_shutdown_timeout_ms(1_000);
    let mut worker = ModernWorker::spawn(&interpreter, options).expect("the protocol shim handshakes");

    let error = worker
        .suggest(&suggest_request("query"))
        .expect_err("invalid UTF-8 is a protocol failure");
    assert!(
        matches!(error, HostError::Protocol(_)),
        "invalid UTF-8 is rejected as HostError::Protocol, got {error:?}"
    );
    assert!(!worker.is_alive(), "invalid protocol bytes stop the worker");
    worker.shutdown();
}

// ---------------------------------------------------------------------------
// Handshake and the process boundary (spec 4.2, 15.6)
// ---------------------------------------------------------------------------

#[test]
fn a_spawned_worker_completes_its_handshake_and_reports_itself_alive() {
    require_host_interpreter!();
    let scratch = Scratch::new("handshake");
    let worker = spawn(&scratch, "modern.handshake", WELL_BEHAVED);

    assert!(
        worker.is_alive(),
        "spawn returns only after the startup handshake, so the worker is alive"
    );

    let exit = worker.shutdown();
    assert!(
        !exit.hard_stopped,
        "a worker that only handshook shuts down cooperatively"
    );
}

#[test]
fn suggest_collects_the_plugins_streamed_items_in_order() {
    require_host_interpreter!();
    let scratch = Scratch::new("suggest");
    let mut worker = spawn(&scratch, "modern.suggest", WELL_BEHAVED);

    let suggestions: Suggestions = worker
        .suggest(&suggest_request("anything"))
        .expect("a healthy worker answers a suggest");

    assert_eq!(
        suggestions.state,
        BatchState::Final,
        "a plugin that returns normally produces a terminal Final batch"
    );
    assert!(
        suggestions.error.is_none(),
        "a Final batch carries no error, got {:?}",
        suggestions.error
    );

    let ids: Vec<&str> = suggestions
        .items
        .iter()
        .map(|item| item.stable_id.0.as_str())
        .collect();
    let expected: Vec<String> = (0..20).map(|index| format!("s-{index:02}")).collect();
    assert_eq!(
        ids,
        expected.iter().map(String::as_str).collect::<Vec<_>>(),
        "every emitted item is collected across partial batches, in emission order"
    );

    assert_eq!(
        suggestions.items[0].plugin_id,
        PluginId("modern.suggest".to_owned()),
        "an item is attributed to the plugin whose worker produced it"
    );
    assert_eq!(
        suggestions.items[0].label, "Item 00",
        "the item's own fields survive the boundary"
    );

    worker.shutdown();
}

#[test]
fn catalog_timeout_override_preserves_the_query_default() {
    require_host_interpreter!();
    let scratch = Scratch::new("catalog-timeout-override");
    let dir = write_plugin(&scratch, "catalog", WELL_BEHAVED);
    // A zero default expires deterministically, even if a reply is queued.
    // Catalog gets its own budget, without changing the next query's bound.
    let opts = options("modern.catalog.override", &dir).with_call_timeout_ms(0);
    let mut worker = ModernWorker::spawn(
        &host_interpreter().expect("host interpreter was checked at test entry"),
        opts,
    )
    .expect("a loadable plugin spawns a worker");

    let items = worker
        .build_catalog_with_timeout_ms(CALL_BUDGET_MS)
        .expect("catalog uses the explicit budget, not the zero query default");
    assert_eq!(items.len(), 2);
    assert!(worker.is_alive());

    let error = worker
        .suggest(&suggest_request("anything"))
        .expect_err("catalog must not extend the configured query deadline");
    assert!(matches!(error, HostError::Timeout), "{error:?}");
    assert!(!worker.is_alive(), "a timed-out query reaps the worker");
    worker.shutdown();
}

#[test]
fn catalog_honors_explicit_and_configured_zero_deadlines() {
    require_host_interpreter!();
    let scratch = Scratch::new("catalog-zero-deadlines");
    let dir = write_plugin(&scratch, "catalog", WELL_BEHAVED);

    for explicit in [false, true] {
        let opts = options("modern.catalog.zero", &dir).with_call_timeout_ms(if explicit {
            CALL_BUDGET_MS
        } else {
            0
        });
        let mut worker = ModernWorker::spawn(
            &host_interpreter().expect("host interpreter was checked at test entry"),
            opts,
        )
        .expect("a loadable plugin spawns a worker");

        let result = if explicit {
            worker.build_catalog_with_timeout_ms(0)
        } else {
            worker.build_catalog()
        };
        let error = result.expect_err("a zero catalog deadline expires deterministically");
        assert!(matches!(error, HostError::Timeout), "{error:?}");
        assert!(!worker.is_alive(), "a timed-out catalog reaps the worker");
        worker.shutdown();
    }
}

#[test]
fn build_catalog_returns_the_plugins_catalog_with_fields_intact() {
    require_host_interpreter!();
    let scratch = Scratch::new("catalog");
    let mut worker = spawn(&scratch, "modern.catalog", WELL_BEHAVED);

    let items: Vec<Item> = worker
        .build_catalog()
        .expect("a healthy worker builds its catalog");

    assert_eq!(items.len(), 2, "the plugin returned two catalog items");
    assert_eq!(
        items[0].stable_id,
        ItemId("cat-1".to_owned()),
        "the plugin's own stable_id is kept (spec 10.2)"
    );
    assert_eq!(items[0].label, "Catalog One");
    assert_eq!(
        items[0].actions.first().map(|a| &a.action_id),
        Some(&ActionId("open".to_owned())),
        "an item's actions cross the boundary"
    );
    let action = items[0].actions.first().expect("catalog action exists");
    assert_eq!(action.icon_reference.as_deref(), Some("icon-open"));
    assert_eq!(
        action.applicable_categories,
        vec![
            Category::PluginDefined("application".to_owned()),
            Category::Application,
        ],
        "non-empty action categories survive the real Python worker"
    );
    assert_eq!(
        action.execution_policy,
        ExecutionPolicy::Plugin,
        "the Python worker does not replace an explicit execution policy"
    );
    assert_eq!(
        items[1].stable_id,
        ItemId("cat-2".to_owned()),
        "catalog order is preserved"
    );
    assert_eq!(items[1].description, "second");

    worker.shutdown();
}

// ---------------------------------------------------------------------------
// Execute (spec 9.7)
// ---------------------------------------------------------------------------

#[test]
fn execute_is_ok_for_a_normal_action_and_a_plugin_raise_leaves_the_worker_usable() {
    require_host_interpreter!();
    let scratch = Scratch::new("execute");
    let mut worker = spawn(&scratch, "modern.execute", WELL_BEHAVED);
    let item = core_item("modern.execute", "s-00");

    let ok = worker
        .execute(&item, Some("open"), None)
        .expect("execute reaches the plugin");
    assert!(
        matches!(ok, ExecuteOutcome::Ok),
        "a normal action completes as Ok, got {ok:?}"
    );

    let failed = worker
        .execute(&item, Some("raise"), None)
        .expect("a plugin that raises in execute is still the Ok path for the host");
    match failed {
        ExecuteOutcome::Failed(PluginError { message, traceback }) => {
            assert!(
                message.contains("execute boom"),
                "the failure carries the plugin's message, got {message:?}"
            );
            assert!(
                traceback.contains("RuntimeError"),
                "the failure carries the plugin's traceback, got {traceback:?}"
            );
        }
        other => panic!("a plugin raising in execute is ExecuteOutcome::Failed, got {other:?}"),
    }

    // A plugin bug is not a transport bug: the worker stays alive and serves on.
    let after = worker
        .suggest(&suggest_request("after"))
        .expect("the worker still answers after a plugin raised in execute");
    assert_eq!(
        after.state,
        BatchState::Final,
        "the worker resumes normal service after a plugin's execute raised"
    );
    assert_eq!(after.items.len(), 20);

    worker.shutdown();
}

// ---------------------------------------------------------------------------
// Plugin faults on the Ok path (spec 15.7): a raise is not a HostError
// ---------------------------------------------------------------------------

#[test]
fn a_plugin_that_raises_in_suggest_fails_the_batch_and_the_worker_stays_alive() {
    require_host_interpreter!();
    let scratch = Scratch::new("suggest-raise");
    let mut worker = spawn(&scratch, "modern.raise", RAISES_IN_SUGGEST);

    let suggestions = worker
        .suggest(&suggest_request("boom"))
        .expect("a plugin raising in suggest is the Ok path: transport is healthy");

    assert_eq!(
        suggestions.state,
        BatchState::Failed,
        "a raised suggest terminates the batch as Failed, not Final or Cancelled"
    );

    // The failure is carried structurally on `error`, symmetric with
    // ExecuteOutcome::Failed — not smuggled into the plugin's print log.
    match &suggestions.error {
        Some(PluginError { message, traceback }) => {
            assert!(
                message.contains("kaboom"),
                "the failed batch's error carries the plugin's message, got {message:?}"
            );
            assert!(
                traceback.contains("ValueError"),
                "the failed batch's error carries the plugin's traceback, got {traceback:?}"
            );
        }
        None => panic!("a Failed batch carries Some(PluginError), got None"),
    }

    // The worker is healthy; the next call succeeds and proves it.
    let catalog = worker
        .build_catalog()
        .expect("the worker stays usable after a plugin raised in suggest");
    assert_eq!(
        catalog.first().map(|item| item.stable_id.0.as_str()),
        Some("alive"),
        "a plugin bug does not take the worker down"
    );

    worker.shutdown();
}

// ---------------------------------------------------------------------------
// Crash containment (spec 24.1, acceptance 31.10)
// ---------------------------------------------------------------------------

#[test]
fn an_interpreter_that_exits_mid_call_is_a_contained_crash_and_a_fresh_worker_still_works() {
    require_host_interpreter!();
    let scratch = Scratch::new("crash");
    let mut worker = spawn(&scratch, "modern.crash", CRASHES_IN_SUGGEST);

    // Returning at all is half the contract: the host must neither panic nor
    // wait forever on a pipe whose writer will never write again.
    let error = worker
        .suggest(&suggest_request("die"))
        .expect_err("an interpreter that exits mid-call is a HostError, not an Ok batch");

    match &error {
        HostError::Crashed { plugin, detail } => {
            assert_eq!(
                plugin,
                &PluginId("modern.crash".to_owned()),
                "the crash is attributed to the plugin whose worker died (acceptance 31.10)"
            );
            assert!(!detail.is_empty(), "a crash carries a non-empty diagnostic tail");
            assert!(
                detail.contains("aborting"),
                "the crash detail is the child's stderr tail, got {detail:?}"
            );
        }
        other => panic!("an interpreter that exits mid-call is HostError::Crashed, got {other:?}"),
    }
    assert!(
        error.to_string().contains("modern.crash"),
        "the crash message names the plugin concerned, got {error}"
    );
    assert!(!worker.is_alive(), "a crashed worker is not reported as alive");

    // The whole point of the process boundary: the host is still standing and
    // can spawn a fresh worker that serves normally (acceptance 31.10).
    let mut fresh = spawn(&scratch, "modern.crash.recovered", WELL_BEHAVED);
    let recovered = fresh
        .suggest(&suggest_request("again"))
        .expect("the host spawns a fresh worker after a crash");
    assert_eq!(
        recovered.state,
        BatchState::Final,
        "a fresh worker serves normally after another worker crashed"
    );
    assert_eq!(recovered.items.len(), 20);

    fresh.shutdown();
    worker.shutdown();
}

// ---------------------------------------------------------------------------
// Cooperative cancellation (spec 9.4, 15.7)
// ---------------------------------------------------------------------------

#[test]
fn a_cancel_handle_raised_from_another_thread_makes_a_cooperative_plugin_return_cancelled() {
    require_host_interpreter!();
    let scratch = Scratch::new("cancel");
    let marker = scratch.join("suggest-running");
    let mut worker = spawn(&scratch, "modern.cancel", CANCEL_THEN_FINISH);

    // The handle must cross to another thread: the thread that raises the flag
    // can never be the thread blocked inside `suggest`.
    let handle: CancelHandle = worker.cancel_handle();
    let request = suggest_request(&marker.to_string_lossy());

    // The thread hands the worker back so the follow-up call runs on the SAME
    // process — the whole point of proving it survived.
    let call = thread::spawn(move || {
        let outcome = worker.suggest(&request);
        (worker, outcome)
    });

    // Rendezvous by bounded polling: the plugin creates `marker` the instant its
    // callback is genuinely in flight, so cancelling only after it appears
    // proves the flag was raised *during* a live call, deterministically and
    // without sleeping-then-assuming.
    assert!(
        wait_until(RESPONSE_LIMIT, || marker.exists()),
        "the plugin's suggest callback must announce that it is running"
    );
    handle.cancel();

    let (mut worker, outcome) = call.join().expect("the suggest thread does not panic");
    let suggestions = outcome.expect("a cancelled suggest is the Ok path: transport is healthy");
    assert_eq!(
        suggestions.state,
        BatchState::Cancelled,
        "a cooperative plugin that sees context.cancelled returns a Cancelled batch (spec 15.7)"
    );

    // The worker SURVIVED the cancellation. Cancel is cooperative, never a hard
    // kill relabelled as Cancelled (kills that mutation).
    assert!(
        worker.is_alive(),
        "a cooperatively cancelled worker stays alive and reusable, not killed"
    );

    // And it is not permanently stuck Cancelled: the host lowers the cancel flag
    // at the start of the next suggest, so a reused worker answers the follow-up
    // normally (kills the 'flag never lowered / reset() is dead' mutation).
    let followup = worker
        .suggest(&suggest_request("after"))
        .expect("a worker reused after a cancel still answers");
    assert_eq!(
        followup.state,
        BatchState::Final,
        "the suggest after a cancellation returns Final, not a stuck Cancelled"
    );
    assert_eq!(
        followup.items.first().map(|item| item.stable_id.0.as_str()),
        Some("after-cancel"),
        "the reused worker ran ITS OWN code on the follow-up"
    );

    worker.shutdown();
}
// ---------------------------------------------------------------------------
// Host-managed background dispatch (spec 13.5, 15.8)
// ---------------------------------------------------------------------------

#[test]
fn background_registration_is_budgeted_and_cancellation_releases_the_guard() {
    require_host_interpreter!();
    let scratch = Scratch::new("background");
    let dir = write_plugin(&scratch, "background", BACKGROUND_TASKS);
    let section = ConcurrencySection {
        max_background_tasks: Some(1),
        ..ConcurrencySection::default()
    };
    let budget = shared_budget_from_section(&section);
    let mut worker = ModernWorker::spawn(
        &host_interpreter().expect("host interpreter was checked at test entry"),
        options("modern.background", &dir).with_shared_budget(budget.clone()),
    )
    .expect("a loadable plugin spawns with a shared background budget");

    let first = worker
        .suggest(&suggest_request(&scratch.join("work").to_string_lossy()))
        .expect("foreground suggestion is not held by a slow background task");
    assert_eq!(first.state, BatchState::Final);
    assert_eq!(
        first.items.first().map(|item| item.stable_id.0.as_str()),
        Some("foreground")
    );

    assert!(
        wait_until(RESPONSE_LIMIT, || {
            worker.background_diagnostics().registered >= 3
        }),
        "the host observes every child registration"
    );
    let diagnostics = worker.background_diagnostics();
    assert_eq!(diagnostics.admitted, 1, "one task owns the declared slot");
    assert_eq!(diagnostics.refused, 2, "over-limit tasks are visibly refused");
    assert_eq!(
        budget.in_flight(BudgetKind::Background),
        1,
        "the admitted task holds the shared Arc-owned guard"
    );

    // A second foreground call completes while the admitted task sleeps. This
    // would block for the task duration if callback-end draining were retained.
    let second = worker
        .suggest(&suggest_request("second"))
        .expect("a slow background task does not delay suggestion delivery");
    assert_eq!(second.state, BatchState::Final);

    worker.cancel_background_tasks();
    assert!(
        wait_until(RESPONSE_LIMIT, || {
            budget.in_flight(BudgetKind::Background) == 0
        }),
        "cancellation releases every host guard"
    );
    worker.shutdown();
}

/// The interpreter these tests cannot run without. Unlike the discovery-driven
/// skip above, a permission gate is a security contract: leaving it unproven
/// because no CPython was found would report a clean run for a check that
/// never happened, so a missing interpreter fails by name here.
fn required_host_interpreter() -> Interpreter {
    host_interpreter().unwrap_or_else(|| {
        panic!("the background-permission tests need a real CPython {REQUIRES_PYTHON}; none was found")
    })
}

#[test]
fn a_denied_manifest_has_its_background_registration_refused_before_any_slot_is_taken() {
    // `permissions.background-execution` is enforced by the host or not at
    // all: the child's shim is plugin-adjacent code a hostile plugin can
    // replace, so the registration frame arrives whatever the manifest said
    // and only this side may answer it.
    let scratch = Scratch::new("background-denied");
    let dir = write_plugin(&scratch, "denied", BACKGROUND_MARKER);
    let marker = scratch.join("denied.marker");
    // A default budget with background slots free, so nothing but the missing
    // permission can be responsible for the refusal.
    let budget = shared_budget_from_section(&ConcurrencySection::default());
    let mut worker = ModernWorker::spawn(
        &required_host_interpreter(),
        options("modern.background.denied", &dir)
            .with_shared_budget(budget.clone())
            .with_background_execution(false),
    )
    .expect("a plugin without the permission still spawns; only its background work is refused");

    let first = worker
        .suggest(&suggest_request(&marker.to_string_lossy()))
        .expect("the foreground suggestion answers as usual");
    assert_eq!(first.state, BatchState::Final);
    assert!(
        wait_until(RESPONSE_LIMIT, || {
            let diagnostics = worker.background_diagnostics();
            diagnostics.refused >= 1 || diagnostics.admitted >= 1
        }),
        "the host reaches an admission decision for the child's registration"
    );
    let diagnostics = worker.background_diagnostics();
    assert_eq!(diagnostics.registered, 1, "the child announced one task");
    assert_eq!(
        diagnostics.admitted, 0,
        "a plugin that never asked for background-execution is admitted nothing: {diagnostics:?}"
    );
    assert_eq!(
        diagnostics.refused, 1,
        "the refusal is the host's, and it is counted: {diagnostics:?}"
    );
    assert_eq!(
        budget.in_flight(BudgetKind::Background),
        0,
        "a refusal that precedes admission takes no budget slot"
    );

    // A second complete round trip gives the child's background loop every
    // opportunity to run the coroutine. Only then is the marker's absence
    // evidence that the work never happened rather than that it was slow.
    let second = worker
        .suggest(&suggest_request(&scratch.join("second.marker").to_string_lossy()))
        .expect("the worker keeps serving after a refusal");
    assert_eq!(second.state, BatchState::Final);
    assert!(
        !marker.exists(),
        "the refused coroutine never ran, so it never wrote {}",
        marker.display()
    );

    worker.shutdown();
}

#[test]
fn granting_background_execution_admits_the_registration_the_denial_refused() {
    // The companion of the refusal above: same fixture, same budget, one
    // manifest decision different. Without this, a host that refused every
    // registration would pass the denial test.
    let scratch = Scratch::new("background-granted");
    let dir = write_plugin(&scratch, "granted", BACKGROUND_MARKER);
    let marker = scratch.join("granted.marker");
    let budget = shared_budget_from_section(&ConcurrencySection::default());
    let mut worker = ModernWorker::spawn(
        &required_host_interpreter(),
        options("modern.background.granted", &dir)
            .with_shared_budget(budget.clone())
            .with_background_execution(true),
    )
    .expect("a loadable plugin spawns a worker");

    let first = worker
        .suggest(&suggest_request(&marker.to_string_lossy()))
        .expect("the foreground suggestion answers as usual");
    assert_eq!(first.state, BatchState::Final);
    assert!(
        wait_until(RESPONSE_LIMIT, || marker.exists()),
        "the admitted coroutine runs on the host-managed background loop and writes {}",
        marker.display()
    );
    assert!(
        wait_until(RESPONSE_LIMIT, || {
            worker.background_diagnostics().completed >= 1
        }),
        "the admitted task reports its terminal state to the host"
    );
    let diagnostics = worker.background_diagnostics();
    assert_eq!(
        diagnostics.admitted, 1,
        "the granted task is admitted: {diagnostics:?}"
    );
    assert_eq!(diagnostics.refused, 0, "nothing was refused: {diagnostics:?}");

    worker.shutdown();
}

// ---------------------------------------------------------------------------
// A whole call is bounded, not just each frame (spec 9.6, 15)
// ---------------------------------------------------------------------------

#[test]
fn a_plugin_that_streams_partials_forever_is_bounded_not_hung() {
    require_host_interpreter!();
    let scratch = Scratch::new("streams-forever");
    let dir = write_plugin(&scratch, "forever", STREAMS_FOREVER);
    // A short AGGREGATE budget for the whole call. A host that reset the budget
    // per frame would never time out against a plugin that keeps streaming; the
    // item cap would also bound a fast flood. Either way the call must END.
    let opts = options("modern.forever", &dir).with_call_timeout_ms(2_000);
    let mut worker = ModernWorker::spawn(
        &host_interpreter().expect("host interpreter was checked at test entry"),
        opts,
    )
    .expect("a loadable plugin spawns a worker");
    let (tx, rx) = mpsc::channel();
    let call = thread::spawn(move || {
        let errored = worker.suggest(&suggest_request("go")).is_err();
        let alive = worker.is_alive();
        let _ = tx.send((errored, alive));
        worker
    });

    let (errored, alive) = rx
        .recv_timeout(RESPONSE_LIMIT)
        .expect("suggest against a forever-streaming plugin must be BOUNDED, not hang the host");
    assert!(
        errored,
        "a plugin that never sends a terminal frame ends the call as a bounded error"
    );
    assert!(
        !alive,
        "exceeding the aggregate call budget stops the worker (a fresh per-frame timeout never fires)"
    );

    let _ = call.join();
}

// ---------------------------------------------------------------------------
// A catalog raise is a load-time failure, not an empty catalog (pinned dec. 2)
// ---------------------------------------------------------------------------

#[test]
fn a_build_catalog_that_raises_is_a_plugin_failed_error_and_the_worker_survives() {
    require_host_interpreter!();
    let scratch = Scratch::new("catalog-raise");
    let mut worker = spawn(&scratch, "modern.catalog.raise", RAISES_IN_CATALOG);

    let error = worker
        .build_catalog()
        .expect_err("a build_catalog that raises is a PluginFailed error, not an empty catalog");
    match &error {
        HostError::PluginFailed { plugin, detail } => {
            assert_eq!(
                plugin.0, "modern.catalog.raise",
                "the failure names the plugin whose catalog raised"
            );
            assert!(
                detail.contains("catalog boom"),
                "the plugin's message survives into the failure detail, got {detail:?}"
            );
        }
        other => panic!("a catalog raise maps to HostError::PluginFailed, got {other:?}"),
    }

    // A catalog fault is the plugin's, not the transport's: the worker stays
    // alive (kills 'a catalog raise is indistinguishable from an empty catalog'
    // and 'a catalog raise reaps the worker').
    assert!(
        worker.is_alive(),
        "a catalog raise leaves the worker healthy and reusable"
    );
    let suggestions = worker
        .suggest(&suggest_request("q"))
        .expect("the worker serves after a catalog raise");
    assert_eq!(suggestions.state, BatchState::Final);
    assert_eq!(
        suggestions.items.first().map(|item| item.stable_id.0.as_str()),
        Some("ok"),
        "the reused worker ran its own suggest after the catalog raise"
    );

    worker.shutdown();
}

// ---------------------------------------------------------------------------
// Bounds (spec 15, transport §1)
// ---------------------------------------------------------------------------

#[test]
fn an_oversized_frame_from_the_plugin_is_a_bounded_protocol_error_that_stops_the_worker() {
    require_host_interpreter!();
    let scratch = Scratch::new("oversized-frame");
    let oversize = MAX_FRAME_BYTES + 1024 * 1024;
    let source = OVERSIZED_FRAME.replace("__N__", &oversize.to_string());
    let mut worker = spawn(&scratch, "modern.frame", &source);

    let error = worker
        .suggest(&suggest_request("big"))
        .expect_err("a line longer than MAX_FRAME_BYTES is a protocol failure, not an Ok batch");

    match &error {
        HostError::Protocol(message) => {
            assert!(
                message.len() < MAX_FRAME_BYTES,
                "a protocol error carries a bounded excerpt, not the whole oversized line ({} bytes)",
                message.len()
            );
            assert!(
                message.contains("frame limit"),
                "the raw peer must hit the framing boundary: {message}"
            );
        }
        other => panic!("an over-long line is HostError::Protocol, got {other:?}"),
    }
    assert!(
        !worker.is_alive(),
        "a desynchronised channel stops the worker rather than leaving it wedged"
    );

    worker.shutdown();
}

#[test]
fn oversized_sdk_suggestion_is_a_fixed_failure_and_preserves_its_worker() {
    require_host_interpreter!();
    let scratch = Scratch::new("oversized-sdk-suggestion");
    let source = format!(
        r#"
import os
from crikey_sdk.plugin import Plugin, Item
class Fixture(Plugin):
    def suggest(self, query, context):
        if query.text == "big":
            context.emit(Item(stable_id="big", label="X" * {}, target="t"))
        else:
            context.emit(Item(stable_id="ok", label="Recovered", target="t",
                              metadata={{"pid": str(os.getpid())}}))
"#,
        MAX_FRAME_BYTES + 1024 * 1024
    );
    let mut worker = spawn(&scratch, "modern.sdk-frame", &source);
    let first = worker.suggest(&suggest_request("ready")).unwrap();
    let pid = first.items[0].metadata["pid"].clone();
    let failed = worker
        .suggest(&suggest_request("big"))
        .expect("the SDK emits a bounded terminal");
    assert_eq!(failed.state, BatchState::Failed);
    assert!(failed.error.is_some(), "oversized content is a visible failure");
    assert!(failed.items.is_empty(), "no truncated success may be published");
    assert!(worker.poll_alive());
    let recovered = worker.suggest(&suggest_request("recovered")).unwrap();
    assert_eq!(recovered.state, BatchState::Final);
    assert_eq!(recovered.items[0].metadata["pid"], pid);
    worker.shutdown();
}

#[test]
fn an_oversized_log_line_is_truncated_with_a_marker_while_the_reply_stays_valid() {
    require_host_interpreter!();
    let scratch = Scratch::new("oversized-log");
    let logged = 64 * 1024;
    let source = OVERSIZED_LOG.replace("__N__", &logged.to_string());
    let mut worker = spawn(&scratch, "modern.log", &source);

    let suggestions = worker
        .suggest(&suggest_request("noisy"))
        .expect("a large log does not break the reply frame");

    assert_eq!(
        suggestions.state,
        BatchState::Final,
        "clamping the log leaves the batch itself valid"
    );
    assert_eq!(
        suggestions.items.first().map(|item| item.stable_id.0.as_str()),
        Some("ok"),
        "the item emitted alongside the huge log still arrives"
    );

    let line = suggestions
        .log
        .iter()
        .find(|line| line.starts_with('L'))
        .expect("the plugin's log line is present");
    assert!(
        line.len() < logged,
        "an oversized log line is truncated, not retained whole ({} bytes)",
        line.len()
    );
    assert!(
        line.len() <= MAX_LOG_LINE_BYTES + 256,
        "a retained log line is bounded to MAX_LOG_LINE_BYTES plus a short marker, got {} bytes",
        line.len()
    );
    assert!(
        line.chars().any(|c| c != 'L'),
        "a truncated log line ends with a marker so it does not read as the whole value"
    );

    worker.shutdown();
}

// ---------------------------------------------------------------------------
// Teardown (spec 24.3)
// ---------------------------------------------------------------------------

#[test]
fn shutdown_returns_a_clean_worker_exit() {
    require_host_interpreter!();
    let scratch = Scratch::new("shutdown");
    let mut worker = spawn(&scratch, "modern.shutdown", WELL_BEHAVED);
    assert!(worker.is_alive(), "the worker is live before shutdown");

    // A last successful call proves the worker was healthy, not already gone.
    worker
        .build_catalog()
        .expect("a live worker serves before shutdown");

    let exit: WorkerExit = worker.shutdown();
    assert_eq!(
        exit.code,
        Some(0),
        "a worker asked to stop exits cleanly rather than being killed"
    );
    assert!(
        !exit.hard_stopped,
        "a cooperative worker is not hard-stopped during an orderly shutdown"
    );
}

#[test]
fn dropping_a_worker_reaps_its_child_and_leaves_no_orphan() {
    require_host_interpreter!();
    let scratch = Scratch::new("drop");

    // The plugin reports its own pid — which is the worker interpreter's pid —
    // so the test can ask the OS about it after the worker is dropped.
    let pid = {
        let mut worker = spawn(&scratch, "modern.drop", PID_WITNESS);
        let suggestions = worker
            .suggest(&suggest_request("pid"))
            .expect("the pid-witness plugin answers");
        let pid: u32 = suggestions.items[0]
            .target
            .parse()
            .expect("the witness reports a numeric pid");
        if process_table_contains(pid).is_some() {
            assert_eq!(
                process_table_contains(pid),
                Some(true),
                "the live child is in the process table before drop"
            );
        }
        pid
        // `worker` is dropped here; Drop must reap the child.
    };

    // Reaped, not merely dead: an exited child that was never waited on stays in
    // the table as a zombie, and a launcher that leaks one per reload leaks them
    // forever. `None` means this platform cannot be asked and the test degrades.
    if process_table_contains(pid).is_some() {
        assert!(
            wait_until(RESPONSE_LIMIT, || process_table_contains(pid) != Some(true)),
            "dropping a worker reaps its child; pid {pid} is still in the process table"
        );
    }
}

#[test]
fn deferred_preview_negotiates_and_contains_callback_failures() {
    let scratch = Scratch::new("preview-callback");
    let interpreter = required_host_interpreter();
    let dir = write_plugin(
        &scratch,
        "preview-callback",
        r#"
from crikey_sdk import Item, Plugin, Preview, PreviewImage
class Fixture(Plugin):
    def suggest(self, query, context):
        self.details = {"selected": "Selected selected"}
        context.emit(Item(stable_id="selected", label="Selected", target="selected", preview_deferred=True))
    async def preview(self, item, context):
        detail = self.details[item.stable_id]
        if item.target == "error":
            raise ValueError("preview failed visibly")
        if item.target == "oversized":
            return Preview(text="x" * (256 * 1024 + 1))
        if item.target == "empty":
            return None
        return Preview(text=detail,
                       image=PreviewImage(1, 1, bytes((1, 2, 3, 255))))
"#,
    );
    let mut worker = ModernWorker::spawn(&interpreter, options("preview-callback", &dir))
        .expect("the real preview worker starts");
    assert!(worker.supports_preview());
    let batch = worker
        .suggest(&suggest_request("preview"))
        .expect("ordinary suggestions work");
    let mut item = batch.items[0].clone();
    assert_eq!(
        item.metadata.get("crikey.preview").map(String::as_str),
        Some("deferred")
    );
    let identity = serde_json::json!({
        "view":7,"query":9,"plugin":"modern.preview-callback","incarnation":11,
        "item":"selected","selection":13,"request":17
    });
    let reply = worker.preview(&item, &identity).expect("preview transport works");
    assert_eq!(reply["identity"], identity);
    assert_eq!(reply["status"], "content");
    assert_eq!(reply["content"]["text"], "Selected selected");
    assert_eq!(
        reply["content"]["image"]["rgba"],
        serde_json::json!([1, 2, 3, 255])
    );
    for (target, expected) in [("error", "failed"), ("oversized", "failed"), ("empty", "empty")] {
        item.target = target.into();
        assert_eq!(
            worker
                .preview(&item, &identity)
                .expect("contained callback reply")["status"],
            expected
        );
    }
    assert_eq!(
        worker
            .suggest(&suggest_request("still usable"))
            .expect("preview failure preserves suggest")
            .items
            .len(),
        1
    );
}

#[test]
fn oversized_escaped_preview_is_a_visible_failure_and_preserves_the_same_worker() {
    let scratch = Scratch::new("preview-budget");
    let interpreter = required_host_interpreter();
    let source = format!(
        "{}\nFixture = PreviewPlugin\n",
        include_str!("../../../compatibility/test-plugins/modern-preview/preview_plugin.py")
    );
    let dir = write_plugin(&scratch, "preview-budget", &source);
    let mut worker = ModernWorker::spawn(&interpreter, options("modern.preview-fixture", &dir))
        .expect("the real preview worker starts");
    let first = worker
        .suggest(&suggest_request("preview budget"))
        .expect("suggestions work");
    let pid = first.items[0].metadata["fixture.pid"].clone();
    let identity = serde_json::json!({
        "view":1,"query":1,"plugin":"modern.preview-fixture","incarnation":1,
        "item":"aggregate","selection":1,"request":1
    });
    for (key, diagnostic) in [
        ("aggregate", "preview aggregate text exceeds 256 KiB"),
        ("escaped-log", "preview frame exceeds 8 MiB after JSON escaping"),
    ] {
        let item = first
            .items
            .iter()
            .find(|item| item.stable_id.0 == key)
            .expect("fixture row");
        let mut selected = identity.clone();
        selected["item"] = serde_json::json!(key);
        let failure = worker
            .preview(item, &selected)
            .expect("invalid preview stays inside callback boundary");
        assert_eq!(failure["identity"], selected);
        assert_eq!(
            failure["status"], "failed",
            "the host must receive a visible error, not lose transport"
        );
        let detail = failure["detail"].as_str().expect("visible diagnostic");
        assert!(detail.contains(diagnostic), "{detail}");
        assert!(detail.len() <= 8192, "failure diagnostics stay bounded");
        assert!(
            failure.get("content").is_none(),
            "invalid content is rejected, not truncated"
        );
        assert!(
            worker.is_alive(),
            "invalid previews must not stop the suggestion worker"
        );

        let next = worker
            .suggest(&suggest_request("still the same worker"))
            .expect("subsequent suggestions survive invalid preview");
        assert!(
            next.items.iter().all(|item| item.metadata["fixture.pid"] == pid),
            "the original process, not a replacement, must answer"
        );
        let valid = next
            .items
            .iter()
            .find(|item| item.stable_id.0 == "B")
            .expect("valid row");
        selected["item"] = serde_json::json!("B");
        let reply = worker
            .preview(valid, &selected)
            .expect("subsequent preview transport works");
        assert_eq!(reply["identity"], selected);
        assert_eq!(reply["status"], "content");
        assert_eq!(
            reply["content"]["text"], "Selected B",
            "preview retains the originating suggestion instance's state"
        );
    }
}

#[test]
fn absent_preview_capability_is_explicit_and_preserves_suggestions() {
    let scratch = Scratch::new("preview-unavailable");
    let interpreter = required_host_interpreter();
    let dir = write_plugin(
        &scratch,
        "preview-unavailable",
        r#"
from crikey_sdk import Item, Plugin
class Fixture(Plugin):
    def suggest(self, query, context):
        context.emit(Item(stable_id="usable", label="Usable", target="usable"))
"#,
    );
    let mut worker = ModernWorker::spawn(&interpreter, options("preview-unavailable", &dir))
        .expect("the ordinary worker starts");
    assert!(!worker.supports_preview());
    let item = worker
        .suggest(&suggest_request("usable"))
        .expect("suggest works")
        .items
        .remove(0);
    let identity = serde_json::json!({"view":1,"query":1,"selection":1,"request":1});
    assert_eq!(
        worker
            .preview(&item, &identity)
            .expect("unsupported is a normal reply")["status"],
        "unsupported"
    );
    assert_eq!(
        worker
            .suggest(&suggest_request("again"))
            .expect("suggest remains usable")
            .items
            .len(),
        1
    );
}

#[test]
fn idle_child_exit_is_observed_without_another_transport_call() {
    let scratch = Scratch::new("idle-exit");
    let interpreter = required_host_interpreter();
    let source = format!(
        "{}\nFixture = IdleExit\n",
        include_str!("../../../compatibility/test-plugins/modern-preview/preview_plugin.py")
    );
    let dir = write_plugin(&scratch, "idle-exit", &source);
    let mut worker = ModernWorker::spawn(&interpreter, options("modern.idle-exit", &dir))
        .expect("idle-exit worker starts");
    assert!(!worker.build_catalog().unwrap().is_empty());
    assert!(worker.poll_alive());
    fs::write(dir.join("exit-now"), "").unwrap();
    let deadline = Instant::now() + RESPONSE_LIMIT;
    while worker.poll_alive() {
        assert!(Instant::now() < deadline, "idle exit was not observed");
        thread::sleep(POLL_INTERVAL);
    }
    assert!(!worker.is_alive());
    let exit = worker.shutdown();
    assert_eq!(exit.code, Some(21), "poll preserves the real exit status");
    assert!(!exit.hard_stopped, "an observed exit needs no hard stop");
}
