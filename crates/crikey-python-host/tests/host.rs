//! Interpreter discovery, worker-pool sharing, and conflicting-version
//! coexistence for the modern CPython worker host (spec 15.6, 15.3, 15.4,
//! 4.2, 14.11; acceptance 31.10, 31.20).
//!
//! These tests are written before the implementation. They pin three things
//! the modern host must get right, and each is only true of the *real* system:
//!
//! * **Discovery order is decisive and reported.** `$CRIKEY_PYTHON` outranks a
//!   `RuntimeProfile::External`, which outranks `python3` on `PATH` (spec
//!   14.11), and [`InterpreterSource`] says which rule won. An interpreter that
//!   does not satisfy the plugin's `requires-python` is a named error, never a
//!   silent fall-through to the next candidate — falling through would run
//!   plugin code under a Python the plugin declared it cannot run on.
//! * **The pool shares one worker per environment.** Two plugins that resolve
//!   to the SAME [`EnvironmentId`] share ONE worker process; two with DIFFERENT
//!   ids get separate processes (spec 15.6, 15.3). This is the mechanism, not a
//!   decoration: it is how conflicting dependency versions coexist.
//! * **Conflicting deps actually coexist (acceptance 31.20).** Two plugins each
//!   declaring a different `acme` version resolve to distinct environments and
//!   distinct workers, and each worker's OWN interpreter imports the version it
//!   was given. The proof is the real `import acme; acme.__version__`, run under
//!   `-S` with distinct `PYTHONPATH`s — not two differing ids.
//!
//! # Why a real interpreter and no doubles
//!
//! A double would pass an in-process host that violates spec 4.2 ("Python code
//! shall not execute on the CriKey user-interface thread") and could never let
//! two conflicting versions of one module be imported at once — a single
//! address space holds `acme` exactly once. So these tests spawn `python3` for
//! real, and a missing or too-old interpreter is a **test failure**, never a
//! skip. There is no `#[ignore]` and no early `return`.
//!
//! # Time and platform
//!
//! No test sleeps. The workers are real processes, so their bounds are wall
//! clock, but every bound is *explicit* — handed in through [`WorkerOptions`],
//! never read from a clock inside library logic — and each is a liveness guard
//! that turns a hang into a named failure, not a timing assertion. Six of the
//! discovery cases synthesise a stand-in interpreter that reports a chosen
//! version; creating one needs a POSIX executable bit, so those are
//! `#[cfg(unix)]`. The development and CI host is Linux, where they run.
//!
//! # Fixtures
//!
//! Every plugin, package "wheel" and cache root is built into a private temp
//! directory at test time and removed when the test ends. The real CriKey SDK
//! reaches each worker through `CRIKEY_MODERN_SDK_DIR` and the assembled import
//! path, so a fixture that says `import acme` proves the import path was built,
//! with no assertion needed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crikey_core::PluginId;
use crikey_package_manager::{
    resolve, EnvironmentInputs, EnvironmentStore, ImportPath, MaterializedEnvironment, PackageIndex,
};
use crikey_python_host::{
    discover_interpreter, discover_interpreter_in, sdk_root, BatchState, DiscoveryEnvironment, Interpreter,
    ModernWorker, RequiresPython, RuntimeProfile, SuggestRequest, Suggestions, WorkerOptions, WorkerPool,
};

// Only the stand-in-interpreter tests name these, and synthesising a stand-in
// needs a POSIX executable bit, so those tests and this import are `#[cfg(unix)]`.
// `HostError` joins them because the only match on it is in that same
// requires-python test, and an unconditional import is an unused-import error
// on Windows under `-D warnings`.
#[cfg(unix)]
use crikey_python_host::{HostError, InterpreterSource, PythonVersion};

// ---------------------------------------------------------------------------
// Bounds
//
// None of these is a performance assertion. Each one exists so that a broken
// implementation fails with a message instead of hanging the suite.
// ---------------------------------------------------------------------------

/// Bound on the startup handshake with a correctly behaving worker.
const STARTUP_TIMEOUT_MS: u64 = 30_000;

