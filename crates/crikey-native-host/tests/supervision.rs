//! Red-first subprocess supervision tests for the native host (spec 16.6,
//! 24.1-24.4; acceptance 31.23).
//!
//! Every fixture is an actual out-of-tree executable.  A test that cannot build
//! the fixture fails loudly: silently replacing a missing plugin with a mock
//! would erase the process-boundary and crash-containment contracts these tests
//! are meant to pin.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use crikey_core::{ItemId, PluginId};
use crikey_native_host::{
    BatchState, ExitKind, HostError, LaunchSpec, NativeSuggestRequest, NativeSupervisor, NativeWorker,
    ResourceLimits, SupervisorConfig, TransportKind, WorkerOptions,
};
use crikey_plugin_supervisor::CircuitBreakerConfig;

const STARTUP_TIMEOUT_MS: u64 = 10_000;
const CALL_TIMEOUT_MS: u64 = 5_000;
const SHUTDOWN_TIMEOUT_MS: u64 = 2_000;
const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// Builds the out-of-tree conformance workspace once and returns both fixture
/// binaries.  Cargo's own lock serialises concurrent test binaries.
fn conformance_binaries() -> (PathBuf, PathBuf) {
    static BINARIES: LazyLock<(PathBuf, PathBuf)> = LazyLock::new(|| {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut workspace = manifest_dir;
        loop {
            if workspace.join("compatibility").is_dir() {
                break;
            }
            assert!(
                workspace.pop(),
                "cannot find workspace root containing compatibility/ from {}",
                env!("CARGO_MANIFEST_DIR")
            );
        }
        let manifest = workspace.join("compatibility/native-conformance/Cargo.toml");
        let target = workspace.join("target/native-conformance");
        let status = Command::new("cargo")
            .arg("build")
            .arg("--manifest-path")
            .arg(&manifest)
            .arg("--target-dir")
            .arg(&target)
            .arg("--bins")
            .status()
            .unwrap_or_else(|error| {
                panic!(
                    "failed to execute conformance fixture build {}: {error}",
                    manifest.display()
                )
            });
        assert!(
            status.success(),
            "conformance fixture build failed (manifest {}) with {status:?}",
            manifest.display()
        );

        let suffix = if cfg!(windows) { ".exe" } else { "" };
        let plugin = target
            .join("debug")
            .join(format!("crikey-conformance-plugin{suffix}"));
        let misbehaving = target
            .join("debug")
            .join(format!("crikey-misbehaving-plugin{suffix}"));
        assert!(
            plugin.is_file(),
            "conformance build did not produce {}",
            plugin.display()
        );
        assert!(
            misbehaving.is_file(),
            "conformance build did not produce {}",
            misbehaving.display()
        );
        (plugin, misbehaving)
    });

    (*BINARIES).clone()
}

fn launch(executable: &Path, plugin: &str, mode: &str, extra: &[(String, String)]) -> LaunchSpec {
    let mut environment = vec![("CRIKEY_CONFORMANCE_MODE".to_owned(), mode.to_owned())];
    environment.extend(extra.iter().cloned());
    LaunchSpec {
        plugin: PluginId(plugin.to_owned()),
        executable: executable.to_path_buf(),
        arguments: vec![mode.to_owned()],
        working_dir: None,
        environment,
    }
}

fn options(transport: TransportKind) -> WorkerOptions {
    let mut options = WorkerOptions::new();
    options.transport = transport;
    options.startup_timeout_ms = STARTUP_TIMEOUT_MS;
    options.call_timeout_ms = CALL_TIMEOUT_MS;
    options.shutdown_timeout_ms = SHUTDOWN_TIMEOUT_MS;
    options
}

fn request(text: &str, generation: u64) -> NativeSuggestRequest {
    NativeSuggestRequest {
        generation,
        text: text.to_owned(),
        normalized: text.to_lowercase(),
        selected_item_id: None,
    }
}

fn env_values(items: &[crikey_core::Item]) -> BTreeMap<String, String> {
    items
        .iter()
        .filter_map(|item| {
            item.stable_id
                .0
                .strip_prefix("env:")
                .map(|key| (key.to_owned(), item.target.clone()))
        })
        .collect()
}

