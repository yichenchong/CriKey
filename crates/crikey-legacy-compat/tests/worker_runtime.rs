//! Interpreter discovery and the out-of-process CPython legacy worker
//! (spec 4.2, 14.11, 24.1, 24.3; roadmap M3 "the CPython legacy worker";
//! acceptance 31.9, 31.10, 31.16, 31.17).
//!
//! These tests are written before the implementation. They pin the boundary
//! between CriKey and a *real* child process running plugin code: how the
//! interpreter that runs it is chosen, how a request and its reply cross the
//! process boundary, and — the part that actually earns the process boundary —
//! what happens when the plugin on the far side misbehaves.
//!
//! # Why a real subprocess and not a double
//!
//! Every contract below is only true of a real child. A double would pass an
//! in-process implementation that violates spec 4.2 ("Python code shall not
//! execute on the CriKey user-interface thread") and spec 24.1 ("a Python
//! interpreter crash shall not terminate CriKey"), because an in-process plugin
//! cannot have a distinct pid, cannot exit mid-call without taking the host
//! with it, and cannot be hard-stopped when it refuses to cooperate. So the
//! worker tests spawn `python3` for real and the failure tests provoke real
//! failures: a real `os._exit`, a real infinite loop, a real desynchronised
//! byte stream.
//!
//! A missing or too-old interpreter is therefore a **test failure**, never a
//! skip. There is no `#[ignore]` and no early `return` in this file: a host
//! that cannot run Python cannot run the Legacy Compatibility Layer, and
//! silently passing would hide exactly that.
//!
//! # Time
//!
//! This is the one CriKey suite where wall-clock time is legitimate. The peer
//! is a real operating-system process, so a deadline against it cannot be
//! virtual: no amount of `tick(now)` makes a spinning child stop spinning.
//! Every bound is nevertheless *explicit* — passed in through
//! [`WorkerOptions`], never read from a clock inside library logic — and no
//! test sleeps. Synchronisation with the child is by rendezvous (a fifo the
//! plugin opens, a pipe that reaches end of file), so the tests are as fast as
//! the machine and never race.
//!
//! `RESPONSE_LIMIT` and friends are liveness guards, not timing assertions:
//! they turn a regression that would hang the run into a named failure.
//!
//! # Platform
//!
//! The worker contract is platform neutral and every worker test below runs
//! anywhere CPython does. Six tests need a *stand-in* interpreter — an
//! executable that reports a version CriKey must reject, or that speaks the
//! protocol badly on purpose — and creating one needs a POSIX executable bit,
//! so those are `#[cfg(unix)]`. The development and CI host is Linux, where
//! they run. The orphan check in the shutdown test reads `/proc` and degrades
//! to "cannot answer" elsewhere rather than asserting vacuously.
//!
//! # Surface under test
//!
//! * `discover_interpreter(&RuntimeProfile) -> Result<Interpreter, WorkerError>`
//!   resolves against the ambient process environment, and
//!   `discover_interpreter_in(&RuntimeProfile, &DiscoveryEnvironment)` against
//!   one supplied explicitly. The explicit form exists **for determinism, not
//!   for convenience**: Rust test binaries are one process running many
//!   threads, so a test that set `CRIKEY_PYTHON` on the ambient environment
//!   would leak into every concurrent test and be unsound besides. No test in
//!   this file mutates the process environment.
//! * Discovery order is fixed and total: `CRIKEY_PYTHON`, then
//!   `RuntimeProfile::External(path)`, then the runtime staged beside the
//!   executable (spec 14.11), then `python3` on `PATH`. An
//!   override that names a broken interpreter is a hard failure, never a
//!   silent fall-through to the next candidate.
//! * `LegacyWorker::spawn(&Interpreter, &LegacyPackage, WorkerOptions)`,
//!   `call(&mut self, LegacyRequest) -> Result<LegacyResponse, WorkerError>`,
//!   `terminate_handle() -> TerminateHandle`, `shutdown(self) -> WorkerExit`.
//!   `call` taking `&mut self` is not incidental: it is how "legacy callbacks
//!   are serialized per plugin instance" (acceptance 31.16) becomes
//!   unrepresentable to get wrong rather than merely tested.
//! * `WorkerError` — `PythonUnavailable`, `UnsupportedVersion`, `Protocol`,
//!   `Crashed`, `Timeout`, `Io`. Every variant names the plugin or the
//!   interpreter it concerns in its `Display`, because an error that cannot be
//!   attributed cannot become an actionable diagnostic (spec 26.2).
//! * A plugin raising an exception is **not** a `WorkerError`. It is an
//!   `Ok(LegacyResponse)` whose outcome is `LegacyOutcome::Failed`, because the
//!   worker is healthy and stays usable; conflating the two would make plugin
//!   bugs look like transport bugs and would lose the log the diagnostic needs.
//!
//! # Fixtures
//!
//! Each test writes the smallest legacy package that can express its contract
//! into its own temp directory, removed when the test ends. The shim package
//! directory (`<CARGO_MANIFEST_DIR>/python`) reaches the child through
//! [`WorkerOptions`]; a fixture that says `import keypirinha` therefore proves
//! the import path was assembled, with no assertion needed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crikey_core::{
    Action, ActionId, ArgumentPolicy, Category, ExecutionPolicy, Generation, HitPolicy, Item, ItemId,
    PluginId,
};
use crikey_input_scheduler::Millis;
use crikey_legacy_compat::{
    discover_interpreter, discover_interpreter_in, DiscoveryEnvironment, InstanceId, Interpreter,
    LegacyCallback, LegacyEventFlags, LegacyOutcome, LegacyPackage, LegacyRequest, LegacyRequestKind,
    LegacyResponse, LegacyWorker, PackageLoader, PluginException, TerminateHandle, WorkerError, WorkerExit,
    WorkerOptions, ENV_MAIN_MODULE_PATH, ENV_PACKAGE_ROOT, MINIMUM_SUPPORTED_PYTHON, WORKER_ENTRY_FILE,
};
// Only the stand-in-interpreter tests name these, and creating a stand-in needs
// a POSIX executable bit, so they are `#[cfg(unix)]` and so is the import.
#[cfg(unix)]
use crikey_legacy_compat::{InterpreterSource, PythonVersion};
use crikey_python_host::RuntimeProfile;
#[cfg(unix)]
use crikey_python_host::BUNDLED_RUNTIME_DIR;

// ---------------------------------------------------------------------------
// Bounds
//
// None of these is a performance assertion. Each one exists so that a broken
// implementation fails with a message instead of hanging the suite.
// ---------------------------------------------------------------------------

/// Ceiling on any rendezvous with a correctly behaving child.
const RESPONSE_LIMIT: Duration = Duration::from_secs(60);

/// Per-call bound handed to workers that are expected to answer promptly.
const CALL_BUDGET_MS: Millis = 30_000;

/// Bound handed to the worker in the tests that make a plugin refuse to stop.
/// Small, because those tests deliberately wait it out.
const HARD_BOUND_MS: Millis = 2_000;

/// Bound on the startup handshake.
const STARTUP_BUDGET_MS: Millis = 30_000;

/// Environment variable carrying a rendezvous fifo to a fixture plugin. Test
/// scaffolding, not part of the worker contract — it travels through the
/// caller-supplied `WorkerOptions` environment like any plugin's own settings
/// would, which is also what pins that such variables reach the child at all.
const FIFO_VAR: &str = "CRIKEY_TEST_FIFO";

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
            "crikey-worker-runtime-{label}-{}-{}",
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
// Stand-in interpreters
//
// A real CPython cannot be made to report version 3.7, and a correct shim will
// never emit a malformed frame. Both contracts therefore need a peer that is
// wrong on purpose. These scripts are that peer.
// ---------------------------------------------------------------------------

/// An executable that answers any version probe with `version` and does
/// nothing else. Used only by discovery tests, which never spawn a worker.
///
/// It ignores its arguments deliberately: the probe's exact argument vector is
/// the implementation's business, and a fixture that hard-coded it would break
/// on a harmless change. What discovery must get right — *which* executable it
/// chose and what version it read from it — is asserted directly.
#[cfg(unix)]
fn version_shim(dir: &Path, name: &str, version: &str) -> PathBuf {
    executable(
        &dir.join(name),
        &format!("#!/bin/sh\n# stand-in interpreter\necho '{version}'\n"),
    )
}

