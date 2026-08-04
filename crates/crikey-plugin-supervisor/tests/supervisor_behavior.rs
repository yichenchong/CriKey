//! Public behavioural contract for the M1 in-memory plugin supervisor.
//! The supervisor owns lifecycle state and diagnostics only; these tests do not
//! assume a worker runtime, a clock, or any internal registry representation.

use std::time::Duration;

use crikey_core::PluginId;
use crikey_plugin_supervisor::{
    CircuitBreakerConfig, CircuitDiagnostics, Deadlines, FailureKind, MemorySupervisor, Supervisor,
    SupervisorError, WorkerState,
};

fn plugin(name: &str) -> PluginId {
    PluginId(name.to_owned())
}

fn supervisor(failure_threshold: u32, cooldown: Duration) -> MemorySupervisor {
    MemorySupervisor::new(CircuitBreakerConfig {
        failure_threshold,
        cooldown,
    })
}

fn register_and_make_ready(supervisor: &mut MemorySupervisor, plugin: &PluginId) {
    supervisor
        .register(plugin)
        .expect("a new plugin can be registered");
    supervisor.start(plugin).expect("a registered plugin starts");
    assert_eq!(supervisor.state(plugin), WorkerState::Starting);
    supervisor
        .mark_ready(plugin)
        .expect("a starting plugin can become ready");
    assert_eq!(supervisor.state(plugin), WorkerState::Ready);
}

fn assert_zero_health(supervisor: &MemorySupervisor, plugin: &PluginId) {
    let health = supervisor.health(plugin);
    assert_eq!(health.startup_failures, 0);
    assert_eq!(health.crashes, 0);
    assert_eq!(health.timeouts, 0);
    assert_eq!(health.soft_timeouts, 0);
    assert_eq!(health.protocol_violations, 0);
    assert_eq!(health.resource_limit_failures, 0);
    assert_eq!(health.cancellations_honoured, 0);
    assert_eq!(health.cancellations_ignored, 0);
    assert_eq!(health.stale_results_rejected, 0);
    assert_eq!(health.obsolete_requests_dropped, 0);
    assert_eq!(health.queue_depth, 0);
    assert_eq!(health.average_latency_ms, 0);
    assert_eq!(health.peak_latency_ms, 0);
    assert_eq!(health.latency_samples, 0);
}

#[test]
fn registration_exposes_not_started_state_and_zero_health() {
    let mut supervisor = supervisor(3, Duration::from_secs(30));
    let applications = plugin("dev.crikey.applications");

    supervisor.register(&applications).expect("registration succeeds");
    assert!(supervisor.contains(&applications));
    let snapshot = supervisor
        .snapshot(&applications)
        .expect("a registered plugin has a snapshot");
    assert_eq!(snapshot.state, WorkerState::NotStarted);
    assert_eq!(snapshot.health, supervisor.health(&applications));
    assert_eq!(snapshot.circuit, CircuitDiagnostics::default());

    assert_eq!(supervisor.state(&applications), WorkerState::NotStarted);
    assert_zero_health(&supervisor, &applications);
}

#[test]
fn duplicate_registration_is_typed_and_preserves_the_existing_record() {
    let mut supervisor = supervisor(3, Duration::from_secs(30));
    let worker = plugin("dev.crikey.duplicate");
    register_and_make_ready(&mut supervisor, &worker);
    supervisor.mark_busy(&worker).expect("a suggestion begins");
    supervisor
        .record_soft_timeout(&worker)
        .expect("a soft timeout is diagnosed");
    let before = supervisor
        .snapshot(&worker)
        .expect("the registered plugin has a snapshot");

    assert_eq!(
        supervisor.register(&worker),
        Err(SupervisorError::AlreadyRegistered {
            plugin: worker.clone(),
        })
    );
    assert_eq!(supervisor.snapshot(&worker), Some(before));
    assert!(supervisor.contains(&worker));
}

#[test]
fn legal_lifecycle_transitions_are_observable_through_the_supervisor_trait() {
    let mut supervisor = supervisor(3, Duration::from_secs(30));
    let applications = plugin("dev.crikey.applications");
    supervisor.register(&applications).expect("registration succeeds");

    supervisor.start(&applications).expect("not-started -> starting");
    assert_eq!(supervisor.state(&applications), WorkerState::Starting);
    supervisor
        .stop(&applications)
        .expect("startup may be cancelled asynchronously");
    assert_eq!(supervisor.state(&applications), WorkerState::NotStarted);
    supervisor
        .start(&applications)
        .expect("the cancelled startup can be attempted again");
    assert_eq!(supervisor.state(&applications), WorkerState::Starting);

    supervisor.mark_ready(&applications).expect("starting -> ready");
    assert_eq!(supervisor.state(&applications), WorkerState::Ready);

    supervisor.mark_busy(&applications).expect("ready -> busy");
    assert_eq!(supervisor.state(&applications), WorkerState::Busy);

    supervisor.mark_ready(&applications).expect("busy -> ready");
    assert_eq!(supervisor.state(&applications), WorkerState::Ready);

    supervisor.restart(&applications).expect("ready -> restarting");
    assert_eq!(supervisor.state(&applications), WorkerState::Restarting);

    supervisor.mark_ready(&applications).expect("restarting -> ready");
    supervisor
        .mark_busy(&applications)
        .expect("the recovered worker can accept work");
    supervisor.stop(&applications).expect("busy -> not-started");
    assert_eq!(supervisor.state(&applications), WorkerState::NotStarted);

    supervisor
        .start(&applications)
        .expect("a stopped plugin can be started again");
    supervisor
        .mark_ready(&applications)
        .expect("the restarted plugin becomes ready");
    supervisor.stop(&applications).expect("ready -> not-started");
    assert_eq!(supervisor.state(&applications), WorkerState::NotStarted);

    supervisor.start(&applications).expect("stopped worker starts");
    supervisor.mark_ready(&applications).expect("startup completes");
    supervisor.restart(&applications).expect("restart begins");
    supervisor
        .stop(&applications)
        .expect("an asynchronous restart may be cancelled");
    assert_eq!(supervisor.state(&applications), WorkerState::NotStarted);
}

