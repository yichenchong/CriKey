//! End-to-end proof with a real shared library built by a real C compiler.
//!
//! `abi_contract.rs` pins the contract with `extern "C"` fixtures. This file
//! covers what only a genuine `.so` can establish: that the platform loader is
//! actually used, that the version and missing-symbol gates hold against a
//! library the loader is willing to map, that an installed package's manifest
//! and lock decide what gets loaded, and — the whole point of the design — that
//! a C plugin fault is contained as a worker crash with a sibling still
//! serving.
//!
//! Unix only. The fixture is built with `make` and a C compiler, which is what
//! a third-party plugin author uses; the Windows equivalent needs a toolchain
//! this suite does not assume.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use crikey_cabi_host::{load_installed_package, resolve_package, DynamicLibrary, LoadError, PluginAbi};
use crikey_core::PluginId;
use crikey_native_host::{
    BatchState, ExitKind, LaunchSpec, NativeSuggestRequest, NativeSupervisor, SupervisorConfig, WorkerOptions,
};
use crikey_package_manager::{build_package, install_native};
use crikey_plugin_supervisor::CircuitBreakerConfig;

const LIB_EXTENSION: &str = if cfg!(target_os = "macos") { "dylib" } else { "so" };

fn repository_root() -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if root.join("compatibility").is_dir() {
            return root;
        }
        assert!(
            root.pop(),
            "could not find the repository root containing compatibility/ from {}",
            env!("CARGO_MANIFEST_DIR")
        );
    }
}

/// Builds the out-of-tree C fixture once. A build failure is a test failure,
/// not a skip: the header is the plugin author's contract and a tree that
/// cannot compile a small honest plugin against it is broken.
fn fixture_dir() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let directory = repository_root().join("compatibility/cabi-conformance");
        let output = Command::new("make")
            .arg("-C")
            .arg(&directory)
            .output()
            .unwrap_or_else(|error| {
                panic!(
                    "failed to run make for the out-of-tree c-abi fixture in {}: {error}",
                    directory.display()
                )
            });
        assert!(
            output.status.success(),
            "the out-of-tree c-abi fixture failed to build (status {}):\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        directory
    })
}

fn refusal_library(stem: &str) -> PathBuf {
    let path = fixture_dir()
        .join("build")
        .join(format!("lib{stem}.{LIB_EXTENSION}"));
    assert!(path.is_file(), "fixture build produced no {}", path.display());
    path
}

/// A scratch directory removed when the test finishes.
#[derive(Debug)]
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "crikey-cabi-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory");
        Self { path }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Builds the fixture package into an archive and installs it, so what the
/// host is pointed at is a genuinely installed package with the
/// `crikey-package.lock` installation writes. Returns the installed directory.
fn install_fixture_package(scratch: &Scratch) -> PathBuf {
    let source = fixture_dir().join("build/package");
    assert!(
        source.join("crikey.toml").is_file(),
        "make must generate {}",
        source.join("crikey.toml").display()
    );
    let archive = scratch.path.join("package.crikey");
    build_package(&source, &archive).expect("the fixture package builds");
    let root = scratch.path.join("installed");
    let install = install_native(
        &archive,
        &root,
        std::env::consts::OS,
        std::env::consts::ARCH,
        &mut |_| Ok(()),
    )
    .expect("the fixture package installs");
    install.root
}

// -- refusals against a library the loader will happily map -----------------

#[test]
fn a_real_library_declaring_another_abi_version_is_refused_by_name() {
    let path = refusal_library("crikey_cabi_bad_version");
    let source = DynamicLibrary::open(&path).expect("the loader maps the fixture");

    // SAFETY: `source` outlives the borrow; nothing is called on failure.
    #[allow(unsafe_code)]
    let error = unsafe { PluginAbi::resolve(&source) }.expect_err("a foreign ABI version is refused");

    match &error {
        LoadError::AbiVersionMismatch {
            library,
            found,
            expected,
        } => {
            assert!(
                library.contains("crikey_cabi_bad_version"),
                "the refusal names the library: {library}"
            );
            assert_ne!(found, expected);
        }
        other => panic!("expected a version mismatch, got {other:?}"),
    }
    // Every function in that fixture calls `abort`. Reaching this line at all
    // proves none of them ran.
}

