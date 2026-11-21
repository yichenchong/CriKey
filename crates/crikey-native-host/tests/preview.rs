//! Real subprocess acceptance using the published Rust SDK example, not a
//! callback mock or an in-process plugin loaded into the launcher.
use crikey_core::{PluginId, PreviewOutcome, PreviewText};
use crikey_native_host::{HostError, LaunchSpec, NativeWorker, WorkerOptions};
use std::{path::PathBuf, process::Command, sync::LazyLock, thread, time::Duration};

fn executable() -> PathBuf {
    static BINARY: LazyLock<PathBuf> = LazyLock::new(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target = root.join("target/preview-conformance");
        let output = Command::new("cargo")
            .current_dir(&root)
            .args([
                "build",
                "-p",
                "crikey-plugin-sdk",
                "--example",
                "preview",
                "--target-dir",
            ])
            .arg(&target)
            .output()
            .expect("build SDK preview example");
        assert!(
            output.status.success(),
            "preview example build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        target
            .join("debug/examples")
            .join(if cfg!(windows) { "preview.exe" } else { "preview" })
    });
    BINARY.clone()
}

fn worker(enabled: bool) -> NativeWorker {
    let spec = LaunchSpec {
        plugin: PluginId("preview-demo".into()),
        executable: executable(),
        arguments: if enabled {
            vec![]
        } else {
            vec!["--no-preview".into()]
        },
        working_dir: None,
        environment: vec![],
        inherit_environment: false,
    };
    let mut options = WorkerOptions::new();
    options.startup_timeout_ms = 10_000;
    options.call_timeout_ms = 5_000;
    NativeWorker::spawn(spec, options).expect("spawn real preview example")
}

#[test]
fn real_native_worker_previews_cancel_and_remain_compatible() {
    let mut worker = worker(true);
    assert!(worker.handshake().preview);
    let pre_cancelled = std::sync::atomic::AtomicBool::new(true);
    assert_eq!(
        worker
            .preview_with_cancel_latched("text", 99, &pre_cancelled)
            .unwrap(),
        PreviewOutcome::Cancelled
    );
    assert!(!worker
        .observations()
        .iter()
        .any(|event| event.kind == "preview_request" && event.generation == 99));
    assert!(
        matches!(worker.preview("text", 100), Err(HostError::PluginFailed { .. })),
        "preview must not manufacture query-local state in another worker"
    );
    let catalog = worker.build_catalog().unwrap();
    assert!(catalog.iter().any(|item| item.stable_id.0 == "slow"
        && item.metadata.get("crikey.preview").map(String::as_str) == Some("deferred")));
    let PreviewOutcome::Content(content) = worker.preview("text", 101).unwrap() else {
        panic!("no selected text")
    };
    let Some(PreviewText::Plain(text)) = content.text else {
        panic!("wrong preview text")
    };
    let pid = text.rsplit(' ').next().unwrap().parse::<u32>().unwrap();
    assert_ne!(
        pid,
        std::process::id(),
        "preview callback must execute in child process"
    );
    assert!(matches!(
        worker.preview("markdown", 102).unwrap(),
        PreviewOutcome::Content(_)
    ));
    let PreviewOutcome::Content(image) = worker.preview("image", 103).unwrap() else {
        panic!("no image")
    };
    assert_eq!(image.image.unwrap().rgba.len(), 16);
    assert_eq!(worker.preview("empty", 104).unwrap(), PreviewOutcome::Empty);
    assert!(matches!(
        worker.preview("error", 105),
        Err(HostError::PluginFailed { .. })
    ));
    let cancel = worker.cancel_handle();
    let requested = std::sync::atomic::AtomicBool::new(false);
    let requested_ref = &requested;
    let cancelled = thread::scope(|scope| {
        scope.spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                // Only the slow callback emits a log for generation 106.
                // Seeing it proves the callback has begun in the child; it
                // cannot complete normally before this cancellation arrives.
                if cancel
                    .observations()
                    .iter()
                    .any(|event| event.kind == "log" && event.generation == 106)
                {
                    requested_ref.store(true, std::sync::atomic::Ordering::Release);
                    cancel.cancel();
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "slow preview callback never signalled its start"
                );
                thread::sleep(Duration::from_millis(1));
            }
        });
        worker.preview_with_cancel_latched("slow", 106, &requested)
    })
    .unwrap();
    assert_eq!(cancelled, PreviewOutcome::Cancelled);
    assert!(matches!(
        worker.preview("text", 107).unwrap(),
        PreviewOutcome::Content(_)
    ));
    let observations = worker.observations();
    let request = observations
        .iter()
        .find(|event| event.kind == "preview_request" && event.generation == 107)
        .unwrap();
    assert!(observations.iter().any(|event| event.kind == "preview_response"
        && event.request_id == request.request_id
        && event.generation == request.generation
        && event.connection_id == request.connection_id));
    worker.shutdown();
}