#[test]
fn illegal_transitions_return_errors_and_leave_the_known_state_unchanged() {
    let mut supervisor = supervisor(3, Duration::from_secs(30));
    let applications = plugin("dev.crikey.applications");
    supervisor.register(&applications).expect("registration succeeds");

    assert!(supervisor.stop(&applications).is_err());
    assert!(supervisor.restart(&applications).is_err());
    assert!(supervisor.mark_busy(&applications).is_err());
    assert!(supervisor.mark_ready(&applications).is_err());
    assert!(supervisor
        .record_failure(&applications, FailureKind::Crash, Duration::ZERO)
        .is_err());
    assert_eq!(supervisor.state(&applications), WorkerState::NotStarted);
    assert_zero_health(&supervisor, &applications);

    supervisor.start(&applications).expect("start succeeds once");
    assert!(supervisor.start(&applications).is_err());
    assert!(supervisor.mark_busy(&applications).is_err());
    assert_eq!(supervisor.state(&applications), WorkerState::Starting);

    supervisor.mark_ready(&applications).expect("starting -> ready");
    assert!(supervisor.start(&applications).is_err());
    assert!(supervisor.mark_ready(&applications).is_err());
    assert_eq!(supervisor.state(&applications), WorkerState::Ready);

    supervisor.mark_busy(&applications).expect("ready -> busy");
    assert!(supervisor.start(&applications).is_err());
    assert!(supervisor.mark_busy(&applications).is_err());
    assert_eq!(supervisor.state(&applications), WorkerState::Busy);

    supervisor.stop(&applications).expect("busy -> not-started");
    assert!(supervisor.stop(&applications).is_err());
    assert_eq!(supervisor.state(&applications), WorkerState::NotStarted);
}

#[test]
fn unknown_plugin_mutations_error_while_state_and_health_queries_are_safe() {
    let mut supervisor = supervisor(2, Duration::from_secs(5));
    let unknown = plugin("dev.crikey.unknown");

    assert_eq!(supervisor.state(&unknown), WorkerState::NotStarted);
    assert_zero_health(&supervisor, &unknown);
    assert!(!supervisor.contains(&unknown));
    assert_eq!(supervisor.snapshot(&unknown), None);
    assert_eq!(
        supervisor.circuit_diagnostics(&unknown),
        CircuitDiagnostics::default()
    );

    assert_eq!(
        supervisor.start(&unknown),
        Err(SupervisorError::UnknownPlugin {
            plugin: unknown.clone(),
        })
    );
    assert!(supervisor.stop(&unknown).is_err());
    assert!(supervisor.restart(&unknown).is_err());
    assert!(supervisor.mark_busy(&unknown).is_err());
    assert!(supervisor.mark_ready(&unknown).is_err());
    assert!(supervisor
        .record_failure(&unknown, FailureKind::Crash, Duration::from_secs(1))
        .is_err());
    assert!(supervisor.record_success(&unknown).is_err());
    assert!(supervisor
        .resume_if_ready(&unknown, Duration::from_secs(10))
        .is_err());
    assert!(supervisor
        .record_latency(&unknown, Duration::from_millis(10))
        .is_err());
    assert!(supervisor.record_soft_timeout(&unknown).is_err());
    assert!(supervisor.record_queue_depth(&unknown, 7).is_err());
    assert!(supervisor.record_cancellation(&unknown, true, 1).is_err());
    assert!(supervisor.record_stale_result_rejected(&unknown, 1).is_err());
    assert!(supervisor.record_obsolete_request_dropped(&unknown, 1).is_err());

    assert_eq!(supervisor.state(&unknown), WorkerState::NotStarted);
    assert_zero_health(&supervisor, &unknown);
}

