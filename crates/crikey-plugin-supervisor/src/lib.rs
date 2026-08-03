//! Plugin supervision (spec 5.2, 13, 24).

mod concurrency;

pub use concurrency::{
    shared_budget_from_section, BudgetGuard, BudgetKind, ConcurrencyBudget, OwnedBudgetGuard,
    PluginBudgetHandle, DEFAULT_ACTION_BUDGET, DEFAULT_BACKGROUND_BUDGET, DEFAULT_CATALOG_BUDGET,
    DEFAULT_SUGGESTION_BUDGET,
};

use std::{collections::HashMap, fmt, time::Duration};

use crikey_core::PluginId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    NotStarted,
    Starting,
    Ready,
    Busy,
    Restarting,
    Suspended,
    Failed,
}

/// Deadlines differ by runtime; legacy callbacks are never hard killed on the
/// modern 500 ms budget (spec 9.6, 25.2, 25.3).
#[derive(Debug, Clone, Copy)]
pub struct Deadlines {
    pub soft: Duration,
    /// Hard deadline after which an in-flight suggestion is failed.
    pub hard: Option<Duration>,
    /// Watchdog for a genuinely hung worker, used for legacy recovery only.
    pub hung_worker: Duration,
}

impl Deadlines {
    pub fn modern_native() -> Self {
        Self {
            soft: Duration::from_millis(50),
            hard: Some(Duration::from_millis(500)),
            hung_worker: Duration::from_secs(30),
        }
    }

    pub fn modern_python() -> Self {
        Self {
            soft: Duration::from_millis(100),
            hard: Some(Duration::from_millis(500)),
            hung_worker: Duration::from_secs(30),
        }
    }

    pub fn legacy() -> Self {
        Self {
            soft: Duration::from_millis(250),
            hard: None,
            hung_worker: Duration::from_secs(60),
        }
    }
}

/// Health counters surfaced by diagnostics (spec 24.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PluginHealth {
    pub startup_failures: u32,
    pub crashes: u32,
    /// Hard suggestion timeouts that failed work and contributed to the circuit.
    pub timeouts: u32,
    /// Soft deadline misses; these are diagnostic only.
    pub soft_timeouts: u32,
    pub protocol_violations: u32,
    pub resource_limit_failures: u32,
    pub cancellations_honoured: u64,
    pub cancellations_ignored: u64,
    pub stale_results_rejected: u64,
    pub obsolete_requests_dropped: u64,
    /// Units of work refused because the plugin was already at its declared
    /// `[concurrency]` limit (spec 13.5). A throttled plugin looks broken from
    /// the outside, so the refusal is a first-class diagnostic.
    pub concurrency_refusals: u64,
    pub queue_depth: u32,
    pub average_latency_ms: u64,
    pub peak_latency_ms: u64,
    /// Number of samples represented by the latency diagnostics.
    pub latency_samples: u64,
}

/// Failures that contribute to a plugin's circuit-breaker streak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Startup,
    Crash,
    /// A hard suggestion timeout while the worker is busy.
    ///
    /// Soft deadline misses must be reported with
    /// [`MemorySupervisor::record_soft_timeout`].
    Timeout,
    ProtocolViolation,
    ResourceLimit,
}

/// Suspends a plugin after repeated failures (spec 13.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CircuitBreakerConfig {
    /// Consecutive failures required to suspend a plugin. Zero disables
    /// suspension while retaining failure diagnostics.
    pub failure_threshold: u32,
    pub cooldown: Duration,
}