#[derive(Debug)]
struct EnvironmentRestore {
    key: String,
    previous: Option<OsString>,
}

impl Drop for EnvironmentRestore {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(&self.key, value),
            None => std::env::remove_var(&self.key),
        }
    }
}

#[test]
fn spawn_handshake_exposes_plugin_identity_capabilities_and_liveness() {
    let (plugin, _) = conformance_binaries();
    let spec = launch(&plugin, "conformance", "echo", &[]);
    let mut worker = NativeWorker::spawn(spec, options(TransportKind::Stdio))
        .expect("echo conformance plugin completes native handshake");

    let handshake = worker.handshake();
    assert_eq!(handshake.plugin_id, "conformance");
    assert_eq!(handshake.plugin_name, "CriKey Native Conformance");
    assert_eq!(handshake.plugin_version, "1.0.0");
    assert!(!handshake.sdk_version.is_empty());
    assert_eq!(handshake.protocol_version, 1);
    assert!(handshake.capabilities.streaming_catalog);
    assert!(handshake.capabilities.streaming_suggestions);
    assert!(handshake.capabilities.cancellation);
    assert!(
        worker.is_alive(),
        "handshake returns only after a live child exists"
    );

    let exit = worker.shutdown();
    assert_eq!(exit.kind, ExitKind::Clean);
    assert_eq!(exit.code, Some(0));
}

/// The env-witness fixture makes the process boundary observable: only the
/// restricted base, the four handshake variables, and explicit launch entries
/// may be present (spec 16.6).  The distinctive variable is deliberately set
/// in this test process but is not included in LaunchSpec.environment.
#[test]
fn spawn_restricts_environment_and_passes_handshake_and_explicit_entries() {
    let (plugin, _) = conformance_binaries();
    let inherited_key = format!("CRIKEY_HOST_ONLY_{}_{}", std::process::id(), unique_suffix());
    let inherited_value = OsString::from("must-not-reach-child");
    let restore = EnvironmentRestore {
        key: inherited_key.clone(),
        previous: std::env::var_os(&inherited_key),
    };
    std::env::set_var(&inherited_key, &inherited_value);

    let explicit_key = "CRIKEY_TEST_EXPLICIT".to_owned();
    let explicit_value = "passed-by-launch-spec".to_owned();
    let spec = launch(
        &plugin,
        "conformance",
        "env-witness",
        &[(explicit_key.clone(), explicit_value.clone())],
    );
    let mut worker = NativeWorker::spawn(spec, options(TransportKind::Stdio))
        .expect("env-witness plugin completes handshake with restricted environment");
    let items = worker
        .build_catalog()
        .expect("env-witness returns its child environment as catalog items");
    let values = env_values(&items);

    assert!(!values.contains_key(&inherited_key));
    assert!(values.contains_key("PATH"), "restricted base retains PATH");
    for key in [
        "CRIKEY_PLUGIN_ID",
        "CRIKEY_PLUGIN_ENDPOINT",
        "CRIKEY_SESSION_TOKEN",
        "CRIKEY_PROTOCOL_VERSION",
    ] {
        assert!(values.contains_key(key), "child received required {key}");
    }
    assert_eq!(
        values.get("CRIKEY_PLUGIN_ID").map(String::as_str),
        Some("conformance")
    );
    assert_eq!(
        values.get("CRIKEY_PROTOCOL_VERSION").map(String::as_str),
        Some("1")
    );
    assert_eq!(values.get(&explicit_key), Some(&explicit_value));
    assert!(values
        .get("CRIKEY_PLUGIN_ENDPOINT")
        .is_some_and(|value| !value.is_empty()));
    assert!(values
        .get("CRIKEY_SESSION_TOKEN")
        .is_some_and(|value| value.len() >= 32));

    let exit = worker.shutdown();
    assert_eq!(exit.kind, ExitKind::Clean);
    drop(restore);
}

fn unique_suffix() -> String {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system clock is after the Unix epoch")
        .as_nanos()
        .to_string()
}

