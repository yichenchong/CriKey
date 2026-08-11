//! Executable contract for the legacy Python module surface (spec 14.2, 14.4,
//! 14.10, 14.12, 26.2; roadmap M3; acceptance 31.11, 31.13, 31.31).
//!
//! These tests defend the four documented Keypirinha-compatible modules that
//! the Legacy Compatibility Layer ships under `<crate>/python/`:
//! `keypirinha`, `keypirinha_util`, `keypirinha_net` and `keypirinha_wintypes`.
//! They are *interpreter* tests, not `LegacyWorker` tests: each one spawns the
//! real CPython found by the M3 discovery rule, runs one small program with
//! `PYTHONPATH` pointing at the shim directory, and asserts on the child's exit
//! status, stdout and stderr. Nothing here imports the crate under test, so the
//! slice stays independent of the worker, the loader and the scheduler.
//!
//! Conventions used throughout:
//!
//! * **The interpreter is mandatory.** Discovery is `CRIKEY_PYTHON` then
//!   `python3` on `PATH` (the same order as the rest of M3, spec 14.11). A
//!   missing interpreter is a *test failure*, never a skip: a suite that
//!   silently skips proves nothing about the compatibility layer.
//! * **`-S` is the pinned isolation flag.** It drops `site` — and therefore
//!   every third-party `dist-packages`/`site-packages` entry — while still
//!   honouring `PYTHONPATH`, which is what keeps the shim directory
//!   load-bearing. `-E` and `-I` were rejected: both make CPython ignore
//!   `PYTHONPATH`, which would silently unhook the shim directory. `-B` is
//!   passed alongside purely so the tests never litter the source tree with
//!   `__pycache__`; it is not an isolation guarantee.
//! * **The child environment is built from scratch**, never inherited. That is
//!   what makes the headless-desktop assertions deterministic: `DISPLAY` and
//!   `WAYLAND_DISPLAY` are guaranteed absent, so the desktop-touching helpers
//!   have exactly one honest answer.
//! * **Programs talk back in `key=value` lines** on stdout and finish with a
//!   `DONE` sentinel, so a truncated or crashed run is distinguishable from a
//!   run that simply reported `False`. Every Rust assertion quotes the full
//!   child output, because a bare `assertion failed` on the far side of a
//!   subprocess boundary is undiagnosable.
//! * **No network, no wall-clock sleeps, no `#[ignore]`, no skips.** The one
//!   test that needs cross-thread visibility uses `threading.Event`
//!   handshakes and a bounded spin, never a timed sleep.
//!
//! The host boundary these tests pin is deliberately small. `keypirinha` holds
//! a single optional *host object*, installed with `keypirinha._set_host(host)`
//! and removed with `keypirinha._clear_host()`. Only `should_terminate()` is
//! mandatory on that object; every other capability is optional and its absence
//! must surface as `keypirinha.HostUnavailableError` naming the operation,
//! never as a bare `AttributeError` from inside the shim (spec 14.12). The
//! full protocol is:
//!
//! ```text
//! should_terminate() -> bool                                     (mandatory)
//! publish_suggestions(plugin, suggestions, match_method, sort_method)
//! publish_catalog(plugin, items, merge)
//! load_settings(plugin) -> dict[str, dict[str, str]]
//! load_resource(plugin, name) -> bytes
//! package_full_path(plugin) -> str
//! package_cache_path(plugin, create) -> str
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

// ---------------------------------------------------------------------------
// Interpreter discovery (spec 14.11; constraint: never skip)
// ---------------------------------------------------------------------------

/// Resolves the CPython interpreter using the M3 discovery order:
/// `CRIKEY_PYTHON` first, then `python3` on `PATH`.
///
/// Panics — loudly and with the search path quoted — when no interpreter is
/// available. The legacy compatibility layer is defined in terms of a real
/// CPython subprocess, so "no interpreter" is a failing environment, not a
/// reason to declare the contract untested.
fn discover_interpreter() -> PathBuf {
    if let Some(configured) = std::env::var_os("CRIKEY_PYTHON") {
        let path = PathBuf::from(&configured);
        assert!(
            path.is_file(),
            "CRIKEY_PYTHON is set to `{}` but that is not a file. Point it at a \
             CPython 3.8+ executable or unset it to fall back to `python3` on PATH.",
            path.display()
        );
        return path;
    }

    // The names the production discovery in `src/interpreter.rs` looks for, in
    // the same order and for the same reason: a Windows CPython installs
    // `python.exe` and usually no `python3.exe` at all, so a helper that knows
    // only the POSIX spelling reports "no interpreter" on a machine that has
    // one.
    const CANDIDATES: &[&str] = if cfg!(windows) {
        &["python3.exe", "python.exe"]
    } else {
        &["python3"]
    };
    let raw_path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&raw_path) {
        for name in CANDIDATES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    panic!(
        "no CPython interpreter found: CRIKEY_PYTHON is unset and none of {CANDIDATES:?} is on \
         PATH (searched `{}`). M3 requires a real interpreter subprocess (spec 14.11); a missing \
         interpreter is a test failure, not a skip.",
        raw_path.to_string_lossy()
    );
}

/// The shim directory the behavior wave populates, resolved from the crate root
/// so tests are independent of the working directory.
fn shim_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("python")
}

/// Isolation flag pinned for every legacy Python process, worker included.
const ISOLATION_FLAG: &str = "-S";

/// The four documented compatibility modules (spec 14.2).
const SHIM_MODULES: [&str; 4] = [
    "keypirinha",
    "keypirinha_util",
    "keypirinha_net",
    "keypirinha_wintypes",
];

// ---------------------------------------------------------------------------
// Scratch directories
// ---------------------------------------------------------------------------