/// A machine-readable reason a supervisor operation was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorError {
    UnknownPlugin {
        plugin: PluginId,
    },
    AlreadyRegistered {
        plugin: PluginId,
    },
    IllegalTransition {
        plugin: PluginId,
        operation: &'static str,
        state: WorkerState,
    },
    InvalidConfig {
        reason: &'static str,
    },
    RegressiveTimestamp {
        plugin: PluginId,
        previous: Duration,
        received: Duration,
    },
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPlugin { plugin } => {
                write!(formatter, "plugin `{}` is not registered", plugin.0.as_str())
            }
            Self::AlreadyRegistered { plugin } => {
                write!(formatter, "plugin `{}` is already registered", plugin.0.as_str())
            }
            Self::IllegalTransition {
                plugin,
                operation,
                state,
            } => write!(
                formatter,
                "cannot {operation} for plugin `{}` while it is {state:?}",
                plugin.0.as_str()
            ),
            Self::InvalidConfig { reason } => {
                write!(formatter, "invalid supervisor configuration: {reason}")
            }
            Self::RegressiveTimestamp {
                plugin,
                previous,
                received,
            } => write!(
                formatter,
                "plugin `{}` event timestamp {received:?} precedes {previous:?}",
                plugin.0.as_str()
            ),
        }
    }
}

impl std::error::Error for SupervisorError {}

pub type Result<T, E = SupervisorError> = std::result::Result<T, E>;

/// Public circuit-breaker diagnostics for one plugin.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CircuitDiagnostics {
    pub failure_streak: u32,
    pub last_failure: Option<FailureKind>,
    pub last_failure_at: Option<Duration>,
    /// Earliest monotonic timestamp at which a suspended plugin may retry.
    pub retry_at: Option<Duration>,
}

/// A coherent point-in-time view of one registered plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginSnapshot {
    pub state: WorkerState,
    pub health: PluginHealth,
    pub circuit: CircuitDiagnostics,
}

pub trait Supervisor {
    fn start(&mut self, plugin: &PluginId) -> Result<()>;
    fn stop(&mut self, plugin: &PluginId) -> Result<()>;
    fn restart(&mut self, plugin: &PluginId) -> Result<()>;
    fn contains(&self, plugin: &PluginId) -> bool;
    fn snapshot(&self, plugin: &PluginId) -> Option<PluginSnapshot>;
    fn state(&self, plugin: &PluginId) -> WorkerState;
    fn health(&self, plugin: &PluginId) -> PluginHealth;
    fn circuit_diagnostics(&self, plugin: &PluginId) -> CircuitDiagnostics;
}

#[derive(Debug)]
struct PluginRecord {
    state: WorkerState,
    health: PluginHealth,
    circuit: CircuitDiagnostics,
    last_event_at: Option<Duration>,
    latency_mean_ns: u128,
    latency_remainder: u128,
}

impl Default for PluginRecord {
    fn default() -> Self {
        Self {
            state: WorkerState::NotStarted,
            health: PluginHealth::default(),
            circuit: CircuitDiagnostics::default(),
            last_event_at: None,
            latency_mean_ns: 0,
            latency_remainder: 0,
        }
    }
}

/// Deterministic in-memory lifecycle and diagnostics registry.
#[derive(Debug)]
pub struct MemorySupervisor {
    circuit_breaker: CircuitBreakerConfig,
    plugins: HashMap<PluginId, PluginRecord>,
}

impl MemorySupervisor {
    pub fn new(circuit_breaker: CircuitBreakerConfig) -> Self {
        Self {
            circuit_breaker,
            plugins: HashMap::new(),
        }
    }

    pub fn register(&mut self, plugin: &PluginId) -> Result<()> {
        if self.plugins.contains_key(plugin) {
            return Err(SupervisorError::AlreadyRegistered {
                plugin: plugin.clone(),
            });
        }

        self.plugins.insert(plugin.clone(), PluginRecord::default());
        Ok(())
    }

    /// Removes a registration during provider startup rollback.
    ///
    /// No failure or cancellation is recorded: the plugin never became a
    /// live runtime, so retaining a diagnostic record would make a later
    /// registration inherit stale health.
    pub fn unregister(&mut self, plugin: &PluginId) -> bool {
        self.plugins.remove(plugin).is_some()
    }