#[test]
fn bad_token_handshake_is_rejected_as_a_handshake_error() {
    let (_, misbehaving) = conformance_binaries();
    let spec = launch(&misbehaving, "conformance", "bad-token", &[]);
    let error = NativeWorker::spawn(spec, options(TransportKind::Stdio))
        .expect_err("a wrong session token must not be accepted");
    match error {
        HostError::Handshake(detail) => {
            assert!(
                detail.contains("token"),
                "diagnostic names token mismatch: {detail}"
            );
        }
        other => panic!("wrong-token handshake returned {other:?}, not HostError::Handshake"),
    }
}

#[test]
fn bad_protocol_version_handshake_is_rejected_as_a_handshake_error() {
    let (_, misbehaving) = conformance_binaries();
    let spec = launch(&misbehaving, "conformance", "bad-version:99", &[]);
    let error = NativeWorker::spawn(spec, options(TransportKind::Stdio))
        .expect_err("an unsupported protocol version must not be accepted");
    match error {
        HostError::Handshake(detail) => {
            assert!(
                detail.contains("99"),
                "diagnostic names unsupported version: {detail}"
            );
        }
        other => panic!("bad-version handshake returned {other:?}, not HostError::Handshake"),
    }
}

#[test]
fn crash_on_suggest_is_contained_and_records_bounded_stderr() {
    let (plugin, _) = conformance_binaries();
    let spec = launch(&plugin, "conformance", "crash-on-suggest", &[]);
    let mut worker = NativeWorker::spawn(spec, options(TransportKind::Stdio))
        .expect("crash-on-suggest completes startup handshake");
    let error = worker
        .suggest(&request("crash", 1))
        .expect_err("a child abort during suggest is a contained HostError");

    match &error {
        HostError::Crashed { plugin, detail } => {
            assert_eq!(plugin, &PluginId("conformance".to_owned()));
            assert!(
                detail.contains("transport closed"),
                "crash diagnostic identifies the observed transport failure phase: {detail}"
            );
            assert!(
                detail.contains("suggest"),
                "crash diagnostic identifies the suggest failure phase: {detail}"
            );
            assert!(
                detail.contains("[crikey-conformance] fatal: crash-on-suggest requested"),
                "crash diagnostic preserves the fixture's stderr witness: {detail}"
            );
        }
        other => panic!("abort during suggest returned {other:?}, not HostError::Crashed"),
    }
    assert!(!worker.is_alive(), "a crashed worker is never reported alive");
    let exit = worker.shutdown();
    assert_eq!(exit.kind, ExitKind::Crashed);
    assert!(!exit.stderr_tail.is_empty());
    assert!(exit.stderr_tail.len() <= STDERR_TAIL_BYTES);
}

#[test]
fn kill_returns_a_killed_exit_record_and_reaps_the_worker() {
    let (plugin, _) = conformance_binaries();
    let spec = launch(&plugin, "conformance", "echo", &[]);
    let mut worker =
        NativeWorker::spawn(spec, options(TransportKind::Stdio)).expect("echo conformance plugin starts");
    let exit = worker.kill();
    assert_eq!(exit.kind, ExitKind::Killed);
    assert!(exit.code.is_some() || exit.signal.is_some());
    assert!(exit.stderr_tail.len() <= STDERR_TAIL_BYTES);
    assert!(!worker.is_alive());
}

#[test]
fn shutdown_returns_a_clean_exit_record() {
    let (plugin, _) = conformance_binaries();
    let spec = launch(&plugin, "conformance", "echo", &[]);
    let worker =
        NativeWorker::spawn(spec, options(TransportKind::Stdio)).expect("echo conformance plugin starts");
    let exit = worker.shutdown();
    assert_eq!(exit.kind, ExitKind::Clean);
    assert_eq!(exit.code, Some(0));
    assert_eq!(exit.signal, None);
    assert!(exit.stderr_tail.len() <= STDERR_TAIL_BYTES);
}

