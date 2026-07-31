//! Red-first native result-stream tests (spec 12.3-12.5, 24.3-24.4;
//! acceptance 31.22, 31.24, 31.25).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, LazyLock};
use std::thread;
use std::time::Duration;

use crikey_core::PluginId;
use crikey_native_host::{
    BatchState, ExitKind, HostError, LaunchSpec, NativeSuggestRequest, NativeWorker, ResourceLimits,
    TransportKind, WorkerOptions, READER_QUEUE_CAPACITY,
};

const STARTUP_TIMEOUT_MS: u64 = 10_000;
const CALL_TIMEOUT_MS: u64 = 5_000;
const SHUTDOWN_TIMEOUT_MS: u64 = 2_000;
const RESPONSE_LIMIT: Duration = Duration::from_secs(10);
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

fn launch(executable: &Path, plugin: &str, mode: &str) -> LaunchSpec {
    LaunchSpec {
        plugin: PluginId(plugin.to_owned()),
        executable: executable.to_path_buf(),
        arguments: vec![mode.to_owned()],
        working_dir: None,
        environment: vec![("CRIKEY_CONFORMANCE_MODE".to_owned(), mode.to_owned())],
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

fn options_with_limits(
    transport: TransportKind,
    limits: ResourceLimits,
    call_timeout_ms: u64,
) -> WorkerOptions {
    let mut options = options(transport);
    options.call_timeout_ms = call_timeout_ms;
    options.limits = limits;
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

#[test]
fn stream_mode_delivers_items_incrementally_across_multiple_batches() {
    let (plugin, _) = conformance_binaries();
    let spec = launch(&plugin, "conformance", "stream:40");
    let mut worker = NativeWorker::spawn(spec, options(TransportKind::Stdio))
        .expect("stream fixture completes startup handshake");
    let suggestions = worker
        .suggest(&request("stream", 1))
        .expect("stream fixture returns a final result");

    assert_eq!(suggestions.state, BatchState::Final);
    assert_eq!(suggestions.items.len(), 40);
    assert!(
        suggestions.batches > 1,
        "n items are delivered in incremental batches"
    );
    assert!(!suggestions.truncated);
    let diagnostics = worker.diagnostics();
    assert!(diagnostics.batches >= suggestions.batches as u64);
    assert!(diagnostics.items >= 40);
    let exit = worker.shutdown();
    assert_eq!(exit.kind, ExitKind::Clean);
}

#[test]
fn a_well_behaved_plugin_stays_within_granted_credit() {
    let (plugin, _) = conformance_binaries();
    let limits = ResourceLimits {
        initial_credits: 1,
        ..ResourceLimits::default()
    };
    let spec = launch(&plugin, "conformance", "echo");
    let mut worker = NativeWorker::spawn(
        spec,
        options_with_limits(TransportKind::Stdio, limits, CALL_TIMEOUT_MS),
    )
    .expect("well-behaved fixture completes startup handshake");
    let suggestions = worker
        .suggest(&request("credit", 1))
        .expect("SDK sink waits for replenished credit");
    assert_eq!(suggestions.state, BatchState::Final);
    assert!(suggestions.batches > 1);
    let diagnostics = worker.diagnostics();
    assert!(diagnostics.credits_granted > 0);
    assert_eq!(diagnostics.truncated_calls, 0);
    let exit = worker.shutdown();
    assert_eq!(exit.kind, ExitKind::Clean);
}

#[test]
fn a_flooding_plugin_is_punished_for_sending_without_credit() {
    let (_, misbehaving) = conformance_binaries();
    let limits = ResourceLimits {
        initial_credits: 1,
        ..ResourceLimits::default()
    };
    let spec = launch(&misbehaving, "misbehaving.flood", "flood");
    let mut worker = NativeWorker::spawn(spec, options_with_limits(TransportKind::Stdio, limits, 2_000))
        .expect("flood fixture completes startup handshake");
    let error = worker
        .suggest(&request("flood", 1))
        .expect_err("zero-credit batches are a protocol violation");
    assert!(matches!(error, HostError::Protocol(_)));
    assert!(!worker.is_alive());
    let exit = worker.shutdown();
    assert_eq!(exit.kind, ExitKind::ProtocolViolation);
    assert!(exit.stderr_tail.len() <= STDERR_TAIL_BYTES);
}

#[test]
fn aggregate_stream_limits_truncate_items_batches_and_bytes_without_hanging() {
    let (plugin, _) = conformance_binaries();
    let limits = ResourceLimits {
        max_items_per_query: 7,
        max_batches_per_query: 2,
        max_bytes_per_query: 512,
        ..ResourceLimits::default()
    };
    let spec = launch(&plugin, "conformance", "stream:200");
    let mut worker = NativeWorker::spawn(
        spec,
        options_with_limits(TransportKind::Stdio, limits, CALL_TIMEOUT_MS),
    )
    .expect("large stream fixture completes startup handshake");
    let (tx, rx) = mpsc::channel();
    let call = thread::spawn(move || {
        let suggestions = worker
            .suggest(&request("bounded", 1))
            .expect("host truncates a stream at aggregate limits");
        let diagnostics = worker.diagnostics();
        let exit = worker.shutdown();
        tx.send((suggestions, diagnostics, exit))
            .expect("bounded stream result receiver remains available");
    });

    let (suggestions, diagnostics, exit) = rx
        .recv_timeout(RESPONSE_LIMIT)
        .expect("aggregate limits must terminate a large stream within a hard bound");
    assert_eq!(suggestions.state, BatchState::Final);
    assert!(suggestions.truncated);
    assert!(suggestions.items.len() <= 7);
    assert!(
        suggestions.batches >= 3,
        "the truncation trigger and terminal frame are counted"
    );
    assert!(diagnostics.bytes > 0);
    assert_eq!(diagnostics.truncated_calls, 1);
    assert_eq!(exit.kind, ExitKind::Clean);
    call.join().expect("bounded stream worker thread does not panic");
}

#[test]
fn aggregate_byte_limit_truncates_when_item_and_batch_caps_are_large() {
    let (plugin, _) = conformance_binaries();
    let limits = ResourceLimits {
        max_items_per_query: 10_000,
        max_batches_per_query: 512,
        max_bytes_per_query: 256,
        ..ResourceLimits::default()
    };
    let mut worker = NativeWorker::spawn(
        launch(&plugin, "conformance", "stream:200"),
        options_with_limits(TransportKind::Stdio, limits, CALL_TIMEOUT_MS),
    )
    .expect("byte-limit stream fixture completes startup handshake");
    let (tx, rx) = mpsc::channel();
    let call = thread::spawn(move || {
        let suggestions = worker
            .suggest(&request("bytes", 1))
            .expect("host returns a bounded result at the byte cap");
        let diagnostics = worker.diagnostics();
        let exit = worker.shutdown();
        tx.send((suggestions, diagnostics, exit))
            .expect("byte-limit result receiver remains available");
    });

    let (suggestions, diagnostics, exit) = rx
        .recv_timeout(RESPONSE_LIMIT)
        .expect("byte cap must terminate the stream within a hard bound");
    assert_eq!(suggestions.state, BatchState::Final);
    assert!(suggestions.truncated);
    assert!(suggestions.items.len() < 200);
    assert_eq!(diagnostics.truncated_calls, 1);
    assert_eq!(exit.kind, ExitKind::Clean);
    call.join().expect("byte-limit worker thread does not panic");
}

#[test]
fn one_aggregate_deadline_times_out_an_ignore_cancel_call_and_kills_worker() {
    let (plugin, _) = conformance_binaries();
    let mut worker = NativeWorker::spawn(
        launch(&plugin, "conformance", "ignore-cancel:10000"),
        options_with_limits(TransportKind::Stdio, ResourceLimits::default(), 200),
    )
    .expect("ignore-cancel fixture completes startup handshake");
    let (tx, rx) = mpsc::channel();
    let call = thread::spawn(move || {
        let error = worker
            .suggest(&request("deadline", 1))
            .expect_err("a call with one aggregate deadline returns Timeout");
        let alive = worker.is_alive();
        let exit = worker.shutdown();
        tx.send((error, alive, exit))
            .expect("deadline result receiver remains available");
    });

    let (error, alive, exit) = rx
        .recv_timeout(RESPONSE_LIMIT)
        .expect("aggregate timeout must return to the caller within a hard bound");
    assert!(matches!(error, HostError::Timeout { .. }));
    assert!(!alive, "timeout kills the unresponsive worker");
    assert_eq!(exit.kind, ExitKind::Killed);
    call.join().expect("deadline worker thread does not panic");
}

#[test]
fn cooperative_cancellation_returns_cancelled_and_worker_serves_followup() {
    const CANCEL_POLL_LIMIT: usize = 10_000;

    let (plugin, _) = conformance_binaries();
    let spec = launch(&plugin, "conformance", "slow:1000");
    let mut worker = NativeWorker::spawn(spec, options(TransportKind::Stdio))
        .expect("slow fixture completes startup handshake");
    let cancel = worker.cancel_handle();
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let call = thread::spawn(move || {
        started_tx
            .send(())
            .expect("cancellation test receiver remains available");
        let outcome = worker.suggest(&request("slow:1000", 1));
        let _ = done_tx.send(());
        (worker, outcome)
    });
    started_rx
        .recv_timeout(RESPONSE_LIMIT)
        .expect("suggest worker thread starts within the hard bound");

    let canceller = thread::spawn(move || {
        for _ in 0..CANCEL_POLL_LIMIT {
            cancel.cancel();
            match done_rx.recv_timeout(Duration::from_millis(1)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    });

    let (mut worker, outcome) = call.join().expect("cancellation worker thread does not panic");
    canceller.join().expect("cancellation loop thread does not panic");
    let suggestions = outcome.expect("cooperative cancellation keeps transport healthy");
    assert_eq!(suggestions.state, BatchState::Cancelled);
    assert!(worker.is_alive(), "Cancel does not kill a cooperative worker");
    let followup = worker
        .suggest(&request("follow-up", 2))
        .expect("worker remains reusable after cancellation");
    assert_eq!(followup.state, BatchState::Final);
    let exit = worker.shutdown();
    assert_eq!(exit.kind, ExitKind::Clean);
}

#[test]
fn failed_suggest_is_an_ok_batch_and_worker_stays_alive() {
    let (plugin, _) = conformance_binaries();
    let spec = launch(&plugin, "conformance", "fail-suggest");
    let mut worker = NativeWorker::spawn(spec, options(TransportKind::Stdio))
        .expect("fail-suggest fixture completes startup handshake");
    let suggestions = worker
        .suggest(&request("failed", 1))
        .expect("plugin failure is carried in a successful transport response");
    assert_eq!(suggestions.state, BatchState::Failed);
    assert!(suggestions.error.is_some());
    let plugin_error = suggestions
        .error
        .as_ref()
        .expect("Failed batch carries a structured plugin error");
    assert!(!plugin_error.message.is_empty());
    assert!(worker.is_alive());
    let exit = worker.shutdown();
    assert_eq!(exit.kind, ExitKind::Clean);
}

#[test]
fn oversized_frame_is_a_protocol_error_and_protocol_violation_exit() {
    let (_, misbehaving) = conformance_binaries();
    let spec = launch(&misbehaving, "misbehaving.oversized", "oversized");
    let mut worker = NativeWorker::spawn(spec, options(TransportKind::Stdio))
        .expect("oversized fixture completes startup handshake");
    let error = worker
        .suggest(&request("oversized", 1))
        .expect_err("an oversized declared frame is rejected before body read");
    assert!(matches!(error, HostError::Protocol(_)));
    assert!(!worker.is_alive());
    let exit = worker.shutdown();
    assert_eq!(exit.kind, ExitKind::ProtocolViolation);
}

#[test]
fn build_catalog_consumes_multiple_stream_batches() {
    let (plugin, _) = conformance_binaries();
    let spec = launch(&plugin, "conformance", "stream:40");
    let mut worker = NativeWorker::spawn(spec, options(TransportKind::Stdio))
        .expect("stream catalog fixture completes startup handshake");
    let items = worker.build_catalog().expect("all catalog batches are consumed");
    assert_eq!(items.len(), 40);
    assert!(worker.diagnostics().batches > 1);
    let exit = worker.shutdown();
    assert_eq!(exit.kind, ExitKind::Clean);
}

#[test]
fn build_catalog_respects_max_catalog_items() {
    let (plugin, _) = conformance_binaries();
    let limits = ResourceLimits {
        max_catalog_items: 2,
        ..ResourceLimits::default()
    };
    let spec = launch(&plugin, "conformance", "echo");
    let mut worker = NativeWorker::spawn(
        spec,
        options_with_limits(TransportKind::Stdio, limits, CALL_TIMEOUT_MS),
    )
    .expect("catalog fixture completes startup handshake");
    let items = worker
        .build_catalog()
        .expect("catalog batches are consumed under the item cap");
    assert_eq!(items.len(), 2);
    let diagnostics = worker.diagnostics();
    assert!(diagnostics.batches >= 1);
    assert!(diagnostics.items >= 2);
    let exit = worker.shutdown();
    assert_eq!(exit.kind, ExitKind::Clean);
}

#[test]
fn health_and_diagnostics_report_progress_after_streamed_batches() {
    let (plugin, _) = conformance_binaries();
    let spec = launch(&plugin, "conformance", "stream:32");
    let mut worker = NativeWorker::spawn(spec, options(TransportKind::Stdio))
        .expect("stream fixture completes startup handshake");
    let before = worker.diagnostics();
    let health_before = worker.health().expect("health check returns a snapshot");
    assert!(health_before.healthy);
    let suggestions = worker
        .suggest(&request("diagnostics", 1))
        .expect("stream fixture returns a final result");
    assert_eq!(suggestions.state, BatchState::Final);
    let after = worker.diagnostics();
    assert!(after.batches > before.batches);
    assert!(after.items > before.items);
    assert!(after.bytes > before.bytes);
    assert!(after.credits_granted > before.credits_granted);
    let health_after = worker.health().expect("health remains available after streaming");
    assert!(health_after.healthy);
    assert_eq!(health_after.in_flight, 0);
    let exit = worker.shutdown();
    assert_eq!(exit.kind, ExitKind::Clean);
}

#[test]
fn uncredited_log_flood_never_exceeds_the_reader_queue_bound() {
    let (_, misbehaving) = conformance_binaries();
    let mut worker = NativeWorker::spawn(
        launch(&misbehaving, "misbehaving.log-flood", "log-flood"),
        options_with_limits(TransportKind::Stdio, ResourceLimits::default(), 500),
    )
    .expect("log-flood fixture completes startup handshake");
    let mut peak = 0usize;
    for _ in 0..100_000 {
        peak = worker.diagnostics().peak_queue_depth as usize;
        if peak == READER_QUEUE_CAPACITY {
            break;
        }
        thread::yield_now();
    }
    assert_eq!(
        peak, READER_QUEUE_CAPACITY,
        "raw uncredited logs must drive the bounded queue to, never past, capacity"
    );
    let _exit = worker.kill();
}

#[test]
fn truncation_witness_observes_cancel_before_flow_and_terminal_batch() {
    let (_, misbehaving) = conformance_binaries();
    let limits = ResourceLimits {
        max_items_per_query: 1,
        ..ResourceLimits::default()
    };
    let mut worker = NativeWorker::spawn(
        launch(&misbehaving, "misbehaving.control-witness", "control-witness"),
        options_with_limits(TransportKind::Stdio, limits, CALL_TIMEOUT_MS),
    )
    .expect("control-witness fixture completes startup handshake");
    let suggestions = worker
        .suggest(&request("witness", 1))
        .expect("truncation witness returns after terminal frame");
    assert!(suggestions.truncated);
    assert!(
        suggestions.batches >= 2,
        "trigger and terminal result frames count"
    );
    let observations = worker.observations();
    let cancel = observations
        .iter()
        .position(|value| value.direction == "host->plugin" && value.kind == "cancel")
        .expect("host cancellation is observed");
    let flow = observations
        .iter()
        .position(|value| value.direction == "host->plugin" && value.kind == "flow")
        .expect("replenishing flow-control frame is observed");
    assert!(cancel < flow, "Cancel must precede replenishing FlowControl");
    assert!(
        observations
            .iter()
            .filter(|value| value.direction == "plugin->host" && value.kind == "results")
            .count()
            >= 2,
        "the plugin terminal result frame must be observed"
    );
    let exit = worker.shutdown();
    assert_eq!(exit.kind, ExitKind::Clean);
}