    pub fn mark_ready(&mut self, plugin: &PluginId) -> Result<()> {
        let record = self.record_mut(plugin)?;
        if !matches!(
            record.state,
            WorkerState::Starting | WorkerState::Busy | WorkerState::Restarting
        ) {
            return Err(Self::illegal_transition(plugin, "mark ready", record.state));
        }

        record.state = WorkerState::Ready;
        Ok(())
    }

    pub fn mark_busy(&mut self, plugin: &PluginId) -> Result<()> {
        let record = self.record_mut(plugin)?;
        if record.state != WorkerState::Ready {
            return Err(Self::illegal_transition(plugin, "mark busy", record.state));
        }

        record.state = WorkerState::Busy;
        Ok(())
    }

    pub fn record_failure(&mut self, plugin: &PluginId, failure: FailureKind, at: Duration) -> Result<()> {
        let CircuitBreakerConfig {
            failure_threshold,
            cooldown,
        } = self.circuit_breaker;
        let record = self.record_mut(plugin)?;
        if !Self::failure_allowed(record.state, failure) {
            return Err(Self::illegal_transition(
                plugin,
                Self::failure_operation(failure),
                record.state,
            ));
        }
        Self::validate_event_timestamp(plugin, record, at)?;

        let next_streak = record.circuit.failure_streak.saturating_add(1);
        let opens_circuit = failure_threshold != 0 && next_streak >= failure_threshold;
        let retry_at = if opens_circuit {
            Some(at.checked_add(cooldown).ok_or(SupervisorError::InvalidConfig {
                reason: "circuit-breaker cooldown overflows the event timestamp",
            })?)
        } else {
            None
        };

        match failure {
            FailureKind::Startup => {
                record.health.startup_failures = record.health.startup_failures.saturating_add(1);
            }
            FailureKind::Crash => {
                record.health.crashes = record.health.crashes.saturating_add(1);
            }
            FailureKind::Timeout => {
                record.health.timeouts = record.health.timeouts.saturating_add(1);
            }
            FailureKind::ProtocolViolation => {
                record.health.protocol_violations = record.health.protocol_violations.saturating_add(1);
            }
            FailureKind::ResourceLimit => {
                record.health.resource_limit_failures =
                    record.health.resource_limit_failures.saturating_add(1);
            }
        }

        record.circuit.failure_streak = next_streak;
        record.circuit.last_failure = Some(failure);
        record.circuit.last_failure_at = Some(at);
        record.circuit.retry_at = retry_at;
        record.last_event_at = Some(at);
        record.state = if opens_circuit {
            WorkerState::Suspended
        } else {
            WorkerState::Failed
        };
        Ok(())
    }

    /// Records a soft suggestion deadline miss without failing work or changing
    /// the circuit-breaker streak.
    pub fn record_soft_timeout(&mut self, plugin: &PluginId) -> Result<()> {
        let record = self.record_mut(plugin)?;
        if record.state != WorkerState::Busy {
            return Err(Self::illegal_transition(
                plugin,
                "record soft timeout",
                record.state,
            ));
        }

        record.health.soft_timeouts = record.health.soft_timeouts.saturating_add(1);
        Ok(())
    }

    pub fn record_success(&mut self, plugin: &PluginId) -> Result<()> {
        let record = self.record_mut(plugin)?;
        if !matches!(record.state, WorkerState::Ready | WorkerState::Busy) {
            return Err(Self::illegal_transition(plugin, "record success", record.state));
        }

        record.circuit.failure_streak = 0;
        record.circuit.retry_at = None;
        Ok(())
    }

