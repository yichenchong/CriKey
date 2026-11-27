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

use crikey_core::structured_arguments::{
    ArgumentValues, FieldId, NonSecretValue, OptionId, Password, StructuredValue, SubmissionFailureCode,
    SubmissionFieldCode, SubmissionFieldFailure, SubmissionOutcome,
};
use crikey_core::PluginId;
use crikey_core::{ActionId, ItemId};
use crikey_native_host::{
    BatchState, ExitKind, LaunchSpec, NativeSuggestRequest, NativeSupervisor, SupervisorConfig, WorkerOptions,
};
use crikey_native_host::{NativeWorker, TransportKind, TypedPreparation};
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
        selected_scope_id: None,
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

#[test]
fn out_of_tree_commands_use_the_catalog_originating_worker() {
    use crikey_core::command::PluginCommands;
    let (executable, _) = conformance_binaries();
    let owner = PluginId("conformance".into());
    let mut supervisor = NativeSupervisor::new(SupervisorConfig::default());
    supervisor
        .register(launch_spec(&executable, "acceptance"), WorkerOptions::new())
        .unwrap();
    let connection;
    {
        let worker = supervisor.worker(&owner, 0).unwrap();
        assert!(
            worker.commands(1).is_err(),
            "new worker has no catalog provenance"
        );
        let catalog = worker.build_catalog().unwrap();
        connection = worker.connection_id();
        let PluginCommands::Declarations(commands) = worker.commands(2).unwrap() else {
            panic!("missing explicit declarations")
        };
        assert_eq!(commands[0].command_id, "conformance-one");
        assert!(catalog
            .iter()
            .any(|item| item.stable_id == commands[0].item_id && item.plugin_id == owner));
    }
    assert_eq!(
        supervisor.live_worker(&owner).unwrap().connection_id(),
        connection
    );
    supervisor.release_worker(&owner, 1);
    assert!(supervisor.live_worker(&owner).is_none());
    // Reacquiring a process must not authorize declarations against a catalog
    // produced by an earlier incarnation.
    assert!(supervisor.worker(&owner, 2).unwrap().commands(3).is_err());
    supervisor.shutdown_all();
}

#[test]
fn raw_worker_command_declarations_cannot_bypass_host_validation() {
    let (_, raw) = conformance_binaries();
    for mode in [
        "commands-invalid",
        "commands-oversized",
        "commands-count",
        "commands-aggregate",
    ] {
        let mut worker =
            crikey_native_host::NativeWorker::spawn(launch_spec(&raw, mode), WorkerOptions::new()).unwrap();
        assert!(worker.handshake().commands);
        worker.build_catalog().unwrap();
        let error = worker.commands(7).unwrap_err();
        assert!(
            !error.to_string().is_empty(),
            "malformed command refusal must be visible"
        );
        worker.shutdown();
    }
}

fn typed_values(password: &str) -> ArgumentValues {
    ArgumentValues(std::collections::BTreeMap::from([
        (
            FieldId("username".into()),
            StructuredValue::NonSecret(NonSecretValue::Text("alice".into())),
        ),
        (
            FieldId("password".into()),
            StructuredValue::Password(Password::new(password.into()).unwrap()),
        ),
        (
            FieldId("region".into()),
            StructuredValue::NonSecret(NonSecretValue::Dropdown(OptionId("remote".into()))),
        ),
    ]))
}

fn catalog_fingerprint(worker: &mut NativeWorker, item: &str, action: Option<&str>) -> [u8; 32] {
    worker.build_catalog().unwrap();
    worker
        .catalog_presentation()
        .unwrap()
        .schemas
        .iter()
        .find(|binding| {
            binding.executable.target.item.0 == item
                && binding.executable.action.as_ref().map(|action| action.0.as_str()) == action
        })
        .unwrap()
        .schema
        .fingerprint()
        .unwrap()
}

