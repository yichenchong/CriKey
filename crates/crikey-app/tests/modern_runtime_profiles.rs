//! A modern plugin's declared `requires-python` must decide *which* interpreter
//! runs it, and two plugins whose requirements cannot be met by one interpreter
//! must end up in two separate operating-system processes (spec 14.11;
//! `docs/spec/crikey-spec-v1.md:1059-1069`).
//!
//! # What is actually proved
//!
//! Not configuration. Each fixture plugin reports, from inside its own child
//! interpreter, the pid it is running as and the absolute path of the executable
//! that started it. The assertions are on those two observed values: two
//! different pids is two processes, and two different executables is the mapping
//! having genuinely selected different interpreters rather than gating both
//! plugins against one.
//!
//! # Why the interpreters are wrappers
//!
//! The rule under test is "pick the installed interpreter whose version
//! satisfies the requirement", and exercising it needs a host offering at least
//! two versions. Most hosts — including every CI runner this suite has to pass
//! on, and the development host, which has exactly one CPython (3.14.4) — offer
//! one. So the fixture builds a search path holding two interpreter *wrappers*:
//! each answers the host's version probe with a distinct version it was told to
//! claim, and for every other invocation `exec`s the one real CPython. From the
//! host's point of view these are two interpreters of two versions, which is
//! precisely the input the mapping rule takes; the plugin code that then runs is
//! real Python in a real separate process, so the pid and executable the child
//! reports are real. The wrapper exports its own path so the child can name the
//! interpreter that started it — nothing else in the child can, because after
//! `exec` `sys.executable` is the underlying CPython for both.
//!
//! Unix only: the wrappers are `sh` scripts, and the mapping rule itself is
//! covered platform-independently by the `RuntimeCatalog` unit tests in
//! `crikey-python-host`.
//!
//! # `PATH`
//!
//! Discovery reads the ambient `PATH`, which is process-global, so both tests in
//! this binary take one lock for their whole body. Nothing else in this file
//! runs concurrently with a `PATH` it did not set.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

use crikey_app::{DisabledPlugins, ModernProvider, PipelineConfig, QueryPipeline};
use crikey_core::PluginId;
use crikey_python_host::{discover_interpreter, RequiresPython, RuntimeProfile};

/// Serialises these tests, because each replaces the process-wide `PATH`.
static PATH_LOCK: Mutex<()> = Mutex::new(());

/// Holds the `PATH` lock for a whole test body and restores the original value
/// on the way out, so neither a failing assertion nor a concurrent test can leak
/// a one-directory `PATH` — or pick a fixture wrapper up as the *real* CPython.
///
/// Acquired before any interpreter is resolved and replaced only afterwards,
/// which is why [`Self::acquire`] and [`Self::replace_with`] are separate steps.
struct SearchPath {
    _guard: MutexGuard<'static, ()>,
    original: Option<std::ffi::OsString>,
}

impl SearchPath {
    fn acquire() -> Self {
        // A test that already failed poisoned the lock; the invariant this lock
        // protects is restored by `Drop`, so the guard is still usable.
        let guard = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        Self {
            _guard: guard,
            original: std::env::var_os("PATH"),
        }
    }

    fn replace_with(&self, directory: &Path) {
        std::env::set_var("PATH", directory);
    }
}

impl Drop for SearchPath {
    fn drop(&mut self) {
        match self.original.take() {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
    }
}

/// A private directory removed when the test that made it ends.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);