    pub fn resume_if_ready(&mut self, plugin: &PluginId, now: Duration) -> Result<()> {
        let record = self.record_mut(plugin)?;
        if record.state != WorkerState::Suspended {
            return Err(Self::illegal_transition(
                plugin,
                "resume suspended plugin",
                record.state,
            ));
        }
        let retry_at = record.circuit.retry_at.ok_or(SupervisorError::InvalidConfig {
            reason: "suspended plugin has no retry timestamp",
        })?;
        Self::validate_event_timestamp(plugin, record, now)?;

        record.last_event_at = Some(now);
        if now >= retry_at {
            record.state = WorkerState::Restarting;
            record.circuit.retry_at = None;
        }
        Ok(())
    }

    /// Records suggestion latency. A sample is valid only while a suggestion
    /// is in flight (`WorkerState::Busy`).
    pub fn record_latency(&mut self, plugin: &PluginId, latency: Duration) -> Result<()> {
        let record = self.record_mut(plugin)?;
        if record.state != WorkerState::Busy {
            return Err(Self::illegal_transition(
                plugin,
                "record suggestion latency",
                record.state,
            ));
        }

        let sample_ns = latency.as_nanos();
        let old_count = record.health.latency_samples;
        if old_count == 0 {
            record.latency_mean_ns = sample_ns;
            record.latency_remainder = 0;
            record.health.latency_samples = 1;
        } else if old_count != u64::MAX {
            let new_count = old_count + 1;
            let denominator = u128::from(new_count);
            let adjusted_sample = record.latency_remainder + sample_ns;
            if adjusted_sample >= record.latency_mean_ns {
                let excess = adjusted_sample - record.latency_mean_ns;
                record.latency_mean_ns += excess / denominator;
                record.latency_remainder = excess % denominator;
            } else {
                let deficit = record.latency_mean_ns - adjusted_sample;
                let decrement = ((deficit - 1) / denominator) + 1;
                record.latency_mean_ns -= decrement;
                record.latency_remainder = decrement * denominator - deficit;
            }
            record.health.latency_samples = new_count;
        }

        record.health.average_latency_ms =
            u64::try_from(record.latency_mean_ns / 1_000_000).unwrap_or(u64::MAX);
        let latency_ms = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX);
        record.health.peak_latency_ms = record.health.peak_latency_ms.max(latency_ms);
        Ok(())
    }

    pub fn record_queue_depth(&mut self, plugin: &PluginId, depth: u32) -> Result<()> {
        self.record_mut(plugin)?.health.queue_depth = depth;
        Ok(())
    }

    pub fn record_cancellation(&mut self, plugin: &PluginId, honoured: bool, delta: u64) -> Result<()> {
        let health = &mut self.record_mut(plugin)?.health;
        if honoured {
            health.cancellations_honoured = health.cancellations_honoured.saturating_add(delta);
        } else {
            health.cancellations_ignored = health.cancellations_ignored.saturating_add(delta);
        }
        Ok(())
    }

    pub fn record_stale_result_rejected(&mut self, plugin: &PluginId, delta: u64) -> Result<()> {
        let health = &mut self.record_mut(plugin)?.health;
        health.stale_results_rejected = health.stale_results_rejected.saturating_add(delta);
        Ok(())
    }

    pub fn record_obsolete_request_dropped(&mut self, plugin: &PluginId, delta: u64) -> Result<()> {
        let health = &mut self.record_mut(plugin)?.health;
        health.obsolete_requests_dropped = health.obsolete_requests_dropped.saturating_add(delta);
        Ok(())
    }

    /// Records `delta` units of work refused by the plugin's concurrency
    /// budget. Legal in any state: admission is decided before a worker is
    /// consulted, so a refusal is not a lifecycle transition.
    pub fn record_concurrency_refusal(&mut self, plugin: &PluginId, delta: u64) -> Result<()> {
        let health = &mut self.record_mut(plugin)?.health;
        health.concurrency_refusals = health.concurrency_refusals.saturating_add(delta);
        Ok(())
    }

    fn failure_allowed(state: WorkerState, failure: FailureKind) -> bool {
        match failure {
            FailureKind::Startup => {
                matches!(state, WorkerState::Starting | WorkerState::Restarting)
            }
            FailureKind::Timeout => state == WorkerState::Busy,
            FailureKind::Crash | FailureKind::ProtocolViolation | FailureKind::ResourceLimit => matches!(
                state,
                WorkerState::Starting | WorkerState::Ready | WorkerState::Busy | WorkerState::Restarting
            ),
        }
    }

    fn failure_operation(failure: FailureKind) -> &'static str {
        match failure {
            FailureKind::Startup => "record startup failure",
            FailureKind::Crash => "record crash",
            FailureKind::Timeout => "record hard timeout",
            FailureKind::ProtocolViolation => "record protocol violation",
            FailureKind::ResourceLimit => "record resource-limit failure",
        }
    }

    fn validate_event_timestamp(plugin: &PluginId, record: &PluginRecord, received: Duration) -> Result<()> {
        if let Some(previous) = record.last_event_at {
            if received < previous {
                return Err(SupervisorError::RegressiveTimestamp {
                    plugin: plugin.clone(),
                    previous,
                    received,
                });
            }
        }
        Ok(())
    }

    fn record_mut(&mut self, plugin: &PluginId) -> Result<&mut PluginRecord> {
        self.plugins
            .get_mut(plugin)
            .ok_or_else(|| Self::unknown_plugin(plugin))
    }

    fn unknown_plugin(plugin: &PluginId) -> SupervisorError {
        SupervisorError::UnknownPlugin {
            plugin: plugin.clone(),
        }
    }

    fn illegal_transition(plugin: &PluginId, operation: &'static str, state: WorkerState) -> SupervisorError {
        SupervisorError::IllegalTransition {
            plugin: plugin.clone(),
            operation,
            state,
        }
    }
}

