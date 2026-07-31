//! Native worker restart and circuit supervision (spec 13.7, 24.1-24.4).

use std::collections::BTreeMap;
use std::time::Duration;

use crikey_core::PluginId;
use crikey_plugin_supervisor::{
    CircuitBreakerConfig, FailureKind, MemorySupervisor, PluginHealth, PluginSnapshot, Supervisor,
    WorkerState,
};

use crate::launch::{LaunchSpec, WorkerOptions};
use crate::worker::{ExitKind, ExitRecord, HostError, NativeWorker};

/// Restart policy and circuit-breaker configuration.
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub max_restarts: u32,
    pub restart_window_ms: u64,
    pub base_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub circuit: CircuitBreakerConfig,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            max_restarts: 3,
            restart_window_ms: 60_000,
            base_backoff_ms: 100,
            max_backoff_ms: 5_000,
            circuit: CircuitBreakerConfig {
                failure_threshold: 3,
                cooldown: Duration::from_millis(60_000),
            },
        }
    }
}

#[derive(Debug)]
struct Registration {
    spec: LaunchSpec,
    options: WorkerOptions,
    worker: Option<NativeWorker>,
    exits: Vec<ExitRecord>,
    restarts: u32,
    failure_times: Vec<u64>,
    last_failure_at: Option<u64>,
}

/// Lazily-started native workers with deterministic caller-supplied time.
#[derive(Debug)]
pub struct NativeSupervisor {
    config: SupervisorConfig,
    memory: MemorySupervisor,
    registrations: BTreeMap<PluginId, Registration>,
}

impl NativeSupervisor {
    pub fn new(config: SupervisorConfig) -> Self {
        Self {
            memory: MemorySupervisor::new(config.circuit),
            config,
            registrations: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, spec: LaunchSpec, options: WorkerOptions) -> Result<(), HostError> {
        let plugin = spec.plugin.clone();
        if self.registrations.contains_key(&plugin) {
            return Err(HostError::ResourceLimit {
                plugin,
                detail: "plugin is already registered".to_owned(),
            });
        }
        self.memory
            .register(&spec.plugin)
            .map_err(|error| HostError::ResourceLimit {
                plugin: spec.plugin.clone(),
                detail: error.to_string(),
            })?;
        self.registrations.insert(
            spec.plugin.clone(),
            Registration {
                spec,
                options,
                worker: None,
                exits: Vec::new(),
                restarts: 0,
                failure_times: Vec::new(),
                last_failure_at: None,
            },
        );
        Ok(())
    }

    pub fn worker(&mut self, plugin: &PluginId, now_ms: u64) -> Result<&mut NativeWorker, HostError> {
        if !self.registrations.contains_key(plugin) {
            return Err(HostError::ResourceLimit {
                plugin: plugin.clone(),
                detail: "plugin is not registered".to_owned(),
            });
        }

        let (already_alive, dead_exit, call_failure) = {
            let registration = self.registrations.get_mut(plugin).expect("checked above");
            let alive = registration.worker.as_mut().is_some_and(NativeWorker::is_alive);
            let call_failure = registration
                .worker
                .as_mut()
                .and_then(NativeWorker::take_failure_kind);
            if alive {
                (true, None, call_failure)
            } else {
                (
                    false,
                    registration.worker.take().map(NativeWorker::shutdown),
                    call_failure,
                )
            }
        };

        if already_alive {
            if let Some(failure) = call_failure {
                let _ = self.record_failure(plugin, failure, now_ms);
            }
        } else if let Some(exit) = dead_exit {
            let failure = call_failure.unwrap_or_else(|| failure_kind_for_exit(&exit));
            let registration = self.registrations.get_mut(plugin).expect("checked above");
            registration.exits.push(exit);
            registration.restarts = registration.restarts.saturating_add(1);
            let _ = self.record_failure(plugin, failure, now_ms);
        }
        if !already_alive {
            if !self.prepare_start(plugin, now_ms)? {
                return Err(HostError::ResourceLimit {
                    plugin: plugin.clone(),
                    detail: "restart budget or circuit breaker is open".to_owned(),
                });
            }
            let (spec, options) = {
                let registration = self.registrations.get(plugin).expect("checked above");
                (registration.spec.clone(), registration.options.clone())
            };
            let replacement = match NativeWorker::spawn(spec, options) {
                Ok(worker) => worker,
                Err(error) => {
                    let failure = failure_kind_for_error(&error);
                    let _ = self.record_failure(plugin, failure, now_ms);
                    return Err(error);
                }
            };
            let _ = self.memory.mark_ready(plugin);
            self.registrations.get_mut(plugin).expect("checked above").worker = Some(replacement);
        }

        self.mark_busy(plugin);
        let registration = self.registrations.get_mut(plugin).expect("checked above");
        registration.worker.as_mut().ok_or(HostError::Closed)
    }

    pub fn restarts(&self, plugin: &PluginId) -> u32 {
        self.registrations.get(plugin).map_or(0, |record| record.restarts)
    }

    pub fn last_exit(&self, plugin: &PluginId) -> Option<&ExitRecord> {
        self.registrations
            .get(plugin)
            .and_then(|record| record.exits.last())
    }

    pub fn exits(&self, plugin: &PluginId) -> &[ExitRecord] {
        self.registrations
            .get(plugin)
            .map_or(&[], |record| record.exits.as_slice())
    }

    pub fn health(&self, plugin: &PluginId) -> PluginHealth {
        self.memory.health(plugin)
    }

    pub fn snapshot(&self, plugin: &PluginId) -> Option<PluginSnapshot> {
        self.memory.snapshot(plugin)
    }

    pub fn is_suspended(&self, plugin: &PluginId, now_ms: u64) -> bool {
        self.memory
            .snapshot(plugin)
            .is_some_and(|snapshot| snapshot.state == WorkerState::Suspended)
            && self.next_retry_at_ms(plugin).is_some_and(|retry| now_ms < retry)
    }

    pub fn next_retry_at_ms(&self, plugin: &PluginId) -> Option<u64> {
        let registration = self.registrations.get(plugin)?;
        let backoff = registration
            .last_failure_at
            .map(|at| at.saturating_add(self.backoff_ms(registration.restarts.saturating_add(1))));
        let circuit = self
            .memory
            .snapshot(plugin)
            .and_then(|snapshot| snapshot.circuit.retry_at)
            .map(|at| at.as_millis().min(u128::from(u64::MAX)) as u64);
        match (backoff, circuit) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        }
    }