        let path = std::env::temp_dir().join(format!(
            "crikey-runtime-profile-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    fn subdir(&self, name: &str) -> PathBuf {
        let path = self.join(name);
        fs::create_dir_all(&path).expect("scratch subdirectory is creatable");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn modern_plugin(id: &str) -> PluginId {
    PluginId(format!("modern.{id}"))
}

/// The one real CPython on this host, resolved *before* `PATH` is replaced.
///
/// Fails loudly rather than skipping, following the standing rule: a host with
/// no supported CPython cannot run modern plugins at all, and a silent green run
/// would hide that.
fn real_cpython() -> PathBuf {
    match discover_interpreter(&RuntimeProfile::Bundled, &RequiresPython(">=3.8".to_owned())) {
        Ok(interpreter) => interpreter.path().to_path_buf(),
        Err(error) => panic!("this suite requires a supported CPython on this host: {error}"),
    }
}

/// Writes an interpreter into `bin` that claims `version` and otherwise *is*
/// `real`.
///
/// The host's version probe is `<interpreter> -c "<expression>"`, the only `-c`
/// invocation it ever makes; every other invocation (`-S <worker entry>`) is
/// handed straight to the real CPython. `CRIKEY_TEST_INTERPRETER` is exported
/// across the `exec` so the plugin running in that child can name which of these
/// wrappers started it. Each probe also appends one line to `probe_log`, which
/// is how the caching test counts how often the host asked.
fn write_interpreter(bin: &Path, name: &str, version: &str, real: &Path, probe_log: &Path) -> PathBuf {
    let path = bin.join(name);
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$1\" = \"-c\" ]; then\n\
         \x20   echo \"{name}\" >> \"{log}\"\n\
         \x20   echo \"{version}\"\n\
         \x20   exit 0\n\
         fi\n\
         CRIKEY_TEST_INTERPRETER=\"{self_path}\" exec \"{real}\" \"$@\"\n",
        log = probe_log.display(),
        self_path = path.display(),
        real = real.display(),
    );
    fs::write(&path, script).expect("fixture interpreter is writable");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("fixture interpreter is executable");
    path
}

/// How many version probes the fixture interpreters have answered.
fn probes(probe_log: &Path) -> usize {
    fs::read_to_string(probe_log).unwrap_or_default().lines().count()
}

/// A plugin that reports, from inside its own interpreter, the process it is and
/// the interpreter that started it. This is the evidence the assertions read.
const REPORTING_SOURCE: &str = "\
import os

from crikey_sdk.plugin import Item, Plugin


class Reporting(Plugin):
    def suggest(self, query, context):
        label = \"pid=%d interpreter=%s\" % (
            os.getpid(),
            os.environ.get(\"CRIKEY_TEST_INTERPRETER\", \"unknown\"),
        )
        context.emit(Item(stable_id=\"report-1\", label=label, target=\"report\"))
";

/// Writes a discoverable modern plugin declaring `requires_python`.
fn write_modern_plugin(root: &Path, id: &str, requires_python: &str) {
    let dir = root.join(id);
    fs::create_dir_all(&dir).expect("plugin directory is creatable");

    let manifest = format!(
        "manifest-version = 1\n\
         \n\
         [plugin]\n\
         id = \"{id}\"\n\
         name = \"{id}\"\n\
         version = \"1.0.0\"\n\
         runtime = \"python\"\n\
         entrypoint = \"reporting:Reporting\"\n\
         \n\
         [python]\n\
         requires-python = \"{requires_python}\"\n"
    );
    fs::write(dir.join("crikey.toml"), manifest).expect("manifest is writable");
    fs::write(dir.join("reporting.py"), REPORTING_SOURCE).expect("plugin module is writable");
}

#[test]
fn two_plugins_with_incompatible_requires_python_run_in_separate_processes() {
    let scratch = Scratch::new("separation");
    let path = SearchPath::acquire();
    let bin = scratch.subdir("bin");
    let real = real_cpython();
    let probe_log = scratch.join("probes");
    // A host offering two versions. `python3` is the name the pre-14.11 rule
    // would have picked for both plugins.
    write_interpreter(&bin, "python3", "3.11.9", &real, &probe_log);
    write_interpreter(&bin, "python3.13", "3.13.1", &real, &probe_log);

    let plugins_root = scratch.subdir("plugins");
    write_modern_plugin(&plugins_root, "newer", ">=3.13");
    write_modern_plugin(&plugins_root, "older", "<3.12");

    let newer = modern_plugin("newer");
    let older = modern_plugin("older");
    let index_root = scratch.subdir("index");
    let cache_root = scratch.join("cache");

    path.replace_with(&bin);
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = ModernProvider::load(
        &mut pipeline,
        &[plugins_root],
        Some(index_root),
        cache_root,
        &DisabledPlugins::default(),
    );

    // Neither requirement may be refused: 3.11.9 and 3.13.1 are both present, so
    // both plugins have an interpreter available to them.
    assert!(
        provider.plugins().contains(&newer) && provider.plugins().contains(&older),
        "both plugins must load; unavailable: {:?}",
        provider.unavailable(),
    );

    let frame = provider
        .drive_query(&mut pipeline, "q", 23)
        .expect("both plugins answer, so the merged frame exists");
    let report = |plugin: &PluginId| -> String {
        frame
            .rows
            .iter()
            .find(|row| row.plugin_name == plugin.0)
            .map(|row| row.label.clone())
            .unwrap_or_else(|| {
                panic!(
                    "plugin {} must report from its own child; rows: {:?}",
                    plugin.0,
                    frame
                        .rows
                        .iter()
                        .map(|row| (row.plugin_name.clone(), row.label.clone()))
                        .collect::<Vec<_>>(),
                )
            })
    };
    let newer_report = report(&newer);
    let older_report = report(&older);

    // The mapping selected a different interpreter for each requirement. Before
    // 14.11 both would name `python3`, the single default.
    assert!(
        newer_report.contains(&format!("interpreter={}", bin.join("python3.13").display())),
        "the >=3.13 plugin must run on the 3.13 interpreter, reported: {newer_report}"
    );
    assert!(
        older_report.contains(&format!("interpreter={}", bin.join("python3").display())),
        "the <3.12 plugin must run on the 3.11 interpreter, reported: {older_report}"
    );

    // And that is not a label difference over one shared worker: the two
    // children are two live processes.
    let pid_of = |report: &str| -> String {
        report
            .split_whitespace()
            .find_map(|field| field.strip_prefix("pid=").map(str::to_owned))
            .expect("each report carries its child's pid")
    };
    assert_ne!(
        pid_of(&newer_report),
        pid_of(&older_report),
        "incompatible requirements must run in separate processes, got one pid: \
         {newer_report} / {older_report}"
    );

    provider.shutdown(180);
}

#[test]
fn a_requires_python_no_installed_interpreter_satisfies_is_refused_by_name() {
    let scratch = Scratch::new("unsatisfiable");
    let path = SearchPath::acquire();
    let bin = scratch.subdir("bin");
    let real = real_cpython();
    let probe_log = scratch.join("probes");
    write_interpreter(&bin, "python3", "3.11.9", &real, &probe_log);
    write_interpreter(&bin, "python3.13", "3.13.1", &real, &probe_log);

    let plugins_root = scratch.subdir("plugins");
    write_modern_plugin(&plugins_root, "impossible", "==3.9.9");

    let impossible = modern_plugin("impossible");
    let index_root = scratch.subdir("index");
    let cache_root = scratch.join("cache");

    path.replace_with(&bin);
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = ModernProvider::load(
        &mut pipeline,
        &[plugins_root],
        Some(index_root),
        cache_root,
        &DisabledPlugins::default(),
    );

    // Never silently served by the default interpreter: the plugin declared a
    // constraint this host cannot meet, so it does not run at all.
    assert!(
        !provider.plugins().contains(&impossible),
        "a plugin whose requires-python cannot be met must not load"
    );

    let reason = provider
        .unavailable()
        .iter()
        .find(|entry| entry.plugin.as_ref() == Some(&impossible))
        .map(|entry| entry.reason.clone())
        .unwrap_or_else(|| {
            panic!(
                "the refusal must be recorded; unavailable: {:?}",
                provider.unavailable()
            )
        });

    // The diagnostic has to be actionable on its own: it names the requirement
    // that could not be met and every version that was actually found.
    assert!(
        reason.contains("==3.9.9"),
        "the refusal must quote the requirement: {reason}"
    );
    for found in ["3.11.9", "3.13.1"] {
        assert!(
            reason.contains(found),
            "the refusal must quote the versions found ({found}): {reason}"
        );
    }

    provider.shutdown(180);
}

#[test]
fn the_interpreters_on_the_search_path_are_probed_once_for_the_whole_load() {
    let scratch = Scratch::new("cached");
    let path = SearchPath::acquire();
    let bin = scratch.subdir("bin");
    let real = real_cpython();
    let probe_log = scratch.join("probes");
    write_interpreter(&bin, "python3", "3.11.9", &real, &probe_log);
    write_interpreter(&bin, "python3.13", "3.13.1", &real, &probe_log);

    // Four plugins, so an uncached scan is unmistakable: it would probe both
    // interpreters once per plugin.
    let plugins_root = scratch.subdir("plugins");
    for id in ["one", "two", "three", "four"] {
        write_modern_plugin(&plugins_root, id, ">=3.13");
    }
    let index_root = scratch.subdir("index");
    let cache_root = scratch.join("cache");

    path.replace_with(&bin);
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let mut provider = ModernProvider::load(
        &mut pipeline,
        &[plugins_root],
        Some(index_root),
        cache_root,
        &DisabledPlugins::default(),
    );
    assert_eq!(
        provider.plugins().len(),
        4,
        "every plugin must load; unavailable: {:?}",
        provider.unavailable()
    );

    // Two probes for the one scan, plus the single confirming probe each plugin's
    // discovery makes of the interpreter it was mapped to. A per-plugin scan
    // would be 2 * 4 + 4 = 12.
    assert_eq!(
        probes(&probe_log),
        2 + 4,
        "the search path must be scanned once for the whole load, not once per plugin"
    );

    provider.shutdown(180);
}