/// An executable that passes discovery as `version`, completes the startup
/// handshake, and then answers the first request with `reply` verbatim —
/// whatever `reply` is, valid protocol or not.
///
/// After answering it blocks reading its own stdin rather than exiting, so the
/// host observes the bad line while the child is demonstrably alive. Otherwise
/// a malformed reply would race the child's exit and could be reported as
/// `Crashed`, and the test would be pinning nothing.
#[cfg(unix)]
fn hostile_worker(dir: &Path, name: &str, version: &str, reply: &str) -> PathBuf {
    executable(
        &dir.join(name),
        &format!(
            "#!/bin/sh\n\
             # Version probe: any argument vector that is not the worker entry.\n\
             case \"$*\" in\n\
             *{entry}*) ;;\n\
             *) echo '{version}'; exit 0 ;;\n\
             esac\n\
             printf '{{\"ready\":true,\"pid\":%d,\"protocol\":1}}\\n' \"$$\"\n\
             read -r _request\n\
             printf '%s\\n' '{reply}'\n\
             while read -r _ignored; do :; done\n",
            entry = WORKER_ENTRY_FILE,
        ),
    )
}

/// As [`hostile_worker`], but exits immediately after writing a non-newline-
/// terminated response. The host must report the buffered fragment instead of
/// treating end of stream as an empty response.
#[cfg(unix)]
fn hostile_worker_unterminated(dir: &Path, name: &str, version: &str, reply: &str) -> PathBuf {
    executable(
        &dir.join(name),
        &format!(
            "#!/bin/sh\n\
             case \"$*\" in\n\
             *{entry}*) ;;\n\
             *) echo '{version}'; exit 0 ;;\n\
             esac\n\
             printf '{{\"ready\":true,\"pid\":%d,\"protocol\":1}}\\n' \"$$\"\n\
             read -r _request\n\
             printf '%s' '{reply}'\n",
            entry = WORKER_ENTRY_FILE,
        ),
    )
}

/// Creates an executable stand-in interpreter at `path`.
///
/// The script is written to a sibling temporary name, made executable, and only
/// then renamed into place. Writing `path` directly is racy: these tests run in
/// parallel threads in one process, so when any thread spawns a child the fork
/// inherits every open descriptor, including another thread's write handle to a
/// script it has just created. The kernel then refuses to execute that script
/// with `ETXTBSY` ("Text file busy") because a writer still holds it open, and
/// discovery fails for a stand-in that is perfectly valid. A rename publishes
/// the finished file under a name no writer ever held, so the window is gone.
#[cfg(unix)]
fn executable(path: &Path, script: &str) -> PathBuf {
    let staging = path.with_extension("staging");
    fs::write(&staging, script).expect("stand-in interpreter is writable");
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))
        .expect("stand-in interpreter is made executable");
    fs::rename(&staging, path).expect("stand-in interpreter is published atomically");
    path.to_path_buf()
}

// ---------------------------------------------------------------------------
// Legacy package fixtures
// ---------------------------------------------------------------------------

/// Writes `<scratch>/<name>/<name>.py` and loads it as a legacy package.
///
/// A loose directory needs no manifest: the package id is the directory's file
/// stem and the main module is the top-level `.py` whose stem matches it.
fn legacy_package(scratch: &Scratch, name: &str, source: &str) -> LegacyPackage {
    let root = scratch.subdir(name);
    fs::write(root.join(format!("{name}.py")), source).expect("fixture plugin is writable");

    PackageLoader::new(scratch.join("package-cache"))
        .load(&root)
        .expect("a directory holding one matching module is a loadable legacy package")
}

/// The shim package directory the worker puts on the child's import path.
fn shim_path() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("python");
    assert!(
        path.join(WORKER_ENTRY_FILE).is_file(),
        "the legacy worker entry {WORKER_ENTRY_FILE} must ship in {}",
        path.display()
    );
    path
}

fn options(plugin: &str) -> WorkerOptions {
    WorkerOptions::new(PluginId(plugin.to_owned()), shim_path())
        .with_startup_timeout_ms(STARTUP_BUDGET_MS)
        .with_call_timeout_ms(CALL_BUDGET_MS)
}