/// Per-call bound handed to workers that are expected to answer promptly.
const CALL_TIMEOUT_MS: u64 = 30_000;

/// The `requires-python` every real fixture in this file satisfies. Discovery
/// order, not the gate, is what these tests are pinning, so the gate is set
/// wide enough that any supported CPython passes it.
const SATISFIABLE: &str = ">=3.8";

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
            "crikey-modern-host-{label}-{}-{}",
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

// ---------------------------------------------------------------------------
// The real SDK
// ---------------------------------------------------------------------------

/// The repository `sdk/python` directory, which ships both
/// `_crikey_modern_worker.py` and the `crikey_sdk` package at run time.
///
/// Handed to each worker explicitly as import-path entry #4 (the SDK), and
/// reached by [`crate::sdk_root`]'s own dev-layout fallback for the worker
/// entry — never through a process-wide `CRIKEY_MODERN_SDK_DIR` this test
/// binary sets, which would race sibling threads' `getenv`/`spawn` (exactly
/// what `worker.rs`/`worker_python.rs` refuse to do).
fn sdk_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python");
    dir.canonicalize().unwrap_or(dir)
}

// ---------------------------------------------------------------------------
// Stand-in interpreters
//
// A real CPython cannot be made to report an old or unsupported version, so the
// discovery-order and requires-python tests need a peer that reports a chosen
// version on demand. This script is that peer.
// ---------------------------------------------------------------------------

/// An executable that answers any version probe with `version` and nothing
/// else. Used only by discovery tests, which never spawn a worker.
///
/// It ignores its arguments deliberately: the probe's exact argument vector is
/// the host's business, and a fixture that hard-coded it would break on a
/// harmless change. What discovery must get right — *which* executable it chose
/// and what version it read — is asserted directly.
#[cfg(unix)]
fn version_shim(dir: &Path, name: &str, version: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(
        &path,
        format!("#!/bin/sh\n# stand-in interpreter\necho '{version}'\n"),
    )
    .expect("stand-in interpreter is writable");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .expect("stand-in interpreter is made executable");
    path
}

// ---------------------------------------------------------------------------
// The interpreter this host actually has
// ---------------------------------------------------------------------------

/// The real CPython on this host. Never skips: if discovery fails, the test
/// that asked for it fails, because a host that cannot run Python cannot run
/// the modern worker at all.
fn host_interpreter() -> Interpreter {
    discover_interpreter(&RuntimeProfile::Bundled, &RequiresPython(SATISFIABLE.to_owned()))
        .expect("this host must provide a supported CPython for the modern worker")
}

// ---------------------------------------------------------------------------
// Package index and environments
// ---------------------------------------------------------------------------

/// Writes one offline "wheel" `<root>/<name>-<version>/<name>/__init__.py` whose
/// module exposes exactly its own `__version__`. The version the plugin reads
/// back is therefore the version the index carried, not a guess.
fn write_wheel(index_root: &Path, name: &str, version: &str) {
    let package = index_root.join(format!("{name}-{version}")).join(name);
    fs::create_dir_all(&package).expect("wheel package directory is creatable");
    fs::write(
        package.join("__init__.py"),
        format!("__version__ = \"{version}\"\n"),
    )
    .expect("wheel module is writable");
}

/// An offline index carrying two conflicting `acme` versions (§31.20).
fn acme_index(scratch: &Scratch) -> PackageIndex {
    let root = scratch.subdir("index");
    write_wheel(&root, "acme", "1.0");
    write_wheel(&root, "acme", "2.0");
    PackageIndex::from_dir(&root).expect("a directory of wheels is a loadable package index")
}