/// A uniquely named scratch directory removed on drop.
///
/// Each test gets its own so the suite is safe under `cargo test`'s default
/// thread-per-test parallelism, and so a child process can be given a private
/// `HOME`/`TMPDIR` without touching the developer's.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "crikey-python-api-{}-{}-{}",
            std::process::id(),
            unique,
            tag
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path)
            .unwrap_or_else(|err| panic!("cannot create scratch dir `{}`: {err}", path.display()));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let target = self.path.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|err| panic!("cannot create `{}`: {err}", parent.display()));
        }
        fs::write(&target, contents)
            .unwrap_or_else(|err| panic!("cannot write `{}`: {err}", target.display()));
        target
    }

    fn mkdir(&self, relative: &str) -> PathBuf {
        let target = self.path.join(relative);
        fs::create_dir_all(&target)
            .unwrap_or_else(|err| panic!("cannot create `{}`: {err}", target.display()));
        target
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Running a Python program against the shims
// ---------------------------------------------------------------------------

/// Test-only host doubles, written next to each program so `sys.path[0]` picks
/// them up. This is *not* part of the shipped shim surface; it exists so the
/// tests can observe the host boundary from pure Python.
const HOST_DOUBLES: &str = r##"
"""Test-only host doubles for the CriKey legacy Python shims."""

import sys
import threading


class MinimalHost:
    """Implements only the mandatory `should_terminate` capability.

    Used to prove that every *optional* host capability degrades into a typed
    `keypirinha.HostUnavailableError` instead of an AttributeError escaping
    from inside the shim (spec 14.12).
    """

    def __init__(self):
        self.terminate = threading.Event()

    def should_terminate(self):
        return self.terminate.is_set()


class RecordingHost(MinimalHost):
    """Implements the whole documented host protocol and records every call."""

    def __init__(self, settings=None, resources=None,
                 package_path="/crikey-test/package",
                 cache_path="/crikey-test/cache"):
        MinimalHost.__init__(self)
        self.suggestion_publications = []
        self.catalog_publications = []
        self.cache_path_requests = []
        self._settings = settings if settings is not None else {}
        self._resources = resources if resources is not None else {}
        self._package_path = package_path
        self._cache_path = cache_path

    def publish_suggestions(self, plugin, suggestions, match_method, sort_method):
        self.suggestion_publications.append(
            (plugin.id, list(suggestions), int(match_method), int(sort_method)))

    def publish_catalog(self, plugin, items, merge):
        self.catalog_publications.append((plugin.id, list(items), bool(merge)))

    def load_settings(self, plugin):
        return self._settings

    def load_resource(self, plugin, name):
        return self._resources[name]

    def package_full_path(self, plugin):
        return self._package_path

    def package_cache_path(self, plugin, create):
        self.cache_path_requests.append(bool(create))
        return self._cache_path


def labels(items):
    return ",".join(item.label() for item in items)


def emit(key, value):
    """Report one observation as a single stdout line."""
    text = str(value).replace("\r", " | ").replace("\n", " | ")
    sys.stdout.write(str(key) + "=" + text + "\n")
    sys.stdout.flush()


def done():
    sys.stdout.write("DONE\n")
    sys.stdout.flush()
"##;

/// One completed child run.
struct PyRun {
    argv: Vec<String>,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl PyRun {
    fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// Everything a human needs to diagnose the failure, including the child's
    /// own traceback. Every assertion in this file appends it.
    fn describe(&self) -> String {
        let code = match self.exit_code {
            Some(code) => code.to_string(),
            None => "killed by signal (no exit code)".to_string(),
        };
        format!(
            "--- python argv ---\n{}\n--- exit code ---\n{}\n--- stdout ---\n{}\n--- stderr ---\n{}\n---",
            self.argv.join(" "),
            code,
            if self.stdout.is_empty() {
                "<empty>"
            } else {
                self.stdout.as_str()
            },
            if self.stderr.is_empty() {
                "<empty>"
            } else {
                self.stderr.as_str()
            },
        )
    }

    fn fields(&self) -> BTreeMap<&str, &str> {
        self.stdout
            .lines()
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .filter_map(|line| line.split_once('='))
            .collect()
    }

    /// The value reported for `key`, or a panic quoting the whole run.
    fn field(&self, key: &str) -> String {
        match self.fields().get(key) {
            Some(value) => (*value).to_string(),
            None => panic!(
                "the Python program never reported `{key}=...`; the shim is missing the behaviour \
                 under test or failed before reaching it.\n{}",
                self.describe()
            ),
        }
    }

    /// A `True`/`False` observation.
    fn flag(&self, key: &str) -> bool {
        match self.field(key).as_str() {
            "True" => true,
            "False" => false,
            other => panic!(
                "expected `{key}` to be a Python bool but the program reported `{other}`.\n{}",
                self.describe()
            ),
        }
    }

    fn int(&self, key: &str) -> i64 {
        let raw = self.field(key);
        raw.parse().unwrap_or_else(|_| {
            panic!(
                "expected `{key}` to be an integer but the program reported `{raw}`.\n{}",
                self.describe()
            )
        })
    }

    /// Asserts a `True` observation, quoting the run on failure.
    fn expect(&self, key: &str, why: &str) {
        assert!(self.flag(key), "{why}\n{}", self.describe());
    }

    /// Asserts an exact string observation, quoting the run on failure.
    fn expect_eq(&self, key: &str, want: &str, why: &str) {
        let got = self.field(key);
        assert_eq!(got, want, "{why}\n{}", self.describe());
    }
}

/// Builds the child environment from scratch.
///
/// Nothing is inherited except `PATH` (so the interpreter can find its own
/// helpers). In particular `DISPLAY` and `WAYLAND_DISPLAY` are absent by
/// construction, which is what makes the desktop-unavailability assertions
/// deterministic rather than dependent on how the suite was launched.
///
/// `HOME` and `TMPDIR` are POSIX spellings, so the same two directories are
/// pinned again under the names Windows reads: CPython's `ntpath.expanduser`
/// consults `USERPROFILE`, then `HOMEDRIVE`/`HOMEPATH`, and never `HOME`, so
/// on Windows the POSIX name isolates nothing. The system variables below are
/// passed through for exactly the reason `PATH` is — they are how a Windows
/// process finds the system it runs on, and a child cleared of them fails
/// before it reaches an assertion. None of them exists on a Unix host, so
/// nothing is added there.
fn child_env(scratch: &TempDir) -> Vec<(String, String)> {
    let scratch_path = scratch.path().display().to_string();
    let mut environment = vec![
        ("PYTHONPATH".to_string(), shim_dir().display().to_string()),
        ("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string()),
        ("PYTHONIOENCODING".to_string(), "utf-8".to_string()),
        ("PYTHONUTF8".to_string(), "1".to_string()),
        ("LC_ALL".to_string(), "C.UTF-8".to_string()),
        ("HOME".to_string(), scratch_path.clone()),
        ("TMPDIR".to_string(), scratch_path.clone()),
        ("CRIKEY_SHIM_DIR".to_string(), shim_dir().display().to_string()),
        (
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()),
        ),
    ];
    for name in ["USERPROFILE", "TEMP", "TMP"] {
        environment.push((name.to_string(), scratch_path.clone()));
    }
    for name in [
        "SystemRoot",
        "SystemDrive",
        "windir",
        "COMSPEC",
        "PATHEXT",
        "NUMBER_OF_PROCESSORS",
    ] {
        if let Ok(value) = std::env::var(name) {
            environment.push((name.to_string(), value));
        }
    }
    environment
}

/// Runs `source` under the pinned isolation flags and returns the raw result.
fn run_python(scratch: &TempDir, source: &str, extra_env: &[(&str, &str)]) -> PyRun {
    run_python_with_flags(scratch, &[ISOLATION_FLAG, "-B"], source, extra_env)
}

fn run_python_with_flags(
    scratch: &TempDir,
    flags: &[&str],
    source: &str,
    extra_env: &[(&str, &str)],
) -> PyRun {
    let shims = shim_dir();
    assert!(
        shims.is_dir(),
        "the legacy Python shim directory `{}` does not exist. The behavior wave must create it \
         with keypirinha.py, keypirinha_util.py, keypirinha_net.py, keypirinha_wintypes.py and \
         _crikey_legacy_worker.py (spec 14.2).",
        shims.display()
    );

    scratch.write("_kptest.py", HOST_DOUBLES);
    let program = scratch.write("program.py", source);

    let interpreter = discover_interpreter();
    let mut command = Command::new(&interpreter);
    command.env_clear();
    for (key, value) in child_env(scratch) {
        command.env(key, value);
    }
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.args(flags).arg(&program).current_dir(scratch.path());

    let mut argv = vec![interpreter.display().to_string()];
    argv.extend(flags.iter().map(|flag| (*flag).to_string()));
    argv.push(program.display().to_string());

    let output = command.output().unwrap_or_else(|err| {
        panic!(
            "failed to spawn the legacy interpreter `{}`: {err}. M3 requires a working CPython \
             subprocess (spec 14.11).",
            interpreter.display()
        )
    });

    PyRun {
        argv,
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Runs `source` and asserts it completed: exit 0 and the `DONE` sentinel seen.
fn run_ok(scratch: &TempDir, source: &str, extra_env: &[(&str, &str)]) -> PyRun {
    let run = run_python(scratch, source, extra_env);
    assert!(
        run.succeeded(),
        "the legacy shim program did not exit 0; the shim under test raised or is missing.\n{}",
        run.describe()
    );
    assert!(
        run.stdout.lines().any(|line| line.trim_end() == "DONE"),
        "the legacy shim program exited 0 but never reached its DONE sentinel, so its \
         observations are incomplete.\n{}",
        run.describe()
    );
    run
}

// ---------------------------------------------------------------------------
// `keypirinha`: module surface
// ---------------------------------------------------------------------------

#[test]
fn importing_keypirinha_exposes_the_documented_public_surface() {
    let scratch = TempDir::new("surface");
    let run = run_ok(
        &scratch,
        r##"
import inspect

import keypirinha as kp
import _kptest as t

MODULE_CALLABLES = [
    "name", "version", "version_string", "should_terminate",
    "_set_host", "_clear_host", "_install_stdout_guard",
]
MODULE_CLASSES = ["Plugin", "CatalogItem", "Settings", "Action"]
MODULE_ENUMS = ["ItemCategory", "ItemArgsHint", "ItemHitHint", "Match", "Sort", "Events"]
MODULE_EXCEPTIONS = [
    "KeypirinhaError", "UndocumentedApiError", "InvalidItemError",
    "SettingsError", "HostUnavailableError",
]
PLUGIN_CALLBACKS = [
    "on_start", "on_catalog", "on_suggest", "on_execute",
    "on_activated", "on_deactivated", "on_events",
]
PLUGIN_METHODS = [
    "create_item", "set_catalog", "merge_catalog", "set_suggestions",
    "should_terminate", "load_settings", "package_full_path",
    "get_package_cache_path", "load_text_resource", "load_binary_resource",
    "friendly_name", "package_full_name",
    "info", "warn", "err", "dbg",
]
SETTINGS_METHODS = ["get", "get_bool", "get_int", "get_float", "sections", "keys", "has"]
ITEM_METHODS = [
    "category", "label", "short_desc", "target", "args_hint", "hit_hint",
    "loop_on_suggest", "icon_handle", "data_bag", "set_data_bag",
]
# The second argument handed to on_execute(). Its accessors map onto
# crikey_core::Action as: action_id.0 -> name(), label -> label(),
# description -> short_desc().
ACTION_METHODS = ["name", "label", "short_desc"]

missing = []


def require(owner, owner_name, names, predicate, kind):
    # A missing owner is already recorded by the MODULE_CLASSES sweep; skip its
    # members rather than crashing, so one run reports EVERY missing symbol
    # instead of dying on the first one.
    if owner is None:
        return
    for name in names:
        value = getattr(owner, name, None)
        if value is None or not predicate(value):
            missing.append(owner_name + "." + name + " (" + kind + ")")


require(kp, "keypirinha", MODULE_CALLABLES, callable, "callable")
require(kp, "keypirinha", MODULE_CLASSES, inspect.isclass, "class")
require(kp, "keypirinha", MODULE_ENUMS, inspect.isclass, "enum")
require(kp, "keypirinha", MODULE_EXCEPTIONS,
        lambda v: inspect.isclass(v) and issubclass(v, BaseException), "exception")

# Resolved through getattr so an absent class degrades into a reported entry
# rather than an UndocumentedApiError that aborts the sweep.
plugin_cls = getattr(kp, "Plugin", None)
settings_cls = getattr(kp, "Settings", None)
item_cls = getattr(kp, "CatalogItem", None)
action_cls = getattr(kp, "Action", None)

require(plugin_cls, "keypirinha.Plugin", PLUGIN_CALLBACKS, callable, "callback")
require(plugin_cls, "keypirinha.Plugin", PLUGIN_METHODS, callable, "method")
require(settings_cls, "keypirinha.Settings", SETTINGS_METHODS, callable, "method")
require(item_cls, "keypirinha.CatalogItem", ITEM_METHODS, callable, "method")
require(action_cls, "keypirinha.Action", ACTION_METHODS, callable, "method")

# Report the full inventory before exercising anything, so even a later crash
# leaves the missing-symbol list on stdout for the failure message to quote.
t.emit("missing", ";".join(missing) if missing else "<none>")
t.emit("missing_count", len(missing))

if not isinstance(getattr(plugin_cls, "id", None), property):
    missing.append("keypirinha.Plugin.id (read-only property)")
if not isinstance(getattr(settings_cls, "DEFAULT_SECTION", None), str):
    missing.append("keypirinha.Settings.DEFAULT_SECTION (str constant)")

t.emit("missing", ";".join(missing) if missing else "<none>")
t.emit("missing_count", len(missing))

# A plugin subclass must be instantiable with no host installed: importing and
# constructing a plugin is not a host operation.
class SurfacePlugin(kp.Plugin):
    pass

plugin = SurfacePlugin()
t.emit("plugin_id_is_str", isinstance(plugin.id, str))
t.emit("plugin_id_nonempty", len(plugin.id) > 0)
t.emit("friendly_name", plugin.friendly_name())

class DeclaredHost:
    def package_full_name(self, plugin):
        return "well-behaved"


class HyphenPackagePlugin(kp.Plugin):
    pass


kp._set_host(DeclaredHost())
declared = HyphenPackagePlugin()
t.emit("declared_package_name", declared.package_full_name())
t.emit("declared_plugin_id", declared.id)
kp._clear_host()

# Default callbacks exist and are inert: an unchanged legacy plugin that
# overrides only one of them must not explode on the others.
plugin.on_start()
plugin.on_catalog()
plugin.on_suggest("query", [])
plugin.on_activated()
plugin.on_deactivated()
plugin.on_events(kp.Events.APPCONFIG)
# on_execute receives the selected item and the chosen action. `action` is None
# when the default action was taken, which an unchanged plugin is entitled to
# assume it may receive.
execute_item = plugin.create_item(
    category=kp.ItemCategory.FILE, label="Executed", short_desc="",
    target="/executed/target", args_hint=kp.ItemArgsHint.FORBIDDEN,
    hit_hint=kp.ItemHitHint.NOARGS)
plugin.on_execute(execute_item, None)
t.emit("default_callbacks_inert", True)

t.emit("settings_default_section", kp.Settings.DEFAULT_SECTION)
t.emit("name", kp.name())
t.emit("version_is_tuple", isinstance(kp.version(), tuple))
t.emit("version_all_ints", all(isinstance(part, int) for part in kp.version()))
t.emit("version_string", kp.version_string())

# Spec 14.13: the layer must not present itself as a Keypirinha component.
branding = kp.name() + " " + kp.version_string()
t.emit("branding_clean", "Keypirinha" not in branding)

t.done()
"##,
        &[],
    );

    let missing_count = run.int("missing_count");
    assert_eq!(
        missing_count,
        0,
        "the `keypirinha` shim is missing documented public symbols: {}\n{}",
        run.field("missing"),
        run.describe()
    );
    run.expect(
        "plugin_id_is_str",
        "keypirinha.Plugin.id must be a string identifying the plugin instance",
    );
    run.expect("plugin_id_nonempty", "keypirinha.Plugin.id must not be empty");
    run.expect_eq(
        "friendly_name",
        "SurfacePlugin",
        "keypirinha.Plugin.friendly_name() must default to the plugin class name",
    );
    run.expect_eq(
        "declared_package_name",
        "well-behaved",
        "Plugin.package_full_name must use the host's declared identifier, not a sanitized module name",
    );
    run.expect_eq(
        "declared_plugin_id",
        "well-behaved.HyphenPackagePlugin",
        "Plugin.id must incorporate the declared package identifier",
    );
    run.expect(
        "default_callbacks_inert",
        "every documented lifecycle callback must have an inert default so a legacy plugin can \
         override only the ones it cares about (spec 14.4)",
    );
    run.expect_eq(
        "settings_default_section",
        "DEFAULT",
        "keypirinha.Settings.DEFAULT_SECTION pins the name of the unnamed top-level config section",
    );
    run.expect_eq(
        "name",
        "CriKey",
        "keypirinha.name() must report the host product, which is CriKey (spec 14.13)",
    );
    run.expect("version_is_tuple", "keypirinha.version() must return a tuple");
    run.expect(
        "version_all_ints",
        "keypirinha.version() must return a tuple of integers",
    );
    run.expect(
        "branding_clean",
        "spec 14.13 forbids presenting the Legacy Compatibility Layer as a Keypirinha component, \
         so neither name() nor version_string() may contain the Keypirinha product name",
    );
}

#[test]
fn a_plugin_subclass_round_trips_every_documented_catalog_item_field() {
    let scratch = TempDir::new("item-roundtrip");
    let run = run_ok(
        &scratch,
        r##"
import keypirinha as kp
import _kptest as t

ICON = object()
BAG = {"key": "value", "count": 3, "nested": ["a", "b"]}


class RoundTripPlugin(kp.Plugin):
    def build(self):
        return self.create_item(
            category=kp.ItemCategory.KEYWORD,
            label="Round Trip",
            short_desc="a short description",
            target="round-trip-target",
            args_hint=kp.ItemArgsHint.REQUIRED,
            hit_hint=kp.ItemHitHint.IGNORE,
            loop_on_suggest=True,
            icon_handle=ICON,
            data_bag=BAG,
        )


plugin = RoundTripPlugin()
item = plugin.build()

t.emit("type_name", type(item).__name__)
t.emit("is_catalog_item", isinstance(item, kp.CatalogItem))
t.emit("category", int(item.category()))
t.emit("category_is_keyword", item.category() == kp.ItemCategory.KEYWORD)
t.emit("label", item.label())
t.emit("short_desc", item.short_desc())
t.emit("target", item.target())
t.emit("args_hint", int(item.args_hint()))
t.emit("hit_hint", int(item.hit_hint()))
t.emit("loop_on_suggest", item.loop_on_suggest())
t.emit("icon_is_identical", item.icon_handle() is ICON)
t.emit("data_bag_equal", item.data_bag() == BAG)

item.set_data_bag({"replaced": True})
t.emit("data_bag_after_set", item.data_bag() == {"replaced": True})

# Documented positional order: category, label, short_desc, target, args_hint,
# hit_hint, then the three optional trailing parameters.
positional = plugin.create_item(
    kp.ItemCategory.FILE, "Positional", "Positional desc", "/positional/target",
    kp.ItemArgsHint.FORBIDDEN, kp.ItemHitHint.NOARGS)
t.emit("positional_category", int(positional.category()))
t.emit("positional_label", positional.label())
t.emit("positional_short_desc", positional.short_desc())
t.emit("positional_target", positional.target())
t.emit("positional_args_hint", int(positional.args_hint()))
t.emit("positional_hit_hint", int(positional.hit_hint()))
t.emit("positional_defaults", (positional.loop_on_suggest(),
                               positional.icon_handle(),
                               positional.data_bag()) == (False, None, None))

# Items are independent values: mutating one must not disturb another.
t.emit("items_independent", item.label() != positional.label()
       and item.data_bag() != positional.data_bag())

t.done()
"##,
        &[],
    );

    run.expect_eq(
        "type_name",
        "CatalogItem",
        "Plugin.create_item() must return a keypirinha.CatalogItem",
    );
    run.expect("is_catalog_item", "the created item must be a CatalogItem");
    run.expect(
        "category_is_keyword",
        "CatalogItem.category() must return the category it was constructed with",
    );
    run.expect_eq(
        "label",
        "Round Trip",
        "CatalogItem.label() must round-trip the constructed label",
    );
    run.expect_eq(
        "short_desc",
        "a short description",
        "CatalogItem.short_desc() must round-trip the constructed short description",
    );
    run.expect_eq(
        "target",
        "round-trip-target",
        "CatalogItem.target() must round-trip the constructed target",
    );
    assert_eq!(
        run.int("args_hint"),
        2,
        "CatalogItem.args_hint() must round-trip ItemArgsHint.REQUIRED\n{}",
        run.describe()
    );
    assert_eq!(
        run.int("hit_hint"),
        2,
        "CatalogItem.hit_hint() must round-trip ItemHitHint.IGNORE\n{}",
        run.describe()
    );
    run.expect(
        "loop_on_suggest",
        "CatalogItem.loop_on_suggest() must round-trip the constructed flag",
    );
    run.expect(
        "icon_is_identical",
        "CatalogItem.icon_handle() must hand back the exact icon handle object it was given; the \
         shim must not copy or re-wrap it",
    );
    run.expect(
        "data_bag_equal",
        "CatalogItem.data_bag() must round-trip the constructed data bag",
    );
    run.expect(
        "data_bag_after_set",
        "CatalogItem.set_data_bag() must replace the data bag observably",
    );
    run.expect_eq(
        "positional_label",
        "Positional",
        "create_item() must accept the documented positional parameter order",
    );
    run.expect_eq(
        "positional_target",
        "/positional/target",
        "create_item() must accept the documented positional parameter order",
    );
    run.expect(
        "positional_defaults",
        "create_item() must default loop_on_suggest to False and icon_handle/data_bag to None",
    );
    run.expect(
        "items_independent",
        "catalog items must be independent values, not views over shared plugin state",
    );
}

#[test]
fn item_category_constants_are_distinct_and_stable_and_unknown_categories_are_rejected() {
    let scratch = TempDir::new("categories");
    let run = run_ok(
        &scratch,
        r##"
import keypirinha as kp
import _kptest as t

CATEGORIES = [("KEYWORD", 1), ("CMDLINE", 2), ("FILE", 3), ("URL", 4),
              ("EXPRESSION", 5), ("REFERENCE", 6), ("ERROR", 7), ("USER_BASE", 100)]
ARGS_HINTS = [("FORBIDDEN", 0), ("ACCEPTED", 1), ("REQUIRED", 2)]
HIT_HINTS = [("NOARGS", 0), ("KEEPALL", 1), ("IGNORE", 2)]


def check(enum_cls, enum_name, expected):
    wrong = []
    values = []
    for member, want in expected:
        got = getattr(enum_cls, member, None)
        if got is None:
            wrong.append(enum_name + "." + member + " missing")
            continue
        values.append(int(got))
        if int(got) != want:
            wrong.append(enum_name + "." + member + "=" + str(int(got)) + " want " + str(want))
    return wrong, values


category_wrong, category_values = check(kp.ItemCategory, "ItemCategory", CATEGORIES)
args_wrong, args_values = check(kp.ItemArgsHint, "ItemArgsHint", ARGS_HINTS)
hit_wrong, hit_values = check(kp.ItemHitHint, "ItemHitHint", HIT_HINTS)

t.emit("unstable", ";".join(category_wrong + args_wrong + hit_wrong) or "<none>")
t.emit("unstable_count", len(category_wrong) + len(args_wrong) + len(hit_wrong))
t.emit("categories_distinct", len(set(category_values)) == len(category_values))
t.emit("args_hints_distinct", len(set(args_values)) == len(args_values))
t.emit("hit_hints_distinct", len(set(hit_values)) == len(hit_values))

t.emit("invalid_is_value_error", issubclass(kp.InvalidItemError, ValueError))
t.emit("invalid_is_keypirinha_error", issubclass(kp.InvalidItemError, kp.KeypirinhaError))


class CategoryPlugin(kp.Plugin):
    pass


plugin = CategoryPlugin()


_UNSET = object()


def make(category=_UNSET, args_hint=_UNSET, hit_hint=_UNSET):
    return plugin.create_item(
        category=kp.ItemCategory.KEYWORD if category is _UNSET else category,
        label="label",
        short_desc="desc",
        target="target",
        args_hint=kp.ItemArgsHint.ACCEPTED if args_hint is _UNSET else args_hint,
        hit_hint=kp.ItemHitHint.NOARGS if hit_hint is _UNSET else hit_hint,
    )


def outcome(**kwargs):
    try:
        item = make(**kwargs)
    except kp.InvalidItemError as exc:
        return "rejected:" + str(getattr(exc, "field", "<no field attribute>"))
    except Exception as exc:  # noqa: BLE001 - deliberately catching the wrong error
        return "wrong-error:" + type(exc).__name__ + ":" + str(exc)
    return "coerced:" + repr(int(item.category()))


t.emit("reject_zero", outcome(category=0))
t.emit("reject_negative", outcome(category=-1))
t.emit("reject_below_user_base", outcome(category=99))
t.emit("reject_string", outcome(category="file"))
t.emit("reject_none", outcome(category=None))
t.emit("reject_float", outcome(category=2.5))
t.emit("reject_bad_args_hint", outcome(args_hint=42))
t.emit("reject_bad_hit_hint", outcome(hit_hint=-3))

# USER_BASE is the documented extension point: anything at or above it is a
# legitimate plugin-defined category and must NOT be rejected.
extension = make(category=int(kp.ItemCategory.USER_BASE) + 7)
t.emit("user_extension_value", int(extension.category()))

t.done()
"##,
        &[],
    );

    assert_eq!(
        run.int("unstable_count"),
        0,
        "item category / argument-hint / hit-hint constants must be stable values the behavior \
         wave may not renumber, but these differ: {}\n{}",
        run.field("unstable"),
        run.describe()
    );
    run.expect(
        "categories_distinct",
        "ItemCategory members must be pairwise distinct",
    );
    run.expect(
        "args_hints_distinct",
        "ItemArgsHint members must be pairwise distinct",
    );
    run.expect(
        "hit_hints_distinct",
        "ItemHitHint members must be pairwise distinct",
    );
    run.expect(
        "invalid_is_value_error",
        "keypirinha.InvalidItemError must subclass ValueError so unchanged plugins catching \
         ValueError still work",
    );
    run.expect(
        "invalid_is_keypirinha_error",
        "keypirinha.InvalidItemError must subclass keypirinha.KeypirinhaError so the layer has one \
         error taxonomy to report on (spec 26.2)",
    );

    for key in [
        "reject_zero",
        "reject_negative",
        "reject_below_user_base",
        "reject_string",
        "reject_none",
        "reject_float",
    ] {
        run.expect_eq(
            key,
            "rejected:category",
            "an unknown item category must be rejected with keypirinha.InvalidItemError carrying \
             field=\"category\", never silently coerced to a valid one",
        );
    }
    run.expect_eq(
        "reject_bad_args_hint",
        "rejected:args_hint",
        "an unknown argument hint must be rejected with InvalidItemError carrying field=\"args_hint\"",
    );
    run.expect_eq(
        "reject_bad_hit_hint",
        "rejected:hit_hint",
        "an unknown hit hint must be rejected with InvalidItemError carrying field=\"hit_hint\"",
    );
    assert_eq!(
        run.int("user_extension_value"),
        107,
        "ItemCategory.USER_BASE is the documented plugin extension point: categories at or above \
         it must be accepted and preserved exactly\n{}",
        run.describe()
    );
}

// ---------------------------------------------------------------------------
// `keypirinha`: publication contracts (spec 7.1, 14.8)
// ---------------------------------------------------------------------------

#[test]
fn set_suggestions_publishes_one_complete_replacement_and_the_newest_call_wins() {
    let scratch = TempDir::new("suggestions");
    let run = run_ok(
        &scratch,
        r##"
import keypirinha as kp
import _kptest as t

host = t.RecordingHost()
kp._set_host(host)


class SuggestPlugin(kp.Plugin):
    def item(self, label):
        return self.create_item(
            category=kp.ItemCategory.KEYWORD, label=label, short_desc="",
            target=label, args_hint=kp.ItemArgsHint.FORBIDDEN,
            hit_hint=kp.ItemHitHint.IGNORE)

    def on_suggest(self, user_input, items_chain):
        self.set_suggestions([self.item("first-a"), self.item("first-b")])
        self.set_suggestions([self.item("second-only")])


plugin = SuggestPlugin()
plugin.on_suggest("query", [])

publications = host.suggestion_publications
t.emit("publication_count", len(publications))
t.emit("first_labels", t.labels(publications[0][1]))
t.emit("first_len", len(publications[0][1]))
t.emit("latest_labels", t.labels(publications[-1][1]))
t.emit("latest_len", len(publications[-1][1]))
t.emit("plugin_id_matches", publications[-1][0] == plugin.id)
t.emit("default_match", publications[-1][2] == int(kp.Match.DEFAULT))
t.emit("default_sort", publications[-1][3] == int(kp.Sort.DEFAULT))

plugin.set_suggestions([plugin.item("third")], kp.Match.FUZZY, kp.Sort.LABEL_ASC)
t.emit("explicit_match", host.suggestion_publications[-1][2] == int(kp.Match.FUZZY))
t.emit("explicit_sort", host.suggestion_publications[-1][3] == int(kp.Sort.LABEL_ASC))
t.emit("explicit_labels", t.labels(host.suggestion_publications[-1][1]))

# An empty publication is a real, complete publication that clears the list.
plugin.set_suggestions([])
t.emit("empty_publication_len", len(host.suggestion_publications[-1][1]))
t.emit("total_publications", len(host.suggestion_publications))

# The published list must be a snapshot: mutating the caller's list afterwards
# must not retroactively alter what was published.
mutable = [plugin.item("snapshot")]
plugin.set_suggestions(mutable)
mutable.append(plugin.item("appended-after-publication"))
t.emit("snapshot_labels", t.labels(host.suggestion_publications[-1][1]))

# Publishing without a host is a host operation and must fail with the typed
# error, not an AttributeError from inside the shim.
kp._set_host(t.MinimalHost())
try:
    plugin.set_suggestions([plugin.item("no-host")])
except kp.HostUnavailableError as exc:
    t.emit("no_host_error", "HostUnavailableError:" + str(getattr(exc, "operation", "<none>")))
except Exception as exc:  # noqa: BLE001
    t.emit("no_host_error", "wrong-error:" + type(exc).__name__ + ":" + str(exc))
else:
    t.emit("no_host_error", "silently-succeeded")

t.done()
"##,
        &[],
    );

    assert_eq!(
        run.int("publication_count"),
        2,
        "two set_suggestions() calls inside one callback must produce two distinct publications; \
         the shim must not silently merge them into one (spec 7.1 `one complete publication`)\n{}",
        run.describe()
    );
    run.expect_eq(
        "first_labels",
        "first-a,first-b",
        "the first publication must carry the complete first list",
    );
    run.expect_eq(
        "latest_labels",
        "second-only",
        "the second set_suggestions() call must publish a complete REPLACEMENT; only the newest \
         list may survive, so the earlier items must not reappear",
    );
    assert_eq!(
        run.int("latest_len"),
        1,
        "the newest publication must contain exactly the newest list; a longer list means the \
         shim appended instead of replacing\n{}",
        run.describe()
    );
    run.expect(
        "plugin_id_matches",
        "each publication must be attributed to the publishing plugin instance",
    );
    run.expect(
        "default_match",
        "set_suggestions() must default match_method to keypirinha.Match.DEFAULT",
    );
    run.expect(
        "default_sort",
        "set_suggestions() must default sort_method to keypirinha.Sort.DEFAULT",
    );
    run.expect(
        "explicit_match",
        "an explicit match_method must reach the host unchanged",
    );
    run.expect(
        "explicit_sort",
        "an explicit sort_method must reach the host unchanged",
    );
    assert_eq!(
        run.int("empty_publication_len"),
        0,
        "publishing an empty list is a complete publication that clears the suggestions; the shim \
         must not treat it as a no-op\n{}",
        run.describe()
    );
    assert_eq!(
        run.int("total_publications"),
        4,
        "every set_suggestions() call must publish exactly once\n{}",
        run.describe()
    );
    run.expect_eq(
        "snapshot_labels",
        "snapshot",
        "a publication must snapshot the list it was given; mutating the plugin's own list \
         afterwards must not retroactively change what was published",
    );
    run.expect_eq(
        "no_host_error",
        "HostUnavailableError:publish_suggestions",
        "publishing through a host that does not implement publish_suggestions must raise the \
         typed keypirinha.HostUnavailableError naming the missing operation, not leak an \
         AttributeError from inside the shim (spec 14.12)",
    );
}

#[test]
fn set_catalog_and_merge_catalog_publish_complete_lists_and_flag_the_merge_intent() {
    let scratch = TempDir::new("catalog");
    let run = run_ok(
        &scratch,
        r##"
import keypirinha as kp
import _kptest as t

host = t.RecordingHost()
kp._set_host(host)


class CatalogPlugin(kp.Plugin):
    def item(self, label):
        return self.create_item(
            category=kp.ItemCategory.KEYWORD, label=label, short_desc="",
            target=label, args_hint=kp.ItemArgsHint.FORBIDDEN,
            hit_hint=kp.ItemHitHint.NOARGS)

    def on_catalog(self):
        self.set_catalog([self.item("base-a"), self.item("base-b")])


plugin = CatalogPlugin()
plugin.on_catalog()
plugin.merge_catalog([plugin.item("merged-c")])

# Spec 14.8: repeated on_catalog() calls are permitted and each rebuild is a
# complete set_catalog publication, not an accumulation.
plugin.on_catalog()

publications = host.catalog_publications
t.emit("publication_count", len(publications))
t.emit("first_labels", t.labels(publications[0][1]))
t.emit("first_merge", publications[0][2])
t.emit("second_labels", t.labels(publications[1][1]))
t.emit("second_merge", publications[1][2])
t.emit("third_labels", t.labels(publications[2][1]))
t.emit("third_merge", publications[2][2])
t.emit("plugin_id_matches", all(pub[0] == plugin.id for pub in publications))

t.done()
"##,
        &[],
    );

    assert_eq!(
        run.int("publication_count"),
        3,
        "each set_catalog()/merge_catalog() call must publish exactly once, and repeated \
         on_catalog() calls must be permitted (spec 14.8)\n{}",
        run.describe()
    );
    run.expect_eq(
        "first_labels",
        "base-a,base-b",
        "set_catalog() must publish the complete list it was given",
    );
    run.expect_eq(
        "first_merge",
        "False",
        "set_catalog() must publish with merge=False, meaning replace",
    );
    run.expect_eq(
        "second_labels",
        "merged-c",
        "merge_catalog() must publish exactly the items it was given; the shim must not fold the \
         previous catalog into the payload",
    );
    run.expect_eq(
        "second_merge",
        "True",
        "merge_catalog() must publish with merge=True so the host, not the shim, owns merging",
    );
    run.expect_eq(
        "third_labels",
        "base-a,base-b",
        "a repeated on_catalog() rebuild must publish a complete replacement, not an accumulation \
         of the earlier catalog and merge (spec 14.8)",
    );
    run.expect_eq(
        "third_merge",
        "False",
        "a catalog rebuild publishes with merge=False",
    );
    run.expect(
        "plugin_id_matches",
        "every catalog publication must be attributed to the publishing plugin instance",
    );
}

// ---------------------------------------------------------------------------
// `keypirinha`: cooperative termination (spec 7.1, 14.5; acceptance 31.17)
// ---------------------------------------------------------------------------

#[test]
fn should_terminate_is_false_until_the_host_raises_the_cooperative_flag() {
    let scratch = TempDir::new("terminate");
    let run = run_ok(
        &scratch,
        r##"
import threading

import keypirinha as kp
import _kptest as t

# With no host installed at all, cooperative termination is simply "keep going".
# This is the documented default so a plugin exercised outside a worker (unit
# tests, developer mode) does not have to install anything.
t.emit("default_no_host", kp.should_terminate())
t.emit("default_no_host_with_delay", kp.should_terminate(0))

host = t.MinimalHost()
kp._set_host(host)


class TerminatePlugin(kp.Plugin):
    pass


plugin = TerminatePlugin()

t.emit("before_flag_module", kp.should_terminate())
t.emit("before_flag_plugin", plugin.should_terminate())

# The flag must be observable from inside a callback that is already running on
# this thread, which is the whole point of cooperative cancellation: the host
# raises it from its stdin reader thread while the callback blocks the main one.
callback_entered = threading.Event()


def raise_flag_from_host_thread():
    callback_entered.wait(30)
    host.terminate.set()


flipper = threading.Thread(target=raise_flag_from_host_thread, daemon=True)
flipper.start()

callback_entered.set()
spins = 0
SPIN_LIMIT = 20_000_000
while not kp.should_terminate():
    spins += 1
    if spins > SPIN_LIMIT:
        raise AssertionError(
            "should_terminate() never became True after the host raised the cooperative flag "
            "from another thread; the shim is caching the value or reading a thread-local")
flipper.join(30)

t.emit("observed_inside_callback", True)
t.emit("after_flag_module", kp.should_terminate())
t.emit("after_flag_plugin", plugin.should_terminate())
t.emit("after_flag_with_delay", kp.should_terminate(0))
t.emit("after_flag_negative_delay", kp.should_terminate(-1))

# The flag is host state, not plugin state: a second instance sees it too.
t.emit("after_flag_other_instance", TerminatePlugin().should_terminate())

kp._clear_host()
t.emit("after_clear_host", kp.should_terminate())

t.done()
"##,
        &[],
    );

    run.expect_eq(
        "default_no_host",
        "False",
        "keypirinha.should_terminate() must be False when no host is installed",
    );
    run.expect_eq(
        "default_no_host_with_delay",
        "False",
        "keypirinha.should_terminate(delay) must accept the documented optional delay argument \
         and still answer False with no host installed",
    );
    run.expect_eq(
        "before_flag_module",
        "False",
        "should_terminate() must be False before the host raises the cooperative flag",
    );
    run.expect_eq(
        "before_flag_plugin",
        "False",
        "Plugin.should_terminate() must delegate to the module-level flag",
    );
    run.expect(
        "observed_inside_callback",
        "a callback already running must observe the flag flip; the shim may not snapshot it at \
         callback entry (spec 7.1 `obsolete running request -> should_terminate() becomes true`)",
    );
    run.expect_eq(
        "after_flag_module",
        "True",
        "keypirinha.should_terminate() must become True once the host raises the flag",
    );
    run.expect_eq(
        "after_flag_plugin",
        "True",
        "Plugin.should_terminate() must report the raised flag (acceptance 31.17)",
    );
    run.expect_eq(
        "after_flag_with_delay",
        "True",
        "should_terminate(delay) must report the raised flag too",
    );
    run.expect_eq(
        "after_flag_negative_delay",
        "True",
        "a non-positive delay must still poll the host; a negative delay must not hide a raised flag",
    );
    run.expect_eq(
        "after_flag_other_instance",
        "True",
        "the cooperative flag lives on the host, so every plugin instance in the worker observes it",
    );
    run.expect_eq(
        "after_clear_host",
        "False",
        "removing the host must return should_terminate() to its documented default",
    );
}

// ---------------------------------------------------------------------------
// `keypirinha`: events, settings, resources, logging
// ---------------------------------------------------------------------------

#[test]
fn keypirinha_events_mirror_the_host_event_flags_bit_for_bit() {
    let scratch = TempDir::new("events");
    let run = run_ok(
        &scratch,
        r##"
import enum

import keypirinha as kp
import _kptest as t

EXPECTED = [("APPCONFIG", 0x01), ("PACKCONFIG", 0x02), ("NETOPTIONS", 0x04),
            ("PACKAGES", 0x08), ("FILESYSTEM", 0x10), ("DESKTOP", 0x20),
            ("STARTMENU", 0x40)]

wrong = []
values = []
for member, want in EXPECTED:
    got = getattr(kp.Events, member, None)
    if got is None:
        wrong.append("keypirinha.Events." + member + " missing")
        continue
    values.append(int(got))
    if int(got) != want:
        wrong.append("keypirinha.Events." + member + "=" + hex(int(got)) + " want " + hex(want))

t.emit("wrong", ";".join(wrong) or "<none>")
t.emit("wrong_count", len(wrong))
t.emit("distinct", len(set(values)) == len(values))
t.emit("is_int_flag", issubclass(kp.Events, enum.IntFlag))

combined = kp.Events.APPCONFIG | kp.Events.FILESYSTEM
t.emit("combined_value", int(combined))
t.emit("combined_contains", bool(combined & kp.Events.APPCONFIG)
       and bool(combined & kp.Events.FILESYSTEM))
t.emit("combined_excludes", not (combined & kp.Events.DESKTOP))
t.emit("all_value", int(kp.Events.ALL))
# Unchanged plugins write the documented Keypirinha spelling `NETOPTIONS`
# (websuggest.py and friends do `flags & kp.Events.NETOPTIONS`), so that is the
# canonical member; `NETWORK` is kept as an alias for the same bit because a
# missing name is an AttributeError inside a legacy plugin.
t.emit("network_alias", int(kp.Events.NETWORK) == int(kp.Events.NETOPTIONS))


class EventPlugin(kp.Plugin):
    def __init__(self):
        kp.Plugin.__init__(self)
        self.seen = []

    def on_events(self, flags):
        self.seen.append(int(flags))


plugin = EventPlugin()
plugin.on_events(combined)
t.emit("callback_received", plugin.seen == [int(combined)])

t.done()
"##,
        &[],
    );

    assert_eq!(
        run.int("wrong_count"),
        0,
        "keypirinha.Events must mirror the host LegacyEventFlags bit for bit so the Rust and \
         Python sides never disagree about a flag, but these differ: {}\n{}",
        run.field("wrong"),
        run.describe()
    );
    run.expect("distinct", "every Events member must be a distinct bit");
    run.expect(
        "is_int_flag",
        "keypirinha.Events must be an enum.IntFlag so legacy plugins can combine and test flags \
         with the bitwise operators the documented API uses",
    );
    assert_eq!(
        run.int("combined_value"),
        0x11,
        "combining Events members with `|` must yield the OR of their bits\n{}",
        run.describe()
    );
    run.expect(
        "combined_contains",
        "a combined flag set must test positive for each member it contains",
    );
    run.expect(
        "combined_excludes",
        "a combined flag set must test negative for members it does not contain",
    );
    assert_eq!(
        run.int("all_value"),
        0x7F,
        "keypirinha.Events.ALL must be the union of every defined flag\n{}",
        run.describe()
    );
    run.expect(
        "network_alias",
        "keypirinha.Events.NETWORK must alias NETOPTIONS so both the documented spelling and the \
         descriptive one resolve to the same bit",
    );
    run.expect(
        "callback_received",
        "Plugin.on_events(flags) must receive the flag set unchanged",
    );
}

#[test]
fn plugin_settings_are_read_through_the_host_with_documented_coercion() {
    let scratch = TempDir::new("settings");
    let run = run_ok(
        &scratch,
        r##"
import keypirinha as kp
import _kptest as t

CONFIG = {
    "DEFAULT": {"greeting": "hello", "enabled": "yes", "count": "7", "ratio": "1.5"},
    "advanced": {"mode": "fast", "enabled": "OFF", "count": "not-a-number"},
}

host = t.RecordingHost(settings=CONFIG)
kp._set_host(host)


class SettingsPlugin(kp.Plugin):
    pass


settings = SettingsPlugin().load_settings()
t.emit("type_name", type(settings).__name__)

t.emit("sections", ",".join(settings.sections()))
t.emit("default_keys", ",".join(settings.keys()))
t.emit("advanced_keys", ",".join(settings.keys("advanced")))

# section=None means the documented DEFAULT section.
t.emit("get_default", settings.get("greeting"))
t.emit("get_default_explicit", settings.get("greeting", kp.Settings.DEFAULT_SECTION))
t.emit("get_sectioned", settings.get("mode", "advanced"))
# INI lookup is ASCII-case-insensitive on both sides of the host boundary: the
# Rust parser folds section and key case while reporting first-seen spellings,
# and this Python view must not disagree with it.
t.emit("get_key_case_insensitive", settings.get("GREETING"))
t.emit("get_section_case_insensitive", settings.get("mode", "ADVANCED"))
t.emit("get_missing_is_none", settings.get("nope") is None)
t.emit("get_missing_fallback", settings.get("nope", fallback="fell-back"))
t.emit("get_missing_section_fallback", settings.get("mode", "no-such-section", "fell-back"))

t.emit("has_present", settings.has("greeting"))
t.emit("has_absent", settings.has("nope"))
t.emit("has_sectioned", settings.has("mode", "advanced"))
t.emit("has_wrong_section", settings.has("mode"))
t.emit("has_key_case_insensitive", settings.has("EnAbLeD"))

t.emit("get_bool_yes", settings.get_bool("enabled"))
t.emit("get_bool_off", settings.get_bool("enabled", "advanced"))
t.emit("get_int", settings.get_int("count"))
t.emit("get_float", settings.get_float("ratio"))
t.emit("get_int_missing_fallback", settings.get_int("nope", fallback=42))
t.emit("get_bool_missing_fallback", settings.get_bool("nope", fallback=True))

typed = kp.Settings(
    {
        "DEFAULT": {
            "enable": "enabled",
            "disable": "disabled",
            "hex": "'0x10'",
            "low": "-2",
            "high": "99",
        }
    }
)
t.emit("get_bool_enabled", typed.get_bool("enable"))
t.emit("get_bool_disabled", typed.get_bool("disable"))
t.emit("get_quoted", typed.get("hex", unquote=True))
t.emit("get_hex", typed.get_int("hex"))
t.emit("get_low_clamped", typed.get_int("low", min=0))
t.emit("get_high_clamped", typed.get_int("high", max=10))


def coercion_failure(call):
    try:
        return "value:" + repr(call())
    except kp.SettingsError as exc:
        return "SettingsError:" + str(exc)
    except Exception as exc:  # noqa: BLE001
        return "wrong-error:" + type(exc).__name__


t.emit("bad_int", coercion_failure(lambda: settings.get_int("count", "advanced")))
t.emit("bad_bool", coercion_failure(lambda: settings.get_bool("mode", "advanced")))
# A supplied fallback wins over a typed failure: unchanged plugins rely on this.
t.emit("bad_int_with_fallback", settings.get_int("count", "advanced", fallback=99))
t.emit("settings_error_is_value_error", issubclass(kp.SettingsError, ValueError))
t.emit("settings_error_is_keypirinha_error", issubclass(kp.SettingsError, kp.KeypirinhaError))

# load_settings() is an optional host capability.
kp._set_host(t.MinimalHost())
try:
    SettingsPlugin().load_settings()
except kp.HostUnavailableError as exc:
    t.emit("no_host_error", "HostUnavailableError:" + str(getattr(exc, "operation", "<none>")))
except Exception as exc:  # noqa: BLE001
    t.emit("no_host_error", "wrong-error:" + type(exc).__name__)
else:
    t.emit("no_host_error", "silently-succeeded")

t.done()
"##,
        &[],
    );

    run.expect_eq(
        "type_name",
        "Settings",
        "Plugin.load_settings() must return a keypirinha.Settings",
    );
    run.expect_eq(
        "sections",
        "DEFAULT,advanced",
        "Settings.sections() must list every section, sorted, so the result is deterministic",
    );
    run.expect_eq(
        "default_keys",
        "count,enabled,greeting,ratio",
        "Settings.keys() must default to the DEFAULT section and return sorted keys",
    );
    run.expect_eq(
        "advanced_keys",
        "count,enabled,mode",
        "Settings.keys(section) must return that section's sorted keys",
    );
    run.expect_eq(
        "get_default",
        "hello",
        "Settings.get(key) must read the DEFAULT section when no section is given",
    );
    run.expect_eq(
        "get_default_explicit",
        "hello",
        "Settings.get(key, DEFAULT_SECTION) must be equivalent to omitting the section",
    );
    run.expect_eq(
        "get_key_case_insensitive",
        "hello",
        "key lookup must be ASCII-case-insensitive, matching the Rust-side config parser and the \
         Keypirinha INI convention",
    );
    run.expect_eq(
        "get_section_case_insensitive",
        "fast",
        "section lookup must be ASCII-case-insensitive, matching the Rust-side config parser",
    );
    run.expect_eq(
        "get_sectioned",
        "fast",
        "Settings.get(key, section) must read that section",
    );
    run.expect(
        "get_missing_is_none",
        "a missing key with no fallback must return None, not raise",
    );
    run.expect_eq(
        "get_missing_fallback",
        "fell-back",
        "a missing key must return the supplied fallback",
    );
    run.expect_eq(
        "get_missing_section_fallback",
        "fell-back",
        "an unknown section must return the supplied fallback rather than raising",
    );
    run.expect("has_present", "Settings.has() must find a present key");
    run.expect_eq(
        "has_absent",
        "False",
        "Settings.has() must report False for an absent key",
    );
    run.expect(
        "has_sectioned",
        "Settings.has(key, section) must find a key in that section",
    );
    run.expect(
        "has_key_case_insensitive",
        "Settings.has() must fold key case too, or a plugin would conclude a key is absent when \
         the host can read it perfectly well",
    );
    run.expect_eq(
        "has_wrong_section",
        "False",
        "Settings.has() must not find a key that only exists in another section",
    );
    run.expect(
        "get_bool_yes",
        "get_bool must accept `yes` as true (documented Keypirinha config spelling)",
    );
    run.expect_eq(
        "get_bool_off",
        "False",
        "get_bool must accept `OFF` as false, case-insensitively",
    );
    assert_eq!(
        run.int("get_int"),
        7,
        "get_int must coerce a decimal string\n{}",
        run.describe()
    );
    run.expect_eq(
        "get_float",
        "1.5",
        "get_float must coerce a decimal string to a float",
    );
    assert_eq!(
        run.int("get_int_missing_fallback"),
        42,
        "get_int must return the fallback for a missing key\n{}",
        run.describe()
    );
    run.expect(
        "get_bool_missing_fallback",
        "get_bool must return the fallback for a missing key",
    );
    run.expect("get_bool_enabled", "get_bool must accept `enabled` as true");
    run.expect_eq(
        "get_bool_disabled",
        "False",
        "get_bool must accept `disabled` as false",
    );
    run.expect_eq(
        "get_quoted",
        "0x10",
        "get(..., unquote=True) must remove matching surrounding quotes",
    );
    assert_eq!(
        run.int("get_hex"),
        16,
        "get_int must parse hexadecimal values with base zero\n{}",
        run.describe()
    );
    assert_eq!(
        run.int("get_low_clamped"),
        0,
        "get_int must clamp below-minimum values\n{}",
        run.describe()
    );
    assert_eq!(
        run.int("get_high_clamped"),
        10,
        "get_int must clamp above-maximum values\n{}",
        run.describe()
    );
    assert!(
        run.field("bad_int").starts_with("SettingsError:"),
        "an uncoercible value with no fallback must raise keypirinha.SettingsError, not return a \
         silently wrong value; got `{}`\n{}",
        run.field("bad_int"),
        run.describe()
    );
    assert!(
        run.field("bad_bool").starts_with("SettingsError:"),
        "an unrecognised boolean spelling with no fallback must raise keypirinha.SettingsError; \
         got `{}`\n{}",
        run.field("bad_bool"),
        run.describe()
    );
    assert_eq!(
        run.int("bad_int_with_fallback"),
        99,
        "a supplied fallback must win over a coercion failure so unchanged plugins keep working\n{}",
        run.describe()
    );
    run.expect(
        "settings_error_is_value_error",
        "keypirinha.SettingsError must subclass ValueError",
    );
    run.expect(
        "settings_error_is_keypirinha_error",
        "keypirinha.SettingsError must subclass keypirinha.KeypirinhaError",
    );
    run.expect_eq(
        "no_host_error",
        "HostUnavailableError:load_settings",
        "settings access without a host capable of it must raise the typed HostUnavailableError \
         naming the operation (spec 14.12, 26.2)",
    );
}

#[test]
fn package_resources_round_trip_as_text_and_bytes_through_the_host() {
    let scratch = TempDir::new("resources");
    let run = run_ok(
        &scratch,
        r##"
import keypirinha as kp
import _kptest as t

RESOURCES = {
    "greeting.txt": "h\u00e9llo r\u00e9sources\n".encode("utf-8"),
    "blob.bin": bytes([0x00, 0x01, 0x02, 0xFF]),
}

host = t.RecordingHost(resources=RESOURCES,
                       package_path="/crikey-test/packages/Example",
                       cache_path="/crikey-test/cache/Example")
kp._set_host(host)


class ResourcePlugin(kp.Plugin):
    pass


plugin = ResourcePlugin()

text = plugin.load_text_resource("greeting.txt")
t.emit("text_is_str", isinstance(text, str))
t.emit("text_value", repr(text))

blob = plugin.load_binary_resource("blob.bin")
t.emit("blob_is_bytes", isinstance(blob, bytes))
t.emit("blob_value", ",".join(str(byte) for byte in blob))

# A binary resource must not be decoded, and a text resource must be decoded
# exactly once: reading the same name both ways must agree.
t.emit("text_matches_bytes",
       plugin.load_binary_resource("greeting.txt") == text.encode("utf-8"))

t.emit("package_full_path", plugin.package_full_path())
t.emit("cache_path", plugin.get_package_cache_path())
t.emit("cache_path_create", plugin.get_package_cache_path(True))
t.emit("cache_create_requests", ",".join(str(flag) for flag in host.cache_path_requests))

kp._set_host(t.MinimalHost())


def unavailable(call):
    try:
        call()
    except kp.HostUnavailableError as exc:
        return "HostUnavailableError:" + str(getattr(exc, "operation", "<none>"))
    except Exception as exc:  # noqa: BLE001
        return "wrong-error:" + type(exc).__name__
    return "silently-succeeded"


t.emit("no_host_text", unavailable(lambda: plugin.load_text_resource("greeting.txt")))
t.emit("no_host_binary", unavailable(lambda: plugin.load_binary_resource("blob.bin")))
t.emit("no_host_package_path", unavailable(plugin.package_full_path))
t.emit("no_host_cache_path", unavailable(plugin.get_package_cache_path))

t.done()
"##,
        &[],
    );

    run.expect("text_is_str", "Plugin.load_text_resource() must return str");
    run.expect_eq(
        "text_value",
        "'héllo résources\\n'",
        "load_text_resource() must decode the resource as UTF-8 without mangling non-ASCII text \
         or rewriting line endings",
    );
    run.expect("blob_is_bytes", "Plugin.load_binary_resource() must return bytes");
    run.expect_eq(
        "blob_value",
        "0,1,2,255",
        "load_binary_resource() must return the bytes verbatim, including a non-UTF-8 byte",
    );
    run.expect(
        "text_matches_bytes",
        "reading one resource as text and as bytes must agree; the shim must decode exactly once",
    );
    run.expect_eq(
        "package_full_path",
        "/crikey-test/packages/Example",
        "Plugin.package_full_path() must report the host's package path",
    );
    run.expect_eq(
        "cache_path",
        "/crikey-test/cache/Example",
        "Plugin.get_package_cache_path() must report the host's cache path",
    );
    run.expect_eq(
        "cache_create_requests",
        "False,True",
        "get_package_cache_path() must default create to False and pass an explicit True through, \
         so a plugin never creates directories by accident",
    );
    run.expect_eq(
        "no_host_text",
        "HostUnavailableError:load_resource",
        "resource access without a capable host must raise the typed error naming the operation",
    );
    run.expect_eq(
        "no_host_binary",
        "HostUnavailableError:load_resource",
        "resource access without a capable host must raise the typed error naming the operation",
    );
    run.expect_eq(
        "no_host_package_path",
        "HostUnavailableError:package_full_path",
        "package path access without a capable host must raise the typed error naming the operation",
    );
    run.expect_eq(
        "no_host_cache_path",
        "HostUnavailableError:package_cache_path",
        "cache path access without a capable host must raise the typed error naming the operation",
    );
}

#[test]
fn logging_helpers_write_to_stderr_and_never_to_the_stdout_protocol_channel() {
    let scratch = TempDir::new("logging");
    let run = run_ok(
        &scratch,
        r##"
import keypirinha as kp
import _kptest as t


class LogPlugin(kp.Plugin):
    pass


plugin = LogPlugin()
plugin.info("informational", 42)
plugin.warn("warned")
plugin.err("failed badly")
plugin.dbg("debug detail")

t.emit("logged", True)
t.done()
"##,
        &[],
    );

    run.expect("logged", "the logging helpers must not raise");

    let stderr_lines: Vec<&str> = run.stderr.lines().map(str::trim_end).collect();
    for expected in [
        "[info][LogPlugin] informational 42",
        "[warn][LogPlugin] warned",
        "[err][LogPlugin] failed badly",
        "[dbg][LogPlugin] debug detail",
    ] {
        assert!(
            stderr_lines.contains(&expected),
            "expected the line `{expected}` on stderr: legacy logging is `[level][friendly_name] \
             message` with arguments joined by single spaces, and stderr is the plugin log channel \
             (spec 14.4, 26.1)\n{}",
            run.describe()
        );
    }
    assert!(
        !run.stdout.contains("informational")
            && !run.stdout.contains("warned")
            && !run.stdout.contains("failed badly")
            && !run.stdout.contains("debug detail"),
        "plugin log output must never reach stdout: stdout is the strict newline-delimited JSON \
         protocol channel and any stray byte corrupts the stream\n{}",
        run.describe()
    );
}

// ---------------------------------------------------------------------------
// `keypirinha_util`
// ---------------------------------------------------------------------------

#[test]
fn keypirinha_util_splits_and_quotes_command_lines_with_win32_escaping_rules() {
    let scratch = TempDir::new("cmdline");
    let run = run_ok(
        &scratch,
        r##"
import keypirinha_util as kpu
import _kptest as t

# Composed from chr() so the Rust source, the Python source and the intent
# cannot drift apart through layered escaping.
BS = chr(92)   # backslash
Q = chr(34)    # double quote
TAB = chr(9)

failures = []


def check(name, got, want):
    if got != want:
        failures.append(name + ": got " + repr(got) + " want " + repr(want))


# Splitting follows CommandLineToArgvW, which is what unchanged Keypirinha
# plugins were written against. It is a pure string function, so it behaves
# identically on every host platform.
check("empty", kpu.cmdline_split(""), [])
check("blank", kpu.cmdline_split("   "), [])
check("simple", kpu.cmdline_split("foo bar"), ["foo", "bar"])
check("runs_of_space", kpu.cmdline_split("  foo   bar  "), ["foo", "bar"])
check("tab_separates", kpu.cmdline_split("foo" + TAB + "bar"), ["foo", "bar"])
check("quoted_group", kpu.cmdline_split('foo "bar baz" qux'), ["foo", "bar baz", "qux"])
check("empty_quoted", kpu.cmdline_split('"" x'), ["", "x"])
check("escaped_quote", kpu.cmdline_split(Q + "a" + BS + Q + "b" + Q), ['a' + Q + 'b'])
check("literal_backslashes", kpu.cmdline_split("a" + BS + BS + "b"), ["a" + BS + BS + "b"])
check("doubled_backslash_before_quote",
      kpu.cmdline_split(Q + "a" + BS + BS + Q), ["a" + BS])
check("adjacent_quoted", kpu.cmdline_split('a"b c"d'), ["ab cd"])

# Quoting is the inverse. Arguments are quoted only when they must be.
check("quote_plain", kpu.cmdline_quote("plain"), "plain")
check("quote_force", kpu.cmdline_quote("plain", True), Q + "plain" + Q)
check("quote_force_tuple", kpu.cmdline_quote(("a", "b"), True),
      Q + "a" + Q + " " + Q + "b" + Q)

type_failures = []
for bad in ({}, ["ok", 1]):
    try:
        kpu.cmdline_quote(bad)
    except TypeError:
        continue
    type_failures.append(repr(bad))
t.emit("type_failures", ";".join(type_failures) or "<none>")
check("quote_space", kpu.cmdline_quote("has space"), Q + "has space" + Q)
check("quote_tab", kpu.cmdline_quote("has" + TAB + "tab"), Q + "has" + TAB + "tab" + Q)
check("quote_empty", kpu.cmdline_quote(""), Q + Q)
check("quote_bare_backslash", kpu.cmdline_quote("back" + BS + "slash"), "back" + BS + "slash")
check("quote_trailing_backslash_with_space",
      kpu.cmdline_quote("has space" + BS), Q + "has space" + BS + BS + Q)
check("quote_embedded_quote",
      kpu.cmdline_quote("a" + Q + "b"), Q + "a" + BS + Q + "b" + Q)
check("quote_list", kpu.cmdline_quote(["a", "b c"]), "a " + Q + "b c" + Q)
check("quote_empty_list", kpu.cmdline_quote([]), "")

t.emit("exact_failures", ";".join(failures) or "<none>")
t.emit("exact_failure_count", len(failures))

# The property that actually matters: quoting then splitting is the identity,
# including for arguments built entirely out of the escaping edge cases.
NASTY = [
    ["plain"],
    ["has space"],
    [""],
    ["a" + Q + "b"],
    ["trailing" + BS],
    ["has space and trailing" + BS],
    ["a" + BS + BS + "b"],
    [BS + Q + "escaped"],
    ["has" + TAB + "tab"],
    ["multi", "args here", Q + "quoted" + Q, "C:" + BS + "Program Files" + BS],
    ["--flag=value with space", "-x", ""],
]
broken = []
for args in NASTY:
    line = kpu.cmdline_quote(args)
    back = kpu.cmdline_split(line)
    if back != args:
        broken.append(repr(args) + " -> " + repr(line) + " -> " + repr(back))

t.emit("roundtrip_failures", ";".join(broken) or "<none>")
t.emit("roundtrip_failure_count", len(broken))
t.emit("roundtrip_cases", len(NASTY))

t.done()
"##,
        &[],
    );

    assert_eq!(
        run.int("exact_failure_count"),
        0,
        "keypirinha_util command-line helpers must implement the documented Win32 \
         (CommandLineToArgvW) quoting rules exactly, on every platform, because they are pure \
         string functions unchanged plugins depend on. Mismatches: {}\n{}",
        run.field("exact_failures"),
        run.describe()
    );
    run.expect_eq(
        "type_failures",
        "<none>",
        "cmdline_quote must reject non-string arguments instead of iterating or coercing them",
    );
    assert_eq!(
        run.int("roundtrip_cases"),
        11,
        "the escaping round-trip must cover every pinned edge case\n{}",
        run.describe()
    );
    assert_eq!(
        run.int("roundtrip_failure_count"),
        0,
        "cmdline_split(cmdline_quote(args)) must be the identity for every argument vector, \
         including empty strings, embedded quotes and trailing backslashes. Broken: {}\n{}",
        run.field("roundtrip_failures"),
        run.describe()
    );
}

#[test]
fn keypirinha_util_expands_environment_variables_and_leaves_unknown_ones_verbatim() {
    let scratch = TempDir::new("expand");
    let run = run_ok(
        &scratch,
        r##"
import keypirinha_util as kpu
import _kptest as t

failures = []


def check(name, got, want):
    if got != want:
        failures.append(name + ": got " + repr(got) + " want " + repr(want))


check("simple", kpu.expand_variables("%CRIKEY_TEST_VAR%"), "expanded-value")
check("embedded", kpu.expand_variables("prefix/%CRIKEY_TEST_VAR%/suffix"),
      "prefix/expanded-value/suffix")
check("twice", kpu.expand_variables("%CRIKEY_TEST_VAR%%CRIKEY_TEST_VAR%"),
      "expanded-valueexpanded-value")
# Windows environment lookup is case-insensitive; unchanged plugins rely on it.
check("case_insensitive", kpu.expand_variables("%crikey_test_var%"), "expanded-value")
# An unknown variable is left verbatim rather than expanded to an empty string:
# a silently emptied path is far worse to debug than a visible marker.
check("unknown_verbatim", kpu.expand_variables("%CRIKEY_NO_SUCH_VAR%"), "%CRIKEY_NO_SUCH_VAR%")
check("unterminated", kpu.expand_variables("%unterminated"), "%unterminated")
check("escaped_percent", kpu.expand_variables("100%% done"), "100% done")
check("no_markers", kpu.expand_variables("no markers here"), "no markers here")
check("empty", kpu.expand_variables(""), "")
check("custom_wins", kpu.expand_variables("%CRIKEY_TEST_VAR%",
                                          custom_vars={"CRIKEY_TEST_VAR": "overridden"}),
      "overridden")
check("custom_only", kpu.expand_variables("%ONLY_CUSTOM%",
                                          custom_vars={"ONLY_CUSTOM": "from-custom"}),
      "from-custom")
check("custom_falls_through", kpu.expand_variables("%CRIKEY_TEST_VAR%",
                                                   custom_vars={"OTHER": "x"}),
      "expanded-value")

t.emit("failures", ";".join(failures) or "<none>")
t.emit("failure_count", len(failures))
t.done()
"##,
        &[("CRIKEY_TEST_VAR", "expanded-value")],
    );

    assert_eq!(
        run.int("failure_count"),
        0,
        "keypirinha_util.expand_variables must implement Windows `%VAR%` expansion with \
         case-insensitive lookup, `%%` escaping, verbatim pass-through of unknown variables, and \
         custom_vars taking precedence over the environment. Mismatches: {}\n{}",
        run.field("failures"),
        run.describe()
    );
}

#[test]
fn keypirinha_util_scans_directories_with_documented_flag_and_depth_semantics() {
    let scratch = TempDir::new("scandir");
    let root = scratch.mkdir("tree");
    scratch.write("tree/alpha.txt", "a");
    scratch.write("tree/beta.log", "b");
    scratch.write("tree/.hidden.txt", "hidden");
    scratch.write("tree/sub/gamma.txt", "c");
    scratch.write("tree/sub/deep/delta.txt", "d");

    let run = run_ok(
        &scratch,
        r##"
import os

import keypirinha_util as kpu
import _kptest as t

ROOT = os.environ["CRIKEY_TEST_SCAN_ROOT"]
FILES = kpu.ScanFlags.FILES
FOLDERS = kpu.ScanFlags.FOLDERS
HIDDEN = kpu.ScanFlags.HIDDEN
RECURSIVE = kpu.ScanFlags.RECURSIVE


def scan(*args, **kwargs):
    return ";".join(kpu.scan_directory(ROOT, *args, **kwargs))


t.emit("files_only", scan("*", FILES))
t.emit("folders_only", scan("*", FOLDERS))
t.emit("default_flags", scan())
t.emit("hidden_default", scan("*.txt", FILES))
t.emit("hidden_included", scan("*.txt", FILES | HIDDEN))
t.emit("max_level_without_flag", scan("*.txt", FILES, 1))
t.emit("pattern_filtered", scan("*.txt", FILES))
t.emit("recursive_txt", scan("*.txt", FILES | RECURSIVE))
t.emit("recursive_depth_limited", scan("*.txt", FILES | RECURSIVE, 1))
t.emit("recursive_folders", scan("*", FOLDERS | RECURSIVE))
t.emit("no_match", scan("*.nothing", FILES))

# Results are relative to the base directory and use the host separator, so a
# plugin can join them back onto the base without guessing.
results = kpu.scan_directory(ROOT, "*.txt", FILES | RECURSIVE)
t.emit("all_relative", all(not os.path.isabs(entry) for entry in results))
t.emit("all_exist", all(os.path.exists(os.path.join(ROOT, entry)) for entry in results))
t.emit("sorted", results == sorted(results))
t.emit("separator_ok", all(chr(92) not in entry for entry in results) if os.sep == "/" else True)

t.emit("default_is_files_and_folders",
       int(kpu.ScanFlags.DEFAULT) == int(FILES | FOLDERS))
t.emit("flags_distinct", len({int(FILES), int(FOLDERS), int(RECURSIVE)}) == 3)

t.done()
"##,
        &[("CRIKEY_TEST_SCAN_ROOT", &root.display().to_string())],
    );

    run.expect_eq(
        "files_only",
        "alpha.txt;beta.log",
        "ScanFlags.FILES must return only the top-level files, sorted",
    );
    run.expect_eq(
        "folders_only",
        "sub",
        "ScanFlags.FOLDERS must return only directories",
    );
    run.expect_eq(
        "default_flags",
        "alpha.txt;beta.log;sub",
        "scan_directory must default to ScanFlags.DEFAULT (files and folders) and pattern `*`, \
         non-recursively",
    );
    run.expect_eq(
        "hidden_default",
        "alpha.txt",
        "hidden entries must stay excluded unless ScanFlags.HIDDEN is set",
    );
    run.expect_eq(
        "hidden_included",
        ".hidden.txt;alpha.txt",
        "ScanFlags.HIDDEN must include dot-hidden entries",
    );
    run.expect_eq(
        "max_level_without_flag",
        "alpha.txt;sub/gamma.txt",
        "max_level must control recursion without a private RECURSIVE flag",
    );
    run.expect_eq(
        "pattern_filtered",
        "alpha.txt",
        "the name pattern must filter by the entry name",
    );
    run.expect_eq(
        "recursive_txt",
        "alpha.txt;sub/deep/delta.txt;sub/gamma.txt",
        "ScanFlags.RECURSIVE must descend the whole tree and report package-relative paths, sorted",
    );
    run.expect_eq(
        "recursive_depth_limited",
        "alpha.txt;sub/gamma.txt",
        "max_level bounds how many directory levels below the base are descended, so max_level=1 \
         reaches `sub` but not `sub/deep`",
    );
    run.expect_eq(
        "recursive_folders",
        "sub;sub/deep",
        "a recursive folder scan must report nested directories too",
    );
    run.expect_eq(
        "no_match",
        "",
        "a pattern matching nothing must return an empty list, not raise",
    );
    run.expect(
        "all_relative",
        "scan_directory must return paths relative to the base directory",
    );
    run.expect(
        "all_exist",
        "every returned path must join back onto the base to an existing entry",
    );
    run.expect("sorted", "scan_directory results must be sorted for determinism");
    run.expect("separator_ok", "returned paths must use the host path separator");
    run.expect(
        "default_is_files_and_folders",
        "ScanFlags.DEFAULT must be FILES | FOLDERS",
    );
    run.expect("flags_distinct", "ScanFlags members must be distinct bits");
}

#[test]
fn keypirinha_util_desktop_helpers_report_honest_unavailability_on_a_headless_host() {
    let scratch = TempDir::new("desktop");
    let run = run_ok(
        &scratch,
        r##"
import os
import sys

import keypirinha_util as kpu
import _kptest as t

# The child environment is built from scratch, so this is guaranteed, not
# incidental: there is no display variable for these helpers to reach.
t.emit("headless", "DISPLAY" not in os.environ and "WAYLAND_DISPLAY" not in os.environ)
# Windows and macOS always have a desktop; only the POSIX-with-no-display case
# is what the shim calls unavailable. The helpers below are therefore only run
# where they must refuse, because running them where they must work would open
# a browser and touch the clipboard of whoever is running the suite.
t.emit("desktop_available", kpu.desktop_available())

t.emit("unavailable_is_runtime_error", issubclass(kpu.UnavailableError, RuntimeError))
# Deliberately NOT an AttributeError or OSError: those are routinely swallowed
# by plugins and by hasattr/getattr, which would turn an honest "cannot" into a
# silent no-op.
t.emit("unavailable_not_attribute_error", not issubclass(kpu.UnavailableError, AttributeError))
t.emit("unavailable_not_os_error", not issubclass(kpu.UnavailableError, OSError))

OPERATIONS = [
    ("set_clipboard", lambda: kpu.set_clipboard("crikey")),
    ("get_clipboard", lambda: kpu.get_clipboard()),
    ("open_url", lambda: kpu.open_url("https://example.invalid/")),
    ("shell_execute", lambda: kpu.shell_execute("/bin/true")),
    ("explore_file", lambda: kpu.explore_file(os.getcwd())),
]

dishonest = []
for name, call in OPERATIONS if not kpu.desktop_available() else []:
    try:
        call()
    except kpu.UnavailableError as exc:
        operation = getattr(exc, "operation", None)
        platform = getattr(exc, "platform", None)
        reason = getattr(exc, "reason", None)
        message = str(exc)
        if operation != name:
            dishonest.append(name + ": operation=" + repr(operation))
        if platform != sys.platform:
            dishonest.append(name + ": platform=" + repr(platform))
        if not reason:
            dishonest.append(name + ": empty reason")
        if name not in message or sys.platform not in message:
            dishonest.append(name + ": message does not name operation and platform: " + message)
    except Exception as exc:  # noqa: BLE001
        dishonest.append(name + ": wrong error " + type(exc).__name__ + ": " + str(exc))
    else:
        dishonest.append(name + ": pretended to succeed")

t.emit("dishonest", ";".join(dishonest) or "<none>")
t.emit("dishonest_count", len(dishonest))
t.emit("operations_checked", len(OPERATIONS) if not kpu.desktop_available() else 0)
t.done()
"##,
        &[],
    );

    run.expect(
        "headless",
        "the child environment must be built from scratch so no desktop session is reachable; \
         otherwise this contract is not deterministic",
    );
    // What "available" means is platform contract, not environment: the shim
    // answers True on Windows and macOS whatever the environment says, because
    // a desktop session is not optional there, and on POSIX it answers for the
    // display this child was deliberately given none of.
    let desktop_is_a_given = cfg!(windows) || cfg!(target_os = "macos");
    run.expect_eq(
        "desktop_available",
        if desktop_is_a_given { "True" } else { "False" },
        "keypirinha_util.desktop_available() must answer for the platform it is on: a given on \
         Windows and macOS, and False on a POSIX host with no display so the diagnostics layer \
         can classify the plugin without provoking an exception",
    );
    run.expect(
        "unavailable_is_runtime_error",
        "keypirinha_util.UnavailableError must subclass RuntimeError",
    );
    run.expect(
        "unavailable_not_attribute_error",
        "UnavailableError must NOT subclass AttributeError: hasattr/getattr would swallow it and \
         turn an honest failure into a silent no-op",
    );
    run.expect(
        "unavailable_not_os_error",
        "UnavailableError must NOT subclass OSError: plugins routinely catch OSError around file \
         and process work and would mask the diagnostic",
    );
    assert_eq!(
        run.int("operations_checked"),
        if desktop_is_a_given { 0 } else { 5 },
        "every desktop-touching helper must be exercised where it must refuse, and none where it \
         would really open a browser or take the clipboard\n{}",
        run.describe()
    );
    assert_eq!(
        run.int("dishonest_count"),
        0,
        "on a headless host every desktop-touching helper must raise \
         keypirinha_util.UnavailableError carrying .operation, .platform and .reason, with a \
         message naming both the operation and the platform, rather than pretending to succeed \
         (spec 26.2 windows-only/unavailable dependency reporting). Problems: {}\n{}",
        run.field("dishonest"),
        run.describe()
    );
}

// ---------------------------------------------------------------------------
// `keypirinha_net`
// ---------------------------------------------------------------------------

#[test]
fn keypirinha_net_builds_requests_without_performing_any_network_io() {
    let scratch = TempDir::new("net");
    let run = run_ok(
        &scratch,
        r##"
import socket
import urllib.request

import _kptest as t


def forbidden(*args, **kwargs):
    raise AssertionError("keypirinha_net performed network I/O during a pure request build")


# Poison every route to the network *before* the module is imported, so import
# time is covered too. Building a request is a pure operation; if any of these
# fire, the test fails loudly instead of quietly reaching the internet.
socket.socket = forbidden
socket.create_connection = forbidden
socket.getaddrinfo = forbidden
urllib.request.OpenerDirector.open = forbidden
urllib.request.urlopen = forbidden

import keypirinha_net as kpnet  # noqa: E402 - deliberately imported after poisoning

t.emit("default_timeout", kpnet.DEFAULT_TIMEOUT)
t.emit("default_timeout_is_number", isinstance(kpnet.DEFAULT_TIMEOUT, (int, float)))

agent = kpnet.user_agent()
t.emit("user_agent", agent)
t.emit("user_agent_is_crikey", agent.startswith("CriKey/"))
t.emit("user_agent_not_keypirinha", "Keypirinha" not in agent)

request = kpnet.build_request("https://example.invalid/path?q=1")
t.emit("request_type", type(request).__name__)
t.emit("url", request.url)
t.emit("timeout", request.timeout)
t.emit("timeout_is_default", request.timeout == kpnet.DEFAULT_TIMEOUT)
t.emit("request_user_agent", request.user_agent)
t.emit("header_user_agent", request.headers["User-Agent"])
t.emit("get_header_lowercase", request.get_header("user-agent"))
t.emit("get_header_mixed", request.get_header("UsEr-AgEnT"))
t.emit("get_header_missing", repr(request.get_header("X-Absent")))
t.emit("get_header_missing_default", request.get_header("X-Absent", "fallback"))

custom = kpnet.build_request(
    "http://example.invalid/",
    headers={"x-test": "1", "user-agent": "Custom/9"},
    timeout=2.5)
t.emit("custom_timeout", custom.timeout)
t.emit("custom_user_agent", custom.user_agent)
t.emit("custom_header_user_agent", custom.headers["User-Agent"])
t.emit("custom_header_normalised", custom.headers["X-Test"])
t.emit("custom_scheme_http_ok", custom.url.startswith("http://"))

explicit = kpnet.build_request("https://example.invalid/", user_agent="Explicit/1")
t.emit("explicit_user_agent", explicit.user_agent)

failures = []
for bad in ["ftp://example.invalid/", "file:///etc/passwd", "not-a-url", "", "javascript:alert(1)"]:
    try:
        kpnet.build_request(bad)
    except kpnet.InvalidUrlError:
        continue
    except Exception as exc:  # noqa: BLE001
        failures.append(repr(bad) + ": wrong error " + type(exc).__name__)
    else:
        failures.append(repr(bad) + ": accepted")
t.emit("bad_url_failures", ";".join(failures) or "<none>")
t.emit("bad_url_failure_count", len(failures))
t.emit("invalid_url_is_value_error", issubclass(kpnet.InvalidUrlError, ValueError))

opener = kpnet.build_urllib_opener()
t.emit("opener_type_ok", isinstance(opener, urllib.request.OpenerDirector))
t.emit("opener_has_user_agent",
       any(name.lower() == "user-agent" and value == agent for name, value in opener.addheaders))

t.emit("no_network_io", True)
t.done()
"##,
        &[],
    );

    run.expect(
        "no_network_io",
        "building requests must not touch the network; sockets were poisoned before import",
    );
    run.expect(
        "default_timeout_is_number",
        "keypirinha_net.DEFAULT_TIMEOUT must be a number of seconds",
    );
    run.expect_eq(
        "default_timeout",
        "10.0",
        "keypirinha_net.DEFAULT_TIMEOUT pins the default request timeout at 10 seconds",
    );
    run.expect(
        "user_agent_is_crikey",
        "keypirinha_net.user_agent() must identify CriKey as the client",
    );
    run.expect(
        "user_agent_not_keypirinha",
        "spec 14.13 forbids presenting the layer as a Keypirinha component, so the user agent must \
         not carry the Keypirinha product name",
    );
    run.expect_eq(
        "request_type",
        "Request",
        "keypirinha_net.build_request() must return a keypirinha_net.Request",
    );
    run.expect_eq(
        "url",
        "https://example.invalid/path?q=1",
        "Request.url must round-trip the URL verbatim, query string included",
    );
    run.expect(
        "timeout_is_default",
        "an omitted timeout must fall back to DEFAULT_TIMEOUT",
    );
    let agent = run.field("user_agent");
    run.expect_eq(
        "request_user_agent",
        &agent,
        "Request.user_agent must default to keypirinha_net.user_agent()",
    );
    run.expect_eq(
        "header_user_agent",
        &agent,
        "the default user agent must also appear in Request.headers under the canonical \
         `User-Agent` spelling",
    );
    run.expect_eq(
        "get_header_lowercase",
        &agent,
        "Request.get_header() must be case-insensitive",
    );
    run.expect_eq(
        "get_header_mixed",
        &agent,
        "Request.get_header() must be case-insensitive for any mixed spelling",
    );
    run.expect_eq(
        "get_header_missing",
        "None",
        "Request.get_header() must return None for an absent header",
    );
    run.expect_eq(
        "get_header_missing_default",
        "fallback",
        "Request.get_header() must return the supplied default for an absent header",
    );
    run.expect_eq("custom_timeout", "2.5", "an explicit timeout must be preserved");
    run.expect_eq(
        "custom_user_agent",
        "Custom/9",
        "a `user-agent` supplied through headers must win over the default and be reflected in \
         Request.user_agent",
    );
    run.expect_eq(
        "custom_header_user_agent",
        "Custom/9",
        "the overriding user agent must also be the value stored under `User-Agent`",
    );
    run.expect_eq(
        "custom_header_normalised",
        "1",
        "header names must be normalised to canonical HTTP casing, so `x-test` is readable as \
         `X-Test`",
    );
    run.expect(
        "custom_scheme_http_ok",
        "plain http must be accepted as well as https",
    );
    run.expect_eq(
        "explicit_user_agent",
        "Explicit/1",
        "the explicit user_agent parameter must win over the default",
    );
    assert_eq!(
        run.int("bad_url_failure_count"),
        0,
        "build_request must reject non-http(s) and malformed URLs with keypirinha_net.InvalidUrlError \
         rather than constructing a request that would later fail deep inside urllib. Problems: {}\n{}",
        run.field("bad_url_failures"),
        run.describe()
    );
    run.expect(
        "invalid_url_is_value_error",
        "keypirinha_net.InvalidUrlError must subclass ValueError",
    );
    run.expect(
        "opener_type_ok",
        "build_urllib_opener() must return a urllib.request.OpenerDirector so unchanged plugins can \
         use it as they always have",
    );
    run.expect(
        "opener_has_user_agent",
        "the opener must carry the CriKey user agent in addheaders",
    );
}

// ---------------------------------------------------------------------------
// `keypirinha_net` policy (spec 14.2, 14.12)
// ---------------------------------------------------------------------------

#[test]
fn keypirinha_net_applies_proxy_agent_timeout_and_redirect_policy() {
    let scratch = TempDir::new("net-policy");
    let run = run_ok(
        &scratch,
        r##"
import urllib.request
import ssl

import keypirinha_net as kpnet
import _kptest as t


class Response:
    code = 200
    msg = "OK"

    def info(self):
        return {}

    def geturl(self):
        return "http://example.invalid/"


class Probe(urllib.request.BaseHandler):
    handler_order = 100

    def http_open(self, request):
        t.emit("applied_timeout", request.timeout)
        return Response()


opener = kpnet.build_urllib_opener(
    proxies={"http": "http://proxy.invalid:8080"},
    extra_handlers=(Probe(),),
    agent="Custom/9",
)
t.emit("proxy_applied", any(
    isinstance(handler, urllib.request.ProxyHandler)
    and handler.proxies.get("http") == "http://proxy.invalid:8080"
    for handler in opener.handlers
))
t.emit("agent_applied", opener.addheaders == [("User-Agent", "Custom/9")])
t.emit("probe_response", opener.open("http://example.invalid/").code)

request = urllib.request.Request("http://example.invalid/")
try:
    kpnet._SafeRedirectHandler().redirect_request(
        request, None, 302, "Found", {}, "file:///etc/passwd"
    )
except kpnet.InvalidUrlError:
    t.emit("unsafe_redirect_rejected", True)
else:
    t.emit("unsafe_redirect_rejected", False)

https_request = urllib.request.Request("https://example.invalid/")
try:
    kpnet._SafeRedirectHandler().redirect_request(
        https_request, None, 302, "Found", {}, "http://example.invalid/"
    )
except kpnet.InvalidUrlError:
    t.emit("https_downgrade_rejected", True)
else:
    t.emit("https_downgrade_rejected", False)

tls_opener = kpnet.build_urllib_opener(ssl_check_hostname=False)
tls_context = next(
    getattr(handler, "_context")
    for handler in tls_opener.handlers
    if getattr(handler, "_context", None) is not None
)
t.emit("tls_hostname_can_be_disabled", tls_context.check_hostname is False)
t.emit("tls_chain_verification_kept", tls_context.verify_mode == ssl.CERT_REQUIRED)

t.done()
"##,
        &[],
    );

    run.expect_eq(
        "applied_timeout",
        "10.0",
        "the opener must apply DEFAULT_TIMEOUT when open() receives no timeout",
    );
    run.expect(
        "proxy_applied",
        "a supplied proxy mapping must reach urllib's ProxyHandler",
    );
    run.expect(
        "agent_applied",
        "an explicit agent must replace urllib's default addheaders",
    );
    run.expect_eq(
        "probe_response",
        "200",
        "the timeout probe must complete without performing network I/O",
    );
    run.expect(
        "unsafe_redirect_rejected",
        "redirects leaving http(s) must be rejected instead of reaching file://",
    );
    run.expect(
        "https_downgrade_rejected",
        "an HTTPS request must not follow a redirect down to plain HTTP, which would silently disable TLS",
    );
    run.expect(
        "tls_hostname_can_be_disabled",
        "the compatibility option must still allow callers to disable hostname matching",
    );
    run.expect(
        "tls_chain_verification_kept",
        "disabling hostname matching must not disable TLS certificate-chain verification",
    );
}

#[test]
fn worker_translation_and_frames_keep_plugin_values_lossless_and_bounded() {
    let scratch = TempDir::new("worker-shim");
    let run = run_ok(
        &scratch,
        r##"
import io
import os
import json
import pathlib
import tempfile

import _crikey_legacy_worker as worker

# The worker installs the original stdout as its protocol stream and redirects
# ordinary stdout to its log capture. Keep a handle to the original stream for
# this focused probe.
wire = worker._PROTOCOL
worker._PROTOCOL = io.StringIO()


def emit(key, value):
    wire.write(key + "=" + str(value) + "\n")
    wire.flush()

fd, name = tempfile.mkstemp()
os.close(fd)
path = pathlib.Path(name)
try:
    path.write_text(
        "[Main]\nKey = first\n  continued=literal\nkey = second\n"
        "multi = first\n  second line\n[main]\nOther: yes\n"
        "url: https://example.invalid/?a=1\n",
        encoding="utf-8",
    )
    parsed = worker._parse_ini(str(path))
finally:
    path.unlink()

item = worker._item_from_wire(
    {
        "category": "legacy-user-107",
        "label": "selected",
        "short_desc": "",
        "target": "selected",
        "args_hint": "accepted",
        "hit_hint": "ignore",
    }
)
emit("duplicate_last_value", parsed["Main"]["Key"])
emit("continuation_preserved", parsed["Main"]["multi"] == "first\nsecond line")
emit("merged_section", parsed["Main"]["Other"])
emit("colon_with_equals", parsed["Main"]["url"])
emit("user_category", item.category() == 107)
generic_extension = worker._item_from_wire(
    {
        "category": "plugin-defined:legacy-user-107",
        "label": "generic",
        "short_desc": "",
        "target": "generic",
        "args_hint": "accepted",
        "hit_hint": "ignore",
    }
)
shadowed = worker._item_from_wire(
    {
        "category": "plugin-defined:application",
        "label": "shadowed",
        "short_desc": "",
        "target": "shadowed",
        "args_hint": "accepted",
        "hit_hint": "ignore",
    }
)
emit("generic_extension_category", generic_extension.category() == 107)
emit("shadowing_category_stays_plugin_defined",
     shadowed.category() == 100)

for name in ("reference", "error"):
    candidate = worker._item_from_wire(
        {
            "category": "plugin-defined:" + name,
            "label": name,
            "short_desc": "",
            "target": name,
            "args_hint": "accepted",
            "hit_hint": "ignore",
        }
    )
    emit("shadowing_" + name + "_stays_plugin_defined",
         candidate.category() == 100)

# NaN is not JSON. The emitter must turn that serialization failure into a
# bounded plugin-exception response rather than writing invalid JSON or dying.
worker._emit(
    {
        "id": 7,
        "callback": "on_suggest",
        "ok": True,
        "outcome": "suggestions",
        "log": [],
        "terminate_polls": 0,
        "items": [{"data_bag": float("nan")}],
    }
)
failure = json.loads(worker._PROTOCOL.getvalue())
emit("nonfinite_is_failure", failure["ok"] is False)
emit("nonfinite_kind", failure["error"]["kind"])

# Lone surrogates must be escaped before the strict UTF-8 protocol stream sees
# them, while still round-tripping through JSON.
surrogate = json.loads(worker._encode({"text": "\ud800"}))
emit("surrogate_round_trip", surrogate["text"] == "\ud800")

wire.write("DONE\n")
wire.flush()
"##,
        &[],
    );

    run.expect_eq(
        "duplicate_last_value",
        "second",
        "duplicate INI keys must use the last value while retaining the first spelling",
    );
    run.expect_eq(
        "continuation_preserved",
        "True",
        "an indented INI continuation must remain part of its key's value",
    );
    run.expect_eq(
        "merged_section",
        "yes",
        "repeated section headers must merge case-insensitively",
    );
    run.expect_eq(
        "colon_with_equals",
        "https://example.invalid/?a=1",
        "a colon-delimited INI value may contain '=' without changing its key",
    );
    run.expect(
        "user_category",
        "plugin-defined category numbers must survive the host-to-worker translation",
    );
    run.expect(
        "nonfinite_is_failure",
        "non-finite JSON values must produce a failure frame",
    );
    run.expect_eq(
        "nonfinite_kind",
        "plugin-exception",
        "serialization failures must be attributable without corrupting the protocol",
    );
    run.expect(
        "surrogate_round_trip",
        "JSON framing must escape lone surrogates so strict UTF-8 output cannot crash the worker",
    );
}

#[test]
fn legacy_worker_loads_sibling_modules_through_package_relative_imports() {
    let scratch = TempDir::new("relative-import");
    scratch.write("package/__init__.py", "");
    scratch.write("package/helper.py", "VALUE = 'sibling-value'\n");
    scratch.write(
        "package/entry.py",
        r#"
from .helper import VALUE

import keypirinha as kp


class Impl(kp.Plugin):
    def __init__(self):
        super().__init__()
        self.value = VALUE
"#,
    );
    let root = scratch.path().join("package");
    let run = run_python(
        &scratch,
        r#"
import _crikey_legacy_worker as worker
import _kptest as t

plugin = worker._load_plugin()
t.emit("loaded", plugin.value)
t.done()
"#,
        &[
            ("CRIKEY_LEGACY_PACKAGE_ROOT", root.to_str().unwrap()),
            ("CRIKEY_LEGACY_MAIN_MODULE", "entry"),
            ("CRIKEY_LEGACY_MAIN_MODULE_PATH", "entry.py"),
        ],
    );
    assert!(
        run.succeeded(),
        "the relative-import fixture failed:\n{}",
        run.describe()
    );
    assert!(
        run.stderr.lines().any(|line| line == "loaded=sibling-value"),
        "the plugin must load its sibling through a relative import:\n{}",
        run.describe()
    );
    assert!(
        run.stderr.lines().any(|line| line == "DONE"),
        "the relative-import fixture must reach its completion sentinel:\n{}",
        run.describe()
    );
}

// ---------------------------------------------------------------------------
// `keypirinha_wintypes` (spec 14.10 windows-only, 14.12; acceptance 31.31)
// ---------------------------------------------------------------------------

#[test]
fn keypirinha_wintypes_imports_successfully_on_every_platform() {
    let scratch = TempDir::new("wintypes-import");
    let run = run_ok(
        &scratch,
        r##"
import sys

import keypirinha_wintypes as kpwt
import _kptest as t

# Importability is the contract: a windows-only module that fails to import on
# Linux would break plugin loading before the layer ever gets to classify it,
# and no honest diagnostic could be produced (spec 14.12, 26.2).
t.emit("imported", True)
t.emit("module_name", kpwt.__name__)
t.emit("windows_only", kpwt.WINDOWS_ONLY)
t.emit("is_available_is_bool", isinstance(kpwt.is_available(), bool))
t.emit("is_available_matches_platform",
       kpwt.is_available() == sys.platform.startswith("win"))

symbols = kpwt.WINDOWS_ONLY_SYMBOLS
t.emit("symbols_is_tuple", isinstance(symbols, tuple))
t.emit("symbols", ",".join(symbols))
REQUIRED = ("kernel32", "user32", "shell32", "ole32", "declare_func", "GUID")
t.emit("symbols_cover_required", all(name in symbols for name in REQUIRED))

t.emit("error_is_runtime_error", issubclass(kpwt.WindowsOnlyError, RuntimeError))
# Not an AttributeError: hasattr() and getattr(..., default) must NOT be able to
# swallow a windows-only access into a silent False/None.
t.emit("error_not_attribute_error", not issubclass(kpwt.WindowsOnlyError, AttributeError))

t.done()
"##,
        &[],
    );

    run.expect(
        "imported",
        "keypirinha_wintypes must import successfully on every platform, including this Linux \
         host: the module is classified windows-only, not unimportable (spec 14.10)",
    );
    run.expect_eq(
        "module_name",
        "keypirinha_wintypes",
        "the imported module must be the shim, not a shadowing module",
    );
    run.expect_eq(
        "windows_only",
        "True",
        "keypirinha_wintypes.WINDOWS_ONLY must advertise the windows-only classification so a \
         plugin importing it is never presented as cross-platform (acceptance 31.31)",
    );
    run.expect(
        "is_available_is_bool",
        "keypirinha_wintypes.is_available() must return a bool",
    );
    run.expect(
        "is_available_matches_platform",
        "is_available() must be True only on Windows",
    );
    run.expect(
        "symbols_is_tuple",
        "keypirinha_wintypes.WINDOWS_ONLY_SYMBOLS must be a tuple the diagnostics layer can \
         enumerate",
    );
    run.expect(
        "symbols_cover_required",
        "WINDOWS_ONLY_SYMBOLS must name at least kernel32, user32, shell32, ole32, declare_func \
         and GUID; an empty or trimmed tuple would let the shim pass by advertising nothing",
    );
    run.expect(
        "error_is_runtime_error",
        "keypirinha_wintypes.WindowsOnlyError must subclass RuntimeError",
    );
    run.expect(
        "error_not_attribute_error",
        "WindowsOnlyError must NOT subclass AttributeError, or hasattr()/getattr(..., default) \
         would silently swallow a Win32 access and the layer would report nothing",
    );
}

#[cfg(not(windows))]
#[test]
fn keypirinha_wintypes_reports_every_win32_entry_point_unavailable_off_windows() {
    let scratch = TempDir::new("wintypes-unavailable");
    let run = run_ok(
        &scratch,
        r##"
import sys

import keypirinha_wintypes as kpwt
import _kptest as t

dishonest = []
for name in kpwt.WINDOWS_ONLY_SYMBOLS:
    try:
        value = getattr(kpwt, name)
    except kpwt.WindowsOnlyError as exc:
        symbol = getattr(exc, "symbol", None)
        platform = getattr(exc, "platform", None)
        message = str(exc)
        if symbol != name:
            dishonest.append(name + ": symbol=" + repr(symbol))
        if platform != sys.platform:
            dishonest.append(name + ": platform=" + repr(platform))
        if name not in message:
            dishonest.append(name + ": message does not name the symbol: " + message)
        if sys.platform not in message:
            dishonest.append(name + ": message does not name the platform: " + message)
        if "indows" not in message:
            dishonest.append(name + ": message does not say it is Windows-only: " + message)
    except Exception as exc:  # noqa: BLE001
        dishonest.append(name + ": wrong error " + type(exc).__name__ + ": " + str(exc))
    else:
        dishonest.append(name + ": resolved to " + repr(value) + " instead of failing")

t.emit("dishonest", ";".join(dishonest) or "<none>")
t.emit("dishonest_count", len(dishonest))
t.emit("symbols_checked", len(kpwt.WINDOWS_ONLY_SYMBOLS))

# hasattr must not be able to launder the failure into a quiet False.
try:
    hasattr(kpwt, "kernel32")
except kpwt.WindowsOnlyError:
    t.emit("hasattr_propagates", True)
except Exception as exc:  # noqa: BLE001
    t.emit("hasattr_propagates", "wrong-error:" + type(exc).__name__)
else:
    t.emit("hasattr_propagates", False)

t.done()
"##,
        &[],
    );

    assert!(
        run.int("symbols_checked") >= 6,
        "every declared Win32 entry point must be exercised, but only {} were declared\n{}",
        run.int("symbols_checked"),
        run.describe()
    );
    assert_eq!(
        run.int("dishonest_count"),
        0,
        "off Windows, every Win32-backed entry point in keypirinha_wintypes must raise \
         WindowsOnlyError carrying .symbol and .platform, with a message naming the symbol, the \
         platform and the windows-only nature of the dependency — never a panic, never a silent \
         success, never a usable stub (spec 14.10, 14.12, 26.2; acceptance 31.31). Problems: {}\n{}",
        run.field("dishonest"),
        run.describe()
    );
    run.expect_eq(
        "hasattr_propagates",
        "True",
        "hasattr() must propagate WindowsOnlyError rather than reporting False, so a plugin cannot \
         probe its way past the classification and so the layer always sees the access",
    );
}

#[cfg(windows)]
#[test]
fn keypirinha_wintypes_resolves_its_win32_entry_points_on_windows() {
    let scratch = TempDir::new("wintypes-windows");
    let run = run_ok(
        &scratch,
        r##"
import keypirinha_wintypes as kpwt
import _kptest as t

t.emit("is_available", kpwt.is_available())

unresolved = []
for name in kpwt.WINDOWS_ONLY_SYMBOLS:
    try:
        getattr(kpwt, name)
    except Exception as exc:  # noqa: BLE001
        unresolved.append(name + ": " + type(exc).__name__ + ": " + str(exc))

t.emit("unresolved", ";".join(unresolved) or "<none>")
t.emit("unresolved_count", len(unresolved))
t.done()
"##,
        &[],
    );

    run.expect_eq(
        "is_available",
        "True",
        "keypirinha_wintypes.is_available() must be True on Windows",
    );
    assert_eq!(
        run.int("unresolved_count"),
        0,
        "on Windows every declared Win32 entry point must resolve; the Linux counterpart test \
         asserts the honest WindowsOnlyError report for the same symbols. Unresolved: {}\n{}",
        run.field("unresolved"),
        run.describe()
    );
}

// ---------------------------------------------------------------------------
// Undocumented internals and stdout hygiene (spec 14.12; 7.1 protocol channel)
// ---------------------------------------------------------------------------

#[test]
fn touching_an_undocumented_shim_internal_raises_an_attributable_diagnostic() {
    let scratch = TempDir::new("undocumented");
    let run = run_ok(
        &scratch,
        r##"
import keypirinha as kp
import _kptest as t

t.emit("is_attribute_error", issubclass(kp.UndocumentedApiError, AttributeError))
t.emit("is_keypirinha_error", issubclass(kp.UndocumentedApiError, kp.KeypirinhaError))


def probe(getter, expected_owner, expected_attribute):
    try:
        getter()
    except kp.UndocumentedApiError as exc:
        problems = []
        if getattr(exc, "module", None) != expected_owner:
            problems.append("module=" + repr(getattr(exc, "module", None)))
        if getattr(exc, "attribute", None) != expected_attribute:
            problems.append("attribute=" + repr(getattr(exc, "attribute", None)))
        if getattr(exc, "diagnostic_code", None) != "undocumented-api-access":
            problems.append("diagnostic_code=" + repr(getattr(exc, "diagnostic_code", None)))
        message = str(exc)
        for needle in (expected_owner, expected_attribute, "documented"):
            if needle not in message:
                problems.append("message missing " + repr(needle) + ": " + message)
        return ";".join(problems) or "ok"
    except Exception as exc:  # noqa: BLE001
        return "wrong-error:" + type(exc).__name__ + ":" + str(exc)
    return "resolved"


# A plugin reaching for an undocumented module-level internal.
t.emit("module_internal", probe(lambda: kp._plugin_registry, "keypirinha", "_plugin_registry"))
# ... and for an undocumented attribute of the Plugin base class.
class NosyPlugin(kp.Plugin):
    pass

plugin = NosyPlugin()
t.emit("plugin_internal",
       probe(lambda: plugin._internal_state, "keypirinha.Plugin", "_internal_state"))
# ... and for a plausible-looking public name the shim deliberately does not
# provide, which must be just as attributable as a private one.
t.emit("absent_public", probe(lambda: kp.load_icon, "keypirinha", "load_icon"))

# Because the diagnostic is an AttributeError subclass, ordinary Python
# protocols keep working instead of exploding in unrelated places.
t.emit("hasattr_false", hasattr(kp, "_plugin_registry"))
t.emit("getattr_default", getattr(kp, "_plugin_registry", "default-used"))
t.emit("plugin_hasattr_false", hasattr(plugin, "_internal_state"))

# Attributes a plugin legitimately sets on itself must keep working: the
# diagnostic is for shim internals, not for plugin state.
plugin.my_own_field = 7
t.emit("plugin_own_attribute", plugin.my_own_field)

t.done()
"##,
        &[],
    );

    run.expect(
        "is_attribute_error",
        "keypirinha.UndocumentedApiError must subclass AttributeError so hasattr(), \
         getattr(..., default), copy and pickle keep behaving correctly",
    );
    run.expect(
        "is_keypirinha_error",
        "keypirinha.UndocumentedApiError must subclass keypirinha.KeypirinhaError so the layer has \
         one error taxonomy to report on (spec 26.2)",
    );
    for key in ["module_internal", "plugin_internal", "absent_public"] {
        run.expect_eq(
            key,
            "ok",
            "reaching for an undocumented internal must raise keypirinha.UndocumentedApiError \
             carrying .module, .attribute and diagnostic_code=\"undocumented-api-access\", with a \
             message naming the owner, the attribute and the fact that it is not documented — not \
             an obscure AttributeError from deep inside the shim (spec 14.12: `CriKey shall produce \
             a specific diagnostic when such behavior is detected`)",
        );
    }
    run.expect_eq(
        "hasattr_false",
        "False",
        "hasattr() on an undocumented internal must be False, not an escaping exception",
    );
    run.expect_eq(
        "getattr_default",
        "default-used",
        "getattr(..., default) on an undocumented internal must yield the default",
    );
    run.expect_eq(
        "plugin_hasattr_false",
        "False",
        "hasattr() on an undocumented Plugin internal must be False",
    );
    run.expect_eq(
        "plugin_own_attribute",
        "7",
        "the undocumented-internal diagnostic must not interfere with attributes a plugin sets on \
         itself",
    );
}

#[test]
fn plugin_print_output_goes_to_stderr_so_the_stdout_protocol_channel_stays_clean() {
    let scratch = TempDir::new("stdout-guard");
    // Not `run_ok`: this program deliberately owns stdout, so the `DONE`
    // sentinel would corrupt the very channel under test.
    let run = run_python(
        &scratch,
        r##"
import sys

import keypirinha as kp

# The worker entry installs this guard before any plugin code runs. It hands
# back the process's original stdout — the strict newline-delimited JSON
# protocol channel — and rebinds sys.stdout so plugin chatter cannot reach it.
protocol = kp._install_stdout_guard()

print("plugin chatter via print")
print("chatter with", "several", "arguments")
sys.stdout.write("plugin chatter via sys.stdout.write\n")
sys.stdout.flush()

# Installing twice must be idempotent: the worker may re-enter setup on reload,
# and a chained redirect would send protocol traffic to stderr.
again = kp._install_stdout_guard()

protocol.write('{"protocol":"line-one"}\n')
protocol.flush()
again.write('{"protocol":"line-two"}\n')
again.flush()

report = [
    "same_stream=" + str(again is protocol),
    "protocol_is_not_stdout=" + str(protocol is not sys.stdout),
    "protocol_writable=" + str(protocol.writable()),
    "protocol_utf8=" + str(protocol.encoding.lower().replace("-", "") == "utf8"),
    "DONE_STDERR",
]
sys.stderr.write("\n".join(report) + "\n")
sys.stderr.flush()
"##,
        &[
            ("PYTHONIOENCODING", "ascii"),
            ("PYTHONUTF8", "0"),
            ("PYTHONCOERCECLOCALE", "0"),
            ("LC_ALL", "C"),
        ],
    );

    assert!(
        run.succeeded(),
        "the stdout-guard program must exit 0\n{}",
        run.describe()
    );

    let stdout_lines: Vec<&str> = run
        .stdout
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(
        stdout_lines,
        vec![r#"{"protocol":"line-one"}"#, r#"{"protocol":"line-two"}"#],
        "stdout must carry the protocol lines and nothing else: it is a strict \
         newline-delimited JSON channel and one stray byte from a plugin desynchronises the \
         worker (spec 7.1, 14.4)\n{}",
        run.describe()
    );
    assert!(
        !run.stdout.contains("chatter"),
        "plugin print() output must never reach stdout\n{}",
        run.describe()
    );

    for expected in [
        "plugin chatter via print",
        "chatter with several arguments",
        "plugin chatter via sys.stdout.write",
    ] {
        assert!(
            run.stderr.contains(expected),
            "expected plugin output `{expected}` on stderr: stderr is reserved for plugin logging, \
             so redirected print() must still be visible to the developer rather than discarded\n{}",
            run.describe()
        );
    }

    let stderr_lines: Vec<&str> = run.stderr.lines().map(str::trim_end).collect();
    assert!(
        stderr_lines.contains(&"DONE_STDERR"),
        "the stdout-guard program did not run to completion\n{}",
        run.describe()
    );
    assert!(
        stderr_lines.contains(&"same_stream=True"),
        "_install_stdout_guard() must be idempotent and return the same protocol stream on a \
         second call, or a worker reload would chain redirects\n{}",
        run.describe()
    );
    assert!(
        stderr_lines.contains(&"protocol_is_not_stdout=True"),
        "the protocol stream returned by _install_stdout_guard() must be distinct from the \
         rebound sys.stdout\n{}",
        run.describe()
    );
    assert!(
        stderr_lines.contains(&"protocol_writable=True"),
        "the protocol stream must be writable\n{}",
        run.describe()
    );
    assert!(
        stderr_lines.contains(&"protocol_utf8=True"),
        "the protocol stream must be UTF-8 encoded so JSON lines survive intact\n{}",
        run.describe()
    );
}

// ---------------------------------------------------------------------------
// Dependency isolation
// ---------------------------------------------------------------------------

#[test]
fn the_shim_modules_import_cleanly_with_system_site_packages_excluded() {
    let scratch = TempDir::new("isolation");
    let run = run_python_with_flags(
        &scratch,
        &[ISOLATION_FLAG, "-B"],
        r##"
import os
import sys
import sysconfig

import keypirinha
import keypirinha_net
import keypirinha_util
import keypirinha_wintypes

import _kptest as t

SHIMS = ("keypirinha", "keypirinha_util", "keypirinha_net", "keypirinha_wintypes")

t.emit("site_loaded", "site" in sys.modules)

shim_dir = os.path.realpath(os.environ["CRIKEY_SHIM_DIR"])
program_dir = os.path.realpath(os.path.dirname(os.path.abspath(__file__)))
allowed = tuple({os.path.realpath(sysconfig.get_paths()["stdlib"]),
                 os.path.realpath(sysconfig.get_paths()["platstdlib"]),
                 shim_dir,
                 program_dir})

# Where pip installs third-party code. On a Debian-style layout these sit
# outside the standard library; on a python-build-standalone or hostedtoolcache
# layout `site-packages` is *inside* it, so allowing the standard library
# wholesale would quietly allow third-party code too. They are therefore
# subtracted from the allowed set rather than assumed to be outside it.
third_party = tuple({os.path.realpath(sysconfig.get_paths()["purelib"]),
                     os.path.realpath(sysconfig.get_paths()["platlib"])})

def under(path, roots):
    return any(path == root or path.startswith(root + os.sep) for root in roots)

def is_allowed(path):
    return under(path, allowed) and not under(path, third_party)

# The check is only worth running if it would actually catch something: a
# module that lived in the third-party directory must be classified foreign
# whatever this interpreter's layout happens to be.
t.emit("purelib_counts_as_foreign",
       not is_allowed(os.path.realpath(os.path.join(third_party[0], "pretend_third_party.py"))))

foreign = []
for name in sorted(sys.modules):
    module = sys.modules[name]
    if module is None:
        continue
    origin = getattr(module, "__file__", None)
    if not origin:
        continue
    real = os.path.realpath(origin)
    if not is_allowed(real):
        foreign.append(name + " -> " + real)

t.emit("foreign", ";".join(foreign) or "<none>")
t.emit("foreign_count", len(foreign))

t.emit("all_shims_from_shim_dir",
       all(os.path.realpath(sys.modules[name].__file__).startswith(shim_dir + os.sep)
           for name in SHIMS))
t.emit("shim_files", ",".join(sorted(
    os.path.basename(os.path.realpath(sys.modules[name].__file__)) for name in SHIMS)))

# The worker entry must live beside the shims and import under the same rules.
t.emit("worker_entry_present",
       os.path.isfile(os.path.join(shim_dir, "_crikey_legacy_worker.py")))

t.done()
"##,
        &[],
    );

    assert!(
        run.succeeded(),
        "the four compatibility modules must import under `{}`, which removes every \
         site-packages / dist-packages entry. An ImportError here means a shim took a third-party \
         dependency, which the legacy layer may not do (spec 15.1 reserves third-party imports for \
         modern plugins, not the compatibility shims)\n{}",
        ISOLATION_FLAG,
        run.describe()
    );
    run.expect_eq(
        "site_loaded",
        "False",
        "`-S` must actually be in effect; if `site` is loaded the isolation proof is vacuous",
    );
    run.expect(
        "purelib_counts_as_foreign",
        "a module in the third-party install directory must be classified foreign, otherwise the \
         foreign-module check below proves nothing. This is asserted rather than assumed because \
         `site-packages` lives inside the standard library on some layouts and beside it on others",
    );
    assert_eq!(
        run.int("foreign_count"),
        0,
        "after importing all four compatibility modules, every loaded module must come from the \
         standard library, the shim directory or the program directory. These came from elsewhere, \
         so a shim has an undeclared dependency: {}\n{}",
        run.field("foreign"),
        run.describe()
    );
    run.expect(
        "all_shims_from_shim_dir",
        "each compatibility module must be loaded from the crate's python/ directory via \
         PYTHONPATH, not from some other entry on sys.path",
    );
    run.expect_eq(
        "shim_files",
        "keypirinha.py,keypirinha_net.py,keypirinha_util.py,keypirinha_wintypes.py",
        "the shim directory must be flat, one module file per documented module, with no package \
         directory or __init__.py, so `import keypirinha` resolves straight off PYTHONPATH",
    );
    run.expect(
        "worker_entry_present",
        "the worker entry _crikey_legacy_worker.py must live beside the shims, since the worker is \
         spawned as `python3 -S <shim_dir>/_crikey_legacy_worker.py` with PYTHONPATH=<shim_dir>",
    );

    // The module list is pinned here rather than inferred, so adding a fifth
    // shim is a deliberate change to this contract.
    assert_eq!(
        SHIM_MODULES.len(),
        4,
        "spec 14.2 enumerates exactly four documented compatibility modules"
    );
}

// ---------------------------------------------------------------------------
// Settings coercions delivered on top of `_coerce` (spec 14.4, 26.2)
// ---------------------------------------------------------------------------

#[test]
fn settings_enum_and_map_coercions_attribute_a_bad_value_to_its_section_and_key() {
    let scratch = TempDir::new("settings-coercions");
    let run = run_ok(
        &scratch,
        r##"
import keypirinha as kp
import _kptest as t

settings = kp.Settings({
    "DEFAULT": {"mode": "Fast", "level": "HIGH"},
    "advanced": {"mode": "sideways"},
})

# The *accepted* spelling is returned, not the configured one, so a plugin can
# compare the result against its own literals.
t.emit("enum_folds_case", settings.get_enum("mode", enum=("slow", "fast")))
t.emit("enum_case_sensitive_fallback",
       settings.get_enum("mode", enum=("fast",), fallback="kept", case_sensitive=True))
t.emit("mapped", settings.get_mapped("level", map={"low": 1, "high": 2}))
t.emit("mapped_absent_key_no_fallback", settings.get_mapped("nope", map={"low": 1}) is None)
t.emit("enum_bad_value_fallback", settings.get_enum("mode", "advanced", ("slow",), "fell-back"))


def attribution(call):
    try:
        return "value:" + repr(call())
    except kp.SettingsError as exc:
        return "{}/{}".format(exc.section, exc.key)
    except Exception as exc:  # noqa: BLE001
        return "wrong-error:" + type(exc).__name__


t.emit("enum_attribution", attribution(lambda: settings.get_enum("mode", "advanced", ("slow",))))
t.emit("mapped_attribution", attribution(lambda: settings.get_mapped("level", map={"low": 1})))

try:
    settings.get_enum("mode", "advanced", ("slow",))
    t.emit("enum_lists_accepted", "no-error")
except kp.SettingsError as exc:
    t.emit("enum_lists_accepted", "'slow'" in str(exc))

t.done()
"##,
        &[],
    );

    run.expect_eq(
        "enum_folds_case",
        "fast",
        "get_enum must match ASCII-case-insensitively by default and answer with the accepted spelling",
    );
    run.expect_eq(
        "enum_case_sensitive_fallback",
        "kept",
        "case_sensitive=True must reject a differently-cased value, and a supplied fallback wins",
    );
    run.expect_eq("mapped", "2", "get_mapped must return the mapped value");
    run.expect(
        "mapped_absent_key_no_fallback",
        "an absent key is not a coercion failure: with no fallback it answers None, like every other accessor",
    );
    run.expect_eq(
        "enum_bad_value_fallback",
        "fell-back",
        "a supplied fallback must win over a typed failure, as it does for get_int",
    );
    run.expect_eq(
        "enum_attribution",
        "advanced/mode",
        "an unaccepted enum value must raise SettingsError carrying the section and key, never a bare ValueError",
    );
    run.expect_eq(
        "mapped_attribution",
        "DEFAULT/level",
        "a value absent from the map must raise SettingsError attributed to the DEFAULT section",
    );
    run.expect(
        "enum_lists_accepted",
        "the SettingsError message must name the accepted spellings; a plugin author reading a log cannot guess them",
    );
}

#[test]
fn settings_multiline_and_stripped_read_continuations_and_quotes() {
    let scratch = TempDir::new("settings-text");
    let run = run_ok(
        &scratch,
        r##"
import keypirinha as kp
import _kptest as t

# The INI reader keeps an indented continuation as an embedded newline, so this
# is exactly the shape a multi-line setting arrives in.
settings = kp.Settings({
    "DEFAULT": {
        "paths": "  /one\n/two\n\n  /three  ",
        "quoted": '  "  padded  "  ',
        "plain": "  bare  ",
    }
})

t.emit("multiline", "|".join(settings.get_multiline("paths")))
t.emit("multiline_keep_blanks", "|".join(settings.get_multiline("paths", keep_empty_lines=True)))
t.emit("multiline_absent", repr(settings.get_multiline("nope")))
t.emit("multiline_absent_fallback", "|".join(settings.get_multiline("nope", fallback=["d"])))
t.emit("stripped_quoted", settings.get_stripped("quoted"))
t.emit("stripped_plain", settings.get_stripped("plain"))
t.emit("stripped_absent", settings.get_stripped("nope") is None)
t.emit("stripped_absent_fallback", settings.get_stripped("nope", fallback="d"))

t.done()
"##,
        &[],
    );

    run.expect_eq(
        "multiline",
        "/one|/two|/three",
        "get_multiline must strip each line and drop blank ones by default",
    );
    run.expect_eq(
        "multiline_keep_blanks",
        "/one|/two||/three",
        "keep_empty_lines must retain the blank line for callers that use it as a separator",
    );
    run.expect_eq(
        "multiline_absent_fallback",
        "d",
        "a supplied fallback must be returned verbatim for an absent key",
    );
    run.expect_eq(
        "stripped_quoted",
        "padded",
        "get_stripped must remove one matching quote pair as well as the surrounding whitespace",
    );
    run.expect_eq(
        "stripped_plain",
        "bare",
        "get_stripped must strip whitespace from an unquoted value",
    );
    run.expect(
        "stripped_absent",
        "an absent key with no fallback answers None, matching the other scalar accessors",
    );
    run.expect_eq(
        "stripped_absent_fallback",
        "d",
        "a supplied fallback must be returned for an absent key",
    );
}

// ---------------------------------------------------------------------------
// keypirinha_util: the round trip and the decode ladder (spec 14.4)
// ---------------------------------------------------------------------------

#[test]
fn kwargs_encoding_round_trips_values_containing_the_separators_and_the_escape() {
    let scratch = TempDir::new("kwargs");
    let run = run_ok(
        &scratch,
        r##"
import keypirinha_util as ku
import _kptest as t

# Every character the format gives meaning to, inside both a name and a value,
# plus a value that is itself encoded output.
inner = ku.kwargs_encode(nested="a&b=c")
cases = {
    "path": r"C:\dir\file",
    "query": "a=b&c=d",
    "escape": "\\",
    "empty": "",
    "payload": inner,
}
encoded = ku.kwargs_encode(**cases)
t.emit("round_trips", ku.kwargs_decode(encoded) == cases)
t.emit("nested_round_trips", ku.kwargs_decode(inner) == {"nested": "a&b=c"})
t.emit("stable_order", encoded == ku.kwargs_encode(**cases))
t.emit("sorted_by_name", encoded.split("&")[0].startswith("empty="))
t.emit("empty_encode", repr(ku.kwargs_encode()))
t.emit("empty_decode", ku.kwargs_decode("") == {})


def rejection(text):
    try:
        return "value:" + repr(ku.kwargs_decode(text))
    except ValueError:
        return "ValueError"
    except Exception as exc:  # noqa: BLE001
        return "wrong-error:" + type(exc).__name__


t.emit("no_assignment", rejection("bare"))
t.emit("trailing_escape", rejection("a=b\\"))
t.emit("repeated_name", rejection("a=1&a=2"))
t.emit("empty_name", rejection("=1"))

typed = ku.kwargs_encode(count=7, enabled=True, ratio=1.5)
t.emit("typed_values", ku.kwargs_decode(typed) == {"count": 7, "enabled": True, "ratio": 1.5})
t.emit("non_str_value", "accepted")

t.done()
"##,
        &[],
    );

    run.expect(
        "round_trips",
        "kwargs_decode must be the exact inverse of kwargs_encode for values containing `&`, `=` and `\\`",
    );
    run.expect(
        "nested_round_trips",
        "encoded output must itself survive being carried as a value",
    );
    run.expect(
        "stable_order",
        "the same arguments must always produce the same string: an item target is compared by value",
    );
    run.expect(
        "sorted_by_name",
        "pairs must be emitted in sorted name order, not dictionary insertion order",
    );
    run.expect_eq(
        "empty_encode",
        "''",
        "no arguments must encode to the empty string",
    );
    run.expect(
        "empty_decode",
        "the empty string must decode to an empty mapping, closing the round trip at zero pairs",
    );
    for (key, why) in [
        (
            "no_assignment",
            "a pair with no unescaped `=` is not something kwargs_encode could have produced",
        ),
        (
            "trailing_escape",
            "a trailing lone escape is truncated input, not a value ending in a backslash",
        ),
        (
            "repeated_name",
            "a repeated name has two readings and must not be given one silently",
        ),
        ("empty_name", "an empty name is not a keyword argument"),
    ] {
        run.expect_eq(key, "ValueError", why);
    }
    run.expect(
        "typed_values",
        "basic bool, int and float values must survive kwargs_encode/kwargs_decode with their types",
    );
    run.expect_eq(
        "non_str_value",
        "accepted",
        "the documented scalar values beyond str must be supported and round-trip",
    );
}

#[test]
fn byte_decoding_walks_the_documented_ladder_from_the_mark_to_the_last_resort() {
    let scratch = TempDir::new("decode");
    let run = run_ok(
        &scratch,
        r##"
import codecs
import os

import keypirinha_util as ku
import _kptest as t

text = "h\u00e9llo \u20ac"

# 1. A byte-order mark is proof, and must be consumed rather than left as a
#    leading U+FEFF in the result.
t.emit("bom_utf8", ku.decode_bytes(codecs.BOM_UTF8 + text.encode("utf-8")) == text)
t.emit("bom_utf16le", ku.decode_bytes(codecs.BOM_UTF16_LE + text.encode("utf-16-le")) == text)
t.emit("bom_utf16be", ku.decode_bytes(codecs.BOM_UTF16_BE + text.encode("utf-16-be")) == text)
# UTF-32-LE's mark begins with UTF-16-LE's, so a shortest-first scan would
# silently read this as UTF-16-LE.
t.emit("bom_utf32le", ku.decode_bytes(codecs.BOM_UTF32_LE + text.encode("utf-32-le")) == text)

# 2. Unmarked UTF-8.
t.emit("plain_utf8", ku.decode_bytes(text.encode("utf-8")) == text)

# 3. Not valid UTF-8: the documented cp1252 guess.
cp1252 = "h\u00e9llo".encode("cp1252")
t.emit("cp1252", ku.decode_bytes(cp1252) == "h\u00e9llo")

# 4. A byte cp1252 leaves undefined must still decode rather than raise.
t.emit("last_resort", ku.decode_bytes(b"\x81\xfe") == "\u0081\u00fe")

# Non-bytes is a TypeError, and bytearray/memoryview are accepted.
t.emit("bytearray", ku.decode_bytes(bytearray(text.encode("utf-8"))) == text)
try:
    ku.decode_bytes(text)
    t.emit("str_rejected", "accepted")
except TypeError:
    t.emit("str_rejected", "TypeError")

scratch = os.environ["CRIKEY_TEST_SCRATCH"]
path = os.path.join(scratch, "sample.txt")
with open(path, "wb") as handle:
    handle.write(codecs.BOM_UTF8 + text.encode("utf-8"))
with ku.chardet_open(path) as handle:
    t.emit("chardet_open", handle.read() == text)

try:
    ku.chardet_open(path, "rb")
    t.emit("binary_mode", "accepted")
except ValueError:
    t.emit("binary_mode", "ValueError")

t.done()
"##,
        &[("CRIKEY_TEST_SCRATCH", &scratch.path().display().to_string())],
    );

    for (key, why) in [
        ("bom_utf8", "a UTF-8 BOM must be honoured and consumed"),
        ("bom_utf16le", "a UTF-16-LE BOM must be honoured and consumed"),
        ("bom_utf16be", "a UTF-16-BE BOM must be honoured and consumed"),
        (
            "bom_utf32le",
            "UTF-32-LE must be tried before UTF-16-LE: its mark begins with UTF-16-LE's, so a shortest-first scan reads the text as interleaved NULs without raising",
        ),
        ("plain_utf8", "unmarked bytes that decode wholly as UTF-8 must be read as UTF-8"),
        ("cp1252", "bytes that are not valid UTF-8 must fall to the documented Windows-1252 guess"),
        (
            "last_resort",
            "a byte cp1252 leaves undefined must fall to Latin-1, which cannot fail: the ladder must be total",
        ),
        ("bytearray", "a bytearray must be accepted without the caller copying it first"),
        ("chardet_open", "chardet_open must open the file in the encoding decode_bytes would have chosen"),
    ] {
        run.expect(key, why);
    }
    run.expect_eq(
        "str_rejected",
        "TypeError",
        "decode_bytes takes bytes; a str argument is a caller error, not something to pass through",
    );
    run.expect_eq(
        "binary_mode",
        "ValueError",
        "a binary mode must be refused: an encoding cannot be applied to a binary handle, and accepting one would claim a detection that did not happen",
    );
}

#[test]
fn windows_only_shell_helpers_refuse_off_windows_instead_of_emulating_win32() {
    let scratch = TempDir::new("windows-only-helpers");
    let run = run_ok(
        &scratch,
        r##"
import sys

import keypirinha as kp
import keypirinha_util as ku
import _kptest as t


def refusal(call):
    try:
        call()
        return "performed"
    except kp.HostUnavailableError as exc:
        return "HostUnavailableError:" + exc.operation
    except Exception as exc:  # noqa: BLE001
        return "wrong-error:" + type(exc).__name__


t.emit("windows", sys.platform.startswith("win"))
t.emit("read_link", refusal(lambda: ku.read_link("shortcut.lnk")))
t.emit("known_folder",
       refusal(lambda: ku.shell_known_folder_path("{F1B32785-6FBA-4FCF-9D55-7B8E7F157091}")))
# An honest refusal must not be launderable into a silent False by the two
# probes plugins reach for first.
t.emit("hasattr_read_link", hasattr(ku, "read_link"))
t.emit("execute_default_action_without_host",
       refusal(lambda: ku.execute_default_action(None, None)))

t.done()
"##,
        &[],
    );

    assert!(
        !run.flag("windows"),
        "this assertion describes the non-Windows behaviour and the CI host must not be Windows.\n{}",
        run.describe()
    );
    run.expect_eq(
        "read_link",
        "HostUnavailableError:read_link",
        "resolving a .lnk needs the Win32 shell link interfaces; off Windows it must refuse, naming the operation, rather than parse the file format and answer a different question (spec 2.3)",
    );
    run.expect_eq(
        "known_folder",
        "HostUnavailableError:shell_known_folder_path",
        "there is no known-folder registry off Windows and no defensible mapping onto XDG, so the call must refuse rather than pick a nearest match",
    );
    run.expect(
        "hasattr_read_link",
        "the name must resolve so the refusal is raised at the call, not swallowed by a hasattr probe (spec 14.12)",
    );
    run.expect_eq(
        "execute_default_action_without_host",
        "HostUnavailableError:execute_default_action",
        "with no host installed the action was not performed, and the plugin must be told so rather than see a silent success",
    );
}