#[test]
fn every_failure_kind_contributes_to_the_circuit_breaker() {
    let cases = [
        FailureKind::Startup,
        FailureKind::Crash,
        FailureKind::Timeout,
        FailureKind::ProtocolViolation,
        FailureKind::ResourceLimit,
    ];

    for (index, failure) in cases.into_iter().enumerate() {
        let mut supervisor = supervisor(1, Duration::from_secs(5));
        let worker = plugin(&format!("dev.crikey.failure-{index}"));
        supervisor.register(&worker).expect("registration succeeds");

        match &failure {
            FailureKind::Startup => {
                supervisor.start(&worker).expect("startup begins");
            }
            FailureKind::Timeout => {
                supervisor.start(&worker).expect("startup begins");
                supervisor.mark_ready(&worker).expect("worker is ready");
                supervisor.mark_busy(&worker).expect("query begins");
            }
            FailureKind::Crash | FailureKind::ProtocolViolation | FailureKind::ResourceLimit => {
                supervisor.start(&worker).expect("startup begins");
                supervisor.mark_ready(&worker).expect("worker is ready");
            }
        }

        let expected_startup_failures = if matches!(&failure, FailureKind::Startup) {
            1
        } else {
            0
        };
        let expected_crashes = if matches!(&failure, FailureKind::Crash) {
            1
        } else {
            0
        };
        let expected_timeouts = if matches!(&failure, FailureKind::Timeout) {
            1
        } else {
            0
        };
        let expected_protocol_violations = if matches!(&failure, FailureKind::ProtocolViolation) {
            1
        } else {
            0
        };
        let expected_resource_limit_failures = if matches!(&failure, FailureKind::ResourceLimit) {
            1
        } else {
            0
        };

        supervisor
            .record_failure(&worker, failure, Duration::from_secs(10))
            .expect("a failure from an active lifecycle state is recorded");

        assert_eq!(supervisor.state(&worker), WorkerState::Suspended);
        let health = supervisor.health(&worker);
        assert_eq!(health.startup_failures, expected_startup_failures);
        assert_eq!(health.crashes, expected_crashes);
        assert_eq!(health.timeouts, expected_timeouts);
        assert_eq!(health.protocol_violations, expected_protocol_violations);
        assert_eq!(health.resource_limit_failures, expected_resource_limit_failures);
        assert_eq!(
            supervisor.circuit_diagnostics(&worker),
            crikey_plugin_supervisor::CircuitDiagnostics {
                failure_streak: 1,
                last_failure: Some(failure),
                last_failure_at: Some(Duration::from_secs(10)),
                retry_at: Some(Duration::from_secs(15)),
            }
        );
    }
}

#[test]
fn suspension_occurs_at_the_configured_threshold_and_is_plugin_scoped() {
    let mut supervisor = supervisor(3, Duration::from_secs(20));
    let flaky = plugin("dev.crikey.flaky");
    let healthy = plugin("dev.crikey.healthy");
    register_and_make_ready(&mut supervisor, &flaky);
    register_and_make_ready(&mut supervisor, &healthy);

    supervisor
        .record_failure(&flaky, FailureKind::Crash, Duration::from_secs(1))
        .expect("first failure is recorded");
    assert_eq!(supervisor.state(&flaky), WorkerState::Failed);
    assert_eq!(supervisor.state(&healthy), WorkerState::Ready);

    supervisor.restart(&flaky).expect("a failed plugin can restart");
    assert_eq!(supervisor.state(&flaky), WorkerState::Restarting);
    supervisor.mark_ready(&flaky).expect("restart completes");
    supervisor.mark_busy(&flaky).expect("query begins");
    supervisor
        .record_failure(&flaky, FailureKind::Timeout, Duration::from_secs(2))
        .expect("second failure is recorded");
    assert_eq!(supervisor.state(&flaky), WorkerState::Failed);

    supervisor.restart(&flaky).expect("a failed plugin can restart");
    supervisor.mark_ready(&flaky).expect("restart completes");
    supervisor
        .record_failure(&flaky, FailureKind::ProtocolViolation, Duration::from_secs(3))
        .expect("threshold failure is recorded");

    assert_eq!(supervisor.state(&flaky), WorkerState::Suspended);
    assert_eq!(supervisor.state(&healthy), WorkerState::Ready);
    let flaky_health = supervisor.health(&flaky);
    assert_eq!(flaky_health.crashes, 1);
    assert_eq!(flaky_health.timeouts, 1);
    assert_eq!(flaky_health.protocol_violations, 1);
    assert_eq!(
        supervisor.circuit_diagnostics(&flaky),
        CircuitDiagnostics {
            failure_streak: 3,
            last_failure: Some(FailureKind::ProtocolViolation),
            last_failure_at: Some(Duration::from_secs(3)),
            retry_at: Some(Duration::from_secs(23)),
        }
    );
    assert_zero_health(&supervisor, &healthy);
}