/// The interpreter this host actually has. Never skips: if discovery fails,
/// the test that asked for it fails.
fn host_interpreter() -> Interpreter {
    discover_interpreter(&RuntimeProfile::LegacyCompatibility)
        .expect("this host must provide a supported CPython for the legacy worker")
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

fn request(plugin: &str, generation: u64, kind: LegacyRequestKind) -> LegacyRequest {
    LegacyRequest {
        plugin: PluginId(plugin.to_owned()),
        instance: InstanceId(1),
        generation: Generation::from_raw(generation),
        kind,
    }
}

/// Calls the worker and checks the reply's envelope before handing it back.
///
/// The envelope echo is checked once here rather than in every test because a
/// reply that answers the wrong instance or generation is wrong no matter what
/// it carries: it is how a superseded instance's late answer gets mistaken for
/// a live one (spec 9.2, acceptance 31.7).
fn call_ok(worker: &mut LegacyWorker, request: LegacyRequest) -> LegacyResponse {
    let (plugin, instance, generation, callback) = (
        request.plugin.clone(),
        request.instance,
        request.generation,
        request.callback(),
    );

    let response = worker
        .call(request)
        .unwrap_or_else(|error| panic!("{callback:?} must complete, got {error}"));

    assert_eq!(
        response.plugin, plugin,
        "a reply is attributed to the plugin that was asked"
    );
    assert_eq!(
        response.instance, instance,
        "a reply echoes the instance it answers, so a superseded instance cannot be mistaken for a live one"
    );
    assert_eq!(
        response.generation, generation,
        "a reply carries the generation of the request that caused it (acceptance 31.7)"
    );
    assert_eq!(
        response.callback, callback,
        "a reply names the callback it answers"
    );

    response
}

fn call_err(worker: &mut LegacyWorker, request: LegacyRequest) -> WorkerError {
    let callback = request.callback();
    match worker.call(request) {
        Ok(response) => panic!("{callback:?} must fail here, got {:?}", response.outcome),
        Err(error) => error,
    }
}

fn catalog(response: &LegacyResponse) -> &[Item] {
    match &response.outcome {
        LegacyOutcome::SetCatalog(items) => items,
        other => panic!("on_catalog must answer with a catalog, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Rendezvous
// ---------------------------------------------------------------------------

/// A fifo a fixture plugin opens to announce that its callback is running.
///
/// This is how the tests that signal a *live* callback stay deterministic
/// without sleeping: opening a fifo for writing blocks until the reader
/// arrives and the read returns when the writer closes, so "the callback has
/// started" is an event, not a guess.
#[derive(Debug)]
struct Rendezvous {
    path: PathBuf,
}

impl Rendezvous {
    fn new(scratch: &Scratch) -> Self {
        let path = scratch.join("callback-started");
        let status = Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo is available on a POSIX host");
        assert!(status.success(), "mkfifo could not create {}", path.display());

        Self { path }
    }

    /// Runs `then` on a worker thread as soon as the plugin announces itself.
    ///
    /// The reader must not run on the test thread: the test thread is about to
    /// block inside `call`, which is where the plugin that does the announcing
    /// is running.
    fn on_start(&self, then: impl FnOnce() + Send + 'static) -> mpsc::Receiver<()> {
        let path = self.path.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            // Returns exactly when the plugin has opened, written and closed.
            let _announced = fs::read(&path);
            then();
            // A failed send only means the test already gave up.
            let _ = sender.send(());
        });

        receiver
    }
}

/// Whether the operating system still holds a table entry for `pid`.
///
/// `None` means this platform cannot be asked without a new dependency. On
/// Linux a child that exited but was never waited on remains a visible zombie,
/// so this distinguishes *reaped* from merely *dead* — which is precisely the
/// difference between a clean shutdown and an orphan.
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
//
// Written against the documented Keypirinha API, because that is the API the
// Legacy Compatibility Layer promises. `__PACKAGE_ROOT__` is substituted with
// the pinned environment key so the fixture and the host cannot drift apart.
// ---------------------------------------------------------------------------

fn source(body: &str) -> String {
    body.replace("__PACKAGE_ROOT__", ENV_PACKAGE_ROOT)
        .replace("__FIFO__", FIFO_VAR)
}

/// Reports the pid the plugin itself is running under.
const PID_WITNESS: &str = r#"
import os
import keypirinha as kp


class Fixture(kp.Plugin):
    def on_start(self):
        pass

    def on_catalog(self):
        self.set_catalog([
            self.create_item(
                category=kp.ItemCategory.KEYWORD,
                label="pid",
                short_desc="the pid the plugin itself observes",
                target=str(os.getpid()),
                args_hint=kp.ItemArgsHint.FORBIDDEN,
                hit_hint=kp.ItemHitHint.NOARGS),
        ])
"#;

/// One item exercising every field a legacy item can express, and one
/// reporting the package root the worker handed the child.
const CATALOG_FIDELITY: &str = r#"
import os
import keypirinha as kp


class Fixture(kp.Plugin):
    def on_start(self):
        pass

    def on_catalog(self):
        self.set_catalog([
            self.create_item(
                category=kp.ItemCategory.KEYWORD,
                label="Ünïcøde \"quoted\" line one\nline two",
                short_desc="описание — с тире",
                target="fixture/target path",
                args_hint=kp.ItemArgsHint.REQUIRED,
                hit_hint=kp.ItemHitHint.IGNORE),
            self.create_item(
                category=kp.ItemCategory.KEYWORD,
                label="package root",
                short_desc="the root the worker gave the child",
                target=os.environ["__PACKAGE_ROOT__"],
                args_hint=kp.ItemArgsHint.FORBIDDEN,
                hit_hint=kp.ItemHitHint.NOARGS),
        ])
"#;

#[test]
fn discovery_and_the_worker_expose_a_standard_error_and_thread_safe_handles() {
    // Compile-time contract. `WorkerError` has to be a real `std::error::Error`
    // so it can be a source in a diagnostic chain, and `Clone` so a supervisor
    // can retain one while returning another. `TerminateHandle` has to cross to
    // another thread, because the thread that raises the flag is never the
    // thread blocked in `call`. `LegacyWorker` has to be `Send` so a supervisor
    // can own it away from the UI thread (spec 4.2).
    fn error<T: std::error::Error + std::fmt::Debug + Clone + Send + Sync + 'static>() {}
    fn shareable<T: Send + Sync + Clone + std::fmt::Debug>() {}
    fn movable<T: Send>() {}

    error::<WorkerError>();
    shareable::<TerminateHandle>();
    movable::<LegacyWorker>();
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

    // All three candidates are present and all three are usable. Only the
    // order decides, and each reports a different version so a wrong choice
    // says which one was taken.
    let environment = DiscoveryEnvironment::empty()
        .with_override(&chosen)
        .with_search_path([search]);

    let interpreter = discover_interpreter_in(&RuntimeProfile::External(profile), &environment)
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

    let interpreter = discover_interpreter_in(&RuntimeProfile::External(chosen.clone()), &environment)
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

    // `LegacyCompatibility` names no path of its own, so the search path is
    // the last rule left.
    let interpreter = discover_interpreter_in(&RuntimeProfile::LegacyCompatibility, &environment)
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

#[test]
fn a_crikey_python_override_naming_a_missing_file_is_reported_as_python_unavailable() {
    let scratch = Scratch::new("discovery-missing");
    let missing = scratch.join("no-such-python");

    // Both remaining rules are made genuinely usable: the runtime profile and
    // the search path each resolve to this host's real interpreter. An override
    // that cannot be honoured must still fail rather than quietly fall through
    // to one of them, because falling through would run plugin code under an
    // interpreter the operator did not choose — worse than not starting at all.
    let real = host_interpreter();
    let real_directory = real
        .path()
        .parent()
        .expect("a discovered interpreter lives in a directory")
        .to_path_buf();
    let environment = DiscoveryEnvironment::empty()
        .with_override(&missing)
        .with_search_path([real_directory]);

    let error = discover_interpreter_in(&RuntimeProfile::External(real.path().to_path_buf()), &environment)
        .expect_err("an override naming a file that does not exist cannot resolve");

    match &error {
        WorkerError::PythonUnavailable { path, .. } => assert_eq!(
            path.as_deref(),
            Some(missing.as_path()),
            "the failure carries the path that could not be used"
        ),
        other => panic!("a missing override interpreter is PythonUnavailable, got {other:?}"),
    }

    assert!(
        error.to_string().contains(&missing.display().to_string()),
        "the message names the interpreter that could not be used, got {error}"
    );
}

#[cfg(unix)]
#[test]
fn search_path_skips_directories_and_non_executable_candidates() {
    let scratch = Scratch::new("discovery-search-filter");
    let directory = scratch.subdir("directory");
    fs::create_dir(directory.join("python3")).expect("the directory candidate is writable");

    let non_executable = scratch.subdir("non-executable");
    let non_executable_path = version_shim(&non_executable, "python3", "3.12.1");
    fs::set_permissions(&non_executable_path, fs::Permissions::from_mode(0o644))
        .expect("the stand-in can be made non-executable");

    let usable = scratch.subdir("usable");
    let usable_path = version_shim(&usable, "python3", "3.11.2");
    let environment = DiscoveryEnvironment::empty().with_search_path([directory, non_executable, usable]);

    let interpreter = discover_interpreter_in(&RuntimeProfile::LegacyCompatibility, &environment)
        .expect("search discovery skips unusable name matches");
    assert_eq!(interpreter.path(), usable_path);
}

// ---------------------------------------------------------------------------
// The bundled runtime (spec 14.11)
//
// The legacy layer resolves interpreters too, and it must resolve them by the
// *same* rule as the modern host: one policy, not two. These pin that a
// runtime staged beside the executable is preferred over PATH here as well,
// and that it faces the same minimum-version gate.
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn a_staged_bundled_runtime_is_chosen_with_no_environment_variable_set() {
    let scratch = Scratch::new("discovery-bundled");
    let install = scratch.subdir("install");
    let bin = install.join(BUNDLED_RUNTIME_DIR).join("bin");
    fs::create_dir_all(&bin).expect("bundled runtime layout is creatable");
    let bundled = version_shim(&bin, "python3", "3.12.4");

    // Newer on PATH, and it still loses: a shipped artefact runs on the
    // runtime it was tested with, not on whatever the machine happens to have.
    let search = scratch.subdir("bin");
    version_shim(&search, "python3", "3.13.9");

    let environment = DiscoveryEnvironment::empty()
        .with_executable_dir(&install)
        .with_search_path([search]);

    let interpreter = discover_interpreter_in(&RuntimeProfile::LegacyCompatibility, &environment)
        .expect("a staged bundled runtime resolves with nothing configured");

    assert_eq!(
        interpreter.path(),
        bundled,
        "the legacy layer prefers the shipped runtime over PATH, exactly as the modern host does"
    );
    assert_eq!(interpreter.version(), PythonVersion::new(3, 12, 4));
    assert_eq!(
        interpreter.source(),
        InterpreterSource::BundledRuntime,
        "discovery reports that the bundled runtime selected the interpreter"
    );
}

#[cfg(unix)]
#[test]
fn an_override_and_an_external_profile_both_still_outrank_the_bundled_runtime() {
    let scratch = Scratch::new("discovery-bundled-outranked");
    let install = scratch.subdir("install");
    let bin = install.join(BUNDLED_RUNTIME_DIR).join("bin");
    fs::create_dir_all(&bin).expect("bundled runtime layout is creatable");
    version_shim(&bin, "python3", "3.12.4");
    let override_python = version_shim(&scratch.path, "override-python", "3.11.5");
    let profile_python = version_shim(&scratch.path, "profile-python", "3.13.2");

    let base = DiscoveryEnvironment::empty().with_executable_dir(&install);

    let overridden = discover_interpreter_in(
        &RuntimeProfile::External(profile_python.clone()),
        &base.clone().with_override(&override_python),
    )
    .expect("an override naming a usable interpreter resolves");
    assert_eq!(
        overridden.path(),
        override_python,
        "CRIKEY_PYTHON is still rule one"
    );

    let profiled = discover_interpreter_in(&RuntimeProfile::External(profile_python.clone()), &base)
        .expect("an external runtime profile naming a usable interpreter resolves");
    assert_eq!(
        profiled.path(),
        profile_python,
        "an explicitly named interpreter still outranks the shipped runtime"
    );
}

#[cfg(unix)]
#[test]
fn a_build_with_no_staged_runtime_discovers_exactly_what_it_did_before() {
    let scratch = Scratch::new("discovery-bundled-absent");
    let install = scratch.subdir("install");
    let search = scratch.subdir("bin");
    let chosen = version_shim(&search, "python3", "3.10.3");

    let environment = DiscoveryEnvironment::empty()
        .with_executable_dir(&install)
        .with_search_path([search]);

    let interpreter = discover_interpreter_in(&RuntimeProfile::LegacyCompatibility, &environment)
        .expect("with no runtime staged, the search path is still the final rule");

    assert_eq!(interpreter.path(), chosen);
    assert_eq!(interpreter.source(), InterpreterSource::SearchPath);
}

#[cfg(unix)]
#[test]
fn a_bundled_runtime_below_the_minimum_version_is_an_error_not_a_fall_through_to_path() {
    let scratch = Scratch::new("discovery-bundled-old");
    let install = scratch.subdir("install");
    let bin = install.join(BUNDLED_RUNTIME_DIR).join("bin");
    fs::create_dir_all(&bin).expect("bundled runtime layout is creatable");
    let bundled = version_shim(&bin, "python3", "3.7.9");

    // A supported interpreter is on PATH. Falling through to it would run
    // legacy plugin code on a runtime the artefact was never validated with.
    let search = scratch.subdir("bin");
    version_shim(&search, "python3", "3.12.1");

    let error = discover_interpreter_in(
        &RuntimeProfile::LegacyCompatibility,
        &DiscoveryEnvironment::empty()
            .with_executable_dir(&install)
            .with_search_path([search]),
    )
    .expect_err("a bundled runtime below the minimum cannot resolve");

    match &error {
        WorkerError::UnsupportedVersion { path, found, minimum } => {
            assert_eq!(
                path, &bundled,
                "the failure names the staged interpreter it probed"
            );
            assert_eq!(*found, PythonVersion::new(3, 7, 9));
            assert_eq!(*minimum, MINIMUM_SUPPORTED_PYTHON);
        }
        other => panic!("an old bundled runtime is UnsupportedVersion, got {other:?}"),
    }
}
#[cfg(unix)]
#[test]
fn an_interpreter_below_the_minimum_version_is_rejected_with_both_the_found_and_minimum_versions() {
    let scratch = Scratch::new("discovery-old");
    let old = version_shim(&scratch.path, "python3.7", "3.7.9");

    let environment = DiscoveryEnvironment::empty().with_override(&old);

    let error = discover_interpreter_in(&RuntimeProfile::LegacyCompatibility, &environment)
        .expect_err("Python 3.7 is below the minimum supported version");

    match &error {
        WorkerError::UnsupportedVersion { path, found, minimum } => {
            assert_eq!(path, &old, "the failure names the interpreter it probed");
            assert_eq!(
                *found,
                PythonVersion::new(3, 7, 9),
                "the failure carries the version that was found"
            );
            assert_eq!(
                *minimum, MINIMUM_SUPPORTED_PYTHON,
                "the failure carries the minimum CriKey supports, so a diagnostic can state both"
            );
        }
        other => panic!("an old interpreter is UnsupportedVersion, got {other:?}"),
    }

    assert_eq!(
        MINIMUM_SUPPORTED_PYTHON,
        PythonVersion::new(3, 8, 0),
        "the documented minimum supported CPython for legacy plugins is 3.8"
    );

    let message = error.to_string();
    assert!(
        message.contains("3.7.9") && message.contains("3.8.0"),
        "the message states both the version found and the minimum required, got {message}"
    );
}

#[test]
fn the_interpreter_on_this_host_is_discovered_and_meets_the_minimum_supported_version() {
    let interpreter = host_interpreter();

    assert!(
        interpreter.path().exists(),
        "discovery resolved to an interpreter that is not on disk: {}",
        interpreter.path().display()
    );
    assert!(
        interpreter.version() >= MINIMUM_SUPPORTED_PYTHON,
        "this host's interpreter {} reports {} which is below the supported minimum {}",
        interpreter.path().display(),
        interpreter.version(),
        MINIMUM_SUPPORTED_PYTHON
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
    // environment. This is what lets every other test use the explicit form
    // without diverging from production behaviour.
    let ambient = discover_interpreter_in(
        &RuntimeProfile::LegacyCompatibility,
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
// The process boundary (spec 4.2)
// ---------------------------------------------------------------------------

#[test]
fn a_spawned_worker_runs_in_a_separate_operating_system_process() {
    let scratch = Scratch::new("separate-process");
    let package = legacy_package(&scratch, "pidfixture", &source(PID_WITNESS));
    let mut worker = LegacyWorker::spawn(&host_interpreter(), &package, options("legacy.pid"))
        .expect("a loadable package spawns a worker");

    assert_ne!(
        worker.process_id(),
        std::process::id(),
        "plugin code must never run in the CriKey process (spec 4.2)"
    );

    call_ok(&mut worker, request("legacy.pid", 1, LegacyRequestKind::Start));
    let response = call_ok(&mut worker, request("legacy.pid", 1, LegacyRequestKind::Catalog));

    // A pid the host merely made up would satisfy the inequality above. The
    // plugin's own view of its pid is what makes the reported one accountable.
    let observed: u32 = catalog(&response)[0]
        .target
        .parse()
        .expect("the fixture reports its own pid as an integer");
    assert_eq!(
        observed,
        worker.process_id(),
        "the pid the host reports is the pid the plugin actually runs under"
    );

    worker.shutdown().expect("a live worker shuts down");
}

#[test]
fn a_started_plugin_returns_its_catalog_with_every_field_intact() {
    let scratch = Scratch::new("catalog");
    let package = legacy_package(&scratch, "catalogfixture", &source(CATALOG_FIDELITY));
    let plugin = PluginId("legacy.catalog".to_owned());
    let mut worker = LegacyWorker::spawn(&host_interpreter(), &package, options("legacy.catalog"))
        .expect("a loadable package spawns a worker");

    call_ok(
        &mut worker,
        request("legacy.catalog", 1, LegacyRequestKind::Start),
    );
    let response = call_ok(
        &mut worker,
        request("legacy.catalog", 1, LegacyRequestKind::Catalog),
    );

    let items = catalog(&response);
    assert_eq!(items.len(), 2, "both catalog items survive the transport");

    let item = &items[0];
    // A label carrying a newline is the framing assertion: the protocol is one
    // JSON object per line, so an unescaped newline inside a value would split
    // one frame into two and desynchronise the stream for good.
    assert_eq!(
        item.label, "Ünïcøde \"quoted\" line one\nline two",
        "a label containing a newline, a quote and non-ASCII text crosses unchanged"
    );
    assert_eq!(
        item.description, "описание — с тире",
        "short_desc arrives as the item description"
    );
    assert_eq!(item.target, "fixture/target path", "target crosses unchanged");
    assert_eq!(
        item.category,
        Category::Keyword,
        "ItemCategory.KEYWORD maps to the keyword category"
    );
    assert_eq!(
        item.argument_policy,
        ArgumentPolicy::Required,
        "ItemArgsHint.REQUIRED maps to a required argument policy"
    );
    assert_eq!(
        item.hit_policy,
        HitPolicy::Ignored,
        "ItemHitHint.IGNORE maps to an ignored hit policy"
    );

    // Ownership and identity are the host's to assign, never the plugin's to
    // claim: a plugin that could name another plugin's id could inject items
    // into its catalog (spec 10.2).
    assert_eq!(
        item.plugin_id, plugin,
        "the host attributes every item to the plugin whose worker produced it"
    );
    assert_eq!(
        item.stable_id,
        ItemId::derived(&plugin, &Category::Keyword, "fixture/target path"),
        "a legacy item carries no identifier of its own, so the host derives a stable one"
    );

    assert_eq!(
        items[1].target,
        package.root.content_root().to_string_lossy(),
        "the worker hands the child its package root through the spawn options"
    );

    worker.shutdown().expect("a live worker shuts down");
}

#[test]
fn caller_environment_cannot_replace_the_worker_entry_module() {
    let scratch = Scratch::new("reserved-env");
    let package = legacy_package(&scratch, "reservedfixture", &source(CATALOG_FIDELITY));
    let mut worker = LegacyWorker::spawn(
        &host_interpreter(),
        &package,
        options("legacy.reserved").with_env(ENV_MAIN_MODULE_PATH, "does-not-exist.py"),
    )
    .expect("protocol environment variables remain owned by the host");

    let response = worker
        .call(request("legacy.reserved", 1, LegacyRequestKind::Start))
        .unwrap_or_else(|error| panic!("reserved env test must start, got {error:?}"));
    assert!(
        matches!(response.outcome, LegacyOutcome::Acknowledged),
        "the package still imports when a caller tries to replace the reserved module path"
    );
    worker.shutdown().expect("a live worker shuts down");
}

#[test]
fn plugin_output_on_stdout_becomes_log_output_and_never_corrupts_the_protocol_stream() {
    let scratch = Scratch::new("stdout");
    let package = legacy_package(
        &scratch,
        "chattyfixture",
        r#"
import sys
import keypirinha as kp


class Fixture(kp.Plugin):
    def on_start(self):
        pass

    def on_catalog(self):
        print("chatter before")
        print("chatter with a } brace and a \" quote")
        sys.stdout.write("chatter without a newline")
        self.set_catalog([
            self.create_item(
                category=kp.ItemCategory.KEYWORD,
                label="survived",
                short_desc="the frame was not eaten by the chatter",
                target="survived-target",
                args_hint=kp.ItemArgsHint.FORBIDDEN,
                hit_hint=kp.ItemHitHint.NOARGS),
        ])
        print("chatter after")
"#,
    );
    let mut worker = LegacyWorker::spawn(&host_interpreter(), &package, options("legacy.chatty"))
        .expect("a loadable package spawns a worker");

    call_ok(&mut worker, request("legacy.chatty", 1, LegacyRequestKind::Start));
    let response = call_ok(
        &mut worker,
        request("legacy.chatty", 1, LegacyRequestKind::Catalog),
    );

    // The call succeeding at all is the contract: stdout is a strict protocol
    // channel, so a plugin's `print` has to be somewhere else entirely.
    let items = catalog(&response);
    assert_eq!(items.len(), 1, "the printed text was not parsed as a frame");
    assert_eq!(
        items[0].label, "survived",
        "the reply frame arrives intact despite the plugin writing to stdout"
    );

    let log = response.log.join("\n");
    for expected in [
        "chatter before",
        "chatter with a } brace and a \" quote",
        "chatter without a newline",
        "chatter after",
    ] {
        assert!(
            log.contains(expected),
            "plugin output is preserved as log output; {expected:?} missing from {log:?}"
        );
    }

    worker.shutdown().expect("a live worker shuts down");
}

#[test]
fn a_suggestion_batch_is_tagged_with_the_generation_of_the_query_that_asked_for_it() {
    let scratch = Scratch::new("suggest");
    let package = legacy_package(
        &scratch,
        "suggestfixture",
        r#"
import keypirinha as kp


class Fixture(kp.Plugin):
    def on_start(self):
        pass

    def on_catalog(self):
        self.set_catalog([])

    def on_suggest(self, user_input, items_chain):
        self.set_suggestions([
            self.create_item(
                category=kp.ItemCategory.EXPRESSION,
                label="suggestion for " + user_input,
                short_desc="dynamic",
                target=user_input,
                args_hint=kp.ItemArgsHint.FORBIDDEN,
                hit_hint=kp.ItemHitHint.IGNORE),
        ], kp.Match.ANY, kp.Sort.NONE)
"#,
    );
    let mut worker = LegacyWorker::spawn(&host_interpreter(), &package, options("legacy.suggest"))
        .expect("a loadable package spawns a worker");

    call_ok(
        &mut worker,
        request("legacy.suggest", 1, LegacyRequestKind::Start),
    );

    // A generation far from the request ordinal: a reply that echoed a counter
    // of its own, or the previous request's generation, would still look
    // plausible against a small number.
    let response = call_ok(
        &mut worker,
        request(
            "legacy.suggest",
            4_097,
            LegacyRequestKind::InitialSuggest {
                query: "keyword arg".to_owned(),
            },
        ),
    );

    assert_eq!(
        response.generation,
        Generation::from_raw(4_097),
        "a suggestion batch is tagged with the generation of the query that requested it, \
         so the aggregator can reject it when stale (acceptance 31.7)"
    );

    match &response.outcome {
        LegacyOutcome::Suggestions(items) => {
            assert_eq!(items.len(), 1, "the suggestion batch arrives");
            assert_eq!(
                items[0].label, "suggestion for keyword arg",
                "the query text reaches on_suggest unchanged"
            );
        }
        other => panic!("on_suggest answers with suggestions, got {other:?}"),
    }

    worker.shutdown().expect("a live worker shuts down");
}

#[test]
fn an_execute_request_delivers_the_selected_item_and_its_action_to_the_plugin() {
    let scratch = Scratch::new("execute");
    let package = legacy_package(
        &scratch,
        "executefixture",
        r#"
import keypirinha as kp


class Fixture(kp.Plugin):
    def on_start(self):
        pass

    def on_catalog(self):
        self.set_catalog([])

    def on_execute(self, item, action):
        print("executed target=" + item.target())
        print("executed label=" + item.label())
        print("executed action=" + ("none" if action is None else action.name()))
"#,
    );
    let mut worker = LegacyWorker::spawn(&host_interpreter(), &package, options("legacy.execute"))
        .expect("a loadable package spawns a worker");

    call_ok(
        &mut worker,
        request("legacy.execute", 1, LegacyRequestKind::Start),
    );

    let plugin = PluginId("legacy.execute".to_owned());
    let selected = Item {
        stable_id: ItemId::derived(&plugin, &Category::Command, "run-me"),
        plugin_id: plugin,
        category: Category::Command,
        label: "Run me".to_owned(),
        description: "the row the user picked".to_owned(),
        target: "run-me".to_owned(),
        search_terms: Vec::new(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Optional,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: Default::default(),
        actions: Vec::new(),
    };
    let action = Action {
        action_id: ActionId("open-containing-folder".to_owned()),
        label: "Open containing folder".to_owned(),
        description: "secondary action".to_owned(),
        applicable_categories: vec![Category::Command],
        icon_reference: None,
        execution_policy: ExecutionPolicy::Plugin,
    };

    let response = call_ok(
        &mut worker,
        request(
            "legacy.execute",
            9,
            LegacyRequestKind::Execute {
                item: Box::new(selected),
                action: Some(action),
            },
        ),
    );

    assert!(
        matches!(response.outcome, LegacyOutcome::Executed),
        "on_execute is acknowledged as executed, got {:?}",
        response.outcome
    );

    let log = response.log.join("\n");
    for expected in [
        "executed target=run-me",
        "executed label=Run me",
        "executed action=open-containing-folder",
    ] {
        assert!(
            log.contains(expected),
            "the selected item and its action reach on_execute; {expected:?} missing from {log:?}"
        );
    }

    worker.shutdown().expect("a live worker shuts down");
}

#[test]
fn activation_deactivation_and_event_callbacks_are_acknowledged() {
    let scratch = Scratch::new("lifecycle");
    let package = legacy_package(
        &scratch,
        "lifecyclefixture",
        r#"
import keypirinha as kp


class Fixture(kp.Plugin):
    def on_start(self):
        pass

    def on_catalog(self):
        self.set_catalog([])

    def on_activated(self):
        print("activated")

    def on_deactivated(self):
        print("deactivated")

    def on_events(self, flags):
        # int(), never str(). kp.Events is an enum.IntFlag, and IntFlag.__str__
        # changed in Python 3.11: str() yields "66" on 3.11+ but
        # "Events.PACKAGE_CONFIG|FILESYSTEM" on 3.8-3.10. int() is identical on
        # every version from the 3.8 floor up, so this fixture cannot pass on
        # the development host and quietly fail under an older interpreter
        # reached through CRIKEY_PYTHON or RuntimeProfile::External.
        print("events=" + str(int(flags)))
"#,
    );
    let mut worker = LegacyWorker::spawn(&host_interpreter(), &package, options("legacy.lifecycle"))
        .expect("a loadable package spawns a worker");

    call_ok(
        &mut worker,
        request("legacy.lifecycle", 1, LegacyRequestKind::Start),
    );

    for (kind, callback, expected) in [
        (
            LegacyRequestKind::Activated,
            LegacyCallback::OnActivated,
            "activated",
        ),
        (
            LegacyRequestKind::Deactivated,
            LegacyCallback::OnDeactivated,
            "deactivated",
        ),
    ] {
        let response = call_ok(&mut worker, request("legacy.lifecycle", 2, kind));
        assert!(
            matches!(response.outcome, LegacyOutcome::Acknowledged),
            "{callback:?} is acknowledged, got {:?}",
            response.outcome
        );
        assert!(
            response.log.join("\n").contains(expected),
            "{callback:?} reached the plugin"
        );
    }

    // A real flag set, never `empty()`: an empty set is never delivered as an
    // on_events callback (spec 14.6), so it would not exercise the path.
    let flags = LegacyEventFlags::PACKAGE_CONFIG | LegacyEventFlags::FILESYSTEM;
    let response = call_ok(
        &mut worker,
        request("legacy.lifecycle", 3, LegacyRequestKind::Events { flags }),
    );
    assert!(
        matches!(response.outcome, LegacyOutcome::Acknowledged),
        "on_events is acknowledged, got {:?}",
        response.outcome
    );
    assert!(
        response
            .log
            .join("\n")
            .contains(&format!("events={}", flags.bits())),
        "the event flags reach on_events as their documented bit set, got {:?}",
        response.log
    );

    worker.shutdown().expect("a live worker shuts down");
}

// ---------------------------------------------------------------------------
// Fault isolation (spec 24.1, acceptance 31.9, 31.10)
// ---------------------------------------------------------------------------

/// A plugin whose catalog callback raises, and whose activation callback does
/// not. The second callback is what proves the worker outlived the first.
const RAISING: &str = r#"
import keypirinha as kp


class Fixture(kp.Plugin):
    def on_start(self):
        pass

    def on_catalog(self):
        self.deliberately_fail()

    def deliberately_fail(self):
        raise ValueError("fixture raised on purpose")

    def on_activated(self):
        print("still alive")
"#;

/// A package whose module raises before it can define a plugin class.
const IMPORT_RAISES: &str = r#"
raise RuntimeError("fixture import failed")
"#;

#[test]
fn an_import_failure_is_a_plugin_failure_and_not_a_worker_crash() {
    let scratch = Scratch::new("import-raises");
    let package = legacy_package(&scratch, "importraises", IMPORT_RAISES);
    let mut worker = LegacyWorker::spawn(&host_interpreter(), &package, options("legacy.import"))
        .expect("the shim handshake is independent of plugin import");

    let response = call_ok(&mut worker, request("legacy.import", 1, LegacyRequestKind::Start));
    let failure = match response.outcome {
        LegacyOutcome::Failed(failure) => failure,
        other => panic!("an import exception is a typed plugin failure, got {other:?}"),
    };
    assert_eq!(failure.exception_type, "RuntimeError");
    assert_eq!(failure.message, "fixture import failed");
    assert!(
        worker.is_running(),
        "a module import failure must not take down the isolated worker"
    );
    worker.shutdown().expect("a worker survives an import failure");
}

#[test]
fn an_exception_raised_in_a_callback_is_reported_with_its_type_message_and_traceback() {
    let scratch = Scratch::new("raising");
    let package = legacy_package(&scratch, "raisingfixture", RAISING);
    let mut worker = LegacyWorker::spawn(&host_interpreter(), &package, options("legacy.raising"))
        .expect("a loadable package spawns a worker");

    call_ok(
        &mut worker,
        request("legacy.raising", 1, LegacyRequestKind::Start),
    );

    // A plugin bug is not a transport failure: the call itself succeeded, and
    // reporting it as `Err` would make an unhealthy plugin indistinguishable
    // from an unhealthy worker.
    let response = call_ok(
        &mut worker,
        request("legacy.raising", 1, LegacyRequestKind::Catalog),
    );

    let failure: &PluginException = match &response.outcome {
        LegacyOutcome::Failed(failure) => failure,
        other => panic!("a raising callback reports a typed plugin failure, got {other:?}"),
    };

    assert_eq!(
        failure.exception_type, "ValueError",
        "the plugin failure carries the Python exception type"
    );
    assert_eq!(
        failure.message, "fixture raised on purpose",
        "the plugin failure carries the exception message"
    );
    assert!(
        failure.traceback.contains("deliberately_fail"),
        "the plugin failure carries a traceback naming the frame that raised, got {:?}",
        failure.traceback
    );
    assert_eq!(
        failure.plugin,
        PluginId("legacy.raising".to_owned()),
        "the failure is attributable to the plugin that raised it (spec 26.2)"
    );
    assert_eq!(
        failure.callback,
        LegacyCallback::OnCatalog,
        "the failure names the callback that raised"
    );

    worker.shutdown().expect("a live worker shuts down");
}

#[test]
fn a_worker_stays_usable_for_the_next_call_after_a_plugin_raises() {
    let scratch = Scratch::new("raising-recovery");
    let package = legacy_package(&scratch, "raisingfixture", RAISING);
    let mut worker = LegacyWorker::spawn(&host_interpreter(), &package, options("legacy.recovery"))
        .expect("a loadable package spawns a worker");

    call_ok(
        &mut worker,
        request("legacy.recovery", 1, LegacyRequestKind::Start),
    );
    call_ok(
        &mut worker,
        request("legacy.recovery", 1, LegacyRequestKind::Catalog),
    );

    assert!(
        worker.is_running(),
        "a plugin exception must not take the worker down with it (spec 24.1)"
    );

    let response = call_ok(
        &mut worker,
        request("legacy.recovery", 2, LegacyRequestKind::Activated),
    );
    assert!(
        matches!(response.outcome, LegacyOutcome::Acknowledged),
        "the worker serves the next callback normally, got {:?}",
        response.outcome
    );
    assert!(
        response.log.join("\n").contains("still alive"),
        "the plugin itself is still running, not merely the worker process"
    );

    worker
        .shutdown()
        .expect("a worker that survived a plugin fault shuts down");
}

#[test]
fn a_plugin_process_that_exits_mid_call_is_reported_as_crashed_and_attributed_to_that_plugin() {
    let scratch = Scratch::new("crash");
    let package = legacy_package(
        &scratch,
        "crashfixture",
        r#"
import os
import keypirinha as kp


class Fixture(kp.Plugin):
    def on_start(self):
        pass

    def on_catalog(self):
        # Not an exception: the interpreter is gone before it can report
        # anything, which is what an interpreter crash looks like (spec 24.1).
        os._exit(17)
"#,
    );
    let mut worker = LegacyWorker::spawn(&host_interpreter(), &package, options("legacy.crash"))
        .expect("a loadable package spawns a worker");
    let child = worker.process_id();

    call_ok(&mut worker, request("legacy.crash", 1, LegacyRequestKind::Start));

    // Returning at all is half the contract: the host must neither panic nor
    // wait forever on a pipe whose writer will never write again.
    let error = call_err(
        &mut worker,
        request("legacy.crash", 1, LegacyRequestKind::Catalog),
    );

    match &error {
        WorkerError::Crashed {
            plugin,
            callback,
            status,
            ..
        } => {
            assert_eq!(
                plugin,
                &PluginId("legacy.crash".to_owned()),
                "the crash is attributed to the plugin whose worker died (acceptance 31.9)"
            );
            assert_eq!(
                *callback,
                LegacyCallback::OnCatalog,
                "the crash names the callback that was in flight"
            );
            assert_eq!(
                *status,
                Some(17),
                "the crash carries the exit status the child reported"
            );
        }
        other => panic!("a worker that exits mid-call is Crashed, got {other:?}"),
    }

    assert!(
        error.to_string().contains("legacy.crash"),
        "the message names the plugin concerned, got {error}"
    );
    assert!(
        !worker.is_running(),
        "a crashed worker is not reported as running"
    );
    assert_ne!(
        process_table_contains(child),
        Some(true),
        "a crashed child is reaped rather than left as a zombie (spec 24.3)"
    );

    worker
        .shutdown()
        .expect("shutting down an already dead worker is not itself a failure");
}

// ---------------------------------------------------------------------------
// Cooperative cancellation and the hard bound (acceptance 31.17)
// ---------------------------------------------------------------------------

#[test]
fn a_cooperative_plugin_observes_the_termination_flag_raised_from_another_thread() {
    let scratch = Scratch::new("cooperative");
    let rendezvous = Rendezvous::new(&scratch);
    let package = legacy_package(
        &scratch,
        "cooperativefixture",
        &source(
            r#"
import os
import keypirinha as kp


class Fixture(kp.Plugin):
    def on_start(self):
        pass

    def on_catalog(self):
        # Announce that the callback is running, then spin until the host asks
        # for it to stop. The announcement is what lets the host signal a
        # callback that is genuinely in flight.
        with open(os.environ["__FIFO__"], "w") as handle:
            handle.write("started")
        while not kp.should_terminate():
            pass
        self.set_catalog([
            self.create_item(
                category=kp.ItemCategory.KEYWORD,
                label="terminated",
                short_desc="should_terminate() went true",
                target="terminated",
                args_hint=kp.ItemArgsHint.FORBIDDEN,
                hit_hint=kp.ItemHitHint.NOARGS),
        ])

"#,
        ),
    );

    let mut worker = LegacyWorker::spawn(
        &host_interpreter(),
        &package,
        options("legacy.cooperative").with_env(FIFO_VAR, rendezvous.path.as_os_str()),
    )
    .expect("a loadable package spawns a worker");

    call_ok(
        &mut worker,
        request("legacy.cooperative", 1, LegacyRequestKind::Start),
    );

    // The handle crosses to another thread on purpose: the thread that raises
    // the flag can never be the thread blocked inside `call`.
    let handle = worker.terminate_handle();
    let signalled = rendezvous.on_start(move || handle.signal());

    let response = call_ok(
        &mut worker,
        request("legacy.cooperative", 1, LegacyRequestKind::Catalog),
    );

    signalled
        .recv_timeout(RESPONSE_LIMIT)
        .expect("the fixture announced its callback and the flag was raised");

    let items = catalog(&response);
    assert_eq!(
        items.len(),
        1,
        "the plugin returned normally after observing the flag"
    );
    assert_eq!(
        items[0].label, "terminated",
        "terminate_handle().signal() makes the plugin's should_terminate() return true \
         (acceptance 31.17)"
    );
    assert!(
        worker.terminate_handle().is_signalled(),
        "the raised flag is observable through the handle"
    );

    worker.shutdown().expect("a live worker shuts down");
}

#[test]
fn a_plugin_that_ignores_cooperative_termination_is_stopped_by_the_hosts_hard_bound() {
    let scratch = Scratch::new("uncooperative");
    let rendezvous = Rendezvous::new(&scratch);
    let package = legacy_package(
        &scratch,
        "uncooperativefixture",
        &source(
            r#"
import os
import keypirinha as kp


class Fixture(kp.Plugin):
    def on_start(self):
        pass

    def on_catalog(self):
        with open(os.environ["__FIFO__"], "w") as handle:
            handle.write("started")
        # Never consults should_terminate(). Cooperation cannot be assumed, so
        # the host needs a bound that does not depend on it.
        while True:
            pass
"#,
        ),
    );

    let mut worker = LegacyWorker::spawn(
        &host_interpreter(),
        &package,
        options("legacy.uncooperative")
            .with_call_timeout_ms(HARD_BOUND_MS)
            .with_env(FIFO_VAR, rendezvous.path.as_os_str()),
    )
    .expect("a loadable package spawns a worker");
    let child = worker.process_id();

    call_ok(
        &mut worker,
        request("legacy.uncooperative", 1, LegacyRequestKind::Start),
    );

    let handle = worker.terminate_handle();
    let _signalled = rendezvous.on_start(move || handle.signal());

    // The bound under test is the host's own, so this test must not rely on it
    // to finish: the call runs on a worker thread and the assertion is the
    // arrival of its result, not the call returning. A host with no hard bound
    // fails here instead of hanging the suite.
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let outcome = worker.call(request("legacy.uncooperative", 1, LegacyRequestKind::Catalog));
        let _ = sender.send((outcome, worker));
    });

    let (outcome, mut worker) = receiver.recv_timeout(RESPONSE_LIMIT).expect(
        "one plugin that refuses to stop must not be able to hang the host forever \
         (spec 24.1, acceptance 31.17)",
    );

    match outcome {
        Ok(response) => panic!(
            "a plugin that never returns cannot produce a reply, got {:?}",
            response.outcome
        ),
        Err(WorkerError::Timeout {
            plugin,
            callback,
            waited_ms,
        }) => {
            assert_eq!(
                plugin,
                PluginId("legacy.uncooperative".to_owned()),
                "the timeout is attributed to the plugin that would not stop"
            );
            assert_eq!(
                callback,
                LegacyCallback::OnCatalog,
                "the timeout names the callback that overran"
            );
            assert!(
                waited_ms >= HARD_BOUND_MS,
                "the host waited at least the bound it was given: {waited_ms}ms < {HARD_BOUND_MS}ms"
            );
        }
        Err(other) => panic!("an overrunning plugin is reported as Timeout, got {other:?}"),
    }

    assert!(
        !worker.is_running(),
        "the hard bound stops the child; a plugin that ignores cooperative \
         termination must not survive its deadline"
    );
    assert_ne!(
        process_table_contains(child),
        Some(true),
        "the hard-stopped child is reaped, not left behind"
    );

    worker
        .shutdown()
        .expect("shutting down after a hard stop is not itself a failure");
}

// ---------------------------------------------------------------------------
// Protocol channel integrity
//
// A correct shim never emits either of these, so the peer here is a stand-in
// that is wrong on purpose. What is under test is the host's decoder: a
// desynchronised channel must be a named error, not a panic and not silence.
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn protocol_failure(label: &str, reply: &str) -> WorkerError {
    let scratch = Scratch::new(label);

    let interpreter_dir = scratch.subdir("bin");
    let fake = hostile_worker(&interpreter_dir, "python3", "3.11.4", reply);
    let package = legacy_package(&scratch, "hostilefixture", &source(PID_WITNESS));

    let interpreter =
        discover_interpreter_in(&RuntimeProfile::External(fake), &DiscoveryEnvironment::empty())
            .expect("the stand-in reports a supported version and so passes discovery");

    let mut worker = LegacyWorker::spawn(&interpreter, &package, options("legacy.hostile"))
        .expect("the stand-in completes the startup handshake, so the worker spawns");

    let error = call_err(
        &mut worker,
        request("legacy.hostile", 1, LegacyRequestKind::Catalog),
    );

    let _ = worker.shutdown();
    error
}

#[cfg(unix)]
fn protocol_failure_unterminated(label: &str, reply: &str) -> WorkerError {
    let scratch = Scratch::new(label);
    let interpreter_dir = scratch.subdir("bin");
    let fake = hostile_worker_unterminated(&interpreter_dir, "python3", "3.11.4", reply);
    let package = legacy_package(&scratch, "hostilefixture", &source(PID_WITNESS));

    let interpreter =
        discover_interpreter_in(&RuntimeProfile::External(fake), &DiscoveryEnvironment::empty())
            .expect("the stand-in reports a supported version and so passes discovery");

    let mut worker = LegacyWorker::spawn(&interpreter, &package, options("legacy.hostile"))
        .expect("the stand-in completes the startup handshake, so the worker spawns");

    let error = call_err(
        &mut worker,
        request("legacy.hostile", 1, LegacyRequestKind::Catalog),
    );

    let _ = worker.shutdown();
    error
}

#[cfg(unix)]
#[test]
fn a_malformed_line_on_the_protocol_channel_is_reported_as_a_protocol_error() {
    let error = protocol_failure("protocol-garbage", "this is not json");

    match &error {
        WorkerError::Protocol { plugin, line, .. } => {
            assert_eq!(
                plugin,
                &PluginId("legacy.hostile".to_owned()),
                "a protocol violation is attributed to the plugin whose worker committed it"
            );
            assert_eq!(
                line, "this is not json",
                "the failure carries the offending line so the diagnostic can quote it"
            );
        }
        other => panic!("a line that is not JSON is a Protocol error, got {other:?}"),
    }

    assert!(
        error.to_string().contains("legacy.hostile"),
        "the message names the plugin concerned, got {error}"
    );
}

#[cfg(unix)]
#[test]
fn a_partial_final_protocol_line_is_reported_instead_of_dropped() {
    let error = protocol_failure_unterminated("protocol-partial", "partial-json");

    match &error {
        WorkerError::Protocol { plugin, line, .. } => {
            assert_eq!(plugin, &PluginId("legacy.hostile".to_owned()));
            assert!(
                line.contains("partial-json"),
                "the unterminated payload remains visible in the diagnostic: {line}"
            );
            assert!(
                line.contains("unterminated"),
                "the diagnostic identifies end-of-stream framing: {line}"
            );
        }
        other => panic!("an unterminated line is a Protocol error, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn a_json_value_that_is_not_an_object_is_reported_as_a_protocol_error() {
    // Well-formed JSON, wrong shape. The protocol is one JSON *object* per
    // line, so a bare array is as much a violation as garbage — and a decoder
    // that only checked "does this parse" would let it through and then index
    // into something that has no fields.
    let error = protocol_failure("protocol-array", "[1, 2, 3]");

    match &error {
        WorkerError::Protocol { plugin, line, .. } => {
            assert_eq!(
                plugin,
                &PluginId("legacy.hostile".to_owned()),
                "a protocol violation is attributed to the plugin whose worker committed it"
            );
            assert_eq!(
                line, "[1, 2, 3]",
                "the failure carries the offending line even when it is valid JSON"
            );
        }
        other => panic!("a JSON value that is not an object is a Protocol error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Teardown (spec 24.3)
// ---------------------------------------------------------------------------

#[test]
fn shutdown_reaps_the_child_process_and_leaves_no_orphan() {
    let scratch = Scratch::new("shutdown");
    let package = legacy_package(&scratch, "pidfixture", &source(PID_WITNESS));
    let mut worker = LegacyWorker::spawn(&host_interpreter(), &package, options("legacy.shutdown"))
        .expect("a loadable package spawns a worker");
    let child = worker.process_id();

    call_ok(
        &mut worker,
        request("legacy.shutdown", 1, LegacyRequestKind::Start),
    );
    assert!(worker.is_running(), "the worker is live before shutdown");

    let exit: WorkerExit = worker.shutdown().expect("a live worker shuts down cleanly");

    assert_eq!(
        exit.code,
        Some(0),
        "a worker asked to stop exits cleanly rather than being killed"
    );
    assert!(
        !exit.hard_stopped,
        "a cooperative worker is not hard-stopped during an orderly shutdown"
    );

    // Reaped, not merely dead: an exited child that was never waited on stays
    // in the process table as a zombie, and a launcher that leaks one per
    // plugin reload leaks them forever.
    assert_ne!(
        process_table_contains(child),
        Some(true),
        "shutdown reaps the child; pid {child} is still in the process table"
    );
}

// ---------------------------------------------------------------------------
// The presentation APIs (spec 11.7, 14.4)
//
// Alternate actions, icon handles, error items and resource enumeration are
// the four APIs the real-plugin corpus is actually blocked on, and all four
// are only observable across the process boundary: the plugin registers them
// in the child and the host has to receive them on the items it decodes. The
// committed `rich-presentation` fixture is the peer, so these tests pin what a
// real package sees rather than what a purpose-built string of Python does.
// ---------------------------------------------------------------------------

/// The committed synthetic package `compatibility/test-plugins/<name>`.
///
/// Loaded from the repository rather than written into scratch space: these
/// contracts are about files that ship — an icon with real bytes, resources in
/// a subdirectory — and a fixture invented per test could not prove the
/// committed one still works.
fn committed_package(scratch: &Scratch, name: &str) -> LegacyPackage {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate lives two directories below the workspace root")
        .join("compatibility/test-plugins")
        .join(name);
    assert!(
        root.is_dir(),
        "the synthetic legacy package `{name}` is missing from {}; these tests do not skip",
        root.display()
    );

    PackageLoader::new(scratch.join("package-cache"))
        .load(&root)
        .expect("a committed synthetic package is loadable")
}

const RICH: &str = "legacy.rich-presentation";

/// Starts the fixture and returns what one `on_suggest` published.
///
/// `on_start` first, and not merely for tidiness: the fixture registers its
/// actions and its icon there, exactly as published packages do, so a host
/// that dropped the registration between callbacks would produce bare items
/// here.
fn rich_suggestions(worker: &mut LegacyWorker) -> Vec<Item> {
    call_ok(worker, request(RICH, 1, LegacyRequestKind::Start));
    let response = call_ok(
        worker,
        request(
            RICH,
            2,
            LegacyRequestKind::InitialSuggest {
                query: "rich".to_owned(),
            },
        ),
    );
    match response.outcome {
        LegacyOutcome::Suggestions(items) => items,
        other => panic!("the fixture publishes suggestions, got {other:?}"),
    }
}

fn item_with_target<'a>(items: &'a [Item], target: &str) -> &'a Item {
    items
        .iter()
        .find(|item| item.target == target)
        .unwrap_or_else(|| {
            panic!(
                "the fixture publishes an item targeting `{target}`; got {:?}",
                items.iter().map(|item| item.target.as_str()).collect::<Vec<_>>()
            )
        })
}

#[test]
fn registered_actions_reach_every_item_of_their_category_and_no_other() {
    let scratch = Scratch::new("legacy-actions");
    let package = committed_package(&scratch, "rich-presentation");
    let mut worker = LegacyWorker::spawn(&host_interpreter(), &package, options(RICH))
        .expect("the committed fixture spawns a worker");

    let items = rich_suggestions(&mut worker);
    let entry = item_with_target(&items, "rich-presentation/entry");

    assert_eq!(
        entry
            .actions
            .iter()
            .map(|action| action.action_id.0.as_str())
            .collect::<Vec<_>>(),
        vec!["copy", "reveal"],
        "the actions arrive in the order the plugin registered them, or the row's \
         alternates are offered in an order the author did not choose",
    );
    assert_eq!(entry.actions[1].label, "Reveal");
    assert_eq!(entry.actions[1].description, "Show where the target lives");
    assert!(
        entry
            .actions
            .iter()
            .all(|action| action.execution_policy == ExecutionPolicy::Plugin),
        "a legacy action is run by the plugin that registered it, never by the host",
    );

    // The error item is categorised ERROR, and `set_actions` registered
    // against KEYWORD only. A host that attached the list to every item would
    // offer "Copy" and "Reveal" on a failure message.
    let refusal = item_with_target(&items, "rich-presentation/escape-refused");
    assert!(
        refusal.actions.is_empty(),
        "actions are registered per category; the error item is in another one",
    );

    worker.shutdown().expect("the worker shuts down cleanly");
}

#[test]
fn executing_a_registered_action_hands_the_plugin_that_action_and_not_the_default() {
    let scratch = Scratch::new("legacy-action-execute");
    let package = committed_package(&scratch, "rich-presentation");
    let mut worker = LegacyWorker::spawn(&host_interpreter(), &package, options(RICH))
        .expect("the committed fixture spawns a worker");

    let items = rich_suggestions(&mut worker);
    let entry = item_with_target(&items, "rich-presentation/entry").clone();
    let reveal = entry
        .actions
        .iter()
        .find(|action| action.action_id == ActionId("reveal".to_owned()))
        .expect("the fixture registered `reveal`")
        .clone();

    let chosen = call_ok(
        &mut worker,
        request(
            RICH,
            3,
            LegacyRequestKind::Execute {
                item: Box::new(entry.clone()),
                action: Some(reveal),
            },
        ),
    );
    assert!(
        matches!(chosen.outcome, LegacyOutcome::Executed),
        "`on_execute` completed, got {:?}",
        chosen.outcome,
    );
    // The fixture echoes the action it was handed. Asserting on the echo, not
    // merely on "the callback ran", is what distinguishes delivering the right
    // action from delivering any action at all.
    assert!(
        chosen.log.iter().any(|line| line.contains("action=reveal")),
        "the plugin must receive the action the user chose; log: {:?}",
        chosen.log,
    );

    // `None` is the documented spelling of "the default action was taken", and
    // a plugin branches on it. It must not arrive as a synthesised action.
    let default = call_ok(
        &mut worker,
        request(
            RICH,
            4,
            LegacyRequestKind::Execute {
                item: Box::new(entry),
                action: None,
            },
        ),
    );
    assert!(
        default.log.iter().any(|line| line.contains("action=<default>")),
        "no chosen action must reach `on_execute` as None; log: {:?}",
        default.log,
    );

    worker.shutdown().expect("the worker shuts down cleanly");
}

#[test]
fn a_loaded_icon_crosses_as_a_package_relative_reference_the_host_can_resolve() {
    let scratch = Scratch::new("legacy-icon");
    let package = committed_package(&scratch, "rich-presentation");
    let mut worker = LegacyWorker::spawn(&host_interpreter(), &package, options(RICH))
        .expect("the committed fixture spawns a worker");

    let items = rich_suggestions(&mut worker);

    assert_eq!(
        item_with_target(&items, "rich-presentation/entry")
            .icon_reference
            .as_deref(),
        Some("icons/badge.png"),
        "an item built with an icon handle must name the file the host will read",
    );
    // `set_default_icon` is not decoration: an item that names no handle of its
    // own inherits it, which is the only way a package gives all its rows one
    // picture without repeating the handle on every item.
    assert_eq!(
        item_with_target(&items, "rich-presentation/resources")
            .icon_reference
            .as_deref(),
        Some("icons/badge.png"),
        "an item with no handle of its own inherits the plugin's default icon",
    );
    // The reference is package-relative and nothing else: an absolute path
    // would resolve against the host's filesystem rather than the package.
    assert!(
        !Path::new("icons/badge.png").is_absolute(),
        "the reference the host resolves is relative to the package directory",
    );

    worker.shutdown().expect("the worker shuts down cleanly");
}

#[test]
fn find_resources_reports_package_relative_names_and_refuses_to_leave_the_package() {
    let scratch = Scratch::new("legacy-resources");
    let package = committed_package(&scratch, "rich-presentation");
    let mut worker = LegacyWorker::spawn(&host_interpreter(), &package, options(RICH))
        .expect("the committed fixture spawns a worker");

    let items = rich_suggestions(&mut worker);

    assert_eq!(
        item_with_target(&items, "rich-presentation/resources").description,
        "icons/badge.png",
        "the names are package-relative, sorted, and exactly what the pattern matches",
    );

    // The escape probe publishes one of two mutually exclusive rows. Asserting
    // both directions is what stops this passing on a plugin that raised for
    // some unrelated reason, or on a `find_resources` that answered nothing.
    item_with_target(&items, "rich-presentation/escape-refused");
    assert!(
        !items
            .iter()
            .any(|item| item.target == "rich-presentation/escape-escaped"),
        "a `..` pattern must be refused, never walked; got {:?}",
        items.iter().map(|item| item.target.as_str()).collect::<Vec<_>>(),
    );

    worker.shutdown().expect("the worker shuts down cleanly");
}