impl Supervisor for MemorySupervisor {
    fn start(&mut self, plugin: &PluginId) -> Result<()> {
        let record = self.record_mut(plugin)?;
        if record.state != WorkerState::NotStarted {
            return Err(Self::illegal_transition(plugin, "start", record.state));
        }

        record.state = WorkerState::Starting;
        Ok(())
    }

    fn stop(&mut self, plugin: &PluginId) -> Result<()> {
        let record = self.record_mut(plugin)?;
        if matches!(record.state, WorkerState::NotStarted | WorkerState::Suspended) {
            return Err(Self::illegal_transition(plugin, "stop", record.state));
        }

        record.state = WorkerState::NotStarted;
        record.circuit.retry_at = None;
        Ok(())
    }

    fn restart(&mut self, plugin: &PluginId) -> Result<()> {
        let record = self.record_mut(plugin)?;
        if !matches!(
            record.state,
            WorkerState::Ready | WorkerState::Busy | WorkerState::Failed
        ) {
            return Err(Self::illegal_transition(plugin, "restart", record.state));
        }

        record.state = WorkerState::Restarting;
        Ok(())
    }

    fn contains(&self, plugin: &PluginId) -> bool {
        self.plugins.contains_key(plugin)
    }

    fn snapshot(&self, plugin: &PluginId) -> Option<PluginSnapshot> {
        self.plugins.get(plugin).map(|record| PluginSnapshot {
            state: record.state,
            health: record.health,
            circuit: record.circuit,
        })
    }

    fn state(&self, plugin: &PluginId) -> WorkerState {
        self.plugins
            .get(plugin)
            .map_or(WorkerState::NotStarted, |record| record.state)
    }

    fn health(&self, plugin: &PluginId) -> PluginHealth {
        self.plugins
            .get(plugin)
            .map_or_else(PluginHealth::default, |record| record.health)
    }

    fn circuit_diagnostics(&self, plugin: &PluginId) -> CircuitDiagnostics {
        self.plugins
            .get(plugin)
            .map_or_else(CircuitDiagnostics::default, |record| record.circuit)
    }
}