#[cfg(unix)]
#[test]
fn unix_socket_transport_serves_the_same_happy_path_suggest() {
    let (plugin, _) = conformance_binaries();
    let spec = launch(&plugin, "conformance", "echo", &[]);
    let mut worker = NativeWorker::spawn(spec, options(TransportKind::UnixSocket))
        .expect("UnixSocket conformance plugin completes startup handshake");
    let suggestions = worker
        .suggest(&request("unix", 1))
        .expect("UnixSocket worker answers suggest");
    assert_eq!(suggestions.state, BatchState::Final);
    assert!(
        suggestions.batches > 1,
        "echo results arrive incrementally over UnixSocket"
    );
    let exit = worker.shutdown();
    assert_eq!(exit.kind, ExitKind::Clean);
}

#[test]
fn supervisor_attributes_same_handshake_id_to_each_host_registration() {
    let (plugin, _) = conformance_binaries();
    let first = PluginId("host.alpha".to_owned());
    let second = PluginId("host.beta".to_owned());
    let mut supervisor = NativeSupervisor::new(SupervisorConfig::default());
    supervisor
        .register(
            launch(&plugin, &first.0, "same-id", &[]),
            options(TransportKind::Stdio),
        )
        .expect("register first same-id plugin");
    supervisor
        .register(
            launch(&plugin, &second.0, "same-id", &[]),
            options(TransportKind::Stdio),
        )
        .expect("register second same-id plugin");

    {
        let worker = supervisor.worker(&first, 0).expect("first worker starts");
        assert_eq!(worker.handshake().plugin_id, "shared.identity");
        let items = worker
            .suggest(&request("first", 1))
            .expect("first worker answers");
        assert_eq!(
            items.items.first().map(|item| &item.plugin_id),
            Some(&first),
            "result ownership comes from LaunchSpec.plugin, not handshake identity"
        );
    }
    {
        let worker = supervisor.worker(&second, 0).expect("second worker starts");
        assert_eq!(worker.handshake().plugin_id, "shared.identity");
        let items = worker
            .suggest(&request("second", 1))
            .expect("second worker answers");
        assert_eq!(
            items.items.first().map(|item| &item.plugin_id),
            Some(&second),
            "result ownership comes from LaunchSpec.plugin, not handshake identity"
        );
    }

    supervisor.shutdown_all();
}

#[test]
fn process_limits_report_only_platform_enforceable_controls() {
    let limits = ResourceLimits {
        max_memory_bytes: Some(64 * 1024 * 1024),
        max_cpu_time_seconds: Some(30),
        max_processes: Some(4),
        max_open_files: Some(128),
        ..ResourceLimits::default()
    };
    let report = limits.platform_report();
    #[cfg(unix)]
    {
        assert_eq!(format!("{:?}", report.memory), "Enforced");
        assert_eq!(format!("{:?}", report.cpu_time), "Enforced");
        assert_eq!(format!("{:?}", report.process_count), "Enforced");
        assert_eq!(format!("{:?}", report.open_files), "Enforced");
    }
    #[cfg(windows)]
    {
        assert_eq!(format!("{:?}", report.memory), "Enforced");
        assert_eq!(format!("{:?}", report.cpu_time), "Enforced");
        assert_eq!(format!("{:?}", report.process_count), "Enforced");
        assert!(
            format!("{:?}", report.open_files).starts_with("Unavailable"),
            "Windows job objects do not expose a per-plugin open-file limit"
        );
    }
}

#[test]
fn supervisor_records_startup_failures_in_the_matching_counter() {
    let mut supervisor = NativeSupervisor::new(SupervisorConfig::default());
    let plugin = PluginId("missing.native".to_owned());
    let spec = launch(
        Path::new("/crikey-test-no-such-native-plugin"),
        &plugin.0,
        "echo",
        &[],
    );
    supervisor
        .register(spec, options(TransportKind::Stdio))
        .expect("register missing plugin");

    let error = supervisor
        .worker(&plugin, 0)
        .expect_err("missing executable is a startup failure");
    assert!(matches!(error, HostError::Spawn(_)));
    let health = supervisor.health(&plugin);
    assert_eq!(health.startup_failures, 1);
    assert_eq!(health.crashes, 0);
    assert_eq!(health.timeouts, 0);
    assert_eq!(health.protocol_violations, 0);
}
#[test]
fn startup_error_names_the_plugin_and_underlying_spawn_cause() {
    let plugin = PluginId("named.native".to_owned());
    let error = NativeWorker::spawn(
        launch(
            Path::new("/crikey-test-no-such-native-plugin"),
            &plugin.0,
            "echo",
            &[],
        ),
        options(TransportKind::Stdio),
    )
    .expect_err("missing executable must fail at startup");
    let detail = error.to_string();
    assert!(
        detail.contains("named.native"),
        "startup error names plugin: {detail}"
    );
    assert!(
        detail.contains("os error 2") || detail.contains("No such file") || detail.contains("not found"),
        "startup error preserves the operating-system cause: {detail}"
    );
}