/// Repeated startup failures must count toward the same circuit as crashes
/// rather than permitting an immediate restart forever. Once the configured
/// streak is reached, the supervisor suspends the plugin and records the
/// cooldown boundary.
#[test]
fn repeated_startup_failures_eventually_suspend_the_plugin() {
    let mut supervisor = supervisor(3, Duration::from_secs(5));
    let worker = plugin("dev.crikey.startup-loop");
    supervisor.register(&worker).expect("registration succeeds");

    supervisor.start(&worker).expect("initial startup begins");
    for timestamp in 1..=3 {
        supervisor
            .record_failure(&worker, FailureKind::Startup, Duration::from_secs(timestamp))
            .expect("startup failure is recorded");
        if timestamp != 3 {
            supervisor
                .restart(&worker)
                .expect("failed startup can be retried");
        }
    }
    assert_eq!(supervisor.state(&worker), WorkerState::Suspended);
    assert_eq!(
        supervisor.circuit_diagnostics(&worker),
        CircuitDiagnostics {
            failure_streak: 3,
            last_failure: Some(FailureKind::Startup),
            last_failure_at: Some(Duration::from_secs(3)),
            retry_at: Some(Duration::from_secs(8)),
        }
    );
    assert_eq!(supervisor.health(&worker).startup_failures, 3);
}

#[test]
fn cooldown_uses_the_failure_timestamp_and_recovers_at_the_exact_boundary() {
    let cooldown = Duration::from_secs(5);
    let mut supervisor = supervisor(2, cooldown);
    let worker = plugin("dev.crikey.recovering");
    register_and_make_ready(&mut supervisor, &worker);

    supervisor
        .record_failure(&worker, FailureKind::Crash, Duration::from_secs(8))
        .expect("first failure is recorded");
    assert_eq!(supervisor.state(&worker), WorkerState::Failed);
    supervisor.restart(&worker).expect("failed worker restarts");
    supervisor.mark_ready(&worker).expect("restart completes");

    supervisor
        .record_failure(&worker, FailureKind::Crash, Duration::from_secs(10))
        .expect("second failure opens the circuit");
    assert_eq!(supervisor.state(&worker), WorkerState::Suspended);
    assert!(supervisor.restart(&worker).is_err());
    assert_eq!(
        supervisor.circuit_diagnostics(&worker),
        CircuitDiagnostics {
            failure_streak: 2,
            last_failure: Some(FailureKind::Crash),
            last_failure_at: Some(Duration::from_secs(10)),
            retry_at: Some(Duration::from_secs(15)),
        }
    );

    supervisor
        .resume_if_ready(&worker, Duration::from_millis(14_999))
        .expect("checking before the boundary is not itself an error");
    assert_eq!(supervisor.state(&worker), WorkerState::Suspended);

    supervisor
        .resume_if_ready(&worker, Duration::from_secs(15))
        .expect("the exact cooldown boundary permits recovery");
    assert_eq!(supervisor.state(&worker), WorkerState::Restarting);

    supervisor.mark_ready(&worker).expect("recovery completes");
    supervisor
        .record_success(&worker)
        .expect("a successful recovery closes and resets the circuit");
    assert_eq!(supervisor.state(&worker), WorkerState::Ready);
    assert_eq!(
        supervisor.circuit_diagnostics(&worker),
        CircuitDiagnostics {
            failure_streak: 0,
            last_failure: Some(FailureKind::Crash),
            last_failure_at: Some(Duration::from_secs(10)),
            retry_at: None,
        }
    );

    supervisor
        .record_failure(&worker, FailureKind::Crash, Duration::from_secs(16))
        .expect("a post-recovery failure starts a fresh streak");
    assert_eq!(supervisor.state(&worker), WorkerState::Failed);
    assert_eq!(supervisor.health(&worker).crashes, 3);
}

#[test]
fn soft_timeouts_are_diagnostic_only_and_require_a_busy_suggestion() {
    let mut supervisor = supervisor(2, Duration::from_secs(5));
    let worker = plugin("dev.crikey.soft-timeout");
    register_and_make_ready(&mut supervisor, &worker);
    let ready = supervisor
        .snapshot(&worker)
        .expect("the registered plugin has a snapshot");

    assert_eq!(
        supervisor.record_soft_timeout(&worker),
        Err(SupervisorError::IllegalTransition {
            plugin: worker.clone(),
            operation: "record soft timeout",
            state: WorkerState::Ready,
        })
    );
    assert_eq!(supervisor.snapshot(&worker), Some(ready));

    supervisor.mark_busy(&worker).expect("a suggestion begins");
    let circuit_before = supervisor.circuit_diagnostics(&worker);
    supervisor
        .record_soft_timeout(&worker)
        .expect("the first soft miss is diagnosed");
    supervisor
        .record_soft_timeout(&worker)
        .expect("the second soft miss is diagnosed");

    let health = supervisor.health(&worker);
    assert_eq!(health.soft_timeouts, 2);
    assert_eq!(health.timeouts, 0);
    assert_eq!(supervisor.state(&worker), WorkerState::Busy);
    assert_eq!(supervisor.circuit_diagnostics(&worker), circuit_before);
}