    pub fn shutdown_all(&mut self) {
        for registration in self.registrations.values_mut() {
            if let Some(worker) = registration.worker.take() {
                registration.exits.push(worker.shutdown());
            }
        }
    }

    fn mark_busy(&mut self, plugin: &PluginId) {
        if self
            .memory
            .snapshot(plugin)
            .is_some_and(|snapshot| snapshot.state == WorkerState::Ready)
        {
            let _ = self.memory.mark_busy(plugin);
        }
    }

    fn prepare_start(&mut self, plugin: &PluginId, now_ms: u64) -> Result<bool, HostError> {
        let (restarts, last_failure, failure_times) = {
            let registration = self.registrations.get(plugin).expect("checked above");
            (
                registration.restarts,
                registration.last_failure_at,
                registration.failure_times.clone(),
            )
        };
        if restarts > self.config.max_restarts && last_failure.is_some() {
            return Ok(false);
        }
        if let Some(last_failure) = last_failure {
            let backoff = self.backoff_ms(restarts.saturating_add(1));
            if now_ms < last_failure.saturating_add(backoff) {
                return Ok(false);
            }
        }
        if self
            .memory
            .snapshot(plugin)
            .is_some_and(|snapshot| snapshot.state == WorkerState::Suspended)
        {
            let _ = self.memory.resume_if_ready(plugin, Duration::from_millis(now_ms));
            if self
                .memory
                .snapshot(plugin)
                .is_some_and(|snapshot| snapshot.state == WorkerState::Suspended)
            {
                return Ok(false);
            }
        }
        if self.config.restart_window_ms != 0 {
            let cutoff = now_ms.saturating_sub(self.config.restart_window_ms);
            let recent = failure_times
                .iter()
                .filter(|timestamp| **timestamp >= cutoff)
                .count();
            if recent > self.config.max_restarts as usize {
                return Ok(false);
            }
        }
        let snapshot = self.memory.snapshot(plugin);
        match snapshot.map(|snapshot| snapshot.state) {
            Some(WorkerState::NotStarted) => {
                let _ = self.memory.start(plugin);
            }
            Some(WorkerState::Failed | WorkerState::Ready) => {
                let _ = self.memory.restart(plugin);
            }
            _ => {}
        }
        Ok(true)
    }

    fn record_failure(&mut self, plugin: &PluginId, kind: FailureKind, now_ms: u64) -> Result<(), HostError> {
        if let Some(registration) = self.registrations.get_mut(plugin) {
            registration.failure_times.push(now_ms);
            registration.last_failure_at = Some(now_ms);
        }
        self.memory
            .record_failure(plugin, kind, Duration::from_millis(now_ms))
            .map_err(|error| HostError::ResourceLimit {
                plugin: plugin.clone(),
                detail: error.to_string(),
            })
    }

    fn backoff_ms(&self, attempt: u32) -> u64 {
        let shift = attempt.saturating_sub(1).min(63);
        self.config
            .base_backoff_ms
            .saturating_mul(1u64.checked_shl(shift).unwrap_or(u64::MAX))
            .min(self.config.max_backoff_ms)
    }
}

fn failure_kind_for_exit(exit: &ExitRecord) -> FailureKind {
    exit.failure_kind.unwrap_or(match exit.kind {
        ExitKind::ProtocolViolation => FailureKind::ProtocolViolation,
        ExitKind::Crashed | ExitKind::Clean | ExitKind::Killed => FailureKind::Crash,
    })
}

fn failure_kind_for_error(error: &HostError) -> FailureKind {
    match error {
        HostError::Spawn(_) | HostError::Handshake(_) => FailureKind::Startup,
        HostError::Protocol(_) => FailureKind::ProtocolViolation,
        HostError::Timeout { .. } => FailureKind::Timeout,
        HostError::Crashed { .. } | HostError::Closed => FailureKind::Crash,
        HostError::ResourceLimit { .. } => FailureKind::ResourceLimit,
        HostError::PluginFailed { .. } => FailureKind::Crash,
    }
}