#[test]
fn real_typed_worker_authenticates_actionless_and_declared_actions() {
    let (executable, _) = conformance_binaries();
    for transport in [TransportKind::default(), TransportKind::Stdio] {
        let mut worker = NativeWorker::spawn(
            launch_spec(&executable, "typed"),
            WorkerOptions::new().with_transport(transport),
        )
        .unwrap();
        assert!(worker.handshake().typed_arguments);
        assert!(worker.handshake().result_presentation);
        let fingerprint = catalog_fingerprint(&mut worker, "typed-login", None);
        let presentation = worker.catalog_presentation().unwrap();
        assert_eq!(presentation.sections[0].id.0, "accounts");
        assert_eq!(presentation.scopes[0].id.0, "secure");
        assert_eq!(presentation.schemas.len(), 2);
        assert!(presentation
            .items
            .iter()
            .all(|item| item.target.owner == *worker.plugin()
                && item.section.as_ref().unwrap().0 == "accounts"
                && item.scopes[0].0 == "secure"));
        assert_eq!(
            worker
                .execute_typed(
                    &ItemId("typed-login".into()),
                    None,
                    fingerprint,
                    typed_values("conformance-secret"),
                )
                .unwrap(),
            SubmissionOutcome::Completed
        );
        assert_eq!(
            worker
                .execute_typed(
                    &ItemId("typed-action".into()),
                    None,
                    fingerprint,
                    typed_values("conformance-secret"),
                )
                .unwrap(),
            SubmissionOutcome::Failed(SubmissionFailureCode::SchemaMismatch)
        );
        assert_eq!(
            worker
                .execute_typed(
                    &ItemId("typed-action".into()),
                    Some(&ActionId("signin".into())),
                    fingerprint,
                    typed_values("conformance-secret"),
                )
                .unwrap(),
            SubmissionOutcome::Completed
        );
        assert_eq!(
            worker
                .execute_typed(
                    &ItemId("typed-login".into()),
                    None,
                    fingerprint,
                    typed_values("wrong-password"),
                )
                .unwrap(),
            SubmissionOutcome::InvalidFields(vec![SubmissionFieldFailure {
                field: FieldId("password".into()),
                code: SubmissionFieldCode::Rejected,
            }])
        );
        assert!(worker.shutdown().stderr_tail.is_empty());
    }
}

#[test]
fn real_scoped_suggestions_retire_dynamic_schema_authority() {
    let (executable, _) = conformance_binaries();
    let mut worker = NativeWorker::spawn(launch_spec(&executable, "typed"), WorkerOptions::new()).unwrap();
    let mut query = request(1, "accounts");
    query.selected_scope_id = Some("secure".into());
    let suggestions = worker.suggest(&query).unwrap();
    assert_eq!(suggestions.state, BatchState::Final);
    assert_eq!(suggestions.items.len(), 1);
    assert_eq!(suggestions.items[0].stable_id.0, "typed-suggestion");
    let sidecar = suggestions.presentation.unwrap();
    assert_eq!(sidecar.sections[0].id.0, "accounts");
    assert_eq!(sidecar.scopes[0].id.0, "secure");
    let fingerprint = sidecar.schemas[0].schema.fingerprint().unwrap();
    assert_eq!(
        worker
            .execute_typed(
                &ItemId("typed-suggestion".into()),
                None,
                fingerprint,
                typed_values("conformance-secret"),
            )
            .unwrap(),
        SubmissionOutcome::Completed
    );
    let TypedPreparation::Ready(confirmed) = worker
        .prepare_typed_execution(&ItemId("typed-suggestion".into()), None, fingerprint)
        .unwrap()
    else {
        panic!("live dynamic schema was not confirmed");
    };
    query.generation = 2;
    query.selected_scope_id = Some("not-secure".into());
    let filtered = worker.suggest(&query).unwrap();
    assert!(
        filtered.items.is_empty(),
        "selected scope must reach the actual callback"
    );
    assert!(filtered.presentation.unwrap().schemas.is_empty());
    assert_eq!(
        worker
            .execute_typed_confirmed(confirmed, typed_values("conformance-secret"))
            .unwrap(),
        SubmissionOutcome::Failed(SubmissionFailureCode::SchemaMismatch)
    );
    worker.shutdown();

    let mut fresh = NativeWorker::spawn(launch_spec(&executable, "typed"), WorkerOptions::new()).unwrap();
    assert!(matches!(
        fresh
            .prepare_typed_execution(&ItemId("typed-suggestion".into()), None, fingerprint,)
            .unwrap(),
        TypedPreparation::Refused(SubmissionFailureCode::SchemaMismatch)
    ));
    assert!(
        fresh.catalog_presentation().is_some(),
        "only static discovery may bootstrap"
    );
    assert!(fresh
        .catalog_presentation()
        .unwrap()
        .schemas
        .iter()
        .all(|binding| binding.executable.target.item.0 != "typed-suggestion"));
    fresh.shutdown();
}

