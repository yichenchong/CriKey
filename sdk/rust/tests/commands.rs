//! Runs the SDK example as a supervised subprocess, initialized by its catalog.
use crikey_core::{command::PluginCommands, ActionId, ItemId, PluginId};
use crikey_native_host::{LaunchSpec, NativeWorker, WorkerOptions};
use std::{
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        LazyLock,
    },
    time::{Duration, Instant},
};

fn executable() -> PathBuf {
    static BINARY: LazyLock<PathBuf> = LazyLock::new(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target = root.join("target/commands-conformance");
        let output = Command::new("cargo")
            .current_dir(&root)
            .args([
                "build",
                "-p",
                "crikey-plugin-sdk",
                "--example",
                "commands",
                "--target-dir",
            ])
            .arg(&target)
            .output()
            .expect("build command example");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        target
            .join("debug/examples")
            .join(if cfg!(windows) { "commands.exe" } else { "commands" })
    });
    BINARY.clone()
}
fn worker(mode: &str) -> NativeWorker {
    let mut options = WorkerOptions::new();
    options.startup_timeout_ms = 10_000;
    options.call_timeout_ms = 5_000;
    NativeWorker::spawn(
        LaunchSpec {
            plugin: PluginId("installed.owner".into()),
            executable: executable(),
            arguments: vec![mode.into()],
            working_dir: None,
            environment: vec![],
            inherit_environment: false,
        },
        options,
    )
    .expect("real command worker")
}
#[test]
fn catalog_initialized_worker_declares_stable_executable_targets() {
    let mut worker = worker("valid");
    assert!(worker.handshake().commands);
    assert!(worker.commands(1).unwrap_err().to_string().contains("catalog"));
    let catalog = worker.build_catalog().unwrap();
    let PluginCommands::Declarations(commands) = worker.commands(2).unwrap() else {
        panic!("missing declarations")
    };
    assert_eq!(commands.len(), 1);
    let command = &commands[0];
    assert_eq!(command.command_id, "open-report");
    assert_eq!(command.item_id, ItemId("report".into()));
    assert_eq!(command.action_id, Some(ActionId("open".into())));
    let item = catalog
        .iter()
        .find(|item| item.stable_id == command.item_id && item.plugin_id == *worker.plugin())
        .unwrap();
    assert!(item
        .actions
        .iter()
        .any(|action| Some(&action.action_id) == command.action_id.as_ref()));
    let observations = worker.observations();
    let request = observations
        .iter()
        .find(|event| event.kind == "commands_request")
        .unwrap();
    assert!(observations.iter().any(|event| event.kind == "commands_response"
        && event.connection_id == request.connection_id
        && event.request_id == request.request_id
        && event.generation == 2));
    worker.shutdown();
}
#[test]
fn unsupported_and_invalid_declarations_are_visible_not_empty_success() {
    let mut unsupported = worker("unsupported");
    assert!(!unsupported.handshake().commands);
    unsupported.build_catalog().unwrap();
    assert_eq!(unsupported.commands(2).unwrap(), PluginCommands::Unsupported);
    assert!(!unsupported
        .observations()
        .iter()
        .any(|event| event.kind == "commands_request"));
    unsupported.shutdown();
    for mode in ["invalid", "oversized", "aggregate", "count"] {
        let mut worker = worker(mode);
        worker.build_catalog().unwrap();
        assert!(
            worker.commands(2).is_err(),
            "mode {mode} must refuse declarations"
        );
        assert!(
            !worker.build_catalog().unwrap().is_empty(),
            "refusal must not break later requests"
        );
        worker.shutdown();
    }
}
#[test]
fn command_cancellation_preserves_early_and_in_flight_identity() {
    let mut worker = worker("slow");
    worker.build_catalog().unwrap();
    assert_eq!(
        worker
            .commands_with_cancel_latched(8, &AtomicBool::new(true))
            .unwrap(),
        PluginCommands::Cancelled
    );
    assert!(!worker
        .observations()
        .iter()
        .any(|event| event.kind == "commands_request" && event.generation == 8));
    let cancel = worker.cancel_handle();
    let cancelled = AtomicBool::new(false);
    let token = &cancelled;
    let outcome = std::thread::scope(|scope| {
        scope.spawn(move || {
            let end = Instant::now() + Duration::from_secs(5);
            while !cancel
                .observations()
                .iter()
                .any(|event| event.kind == "log" && event.generation == 9)
            {
                assert!(Instant::now() < end, "command callback never started");
                std::thread::sleep(Duration::from_millis(1));
            }
            token.store(true, Ordering::Release);
            cancel.cancel();
        });
        worker.commands_with_cancel_latched(9, &cancelled)
    })
    .unwrap();
    assert_eq!(outcome, PluginCommands::Cancelled);
    worker.shutdown();
}