#[test]
fn startup_and_hard_timeout_failures_require_their_lifecycle_phases() {
    let mut supervisor = supervisor(0, Duration::from_secs(5));
    let worker = plugin("dev.crikey.failure-phases");
    supervisor.register(&worker).expect("registration succeeds");
    supervisor.start(&worker).expect("startup begins");

    let starting = supervisor.snapshot(&worker).expect("snapshot is available");
    assert_eq!(
        supervisor.record_failure(&worker, FailureKind::Timeout, Duration::from_secs(1)),
        Err(SupervisorError::IllegalTransition {
            plugin: worker.clone(),
            operation: "record hard timeout",
            state: WorkerState::Starting,
        })
    );
    assert_eq!(supervisor.snapshot(&worker), Some(starting));
    supervisor
        .record_failure(&worker, FailureKind::Startup, Duration::from_secs(1))
        .expect("a startup failure is valid while starting");

    supervisor.restart(&worker).expect("failed startup restarts");
    let restarting = supervisor.snapshot(&worker).expect("snapshot is available");
    assert!(supervisor
        .record_failure(&worker, FailureKind::Timeout, Duration::from_secs(2))
        .is_err());
    assert_eq!(supervisor.snapshot(&worker), Some(restarting));
    supervisor
        .record_failure(&worker, FailureKind::Startup, Duration::from_secs(2))
        .expect("a startup failure is valid while restarting");

    supervisor.restart(&worker).expect("the worker restarts again");
    supervisor.mark_ready(&worker).expect("restart completes");
    let ready = supervisor.snapshot(&worker).expect("snapshot is available");
    assert!(supervisor
        .record_failure(&worker, FailureKind::Startup, Duration::from_secs(3))
        .is_err());
    assert!(supervisor
        .record_failure(&worker, FailureKind::Timeout, Duration::from_secs(3))
        .is_err());
    assert_eq!(supervisor.snapshot(&worker), Some(ready));

    supervisor.mark_busy(&worker).expect("a suggestion begins");
    let busy = supervisor.snapshot(&worker).expect("snapshot is available");
    assert!(supervisor
        .record_failure(&worker, FailureKind::Startup, Duration::from_secs(3))
        .is_err());
    assert_eq!(supervisor.snapshot(&worker), Some(busy));
    supervisor
        .record_failure(&worker, FailureKind::Timeout, Duration::from_secs(3))
        .expect("a hard timeout is valid while busy");

    let health = supervisor.health(&worker);
    assert_eq!(health.startup_failures, 2);
    assert_eq!(health.timeouts, 1);
    assert_eq!(
        supervisor.circuit_diagnostics(&worker),
        CircuitDiagnostics {
            failure_streak: 3,
            last_failure: Some(FailureKind::Timeout),
            last_failure_at: Some(Duration::from_secs(3)),
            retry_at: None,
        }
    );
}

#[test]
fn zero_failure_threshold_disables_suspension_without_hiding_the_streak() {
    let mut supervisor = supervisor(0, Duration::from_secs(5));
    let worker = plugin("dev.crikey.no-circuit");
    register_and_make_ready(&mut supervisor, &worker);

    for (index, at) in [1, 2, 3].into_iter().enumerate() {
        if index != 0 {
            supervisor.restart(&worker).expect("failed worker restarts");
            supervisor.mark_ready(&worker).expect("restart completes");
        }
        supervisor
            .record_failure(&worker, FailureKind::Crash, Duration::from_secs(at))
            .expect("the failure is recorded with the circuit disabled");

        assert_eq!(supervisor.state(&worker), WorkerState::Failed);
        assert_eq!(
            supervisor.circuit_diagnostics(&worker),
            CircuitDiagnostics {
                failure_streak: u32::try_from(index + 1).expect("small test index"),
                last_failure: Some(FailureKind::Crash),
                last_failure_at: Some(Duration::from_secs(at)),
                retry_at: None,
            }
        );
    }
    assert_eq!(supervisor.health(&worker).crashes, 3);
}

#[test]
fn suspended_plugins_cannot_bypass_cooldown_with_stop_and_start() {
    let mut supervisor = supervisor(1, Duration::from_secs(5));
    let worker = plugin("dev.crikey.no-bypass");
    register_and_make_ready(&mut supervisor, &worker);
    supervisor
        .record_failure(&worker, FailureKind::Crash, Duration::from_secs(10))
        .expect("the failure opens the circuit");
    let suspended = supervisor.snapshot(&worker).expect("snapshot is available");

    assert_eq!(
        supervisor.stop(&worker),
        Err(SupervisorError::IllegalTransition {
            plugin: worker.clone(),
            operation: "stop",
            state: WorkerState::Suspended,
        })
    );
    assert!(supervisor.start(&worker).is_err());
    assert!(supervisor.restart(&worker).is_err());
    assert_eq!(supervisor.snapshot(&worker), Some(suspended));

    supervisor
        .resume_if_ready(&worker, Duration::from_millis(14_999))
        .expect("a pre-boundary check is accepted");
    assert!(supervisor.stop(&worker).is_err());
    assert_eq!(supervisor.state(&worker), WorkerState::Suspended);
    supervisor
        .resume_if_ready(&worker, Duration::from_secs(15))
        .expect("the exact boundary permits recovery");
    assert_eq!(supervisor.state(&worker), WorkerState::Restarting);
}