/// Resolves `dependencies` against `index`, then materialises the content-
/// addressed environment they lock to. The returned env's `site_dir` is what an
/// import path adds so the plugin's own Python can import the locked packages.
fn materialize(
    store: &EnvironmentStore,
    index: &PackageIndex,
    interpreter: &Interpreter,
    dependencies: &[String],
) -> MaterializedEnvironment {
    let lockfile = resolve(SATISFIABLE, dependencies, index)
        .expect("declared dependencies resolve against the offline index");

    let inputs = EnvironmentInputs {
        python_version: interpreter.version().to_string(),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        locked: lockfile.packages.clone(),
        native_build_options: Vec::new(),
    };

    let env = store
        .ensure(&inputs, index)
        .expect("a resolved environment materialises");
    assert_eq!(
        env.id,
        inputs.environment_id(),
        "a materialised environment carries the deterministic id its inputs decide"
    );
    env
}

// ---------------------------------------------------------------------------
// Plugin fixtures
// ---------------------------------------------------------------------------

/// Writes a modern plugin module into its own source directory and returns that
/// directory (import-path entry #1). `module` is the file stem; the entrypoint
/// the worker loads is `"<module>:<Class>"`.
fn plugin_source(scratch: &Scratch, dir: &str, module: &str, body: &str) -> PathBuf {
    let root = scratch.subdir(dir);
    fs::write(root.join(format!("{module}.py")), body).expect("fixture plugin is writable");
    root
}

/// A plugin that imports its managed `acme` and emits one item whose target is
/// the version its OWN interpreter loaded. The import is at module top level so
/// a mis-assembled path fails to load rather than passing vacuously.
const ACME_PROBE: &str = "\
import acme
from crikey_sdk import Item, Plugin