#[test]
fn cancellation_before_preview_entry_is_replayed_and_does_not_leak() {
    let mut worker = worker(true);
    worker.build_catalog().unwrap();
    let connection_id = worker.connection_id();
    let cancel = worker.cancel_handle();
    cancel.reset();

    // Deterministically occupy the endpoint's handle-installed / call-entry
    // gap: cancellation still targets the completed catalog request here.
    cancel.cancel();
    let requested = std::sync::atomic::AtomicBool::new(false);
    // The actual callback waits indefinitely for cooperative cancellation;
    // WorkerOptions::call_timeout_ms only bounds a broken replay's failure.
    assert_eq!(
        worker
            .preview_with_cancel_latched("slow", 201, &requested)
            .unwrap(),
        PreviewOutcome::Cancelled,
    );
    let observations = worker.observations();
    // Cancellation enqueues without waiting for trace publication. The real
    // terminal response establishes dispatch; FIFO must still put the old
    // cancellation before this preview's request and replayed cancellation.
    let old_cancel_position = observations
        .iter()
        .position(|event| event.direction == "host->plugin" && event.kind == "cancel")
        .unwrap();
    let old_cancel = &observations[old_cancel_position];
    let request_position = observations
        .iter()
        .position(|event| event.kind == "preview_request" && event.generation == 201)
        .unwrap();
    assert!(old_cancel_position < request_position);
    let request = &observations[request_position];
    assert_ne!(old_cancel.request_id, request.request_id);
    assert!(observations[request_position + 1..]
        .iter()
        .any(|event| event.kind == "cancel"
            && event.request_id == request.request_id
            && event.generation == request.generation
            && event.connection_id == connection_id));
    assert!(observations.iter().any(|event| event.kind == "preview_response"
        && event.request_id == request.request_id
        && event.generation == request.generation
        && event.connection_id == connection_id));

    // Do not reset the handle: consuming this call's cancellation is the
    // worker's responsibility, not a prerequisite for the next callback.
    assert!(matches!(
        worker.preview("text", 202).unwrap(),
        PreviewOutcome::Content(_)
    ));
    assert_eq!(worker.connection_id(), connection_id);
    assert!(!worker
        .observations()
        .iter()
        .any(|event| event.kind == "cancel" && event.generation == 202));
    worker.shutdown();
}

#[test]
fn older_worker_without_preview_keeps_catalog_and_explicit_unsupported() {
    let mut worker = worker(false);
    assert!(!worker.handshake().preview);
    assert!(!worker.build_catalog().unwrap().is_empty());
    assert_eq!(worker.preview("text", 1).unwrap(), PreviewOutcome::Unsupported);
    worker.shutdown();
}

#[test]
fn supervisor_preview_borrows_only_the_originating_live_incarnation() {
    use crikey_native_host::{NativeSupervisor, SupervisorConfig};
    let plugin = PluginId("preview-demo".into());
    let mut supervisor = NativeSupervisor::new(SupervisorConfig::default());
    supervisor
        .register(
            LaunchSpec {
                plugin: plugin.clone(),
                executable: executable(),
                arguments: vec![],
                working_dir: None,
                environment: vec![],
                inherit_environment: false,
            },
            WorkerOptions::new(),
        )
        .unwrap();
    assert!(
        supervisor.live_worker(&plugin).is_none(),
        "selection must not start a new worker"
    );
    assert_eq!(supervisor.poll_live_connection(&plugin), None);
    let incarnation = {
        let worker = supervisor.worker(&plugin, 0).unwrap();
        worker.build_catalog().unwrap();
        worker.connection_id()
    };
    assert_eq!(supervisor.poll_live_connection(&plugin), Some(incarnation));
    let worker = supervisor.live_worker(&plugin).unwrap();
    assert_eq!(worker.connection_id(), incarnation);
    assert!(matches!(
        worker.preview("text", 1).unwrap(),
        PreviewOutcome::Content(_)
    ));
    worker.kill();
    assert!(
        supervisor.live_worker(&plugin).is_none(),
        "preview must not revive a dead owner"
    );
    assert_eq!(supervisor.poll_live_connection(&plugin), None);
    assert_eq!(
        supervisor.restarts(&plugin),
        0,
        "inspection cannot spend restart budget"
    );
    assert!(supervisor.worker(&plugin, 0).is_err());
    let retry_at = supervisor.next_retry_at_ms(&plugin).expect("restart backoff");
    let replacement = supervisor.worker(&plugin, retry_at).unwrap();
    assert_ne!(replacement.connection_id(), incarnation);
    let replacement_incarnation = replacement.connection_id();
    assert!(
        matches!(
            replacement.preview("text", 2),
            Err(HostError::PluginFailed { .. })
        ),
        "replacement must not pretend to own the original item state"
    );
    assert_eq!(
        supervisor.poll_live_connection(&plugin),
        Some(replacement_incarnation)
    );
    supervisor.shutdown_all();
    assert_eq!(supervisor.poll_live_connection(&plugin), None);
}