#[test]
fn catalog_and_suggestion_schemas_for_the_same_executable_remain_independently_usable() {
    let (executable, _) = conformance_binaries();
    for discover_catalog_first in [true, false] {
        let mut worker =
            NativeWorker::spawn(launch_spec(&executable, "typed"), WorkerOptions::new()).unwrap();
        let catalog_hash = if discover_catalog_first {
            Some(catalog_fingerprint(&mut worker, "typed-login", None))
        } else {
            None
        };
        let suggestions = worker.suggest(&request(1, "catalog-overlap")).unwrap();
        let dynamic_hash = suggestions.presentation.unwrap().schemas[0]
            .schema
            .fingerprint()
            .unwrap();
        // A fresh static source supplies the canonical fingerprint even when
        // this worker has not built its catalog yet.
        let catalog_hash = catalog_hash.unwrap_or_else(|| {
            let mut source =
                NativeWorker::spawn(launch_spec(&executable, "typed"), WorkerOptions::new()).unwrap();
            let hash = catalog_fingerprint(&mut source, "typed-login", None);
            source.shutdown();
            hash
        });
        assert_ne!(catalog_hash, dynamic_hash);
        let TypedPreparation::Ready(confirmed) = worker
            .prepare_typed_execution(&ItemId("typed-login".into()), None, catalog_hash)
            .unwrap()
        else {
            panic!("a live suggestion must not shadow the canonical catalog schema");
        };
        assert_eq!(
            worker
                .execute_typed_confirmed(confirmed, typed_values("conformance-secret"))
                .unwrap(),
            SubmissionOutcome::Completed
        );
        assert_eq!(
            worker
                .execute_typed(
                    &ItemId("typed-login".into()),
                    None,
                    dynamic_hash,
                    typed_values("conformance-secret"),
                )
                .unwrap(),
            SubmissionOutcome::Completed
        );
        worker.shutdown();
    }
}

#[test]
fn healthy_suggestion_first_worker_bootstraps_static_schema_without_regenerating_dynamic_authority() {
    let (executable, _) = conformance_binaries();
    let mut source = NativeWorker::spawn(launch_spec(&executable, "typed"), WorkerOptions::new()).unwrap();
    let fingerprint = catalog_fingerprint(&mut source, "typed-login", None);
    source.shutdown();

    let owner = PluginId("conformance".into());
    let mut supervisor = NativeSupervisor::new(SupervisorConfig::default());
    supervisor
        .register(launch_spec(&executable, "typed"), WorkerOptions::new())
        .unwrap();
    let mut query = request(1, "accounts");
    query.selected_scope_id = Some("secure".into());
    let (connection, dynamic_fingerprint, dynamic_confirmed) = {
        let worker = supervisor.worker(&owner, 0).unwrap();
        let suggestions = worker.suggest(&query).unwrap();
        assert_eq!(suggestions.items[0].stable_id.0, "typed-suggestion");
        let dynamic_fingerprint = suggestions.presentation.unwrap().schemas[0]
            .schema
            .fingerprint()
            .unwrap();
        let TypedPreparation::Ready(confirmed) = worker
            .prepare_typed_execution(&ItemId("typed-suggestion".into()), None, dynamic_fingerprint)
            .unwrap()
        else {
            panic!("live dynamic schema was not confirmed");
        };
        assert!(
            worker.catalog_presentation().is_none(),
            "an admitted dynamic binding must not trigger static discovery"
        );
        (worker.connection_id(), dynamic_fingerprint, confirmed)
    };

    let worker = supervisor.worker(&owner, 1).unwrap();
    assert_eq!(
        worker.connection_id(),
        connection,
        "reacquisition must stay healthy"
    );
    let TypedPreparation::Ready(confirmed) = worker
        .prepare_typed_execution(&ItemId("typed-login".into()), None, fingerprint)
        .unwrap()
    else {
        panic!("suggestion-first worker did not bootstrap the missing static schema");
    };
    assert!(worker.catalog_presentation().is_some());
    assert_eq!(
        worker
            .execute_typed_confirmed(confirmed, typed_values("conformance-secret"))
            .unwrap(),
        SubmissionOutcome::Completed
    );
    assert_eq!(
        worker
            .execute_typed_confirmed(dynamic_confirmed, typed_values("conformance-secret"))
            .unwrap(),
        SubmissionOutcome::Completed,
        "static discovery must preserve the existing dynamic binding"
    );

    query.generation = 2;
    query.selected_scope_id = Some("not-secure".into());
    assert!(worker.suggest(&query).unwrap().items.is_empty());
    assert!(matches!(
        worker
            .prepare_typed_execution(&ItemId("typed-suggestion".into()), None, dynamic_fingerprint)
            .unwrap(),
        TypedPreparation::Refused(SubmissionFailureCode::SchemaMismatch)
    ));
    supervisor.shutdown_all();
}