class AcmePlugin(Plugin):
    def suggest(self, query, context):
        context.emit(Item(stable_id=\"acme\", label=\"acme\", target=acme.__version__))
";

/// A plugin whose `<class>` emits exactly one item whose `stable_id`, label and
/// target are all `target`. Two such plugins with different `target`s answer a
/// `suggest` distinguishably, which is how a test proves WHICH plugin's code a
/// worker ran — the only observable that separates a shared worker from a
/// wrongly-shared one.
fn emitter(class: &str, target: &str) -> String {
    format!(
        "from crikey_sdk import Item, Plugin\n\
         \n\
         \n\
         class {class}(Plugin):\n\
         \x20   def suggest(self, query, context):\n\
         \x20       context.emit(Item(stable_id=\"{target}\", label=\"{target}\", target=\"{target}\"))\n"
    )
}

/// Assembles the options for one worker: source dir first, then the managed
/// environment, then the real SDK — the exact import order spec 15.4 fixes.
fn worker_options(
    plugin: &str,
    entrypoint: &str,
    source: &Path,
    env: &MaterializedEnvironment,
) -> WorkerOptions {
    let import_path = ImportPath::assemble(source, &[], env, &sdk_dir());
    WorkerOptions::new(PluginId(plugin.to_owned()), entrypoint, import_path)
        .with_startup_timeout_ms(STARTUP_TIMEOUT_MS)
        .with_call_timeout_ms(CALL_TIMEOUT_MS)
        .with_shutdown_timeout_ms(CALL_TIMEOUT_MS)
}

/// Drives one suggest against a worker and returns the single item's target.
///
/// The plugins under test emit exactly one item, so a batch of any other size
/// is a defect: an assembled import path that imported the wrong module, or a
/// worker that dropped or duplicated a partial.
fn probe_version(worker: &mut ModernWorker) -> String {
    let request = SuggestRequest {
        generation: 1,
        text: "acme".to_owned(),
        normalized: "acme".to_owned(),
        selected_item_id: None,
    };

    let suggestions: Suggestions = worker
        .suggest(&request)
        .expect("a cooperative plugin answers a suggest");

    assert!(
        matches!(suggestions.state, BatchState::Final),
        "a plugin that returns normally produces a final batch, got {:?}",
        suggestions.state
    );
    assert_eq!(
        suggestions.items.len(),
        1,
        "the probe plugin emits exactly one item; a different count means the import path was wrong"
    );
    suggestions.items[0].target.clone()
}

// ---------------------------------------------------------------------------
// Discovery order (spec 14.11)
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn discovery_prefers_the_crikey_python_override_over_the_profile_and_the_search_path() {
    let scratch = Scratch::new("discovery-override");
    let search = scratch.subdir("bin");
    let chosen = version_shim(&scratch.path, "override-python", "3.12.1");
    let profile = version_shim(&scratch.path, "profile-python", "3.11.2");
    version_shim(&search, "python3", "3.10.3");

    // All three candidates are present and all three satisfy the requirement,
    // so only the order can decide; each reports a different version so a wrong
    // choice names itself.
    let environment = DiscoveryEnvironment::empty()
        .with_override(&chosen)
        .with_search_path([search]);

    let interpreter = discover_interpreter_in(
        &RuntimeProfile::External(profile),
        &RequiresPython(SATISFIABLE.to_owned()),
        &environment,
    )
    .expect("an override naming a usable interpreter resolves");

    assert_eq!(
        interpreter.path(),
        chosen,
        "CRIKEY_PYTHON outranks both the runtime profile and PATH (spec 14.11)"
    );
    assert_eq!(
        interpreter.version(),
        PythonVersion::new(3, 12, 1),
        "the reported version is read from the interpreter that was chosen"
    );
    assert_eq!(
        interpreter.source(),
        InterpreterSource::EnvironmentOverride,
        "discovery reports which rule selected the interpreter"
    );
}

#[cfg(unix)]
#[test]
fn discovery_falls_back_to_the_external_runtime_profile_before_the_search_path() {
    let scratch = Scratch::new("discovery-profile");
    let search = scratch.subdir("bin");
    let chosen = version_shim(&scratch.path, "profile-python", "3.11.2");
    version_shim(&search, "python3", "3.10.3");

    let environment = DiscoveryEnvironment::empty().with_search_path([search]);

    let interpreter = discover_interpreter_in(
        &RuntimeProfile::External(chosen.clone()),
        &RequiresPython(SATISFIABLE.to_owned()),
        &environment,
    )
    .expect("an external runtime profile naming a usable interpreter resolves");

    assert_eq!(
        interpreter.path(),
        chosen,
        "with no override, a managed external runtime outranks PATH (spec 14.11)"
    );
    assert_eq!(
        interpreter.version(),
        PythonVersion::new(3, 11, 2),
        "the reported version is read from the profile's interpreter"
    );
    assert_eq!(
        interpreter.source(),
        InterpreterSource::RuntimeProfile,
        "discovery reports that the runtime profile selected the interpreter"
    );
}

#[cfg(unix)]
#[test]
fn discovery_falls_back_to_python3_on_the_search_path_when_nothing_overrides_it() {
    let scratch = Scratch::new("discovery-path");
    let search = scratch.subdir("bin");
    let chosen = version_shim(&search, "python3", "3.10.3");

    let environment = DiscoveryEnvironment::empty().with_search_path([search]);

    // A non-External profile names no path of its own, so the search path is
    // the last rule left.
    let interpreter = discover_interpreter_in(
        &RuntimeProfile::Bundled,
        &RequiresPython(SATISFIABLE.to_owned()),
        &environment,
    )
    .expect("python3 on the search path is the final discovery rule");

    assert_eq!(
        interpreter.path(),
        chosen,
        "with no override and no external profile, discovery takes python3 from PATH"
    );
    assert_eq!(
        interpreter.version(),
        PythonVersion::new(3, 10, 3),
        "the reported version is read from the interpreter found on PATH"
    );
    assert_eq!(
        interpreter.source(),
        InterpreterSource::SearchPath,
        "discovery reports that the search path selected the interpreter"
    );
}

// ---------------------------------------------------------------------------
// requires-python is a hard gate, never a silent fall-through (spec 15.2, §4)
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn an_interpreter_that_does_not_satisfy_requires_python_is_rejected_not_silently_skipped() {
    let scratch = Scratch::new("requires-python");
    let old = version_shim(&scratch.path, "python3.10", "3.10.3");

    // The other two rules are made genuinely usable: this host's real python3
    // sits on the search path. If the chosen interpreter fell through when it
    // failed the requirement, discovery would succeed under a Python the plugin
    // declared it cannot run on — worse than not starting. It must error.
    let real = host_interpreter();
    let real_directory = real
        .path()
        .parent()
        .expect("a discovered interpreter lives in a directory")
        .to_path_buf();
    let environment = DiscoveryEnvironment::empty()
        .with_override(&old)
        .with_search_path([real_directory]);

    let required = RequiresPython(">=3.12".to_owned());
    let error = discover_interpreter_in(&RuntimeProfile::Bundled, &required, &environment)
        .expect_err("an interpreter below requires-python cannot resolve");

    match &error {
        HostError::UnsatisfiedRequiresPython { required, found } => {
            assert_eq!(
                required, ">=3.12",
                "the failure carries the requirement that was not met"
            );
            assert!(
                found.contains("3.10"),
                "the failure carries the version that was found, got {found:?}"
            );
        }
        other => panic!("an unsatisfying interpreter is UnsatisfiedRequiresPython, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The real interpreter on this host (spec 4.2)
// ---------------------------------------------------------------------------

#[test]
fn the_interpreter_on_this_host_is_discovered_and_satisfies_a_requirement_it_meets() {
    let interpreter = host_interpreter();

    assert!(
        interpreter.path().exists(),
        "discovery resolved to an interpreter that is not on disk: {}",
        interpreter.path().display()
    );

    // The version must be read from the interpreter, not guessed. Ask it the
    // same question independently and require the same answer.
    let reported = Command::new(interpreter.path())
        .args(["-c", "import sys; print('%d.%d.%d' % sys.version_info[:3])"])
        .output()
        .expect("the discovered interpreter is executable");
    let reported = String::from_utf8(reported.stdout)
        .expect("an interpreter reports its version as text")
        .trim()
        .to_owned();
    assert_eq!(
        interpreter.version().to_string(),
        reported,
        "the discovered version is the one the interpreter itself reports"
    );

    // The convenience form is exactly the explicit form over the ambient
    // environment; this is what lets every other test use the explicit form
    // without diverging from production behaviour.
    let ambient = discover_interpreter_in(
        &RuntimeProfile::Bundled,
        &RequiresPython(SATISFIABLE.to_owned()),
        &DiscoveryEnvironment::from_process(),
    )
    .expect("discovery over the ambient environment resolves on this host");
    assert_eq!(
        ambient.path(),
        interpreter.path(),
        "discover_interpreter resolves against the ambient process environment"
    );
}

// ---------------------------------------------------------------------------
// The pool shares one worker per environment (spec 15.6, 15.3)
// ---------------------------------------------------------------------------

#[test]
fn plugins_sharing_an_environment_id_share_one_worker_and_distinct_ids_get_separate_workers() {
    let scratch = Scratch::new("pool-sharing");
    let interpreter = host_interpreter();
    let index = PackageIndex::from_dir(&scratch.subdir("empty-index"))
        .expect("an empty directory is an empty package index");
    let store = EnvironmentStore::new(scratch.subdir("cache"));

    // Two environments that differ only in a native build option, so their ids
    // differ by construction while everything else is held equal.
    let base = EnvironmentInputs {
        python_version: interpreter.version().to_string(),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        locked: Vec::new(),
        native_build_options: Vec::new(),
    };
    let mut other = base.clone();
    other.native_build_options = vec!["distinct".to_owned()];
    assert_ne!(
        base.environment_id(),
        other.environment_id(),
        "environments with different inputs have different ids"
    );

    let env_a = store.ensure(&base, &index).expect("env a materialises");
    let env_b = store.ensure(&other, &index).expect("env b materialises");

    // Two DISTINCT plugins: different source dirs (import-path entry #1) AND
    // different entrypoints, but the SAME environment id. The env id alone is
    // identical, so a pool keyed on (interpreter, env) would hand the second
    // plugin the FIRST plugin's process — and that one worker answers with the
    // code it loaded first, because the protocol carries no per-call plugin
    // routing (pinned decision 1).
    let alpha_src = plugin_source(&scratch, "alpha", "alpha_mod", &emitter("Alpha", "alpha"));
    let beta_src = plugin_source(&scratch, "beta", "beta_mod", &emitter("Beta", "beta"));

    let mut pool = WorkerPool::new();

    // Alpha spawns a worker.
    {
        let worker = pool
            .worker_for(
                &interpreter,
                &env_a.id,
                worker_options("share.alpha", "alpha_mod:Alpha", &alpha_src, &env_a),
            )
            .expect("the first plugin spawns a worker");
        assert!(worker.is_alive(), "a freshly spawned worker is alive");
    }
    assert_eq!(pool.worker_count(), 1, "one plugin, one worker");

    // Beta: SAME env id, DIFFERENT source and entrypoint. It must get its OWN
    // worker, not collapse into alpha's.
    {
        let worker = pool
            .worker_for(
                &interpreter,
                &env_a.id,
                worker_options("share.beta", "beta_mod:Beta", &beta_src, &env_a),
            )
            .expect("a distinct plugin in the same env resolves");
        assert!(worker.is_alive(), "beta's own worker is alive");
    }
    assert_eq!(
        pool.worker_count(),
        2,
        "two DIFFERENT plugins sharing an env id must NOT collapse to one worker (pinned decision 1)"
    );

    // A TRUE duplicate of alpha — same interpreter, env id, entrypoint AND
    // import path — shares alpha's existing worker: no third process.
    {
        let worker = pool
            .worker_for(
                &interpreter,
                &env_a.id,
                worker_options("share.alpha.again", "alpha_mod:Alpha", &alpha_src, &env_a),
            )
            .expect("a genuine duplicate resolves");
        assert!(worker.is_alive(), "the shared worker is still alive");
    }
    assert_eq!(
        pool.worker_count(),
        2,
        "a genuinely identical plugin shares the existing worker (spec 15.6)"
    );

    // Each worker answers with ITS OWN plugin's code. If beta had been handed
    // alpha's process, this suggest would come back as "alpha".
    let alpha_target = {
        let worker = pool
            .worker_for(
                &interpreter,
                &env_a.id,
                worker_options("share.alpha", "alpha_mod:Alpha", &alpha_src, &env_a),
            )
            .expect("alpha's worker is still available");
        probe_version(worker)
    };
    let beta_target = {
        let worker = pool
            .worker_for(
                &interpreter,
                &env_a.id,
                worker_options("share.beta", "beta_mod:Beta", &beta_src, &env_a),
            )
            .expect("beta's worker is still available");
        probe_version(worker)
    };
    assert_eq!(alpha_target, "alpha", "alpha's worker runs alpha's code");
    assert_eq!(
        beta_target, "beta",
        "beta's worker runs BETA's code, not alpha's — a distinct plugin is never handed a shared worker"
    );
    assert_eq!(
        pool.worker_count(),
        2,
        "driving the two workers neither merged nor spawned any extra process"
    );

    // A distinct EnvironmentId always gets a separate worker.
    {
        let worker = pool
            .worker_for(
                &interpreter,
                &env_b.id,
                worker_options("share.env-b", "alpha_mod:Alpha", &alpha_src, &env_b),
            )
            .expect("a plugin in a different environment spawns its own worker");
        assert!(worker.is_alive(), "the separate worker is alive");
    }
    assert_eq!(
        pool.worker_count(),
        3,
        "a distinct EnvironmentId gets a SEPARATE worker process (spec 15.3)"
    );
}

// ---------------------------------------------------------------------------
// Conflicting dependency versions coexist (acceptance 31.20)
// ---------------------------------------------------------------------------

#[test]
fn conflicting_dependency_versions_coexist_in_separate_live_workers() {
    let scratch = Scratch::new("conflicting-deps");
    let interpreter = host_interpreter();
    let index = acme_index(&scratch);
    let store = EnvironmentStore::new(scratch.subdir("cache"));

    // Each plugin pins a DIFFERENT acme, so each resolves to its own lockfile,
    // its own EnvironmentId, and its own materialised env directory.
    let env_a = materialize(&store, &index, &interpreter, &["acme==1.0".to_owned()]);
    let env_b = materialize(&store, &index, &interpreter, &["acme==2.0".to_owned()]);
    assert_ne!(
        env_a.id, env_b.id,
        "different locked versions decide different environment ids"
    );
    assert_ne!(
        env_a.site_dir, env_b.site_dir,
        "different environments materialise to different directories"
    );

    // Distinct source directories, each import-path entry #1 for its worker.
    let source_a = plugin_source(&scratch, "plugin-a", "acme_probe", ACME_PROBE);
    let source_b = plugin_source(&scratch, "plugin-b", "acme_probe", ACME_PROBE);

    let mut pool = WorkerPool::new();

    // Spawn BOTH workers first, so both processes are alive simultaneously
    // before either is driven.
    {
        let worker = pool
            .worker_for(
                &interpreter,
                &env_a.id,
                worker_options("acme.one", "acme_probe:AcmePlugin", &source_a, &env_a),
            )
            .expect("plugin A's worker spawns");
        assert!(worker.is_alive(), "plugin A's worker is alive");
    }
    {
        let worker = pool
            .worker_for(
                &interpreter,
                &env_b.id,
                worker_options("acme.two", "acme_probe:AcmePlugin", &source_b, &env_b),
            )
            .expect("plugin B's worker spawns");
        assert!(worker.is_alive(), "plugin B's worker is alive");
    }
    assert_eq!(
        pool.worker_count(),
        2,
        "conflicting versions coexist in two separate workers"
    );

    // Drive B first, then A: A answering correctly AFTER B ran proves both
    // interpreters are alive at the same time, each importing its own acme.
    let version_b = {
        let worker = pool
            .worker_for(
                &interpreter,
                &env_b.id,
                worker_options("acme.two", "acme_probe:AcmePlugin", &source_b, &env_b),
            )
            .expect("plugin B's worker is still available");
        probe_version(worker)
    };
    let version_a = {
        let worker = pool
            .worker_for(
                &interpreter,
                &env_a.id,
                worker_options("acme.one", "acme_probe:AcmePlugin", &source_a, &env_a),
            )
            .expect("plugin A's worker is still available");
        probe_version(worker)
    };

    assert_eq!(
        version_a, "1.0",
        "plugin A's own interpreter imported acme 1.0 under -S with its own PYTHONPATH"
    );
    assert_eq!(
        version_b, "2.0",
        "plugin B's own interpreter imported acme 2.0 under -S with its own PYTHONPATH"
    );

    // Both are still the same two live workers after all the traffic.
    assert_eq!(
        pool.worker_count(),
        2,
        "driving the workers neither merged nor spawned any extra process"
    );
}

// ---------------------------------------------------------------------------
// The SDK the workers run against is the real, shipped one
// ---------------------------------------------------------------------------

#[test]
fn the_worker_host_resolves_the_real_shipped_sdk() {
    // These tests set no CRIKEY_MODERN_SDK_DIR override, so sdk_root() must
    // resolve the shipped SDK through its OWN fallback (installed `modern-sdk`
    // beside the exe, else the dev `sdk/python`) — the path the workers really
    // run against, not one this test handed itself.
    let resolved = sdk_root();
    assert!(
        resolved.join("crikey_sdk").is_dir(),
        "the real SDK package must live under the resolved sdk_root {}",
        resolved.display()
    );
    assert!(
        resolved.join("_crikey_modern_worker.py").is_file(),
        "sdk_root must ship the modern worker entry it launches, under {}",
        resolved.display()
    );
    // In the dev layout the fallback is this crate's `../../sdk/python` sibling.
    let expected = sdk_dir();
    assert_eq!(
        resolved.canonicalize().unwrap_or_else(|_| resolved.clone()),
        expected,
        "with nothing overriding it, sdk_root falls back to the shipped dev SDK"
    );
}
