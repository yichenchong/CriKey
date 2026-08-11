//! End-to-end proof that a third-party-shaped Rust native plugin crosses the
//! supervised protocol boundary (spec 16.1, 16.3, 16.4, 16.5, 16.6; result
//! streaming §12.3–12.5; acceptance §31.21–§31.23 and §31.30).
//!
//! The fixture is built as its own Cargo workspace. This test never links the
//! fixture into the host test binary: the child reports its own process id in
//! each streamed item, which is compared with this process id.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use crikey_core::PluginId;
use crikey_native_host::{
    BatchState, ExitKind, LaunchSpec, NativeSuggestRequest, NativeSupervisor, SupervisorConfig, WorkerOptions,
};
use crikey_plugin_supervisor::CircuitBreakerConfig;

/// Builds the out-of-tree conformance workspace once and returns both plugin
/// binaries. Cargo's own lock serialises concurrent test binaries.
fn conformance_binaries() -> (PathBuf, PathBuf) {
    static BINARIES: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    BINARIES
        .get_or_init(|| {
            let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            loop {
                if root.join("compatibility").is_dir() {
                    break;
                }
                assert!(
                    root.pop(),
                    "could not find repository root containing compatibility/ from {}",
                    env!("CARGO_MANIFEST_DIR")
                );
            }

            let manifest = root.join("compatibility/native-conformance/Cargo.toml");
            let target_dir = root.join("target/native-conformance");
            let output = Command::new("cargo")
                .arg("build")
                .arg("--manifest-path")
                .arg(&manifest)
                .arg("--target-dir")
                .arg(&target_dir)
                .output()
                .unwrap_or_else(|error| {
                    panic!(
                        "failed to run cargo for out-of-tree conformance fixture {}: {error}",
                        manifest.display()
                    )
                });
            assert!(
                output.status.success(),
                "out-of-tree conformance fixture build failed (status {}):\nstdout:\n{}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );

            let suffix = if cfg!(windows) { ".exe" } else { "" };
            let conformance = target_dir
                .join("debug")
                .join(format!("crikey-conformance-plugin{suffix}"));
            let misbehaving = target_dir
                .join("debug")
                .join(format!("crikey-misbehaving-plugin{suffix}"));
            assert!(
                conformance.is_file(),
                "fixture build succeeded but {} is missing",
                conformance.display()
            );
            assert!(
                misbehaving.is_file(),
                "fixture build succeeded but {} is missing",
                misbehaving.display()
            );
            (conformance, misbehaving)
        })
        .clone()
}

fn launch_spec(executable: &Path, mode: &str) -> LaunchSpec {
    LaunchSpec {
        plugin: PluginId("conformance".to_owned()),
        executable: executable.to_path_buf(),
        arguments: Vec::new(),
        working_dir: None,
        environment: vec![("CRIKEY_CONFORMANCE_MODE".to_owned(), mode.to_owned())],
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

/// One acceptance sequence: stream from an out-of-tree SDK plugin, cancel a
/// cooperative slow call without losing the worker, record a killed process,
/// and let the supervisor restart the same executable for a fresh query.
#[test]
fn out_of_tree_plugin_streams_cancels_isolated_and_restarts() {
    let (conformance, _misbehaving) = conformance_binaries();
    let plugin = PluginId("conformance".to_owned());
    let mut supervisor = NativeSupervisor::new(SupervisorConfig {
        max_restarts: 3,
        restart_window_ms: 60_000,
        base_backoff_ms: 0,
        max_backoff_ms: 0,
        circuit: CircuitBreakerConfig {
            failure_threshold: 0,
            cooldown: Duration::ZERO,
        },
    });
    supervisor
        .register(launch_spec(&conformance, "acceptance"), WorkerOptions::new())
        .expect("supervisor registers the out-of-tree acceptance fixture");

    // §31.21/§31.22: one published-SDK plugin connects and returns multiple
    // batches, rather than one host-side synthetic result.
    let first_pid = {
        let worker = supervisor
            .worker(&plugin, 0)
            .expect("supervisor lazily starts the out-of-tree fixture");
        let streamed = worker
            .suggest(&request(1, "incremental"))
            .expect("acceptance fixture returns a result");
        assert_eq!(streamed.state, BatchState::Final);
        assert_eq!(streamed.items.len(), 35);
        assert!(
            streamed.batches >= 3,
            "35 items must cross at least three result batches, got {}",
            streamed.batches
        );
        let child_pid = streamed
            .items
            .first()
            .and_then(|item| item.target.parse::<u32>().ok())
            .expect("fixture reports its child pid in a streamed item target");
        assert_ne!(
            child_pid,
            std::process::id(),
            "the host test process must never be the plugin process (§31.30)"
        );
        child_pid
    };

    // §12.5 and §9.4: the same worker answers a query beginning with `slow`
    // as CANCELLED and remains alive. The CANCELLER runs off-thread because it
    // needs only an owned `CancelHandle`, while the blocking call stays on this
    // thread and keeps sole ownership of `&mut NativeWorker`. Repeated bounded
    // writes avoid a race where cancellation arrives before ordinary suggest
    // clears its flag.
    let killed = {
        let worker = supervisor
            .worker(&plugin, 1)
            .expect("the streaming worker remains available for cancellation");
        let cancel = worker.cancel_handle();
        let finished = Arc::new(AtomicBool::new(false));
        let cancelled = thread::scope(|scope| {
            let finished_in_thread = Arc::clone(&finished);
            scope.spawn(move || {
                // Bounded, and paced: it gives up long before the suite would
                // hang, so a lost Cancel frame fails the assertion instead of
                // wedging the test. The pause matters as much as the bound —
                // spinning `yield_now` on a two-core runner starves the very
                // worker thread that has to notice the cancellation, and the
                // call then hits its own timeout and is reported as a crashed
                // transport rather than a cancelled batch.
                for _ in 0..2_000 {
                    if finished_in_thread.load(Ordering::Acquire) {
                        return;
                    }
                    cancel.cancel();
                    thread::sleep(Duration::from_millis(1));
                }
            });
            let outcome = worker.suggest(&request(2, "slow cancel-me"));
            finished.store(true, Ordering::Release);
            outcome
        })
        .expect("cooperative cancellation is an Ok result");

        assert_eq!(cancelled.state, BatchState::Cancelled);
        assert!(worker.is_alive(), "cancelled worker must remain reusable");

        // §24.1/§31.23: killing the child is recorded as Killed, not confused
        // with a plugin FAILED batch or a host panic.
        let killed = worker.kill();
        assert_eq!(killed.kind, ExitKind::Killed);
        killed
    };
    assert_eq!(killed.kind, ExitKind::Killed);

    // §31.23: the supervisor restarts that same executable after its recorded
    // exit and the fresh process serves a new streaming query successfully.
    let restarted = supervisor
        .worker(&plugin, 1_000)
        .expect("supervisor restarts the killed fixture within its budget");
    let fresh = restarted
        .suggest(&request(3, "after-restart"))
        .expect("restarted fixture serves a fresh query");
    assert_eq!(fresh.state, BatchState::Final);
    assert_eq!(fresh.items.len(), 35);
    assert!(
        fresh.batches >= 3,
        "fresh query must still cross multiple result batches, got {}",
        fresh.batches
    );
    let restarted_pid = fresh
        .items
        .first()
        .and_then(|item| item.target.parse::<u32>().ok())
        .expect("restarted fixture reports its child pid");
    assert_ne!(
        restarted_pid,
        std::process::id(),
        "the restarted plugin must remain outside the host process (§31.30)"
    );
    assert_ne!(
        restarted_pid, first_pid,
        "supervisor recovery must launch a fresh plugin process"
    );
    assert_eq!(supervisor.restarts(&plugin), 1);
    assert_eq!(
        supervisor.last_exit(&plugin).map(|record| record.kind),
        Some(ExitKind::Killed)
    );
    supervisor.shutdown_all();
}