#[test]
fn shutdown_all_closes_the_supervisor_without_allowing_respawn() {
    let mut supervisor = NativeSupervisor::new(SupervisorConfig::default());
    let plugin = PluginId("closed.native".to_owned());
    let spec = launch(
        Path::new("/crikey-test-no-such-native-plugin"),
        &plugin.0,
        "echo",
        &[],
    );
    supervisor
        .register(spec.clone(), options(TransportKind::Stdio))
        .expect("register plugin before shutdown");
    supervisor.shutdown_all();
    assert!(matches!(supervisor.worker(&plugin, 0), Err(HostError::Closed)));
    assert!(matches!(
        supervisor.register(spec, options(TransportKind::Stdio)),
        Err(HostError::Closed)
    ));
}

#[test]
fn supervisor_records_timeout_failures_instead_of_crashes() {
    let (_, misbehaving) = conformance_binaries();
    let plugin = PluginId("hang.native".to_owned());
    let mut supervisor = NativeSupervisor::new(SupervisorConfig::default());
    supervisor
        .register(
            launch(&misbehaving, &plugin.0, "hang", &[]),
            options(TransportKind::Stdio).with_call_timeout_ms(50),
        )
        .expect("register hanging plugin");

    {
        let worker = supervisor.worker(&plugin, 0).expect("hanging plugin starts");
        assert!(matches!(
            worker.suggest(&request("timeout", 1)),
            Err(HostError::Timeout { .. })
        ));
    }
    let _ = supervisor
        .worker(&plugin, 100)
        .expect_err("the timeout killed the worker before restart");
    let health = supervisor.health(&plugin);
    assert_eq!(health.timeouts, 1);
    assert_eq!(health.crashes, 0);
    assert_eq!(health.protocol_violations, 0);
}

#[test]
fn supervisor_records_protocol_violations_instead_of_crashes() {
    let (_, misbehaving) = conformance_binaries();
    let plugin = PluginId("oversized.native".to_owned());
    let mut supervisor = NativeSupervisor::new(SupervisorConfig::default());
    supervisor
        .register(
            launch(&misbehaving, &plugin.0, "oversized", &[]),
            options(TransportKind::Stdio),
        )
        .expect("register oversized-frame plugin");

    {
        let worker = supervisor.worker(&plugin, 0).expect("oversized plugin starts");
        assert!(matches!(
            worker.suggest(&request("protocol", 1)),
            Err(HostError::Protocol(_))
        ));
    }
    let _ = supervisor
        .worker(&plugin, 100)
        .expect_err("the protocol violation killed the worker before restart");
    let health = supervisor.health(&plugin);
    assert_eq!(health.protocol_violations, 1);
    assert_eq!(health.crashes, 0);
    assert_eq!(health.timeouts, 0);
}

#[test]
fn supervisor_records_observed_host_resource_failures() {
    let (plugin_binary, _) = conformance_binaries();
    let plugin = PluginId("resource.native".to_owned());
    let mut supervisor = NativeSupervisor::new(SupervisorConfig::default());
    supervisor
        .register(
            launch(&plugin_binary, &plugin.0, "echo", &[]),
            options(TransportKind::Stdio),
        )
        .expect("register resource-limit test plugin");

    {
        let worker = supervisor.worker(&plugin, 0).expect("echo plugin starts");
        let oversized_argument = "x".repeat(8 * 1024 * 1024);
        assert!(matches!(
            worker.execute(
                &ItemId("catalog:echo".to_owned()),
                None,
                Some(&oversized_argument)
            ),
            Err(HostError::ResourceLimit { .. })
        ));
    }
    let _ = supervisor
        .worker(&plugin, 1)
        .expect("resource rejection is local and leaves the child alive");
    let health = supervisor.health(&plugin);
    assert_eq!(health.resource_limit_failures, 1);
    assert_eq!(health.crashes, 0);
    assert_eq!(health.timeouts, 0);
    assert_eq!(health.protocol_violations, 0);
    supervisor.shutdown_all();
}