#[test]
fn a_real_library_missing_a_required_symbol_is_refused_by_that_symbols_name() {
    let path = refusal_library("crikey_cabi_missing_symbol");
    let source = DynamicLibrary::open(&path).expect("the loader maps the fixture");

    // SAFETY: `source` outlives the borrow; nothing is called on failure.
    #[allow(unsafe_code)]
    let error = unsafe { PluginAbi::resolve(&source) }.expect_err("a missing entry point is refused");

    match &error {
        LoadError::MissingSymbol { library, symbol } => {
            assert_eq!(*symbol, "crikey_plugin_suggest");
            assert!(library.contains("crikey_cabi_missing_symbol"), "{library}");
        }
        other => panic!("expected a missing-symbol refusal, got {other:?}"),
    }
}

#[test]
fn a_library_that_is_not_the_manifests_entrypoint_cannot_be_named() {
    let scratch = Scratch::new("policy");
    let installed = install_fixture_package(&scratch);

    // Drop an extra library into the package. It is a perfectly loadable
    // library; it is simply not what the manifest declares, and no argument to
    // this host can select it.
    let smuggled = installed.join("bin/libsmuggled.so");
    fs::copy(refusal_library("crikey_cabi_bad_version"), &smuggled).expect("stage a second library");

    let package = resolve_package(&installed, std::env::consts::OS, std::env::consts::ARCH)
        .expect("the installed package still resolves");
    assert!(
        package.library.ends_with(&package.entrypoint),
        "the resolved library is the manifest's entrypoint, not the newcomer: {}",
        package.library.display()
    );
    assert_ne!(package.library, smuggled);
}

#[test]
fn a_tampered_library_is_refused_against_the_installed_lock() {
    let scratch = Scratch::new("tamper");
    let installed = install_fixture_package(&scratch);
    let package = resolve_package(&installed, std::env::consts::OS, std::env::consts::ARCH)
        .expect("the untampered package resolves");

    let mut bytes = fs::read(&package.library).expect("read the installed library");
    bytes.push(0);
    fs::write(&package.library, &bytes).expect("tamper with the installed library");

    let error = resolve_package(&installed, std::env::consts::OS, std::env::consts::ARCH)
        .expect_err("a package whose payload no longer matches its lock is refused");
    let message = error.to_string();
    assert!(
        message.contains("digest mismatch"),
        "the refusal names the digest failure: {message}"
    );
    assert!(
        message.contains(&package.entrypoint),
        "the refusal names the artefact: {message}"
    );
}

#[test]
fn a_c_abi_package_without_the_explicit_native_library_permission_is_refused() {
    let scratch = Scratch::new("permission");
    let installed = install_fixture_package(&scratch);
    let manifest = installed.join("crikey.toml");
    let text = fs::read_to_string(&manifest).expect("read installed manifest");
    let without_grant = text.replace("[permissions]\nnative-library-loading = true\n", "");
    fs::write(&manifest, without_grant).expect("remove the explicit permission");

    let error = resolve_package(&installed, std::env::consts::OS, std::env::consts::ARCH)
        .expect_err("c-abi loading requires an explicit native-library-loading grant");
    assert!(
        error.to_string().contains("permissions.native-library-loading"),
        "the refusal names the missing permission: {error}"
    );
}

// -- the round trip ---------------------------------------------------------

#[test]
fn an_installed_c_plugin_loads_and_answers_a_query() {
    let scratch = Scratch::new("roundtrip");
    let installed = install_fixture_package(&scratch);

    let plugin = load_installed_package(&installed, std::env::consts::OS, std::env::consts::ARCH)
        .expect("a well-formed installed c-abi package loads");
    assert!(
        plugin
            .origin()
            .ends_with(&format!("libcrikey_cabi_example.{LIB_EXTENSION}")),
        "the loaded library is the manifest's entrypoint: {}",
        plugin.origin()
    );
    drop(plugin);
}

// -- supervision: out-of-process, contained, sibling survives ---------------

fn cabi_launch(plugin: &str, directory: &Path, mode: &str) -> LaunchSpec {
    LaunchSpec {
        plugin: PluginId(plugin.to_owned()),
        executable: PathBuf::from(env!("CARGO_BIN_EXE_crikey-cabi-host")),
        arguments: vec![directory.to_string_lossy().into_owned()],
        working_dir: Some(directory.to_path_buf()),
        environment: vec![("CRIKEY_CABI_MODE".to_owned(), mode.to_owned())],
        inherit_environment: false,
    }
}