#[test]
fn released_worker_bootstraps_static_schema_but_refuses_stale_confirmation_and_fingerprint() {
    let (executable, _) = conformance_binaries();
    let owner = PluginId("conformance".into());
    let mut supervisor = NativeSupervisor::new(SupervisorConfig::default());
    supervisor
        .register(launch_spec(&executable, "typed"), WorkerOptions::new())
        .unwrap();
    let (fingerprint, connection, confirmed) = {
        let worker = supervisor.worker(&owner, 0).unwrap();
        let fingerprint = catalog_fingerprint(worker, "typed-login", None);
        let TypedPreparation::Ready(confirmed) = worker
            .prepare_typed_execution(&ItemId("typed-login".into()), None, fingerprint)
            .unwrap()
        else {
            panic!("static schema was not confirmed");
        };
        (fingerprint, worker.connection_id(), confirmed)
    };
    supervisor.release_worker(&owner, 1);
    let worker = supervisor.worker(&owner, 2).unwrap();
    assert_ne!(worker.connection_id(), connection);
    assert!(worker.catalog_presentation().is_none());
    assert_eq!(
        worker
            .execute_typed_confirmed(confirmed, typed_values("conformance-secret"))
            .unwrap(),
        SubmissionOutcome::Failed(SubmissionFailureCode::SchemaMismatch)
    );
    assert!(
        worker.catalog_presentation().is_none(),
        "confirmed execution must never discover"
    );
    assert_eq!(
        worker
            .execute_typed(
                &ItemId("typed-login".into()),
                None,
                fingerprint,
                typed_values("conformance-secret"),
            )
            .unwrap(),
        SubmissionOutcome::Completed
    );
    assert!(
        worker.catalog_presentation().is_some(),
        "fresh static worker must discover its own schema"
    );
    supervisor.shutdown_all();

    let mut changed =
        NativeWorker::spawn(launch_spec(&executable, "typed-changed"), WorkerOptions::new()).unwrap();
    assert_eq!(
        changed
            .execute_typed(
                &ItemId("typed-login".into()),
                None,
                fingerprint,
                typed_values("conformance-secret"),
            )
            .unwrap(),
        SubmissionOutcome::Failed(SubmissionFailureCode::SchemaMismatch)
    );
    assert_ne!(
        changed.catalog_presentation().unwrap().schemas[0]
            .schema
            .fingerprint()
            .unwrap(),
        fingerprint
    );
    changed.shutdown();
}

#[test]
fn real_worker_privacy_survives_background_panics_and_later_callbacks() {
    let (executable, _) = conformance_binaries();
    for transport in [TransportKind::default(), TransportKind::Stdio] {
        let mut worker = NativeWorker::spawn(
            launch_spec(&executable, "typed-privacy"),
            WorkerOptions::new().with_transport(transport),
        )
        .unwrap();
        let fingerprint = catalog_fingerprint(&mut worker, "typed-login", None);
        let connection = worker.connection_id();
        assert_eq!(
            worker
                .execute_typed(
                    &ItemId("typed-login".into()),
                    None,
                    fingerprint,
                    typed_values("private-incarnation-marker"),
                )
                .unwrap(),
            SubmissionOutcome::Failed(SubmissionFailureCode::Failed)
        );
        for generation in 1..=2 {
            let response = worker
                .suggest(&request(generation, "later unrelated callback"))
                .unwrap();
            assert_eq!(response.state, BatchState::Failed);
            assert!(response.log.is_empty());
            let error = response.error.unwrap();
            assert!(error.message.is_empty());
            assert!(error.detail.is_empty());
            assert_eq!(worker.connection_id(), connection);
            assert!(worker.is_alive());
        }
        let exit = worker.shutdown();
        assert!(
            exit.stderr_tail.is_empty(),
            "incarnation stderr must remain suppressed"
        );
    }
}

#[test]
fn real_worker_hostile_field_failures_become_fixed_global_refusals() {
    let (executable, _) = conformance_binaries();
    let mut worker = NativeWorker::spawn(
        launch_spec(&executable, "typed-invalid-failure"),
        WorkerOptions::new(),
    )
    .unwrap();
    let fingerprint = catalog_fingerprint(&mut worker, "typed-login", None);
    assert_eq!(
        worker
            .execute_typed(
                &ItemId("typed-login".into()),
                None,
                fingerprint,
                typed_values("conformance-secret"),
            )
            .unwrap(),
        SubmissionOutcome::Failed(SubmissionFailureCode::Failed)
    );
    assert!(worker.is_alive());
    assert!(worker.shutdown().stderr_tail.is_empty());
}