#[test]
fn supervisor_resets_failure_streak_after_a_completed_request() {
    let (plugin_binary, _) = conformance_binaries();
    let plugin = PluginId("sequence.native".to_owned());
    let sequence_file = std::env::temp_dir().join(format!(
        "crikey-native-sequence-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let _ = fs::remove_file(&sequence_file);
    let extra = [(
        "CRIKEY_SEQUENCE_FILE".to_owned(),
        sequence_file.display().to_string(),
    )];
    let mut supervisor = NativeSupervisor::new(SupervisorConfig {
        max_restarts: 3,
        restart_window_ms: 60_000,
        base_backoff_ms: 0,
        max_backoff_ms: 0,
        circuit: CircuitBreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_secs(60),
        },
    });
    supervisor
        .register(
            launch(&plugin_binary, &plugin.0, "sequence", &extra),
            options(TransportKind::Stdio),
        )
        .expect("register sequence fixture");

    {
        let worker = supervisor
            .worker(&plugin, 0)
            .expect("first sequence worker starts");
        assert!(matches!(
            worker.suggest(&request("first-crash", 1)),
            Err(HostError::Crashed { .. })
        ));
    }
    {
        let worker = supervisor
            .worker(&plugin, 1)
            .expect("sequence fixture restarts after first crash");
        let result = worker
            .suggest(&request("successful-recovery", 2))
            .expect("second sequence process completes a request");
        assert_eq!(result.state, BatchState::Final);
    }
    {
        let worker = supervisor
            .worker(&plugin, 1)
            .expect("supervisor observes the completed recovery request");
        let exit = worker.kill();
        assert_eq!(exit.kind, ExitKind::Killed);
    }
    {
        let worker = supervisor
            .worker(&plugin, 2)
            .expect("sequence fixture starts its third process");
        assert!(matches!(
            worker.suggest(&request("second-crash", 3)),
            Err(HostError::Crashed { .. })
        ));
    }

    assert_eq!(supervisor.health(&plugin).crashes, 2);
    assert!(
        !supervisor.is_suspended(&plugin, 2),
        "a successful request between crashes resets the circuit streak"
    );
    assert_eq!(
        supervisor
            .snapshot(&plugin)
            .expect("sequence plugin remains registered")
            .circuit
            .failure_streak,
        1
    );
    supervisor.shutdown_all();
    let _ = fs::remove_file(sequence_file);
}
#[test]
fn supervisor_does_not_reset_failures_for_success_then_child_kill_cycles() {
    let (plugin_binary, _) = conformance_binaries();
    let plugin = PluginId("success-then-kill.native".to_owned());
    let mut supervisor = NativeSupervisor::new(SupervisorConfig {
        max_restarts: 3,
        restart_window_ms: 60_000,
        base_backoff_ms: 0,
        max_backoff_ms: 0,
        circuit: CircuitBreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_secs(60),
        },
    });
    supervisor
        .register(
            launch(&plugin_binary, &plugin.0, "echo", &[]),
            options(TransportKind::Stdio),
        )
        .expect("register success-then-kill fixture");

    for now_ms in [0, 1] {
        let worker = supervisor
            .worker(&plugin, now_ms)
            .expect("worker starts or restarts after the previous kill");
        let result = worker
            .suggest(&request("success-before-kill", now_ms))
            .expect("echo request succeeds before the deliberate kill");
        assert_eq!(result.state, BatchState::Final);
        assert_eq!(worker.kill().kind, ExitKind::Killed);
    }

    let error = supervisor
        .worker(&plugin, 2)
        .expect_err("repeated child failures open the circuit");
    assert!(matches!(error, HostError::ResourceLimit { .. }));
    assert!(supervisor.is_suspended(&plugin, 2));
    assert_eq!(supervisor.health(&plugin).crashes, 2);
    supervisor.shutdown_all();
}