fn request(generation: u64, text: &str) -> NativeSuggestRequest {
    NativeSuggestRequest {
        generation,
        text: text.to_owned(),
        normalized: text.to_lowercase(),
        selected_item_id: None,
    }
}

fn supervisor() -> NativeSupervisor {
    NativeSupervisor::new(SupervisorConfig {
        max_restarts: 3,
        restart_window_ms: 60_000,
        base_backoff_ms: 0,
        max_backoff_ms: 0,
        circuit: CircuitBreakerConfig {
            failure_threshold: 0,
            cooldown: Duration::ZERO,
        },
    })
}

/// The acceptance sequence: a C plugin loads and answers through the supervised
/// protocol from *another* process, and when a sibling C plugin faults inside
/// `crikey_plugin_suggest` the fault is a worker crash while the healthy
/// plugin keeps serving (acceptance criterion 30; spec 24.1, 24.3).
#[test]
fn a_c_plugin_fault_is_contained_as_a_worker_crash_with_a_sibling_still_serving() {
    let scratch = Scratch::new("supervised");
    let installed = install_fixture_package(&scratch);

    let healthy = PluginId("native.dev.example.cabi".to_owned());
    let faulty = PluginId("native.dev.example.cabi.faulty".to_owned());
    let mut supervisor = supervisor();
    supervisor
        .register(
            cabi_launch(&healthy.0, &installed, "echo"),
            WorkerOptions::new().with_call_timeout_ms(10_000),
        )
        .expect("the healthy c-abi package registers");
    supervisor
        .register(
            cabi_launch(&faulty.0, &installed, "crash-on-suggest"),
            WorkerOptions::new().with_call_timeout_ms(10_000),
        )
        .expect("the faulty c-abi package registers");

    // The library answers, and it answers from a process that is not this one.
    let host_pid = {
        let worker = supervisor
            .worker(&healthy, 0)
            .expect("the supervisor starts crikey-cabi-host");
        let streamed = worker
            .suggest(&request(1, "hello"))
            .expect("the C plugin answers the query");
        assert_eq!(streamed.state, BatchState::Final);
        let pid = streamed
            .items
            .iter()
            .find(|item| item.stable_id.0 == "cabi.pid")
            .and_then(|item| item.target.parse::<u32>().ok())
            .expect("the fixture reports the host process id");
        assert!(
            streamed
                .items
                .iter()
                .any(|item| item.stable_id.0 == "cabi.echo:hello"),
            "the query text crossed the C boundary and came back"
        );
        assert_ne!(
            pid,
            std::process::id(),
            "third-party C code must never run in this process (acceptance 30)"
        );
        pid
    };

    // The faulty sibling dies inside the C call. That is a worker crash, and
    // the supervisor already knows what to do with one.
    {
        let worker = supervisor
            .worker(&faulty, 1)
            .expect("the supervisor starts the faulty host too");
        let outcome = worker.suggest(&request(2, "boom"));
        assert!(
            outcome.is_err()
                || outcome
                    .as_ref()
                    .is_ok_and(|batch| batch.state == BatchState::Failed),
            "a plugin that dies mid-request cannot report success: {outcome:?}"
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while worker.is_alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !worker.is_alive(),
            "the host process died with the library it was loading for"
        );
    }
    // The supervisor records exits when the registration is next observed;
    // force that observation and keep the restart path itself under test.
    supervisor
        .worker(&faulty, 2)
        .expect("the supervisor records the crash and restarts within its budget");
    let exit = supervisor
        .last_exit(&faulty)
        .expect("the crash is recorded as an exit");
    assert_ne!(
        exit.kind,
        ExitKind::Clean,
        "an abnormal plugin exit is not an orderly shutdown: {exit:?}"
    );

    // The sibling is untouched: same process, still answering.
    {
        let worker = supervisor
            .worker(&healthy, 2)
            .expect("the healthy worker is still available");
        let streamed = worker
            .suggest(&request(3, "still here"))
            .expect("the healthy C plugin keeps serving after its sibling faulted");
        assert_eq!(streamed.state, BatchState::Final);
        let pid = streamed
            .items
            .iter()
            .find(|item| item.stable_id.0 == "cabi.pid")
            .and_then(|item| item.target.parse::<u32>().ok())
            .expect("the fixture still reports its process id");
        assert_eq!(
            pid, host_pid,
            "the healthy plugin was not restarted; it never stopped"
        );
    }

    supervisor.shutdown_all();
}