#[test]
fn event_timestamps_are_monotonic_and_regressions_are_atomic() {
    let mut supervisor = supervisor(2, Duration::from_secs(5));
    let worker = plugin("dev.crikey.monotonic-time");
    register_and_make_ready(&mut supervisor, &worker);
    supervisor
        .record_failure(&worker, FailureKind::Crash, Duration::from_secs(100))
        .expect("the first timestamp is accepted");
    supervisor.restart(&worker).expect("failed worker restarts");
    supervisor.mark_ready(&worker).expect("restart completes");
    let before_regression = supervisor.snapshot(&worker).expect("snapshot is available");

    assert_eq!(
        supervisor.record_failure(&worker, FailureKind::Crash, Duration::from_secs(99)),
        Err(SupervisorError::RegressiveTimestamp {
            plugin: worker.clone(),
            previous: Duration::from_secs(100),
            received: Duration::from_secs(99),
        })
    );
    assert_eq!(supervisor.snapshot(&worker), Some(before_regression));

    supervisor
        .record_failure(&worker, FailureKind::Crash, Duration::from_secs(101))
        .expect("a later timestamp opens the circuit");
    supervisor
        .resume_if_ready(&worker, Duration::from_secs(105))
        .expect("a monotonic pre-boundary check is accepted");
    let before_resume_regression = supervisor.snapshot(&worker).expect("snapshot is available");
    assert_eq!(
        supervisor.resume_if_ready(&worker, Duration::from_secs(104)),
        Err(SupervisorError::RegressiveTimestamp {
            plugin: worker.clone(),
            previous: Duration::from_secs(105),
            received: Duration::from_secs(104),
        })
    );
    assert_eq!(supervisor.snapshot(&worker), Some(before_resume_regression));

    supervisor
        .resume_if_ready(&worker, Duration::from_secs(106))
        .expect("the retry boundary remains based on the non-regressive failure");
    assert_eq!(supervisor.state(&worker), WorkerState::Restarting);
}

#[test]
fn retry_timestamp_overflow_is_a_typed_atomic_configuration_failure() {
    let mut supervisor = supervisor(1, Duration::from_nanos(1));
    let worker = plugin("dev.crikey.timestamp-overflow");
    register_and_make_ready(&mut supervisor, &worker);
    let before = supervisor.snapshot(&worker).expect("snapshot is available");

    assert_eq!(
        supervisor.record_failure(&worker, FailureKind::Crash, Duration::MAX),
        Err(SupervisorError::InvalidConfig {
            reason: "circuit-breaker cooldown overflows the event timestamp",
        })
    );
    assert_eq!(supervisor.snapshot(&worker), Some(before));

    supervisor
        .record_failure(&worker, FailureKind::Crash, Duration::ZERO)
        .expect("the rejected event did not advance time or diagnostics");
    assert_eq!(supervisor.state(&worker), WorkerState::Suspended);
}

#[test]
fn zero_cooldown_recovers_at_the_failure_timestamp_boundary() {
    let mut supervisor = supervisor(1, Duration::ZERO);
    let worker = plugin("dev.crikey.zero-cooldown");
    register_and_make_ready(&mut supervisor, &worker);
    supervisor
        .record_failure(&worker, FailureKind::Crash, Duration::from_secs(7))
        .expect("the failure opens the circuit");
    assert_eq!(
        supervisor.circuit_diagnostics(&worker).retry_at,
        Some(Duration::from_secs(7))
    );

    supervisor
        .resume_if_ready(&worker, Duration::from_secs(7))
        .expect("zero cooldown permits recovery at the same timestamp");
    assert_eq!(supervisor.state(&worker), WorkerState::Restarting);
}