#[test]
fn supervisor_restarts_crashes_with_caller_clock_and_keeps_a_sibling_healthy() {
    let (plugin, _) = conformance_binaries();
    let bad = PluginId("conformance.crashing".to_owned());
    let sibling = PluginId("conformance.healthy".to_owned());
    let mut supervisor = NativeSupervisor::new(SupervisorConfig {
        max_restarts: 1,
        restart_window_ms: 10_000,
        base_backoff_ms: 0,
        max_backoff_ms: 0,
        circuit: CircuitBreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_millis(500),
        },
    });
    supervisor
        .register(
            launch(&plugin, &bad.0, "crash-on-suggest", &[]),
            options(TransportKind::Stdio),
        )
        .expect("register crashing plugin");
    supervisor
        .register(
            launch(&plugin, &sibling.0, "echo", &[]),
            options(TransportKind::Stdio),
        )
        .expect("register healthy sibling plugin");

    {
        let worker = supervisor.worker(&bad, 100).expect("first worker starts lazily");
        let catalog = worker
            .build_catalog()
            .expect("crash-on-suggest fixture still builds its catalog");
        assert_eq!(
            catalog.first().map(|item| &item.plugin_id),
            Some(&bad),
            "items use LaunchSpec identity, not self-reported handshake id"
        );
        let error = worker
            .suggest(&request("first-crash", 1))
            .expect_err("first crash is surfaced to the supervisor caller");
        assert!(matches!(error, HostError::Crashed { .. }));
    }
    {
        let worker = supervisor
            .worker(&sibling, 100)
            .expect("healthy sibling starts while another plugin crashed");
        let suggestions = worker
            .suggest(&request("sibling-before-restart", 1))
            .expect("healthy sibling remains available");
        assert_eq!(suggestions.state, BatchState::Final);
        assert_eq!(
            suggestions.items.first().map(|item| &item.plugin_id),
            Some(&sibling),
            "healthy sibling items retain its LaunchSpec identity"
        );
    }

    {
        let worker = supervisor
            .worker(&bad, 200)
            .expect("dead worker respawns within restart budget");
        assert!(worker.is_alive());
        let error = worker
            .suggest(&request("second-crash", 2))
            .expect_err("respawned crash is surfaced");
        assert!(matches!(error, HostError::Crashed { .. }));
    }
    assert_eq!(supervisor.restarts(&bad), 1);
    assert!(!supervisor.exits(&bad).is_empty());
    assert_eq!(supervisor.health(&bad).crashes, 1);
    assert_eq!(supervisor.health(&bad).timeouts, 0);
    assert_eq!(supervisor.health(&bad).protocol_violations, 0);

    {
        let worker = supervisor
            .worker(&sibling, 250)
            .expect("healthy sibling remains available after restart");
        let suggestions = worker
            .suggest(&request("sibling-after-restart", 2))
            .expect("sibling still serves throughout restart accounting");
        assert_eq!(suggestions.state, BatchState::Final);
        assert_eq!(
            suggestions.items.first().map(|item| &item.plugin_id),
            Some(&sibling),
            "sibling identity remains stable after the other worker restarts"
        );
    }

    let error = supervisor
        .worker(&bad, 300)
        .expect_err("a second crash inside the restart window exhausts the budget");
    assert!(matches!(error, HostError::ResourceLimit { .. }));
    assert!(supervisor.exits(&bad).len() >= 2);
    assert_eq!(supervisor.health(&bad).crashes, 2);
    assert!(supervisor.is_suspended(&bad, 300));
    let retry = supervisor
        .next_retry_at_ms(&bad)
        .expect("an open circuit reports a next retry timestamp");
    assert!(
        retry >= 800,
        "retry is after the configured 500ms cooldown: {retry}"
    );

    supervisor.shutdown_all();
}