#[test]
fn failed_restarting_and_suspended_transition_rejections_are_atomic() {
    let mut supervisor = supervisor(3, Duration::from_secs(10));
    let worker = plugin("dev.crikey.transition-matrix");
    register_and_make_ready(&mut supervisor, &worker);
    supervisor
        .record_failure(&worker, FailureKind::Crash, Duration::from_secs(1))
        .expect("the worker fails");
    let failed = supervisor.snapshot(&worker).expect("snapshot is available");

    assert!(supervisor.start(&worker).is_err());
    assert!(supervisor.mark_ready(&worker).is_err());
    assert!(supervisor.mark_busy(&worker).is_err());
    assert!(supervisor.record_success(&worker).is_err());
    assert!(supervisor
        .resume_if_ready(&worker, Duration::from_secs(1))
        .is_err());
    assert!(supervisor
        .record_latency(&worker, Duration::from_millis(1))
        .is_err());
    assert!(supervisor.record_soft_timeout(&worker).is_err());
    assert!(supervisor
        .record_failure(&worker, FailureKind::Crash, Duration::from_secs(2))
        .is_err());
    assert_eq!(supervisor.snapshot(&worker), Some(failed));

    supervisor.restart(&worker).expect("failed worker restarts");
    let restarting = supervisor.snapshot(&worker).expect("snapshot is available");
    assert!(supervisor.start(&worker).is_err());
    assert!(supervisor.restart(&worker).is_err());
    assert!(supervisor.mark_busy(&worker).is_err());
    assert!(supervisor.record_success(&worker).is_err());
    assert!(supervisor
        .resume_if_ready(&worker, Duration::from_secs(1))
        .is_err());
    assert!(supervisor
        .record_latency(&worker, Duration::from_millis(1))
        .is_err());
    assert!(supervisor.record_soft_timeout(&worker).is_err());
    assert_eq!(supervisor.snapshot(&worker), Some(restarting));

    supervisor.mark_ready(&worker).expect("restart completes");
    supervisor
        .record_failure(&worker, FailureKind::Crash, Duration::from_secs(2))
        .expect("the second failure is recorded");
    supervisor.restart(&worker).expect("failed worker restarts");
    supervisor.mark_ready(&worker).expect("restart completes");
    supervisor
        .record_failure(&worker, FailureKind::Crash, Duration::from_secs(3))
        .expect("the threshold failure suspends the worker");
    let suspended = supervisor.snapshot(&worker).expect("snapshot is available");

    assert!(supervisor.start(&worker).is_err());
    assert!(supervisor.stop(&worker).is_err());
    assert!(supervisor.restart(&worker).is_err());
    assert!(supervisor.mark_ready(&worker).is_err());
    assert!(supervisor.mark_busy(&worker).is_err());
    assert!(supervisor.record_success(&worker).is_err());
    assert!(supervisor
        .record_failure(&worker, FailureKind::Crash, Duration::from_secs(4))
        .is_err());
    assert!(supervisor
        .record_latency(&worker, Duration::from_millis(1))
        .is_err());
    assert!(supervisor.record_soft_timeout(&worker).is_err());
    assert_eq!(supervisor.snapshot(&worker), Some(suspended));

    supervisor
        .resume_if_ready(&worker, Duration::from_secs(13))
        .expect("resume is the only legal suspended transition");
    assert_eq!(supervisor.state(&worker), WorkerState::Restarting);

    let stoppable = plugin("dev.crikey.failed-stop");
    register_and_make_ready(&mut supervisor, &stoppable);
    supervisor
        .record_failure(&stoppable, FailureKind::Crash, Duration::from_secs(1))
        .expect("the second worker fails");
    supervisor
        .stop(&stoppable)
        .expect("a failed worker may still be stopped");
    assert_eq!(supervisor.state(&stoppable), WorkerState::NotStarted);
    supervisor
        .start(&stoppable)
        .expect("the stopped failed worker can start again");
    assert_eq!(supervisor.state(&stoppable), WorkerState::Starting);
}

#[test]
fn state_and_diagnostics_for_registered_plugins_are_isolated() {
    let mut supervisor = supervisor(4, Duration::from_secs(10));
    let left = plugin("dev.crikey.left");
    let right = plugin("dev.crikey.right");
    register_and_make_ready(&mut supervisor, &left);
    register_and_make_ready(&mut supervisor, &right);

    supervisor.mark_busy(&left).expect("left accepts a query");
    supervisor
        .record_queue_depth(&left, 6)
        .expect("left queue depth is recorded");
    supervisor
        .record_latency(&left, Duration::from_millis(24))
        .expect("left latency is recorded");
    supervisor
        .record_cancellation(&left, false, 2)
        .expect("left cancellation compliance is recorded");
    supervisor
        .record_failure(&left, FailureKind::Timeout, Duration::from_secs(1))
        .expect("left failure is recorded");

    assert_eq!(supervisor.state(&left), WorkerState::Failed);
    let left_health = supervisor.health(&left);
    assert_eq!(left_health.timeouts, 1);
    assert_eq!(left_health.queue_depth, 6);
    assert_eq!(left_health.average_latency_ms, 24);
    assert_eq!(left_health.peak_latency_ms, 24);
    assert_eq!(left_health.cancellations_ignored, 2);

    assert_eq!(supervisor.state(&right), WorkerState::Ready);
    assert_zero_health(&supervisor, &right);
}

#[test]
fn cancellation_stale_and_obsolete_counters_use_deltas_and_saturate() {
    let mut supervisor = supervisor(3, Duration::from_secs(30));
    let worker = plugin("dev.crikey.metrics");
    supervisor.register(&worker).expect("registration succeeds");

    supervisor
        .record_cancellation(&worker, true, u64::MAX)
        .expect("honoured cancellations are recorded");
    supervisor
        .record_cancellation(&worker, true, 1)
        .expect("an overflowing increment saturates");
    supervisor
        .record_cancellation(&worker, false, u64::MAX)
        .expect("ignored cancellations are recorded separately");
    supervisor
        .record_cancellation(&worker, false, 1)
        .expect("an overflowing increment saturates");
    supervisor
        .record_stale_result_rejected(&worker, u64::MAX)
        .expect("stale rejections are recorded");
    supervisor
        .record_stale_result_rejected(&worker, 1)
        .expect("an overflowing increment saturates");
    supervisor
        .record_obsolete_request_dropped(&worker, u64::MAX)
        .expect("obsolete drops are recorded");
    supervisor
        .record_obsolete_request_dropped(&worker, 1)
        .expect("an overflowing increment saturates");

    let health = supervisor.health(&worker);
    assert_eq!(health.cancellations_honoured, u64::MAX);
    assert_eq!(health.cancellations_ignored, u64::MAX);
    assert_eq!(health.stale_results_rejected, u64::MAX);
    assert_eq!(health.obsolete_requests_dropped, u64::MAX);
}

#[test]
fn queue_depth_is_a_current_gauge_not_an_accumulating_counter() {
    let mut supervisor = supervisor(3, Duration::from_secs(30));
    let worker = plugin("dev.crikey.queue");
    supervisor.register(&worker).expect("registration succeeds");

    supervisor
        .record_queue_depth(&worker, 9)
        .expect("queue depth is recorded");
    assert_eq!(supervisor.health(&worker).queue_depth, 9);

    supervisor
        .record_queue_depth(&worker, 2)
        .expect("a new observation replaces the old depth");
    assert_eq!(supervisor.health(&worker).queue_depth, 2);

    supervisor
        .record_queue_depth(&worker, u32::MAX)
        .expect("the full public counter range is supported");
    assert_eq!(supervisor.health(&worker).queue_depth, u32::MAX);

    supervisor
        .record_queue_depth(&worker, 0)
        .expect("a drained queue reports zero");
    assert_eq!(supervisor.health(&worker).queue_depth, 0);
}

#[test]
fn latency_health_reports_running_average_and_peak_in_milliseconds() {
    let mut supervisor = supervisor(3, Duration::from_secs(30));
    let worker = plugin("dev.crikey.latency");
    register_and_make_ready(&mut supervisor, &worker);
    supervisor.mark_busy(&worker).expect("a suggestion begins");

    for milliseconds in [10, 20, 30] {
        supervisor
            .record_latency(&worker, Duration::from_millis(milliseconds))
            .expect("latency sample is recorded");
    }
    let health = supervisor.health(&worker);
    assert_eq!(health.average_latency_ms, 20);
    assert_eq!(health.peak_latency_ms, 30);
    assert_eq!(health.latency_samples, 3);

    supervisor
        .record_latency(&worker, Duration::from_millis(40))
        .expect("a later latency sample is recorded");
    let health = supervisor.health(&worker);
    assert_eq!(health.average_latency_ms, 25);
    assert_eq!(health.peak_latency_ms, 40);
    assert_eq!(health.latency_samples, 4);
}

#[test]
fn suggestion_latency_rejects_other_states_and_handles_fractional_and_huge_samples() {
    let mut supervisor = supervisor(3, Duration::from_secs(30));
    let worker = plugin("dev.crikey.fractional-latency");
    register_and_make_ready(&mut supervisor, &worker);
    let ready = supervisor.snapshot(&worker).expect("snapshot is available");

    assert_eq!(
        supervisor.record_latency(&worker, Duration::from_millis(1)),
        Err(SupervisorError::IllegalTransition {
            plugin: worker.clone(),
            operation: "record suggestion latency",
            state: WorkerState::Ready,
        })
    );
    assert_eq!(supervisor.snapshot(&worker), Some(ready));

    supervisor.mark_busy(&worker).expect("a suggestion begins");
    supervisor
        .record_latency(&worker, Duration::from_micros(500))
        .expect("a sub-millisecond sample is recorded");
    let health = supervisor.health(&worker);
    assert_eq!(health.average_latency_ms, 0);
    assert_eq!(health.peak_latency_ms, 0);
    assert_eq!(health.latency_samples, 1);

    supervisor
        .record_latency(&worker, Duration::from_micros(1_500))
        .expect("fractional milliseconds contribute before truncation");
    let health = supervisor.health(&worker);
    assert_eq!(health.average_latency_ms, 1);
    assert_eq!(health.peak_latency_ms, 1);
    assert_eq!(health.latency_samples, 2);

    supervisor
        .record_latency(&worker, Duration::MAX)
        .expect("an unrepresentable millisecond value saturates");
    supervisor
        .record_latency(&worker, Duration::ZERO)
        .expect("the online mean accepts a smaller sample after saturation");
    let health = supervisor.health(&worker);
    assert_eq!(health.average_latency_ms, u64::MAX);
    assert_eq!(health.peak_latency_ms, u64::MAX);
    assert_eq!(health.latency_samples, 4);
}

#[test]
fn deadline_profiles_keep_legacy_callbacks_free_of_modern_hard_kills() {
    let legacy = Deadlines::legacy();
    assert_eq!(legacy.soft, Duration::from_millis(250));
    assert_eq!(legacy.hard, None);
    assert_eq!(legacy.hung_worker, Duration::from_secs(60));

    let native = Deadlines::modern_native();
    assert_eq!(native.soft, Duration::from_millis(50));
    assert_eq!(native.hard, Some(Duration::from_millis(500)));
    assert_eq!(native.hung_worker, Duration::from_secs(30));

    let python = Deadlines::modern_python();
    assert_eq!(python.soft, Duration::from_millis(100));
    assert_eq!(python.hard, Some(Duration::from_millis(500)));
    assert_eq!(python.hung_worker, Duration::from_secs(30));
}
