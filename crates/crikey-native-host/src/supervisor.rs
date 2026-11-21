//! Native worker restart and circuit supervision (spec 13.7, 24.1-24.4).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crikey_core::PluginId;
use crikey_plugin_supervisor::{
    CircuitBreakerConfig, FailureKind, MemorySupervisor, PluginHealth, PluginSnapshot, Supervisor,
    WorkerState,
};

use crate::launch::{LaunchSpec, WorkerOptions};
use crate::worker::{ExitKind, ExitRecord, HostError, NativeStartupOwner, NativeWorker};

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
    startup: Option<NativeStartupOwner>,
    exits: Vec<ExitRecord>,
    /// Last already-validated release; retired generations need no live owner.
    retired: Option<ExitRecord>,
    restarts: u32,
    failure_times: VecDeque<u64>,
    last_failure_at: Option<u64>,
}

/// One-way process-start revocation without taking a busy supervisor lock.
#[derive(Debug, Clone, Default)]
pub struct NativeAdmission(Arc<Mutex<DispatchState>>);
#[derive(Debug, Default)]
struct DispatchState {
    closed: bool,
    committed: usize,
}
pub(crate) struct NativeDispatch(NativeAdmission);
impl NativeAdmission {
    pub fn close(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).closed
    }
    pub(crate) fn commit(&self, callback: bool) -> Option<NativeDispatch> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if callback && state.closed {
            return None;
        }
        state.committed = state.committed.checked_add(1)?;
        Some(NativeDispatch(self.clone()))
    }
    fn committed(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).committed
    }
}
impl Drop for NativeDispatch {
    fn drop(&mut self) {
        self.0 .0.lock().unwrap_or_else(|e| e.into_inner()).committed -= 1;
    }
}

/// Lazily-started native workers with deterministic caller-supplied time.
#[derive(Debug)]
pub struct NativeSupervisor {
    config: SupervisorConfig,
    memory: MemorySupervisor,
    registrations: BTreeMap<PluginId, Registration>,
    closed: bool,
    cleanup_failed: bool,
    settling: bool,
    admission: NativeAdmission,
}

impl NativeSupervisor {
    pub fn new(config: SupervisorConfig) -> Self {
        Self {
            memory: MemorySupervisor::new(config.circuit),
            config,
            registrations: BTreeMap::new(),
            closed: false,
            cleanup_failed: false,
            settling: false,
            admission: NativeAdmission::default(),
        }
    }

    pub fn register(&mut self, spec: LaunchSpec, options: WorkerOptions) -> Result<(), HostError> {
        if self.closed || self.admission.is_closed() {
            return Err(HostError::Closed);
        }
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
                startup: None,
                exits: Vec::new(),
                retired: None,
                restarts: 0,
                failure_times: VecDeque::new(),
                last_failure_at: None,
            },
        );
        Ok(())
    }

    /// Observes an existing connection without starting, replacing, or cleaning
    /// up a worker. Dead workers remain owned here until a normal operation or
    /// shutdown performs cleanup and the usual restart/failure accounting.
    /// Background callers can publish `None` before that potentially blocking
    /// cleanup; UI readers should consume their published snapshot instead.
    pub fn poll_live_connection(&mut self, plugin: &PluginId) -> Option<u64> {
        if self.closed
            || self.admission.is_closed()
            || self
                .memory
                .snapshot(plugin)
                .is_some_and(|snapshot| snapshot.state == WorkerState::Suspended)
        {
            return None;
        }
        let worker = self.registrations.get_mut(plugin)?.worker.as_mut()?;
        worker.poll_alive().then(|| worker.connection_id())
    }

    /// Observes the actual process without starting or replacing a worker.
    pub fn poll_live_incarnation(&mut self, plugin: &PluginId) -> Option<u64> {
        if self.closed
            || self.admission.is_closed()
            || self
                .memory
                .snapshot(plugin)
                .is_some_and(|s| s.state == WorkerState::Suspended)
        {
            return None;
        }
        let worker = self.registrations.get_mut(plugin)?.worker.as_mut()?;
        worker.poll_alive().then(|| worker.incarnation())
    }

    /// Borrows an already running worker without starting or replacing it.
    /// Selection-bound requests must use this path: a fresh process does not
    /// own the item state produced by the original catalog/suggestion call.
    /// Caller retains the supervisor lock for the complete operation.
    pub fn live_worker(&mut self, plugin: &PluginId) -> Option<&mut NativeWorker> {
        if self.closed
            || self.admission.is_closed()
            || self
                .memory
                .snapshot(plugin)
                .is_some_and(|snapshot| snapshot.state == WorkerState::Suspended)
        {
            return None;
        }
        let worker = self.registrations.get_mut(plugin)?.worker.as_mut()?;
        if worker.is_alive() {
            Some(worker)
        } else {
            None
        }
    }

    pub fn worker(&mut self, plugin: &PluginId, now_ms: u64) -> Result<&mut NativeWorker, HostError> {
        if self.closed || self.admission.is_closed() {
            return Err(HostError::Closed);
        }
        if !self.registrations.contains_key(plugin) {
            return Err(HostError::ResourceLimit {
                plugin: plugin.clone(),
                detail: "plugin is not registered".to_owned(),
            });
        }
        {
            let registration = self.registrations.get_mut(plugin).expect("checked above");
            if let Some(startup) = registration.startup.as_mut() {
                registration.retired = Some(startup.settle()?);
                registration.startup.take();
            }
        }
        if self
            .memory
            .snapshot(plugin)
            .is_some_and(|snapshot| snapshot.state == WorkerState::Suspended)
        {
            self.discard_worker(plugin);
        }
        let (already_alive, dead_exit, call_failure, call_success) = {
            let registration = self.registrations.get_mut(plugin).expect("checked above");
            let alive = registration.worker.as_mut().is_some_and(NativeWorker::is_alive);
            let call_failure = registration
                .worker
                .as_mut()
                .and_then(NativeWorker::take_failure_kind);
            let call_success = registration
                .worker
                .as_mut()
                .is_some_and(NativeWorker::take_call_success);
            if alive {
                (true, None, call_failure, call_success)
            } else {
                let exit = match registration.worker.as_mut() {
                    Some(worker) => Some(worker.shutdown_checked()?),
                    None => None,
                };
                registration.worker.take();
                (false, exit, call_failure, call_success)
            }
        };

        if already_alive {
            if let Some(failure) = call_failure {
                let _ = self.record_failure(plugin, failure, now_ms);
            } else if call_success {
                self.recover_failed_state(plugin);
                let _ = self.memory.record_success(plugin);
            }
            if self
                .memory
                .snapshot(plugin)
                .is_some_and(|snapshot| snapshot.state == WorkerState::Suspended)
            {
                self.discard_worker(plugin);
                return Err(HostError::ResourceLimit {
                    plugin: plugin.clone(),
                    detail: "restart circuit breaker is open".to_owned(),
                });
            }
        } else if let Some(exit) = dead_exit {
            self.record_failed_exit(plugin, exit, call_failure, now_ms);
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
            let registration = self.registrations.get_mut(plugin).expect("checked above");
            let replacement = match NativeWorker::spawn_with_admission(
                spec,
                options,
                self.admission.clone(),
                &mut registration.startup,
            ) {
                Ok(worker) => worker,
                Err(error) => {
                    let registration = self.registrations.get_mut(plugin).expect("checked above");
                    registration.restarts = registration.restarts.saturating_add(1);
                    self.cleanup_failed |= self.admission.is_closed() && registration.startup.is_some();
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
            .map(|at| at.saturating_add(self.backoff_ms(registration.restarts)));
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

    /// Releases a child with checked settlement, preserving its registration and
    /// prior failure, restart, and backoff accounting. Pending worker failures
    /// are recorded once at the caller's policy time before releasing the child.
    /// A later `worker` call may start a new child subject to that policy.
    /// Missing registrations or children are already released.
    ///
    /// Shutdown may block: call only from a background operation, never the UI.
    pub fn release_worker(&mut self, plugin: &PluginId, now_ms: u64) {
        let (alive, failure, outcome) = {
            let Some(registration) = self.registrations.get_mut(plugin) else {
                return;
            };
            let Some(worker) = registration.worker.as_mut() else {
                return;
            };
            let alive = worker.is_alive();
            let failure = worker.take_failure_kind();
            let outcome = worker.shutdown_checked();
            if outcome.is_ok() {
                registration.worker.take();
            }
            (alive, failure, outcome)
        };
        if let Ok(exit) = outcome {
            if !alive {
                self.record_failed_exit(plugin, exit, failure, now_ms);
            } else {
                if let Some(registration) = self.registrations.get_mut(plugin) {
                    registration.retired = Some(exit);
                }
                if let Some(failure) = failure {
                    let _ = self.record_failure(plugin, failure, now_ms);
                }
            }
        }
    }

    /// Returns this supervisor's permanent, cancellation-only start revocation.
    pub fn admission(&self) -> NativeAdmission {
        self.admission.clone()
    }

    /// Stops every child and permanently closes this supervisor.
    pub fn shutdown_all(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.admission.close();
        for registration in self.registrations.values_mut() {
            if let Some(startup) = registration.startup.as_mut() {
                match startup.settle() {
                    Ok(exit) => {
                        registration.retired = Some(exit);
                        registration.startup.take();
                    }
                    Err(_) => self.cleanup_failed = true,
                }
            }
            if let Some(worker) = registration.worker.as_mut() {
                match worker.shutdown_checked() {
                    Ok(exit) => {
                        registration.exits.push(exit);
                        registration.worker.take();
                    }
                    Err(_) => self.cleanup_failed = true,
                }
            }
        }
    }

    /// Retained actual process outcomes, including healthy catalog releases.
    pub fn termination_ledger(&self) -> Vec<(PluginId, ExitRecord)> {
        self.registrations
            .iter()
            .flat_map(|(owner, registration)| {
                registration
                    .exits
                    .iter()
                    .chain(registration.retired.iter())
                    .cloned()
                    .map(|exit| (owner.clone(), exit))
            })
            .collect()
    }

    /// Permanent shutdown with retained, physical process-exit validation.
    /// Historical guest health is separate from physical ownership settlement.
    pub fn shutdown_all_checked(&mut self) -> Result<(), HostError> {
        if self.settling {
            return Err(HostError::Closed);
        }
        self.shutdown_all();
        if self.admission.committed() != 0 {
            return Err(HostError::Closed);
        }
        if self
            .registrations
            .values()
            .any(|registration| registration.worker.is_some() || registration.startup.is_some())
        {
            return Err(HostError::Closed);
        }
        if self.cleanup_failed {
            return Err(HostError::Closed);
        }
        for (plugin, registration) in &self.registrations {
            for exit in &registration.exits {
                if exit.code.is_none() && exit.signal.is_none() {
                    return Err(HostError::PluginFailed {
                        plugin: plugin.clone(),
                        detail: "native process termination was not cleanly confirmed".into(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Settles the canonical shared owner without holding its mutex across I/O
    /// or joins. Failed and unwinding owners are restored by the transfer guard.
    pub fn shutdown_shared(supervisor: &Arc<Mutex<Self>>) -> Result<(), HostError> {
        {
            let mut state = supervisor.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed {
                return state.shutdown_all_checked();
            }
            state.closed = true;
            state.settling = true;
            state.admission.close();
        }
        let mut run = NativeShutdownRun {
            supervisor: Arc::clone(supervisor),
            complete: false,
        };
        let mut cursor = None;
        loop {
            let transfer = {
                use std::ops::Bound::{Excluded, Unbounded};
                let mut state = supervisor.lock().unwrap_or_else(|e| e.into_inner());
                let next = match &cursor {
                    Some(last) => state
                        .registrations
                        .range_mut::<PluginId, _>((Excluded(last), Unbounded))
                        .next(),
                    None => state.registrations.iter_mut().next(),
                };
                next.map(|(plugin, registration)| {
                    let plugin = plugin.clone();
                    NativeShutdownTransfer {
                        supervisor: Arc::clone(supervisor),
                        plugin,
                        worker: registration.worker.take(),
                        startup: registration.startup.take(),
                        worker_exit: None,
                        startup_exit: None,
                        failed: false,
                    }
                })
            };
            let Some(mut transfer) = transfer else {
                break;
            };
            cursor = Some(transfer.plugin.clone());
            transfer.settle();
            drop(transfer);
        }
        let mut state = supervisor.lock().unwrap_or_else(|e| e.into_inner());
        state.settling = false;
        let result = state.shutdown_all_checked();
        drop(state);
        run.complete = true;
        result
    }

    fn discard_worker(&mut self, plugin: &PluginId) {
        if let Some(registration) = self.registrations.get_mut(plugin) {
            if let Some(worker) = registration.worker.as_mut() {
                if let Ok(exit) = worker.finish_forced_checked() {
                    registration.exits.push(exit);
                    registration.worker.take();
                }
            }
        }
    }

    fn recover_failed_state(&mut self, plugin: &PluginId) {
        if self
            .memory
            .snapshot(plugin)
            .is_some_and(|snapshot| snapshot.state == WorkerState::Failed)
        {
            let _ = self.memory.restart(plugin);
            let _ = self.memory.mark_ready(plugin);
        }
    }

    fn mark_busy(&mut self, plugin: &PluginId) {
        self.recover_failed_state(plugin);
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
        if let Some(last_failure) = last_failure {
            if self.config.restart_window_ms == 0 {
                if restarts > self.config.max_restarts {
                    return Ok(false);
                }
            } else {
                let cutoff = now_ms.saturating_sub(self.config.restart_window_ms);
                let recent = failure_times
                    .iter()
                    .filter(|timestamp| **timestamp >= cutoff)
                    .count();
                if recent > self.config.max_restarts as usize {
                    return Ok(false);
                }
            }
            let backoff = self.backoff_ms(restarts);
            if now_ms < last_failure.saturating_add(backoff) {
                return Ok(false);
            }
        }
        let snapshot = self.memory.snapshot(plugin);
        match snapshot.map(|snapshot| snapshot.state) {
            Some(WorkerState::NotStarted) => {
                let _ = self.memory.start(plugin);
            }
            Some(WorkerState::Suspended) => {
                // An open circuit refuses restarts until its cooldown elapses.
                // Without this arm a freshly opened circuit would still spawn a
                // replacement, handing the caller a live worker and silently
                // discarding the failure accounting that just suspended it.
                if self
                    .memory
                    .resume_if_ready(plugin, Duration::from_millis(now_ms))
                    .is_err()
                {
                    return Ok(false);
                }
                if self
                    .memory
                    .snapshot(plugin)
                    .is_some_and(|snapshot| snapshot.state == WorkerState::Suspended)
                {
                    return Ok(false);
                }
            }
            Some(WorkerState::Failed | WorkerState::Ready) => {
                let _ = self.memory.restart(plugin);
            }
            _ => {}
        }
        Ok(true)
    }

    fn record_failed_exit(
        &mut self,
        plugin: &PluginId,
        exit: ExitRecord,
        failure: Option<FailureKind>,
        now_ms: u64,
    ) {
        let failure = failure.unwrap_or_else(|| failure_kind_for_exit(&exit));
        let registration = self.registrations.get_mut(plugin).expect("worker is registered");
        registration.exits.push(exit);
        registration.restarts = registration.restarts.saturating_add(1);
        let _ = self.record_failure(plugin, failure, now_ms);
    }

    fn record_failure(&mut self, plugin: &PluginId, kind: FailureKind, now_ms: u64) -> Result<(), HostError> {
        let window = self.config.restart_window_ms;
        let max_entries = usize::try_from(self.config.max_restarts)
            .unwrap_or(usize::MAX)
            .saturating_add(1);
        if let Some(registration) = self.registrations.get_mut(plugin) {
            if window == 0 {
                registration.failure_times.clear();
            } else {
                let cutoff = now_ms.saturating_sub(window);
                registration
                    .failure_times
                    .retain(|timestamp| *timestamp >= cutoff);
            }
            while registration.failure_times.len() >= max_entries {
                let _ = registration.failure_times.pop_front();
            }
            registration.failure_times.push_back(now_ms);
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

struct NativeShutdownRun {
    supervisor: Arc<Mutex<NativeSupervisor>>,
    complete: bool,
}
impl Drop for NativeShutdownRun {
    fn drop(&mut self) {
        if !self.complete {
            let mut state = self.supervisor.lock().unwrap_or_else(|e| e.into_inner());
            state.settling = false;
            state.cleanup_failed = true;
        }
    }
}

struct NativeShutdownTransfer {
    supervisor: Arc<Mutex<NativeSupervisor>>,
    plugin: PluginId,
    worker: Option<NativeWorker>,
    startup: Option<NativeStartupOwner>,
    worker_exit: Option<ExitRecord>,
    startup_exit: Option<ExitRecord>,
    failed: bool,
}
impl NativeShutdownTransfer {
    fn settle(&mut self) {
        if let Some(startup) = self.startup.as_mut() {
            match startup.settle() {
                Ok(exit) => {
                    self.startup_exit = Some(exit);
                    drop(self.startup.take());
                }
                Err(_) => self.failed = true,
            }
        }
        if let Some(worker) = self.worker.as_mut() {
            match worker.shutdown_checked() {
                Ok(exit) => {
                    self.worker_exit = Some(exit);
                    drop(self.worker.take());
                }
                Err(_) => self.failed = true,
            }
        }
    }
}
impl Drop for NativeShutdownTransfer {
    fn drop(&mut self) {
        let mut state = self.supervisor.lock().unwrap_or_else(|e| e.into_inner());
        let registration = state
            .registrations
            .get_mut(&self.plugin)
            .expect("closed registration remains owned");
        registration.worker = self.worker.take();
        registration.startup = self.startup.take();
        if let Some(exit) = self.worker_exit.take() {
            registration.exits.push(exit);
        }
        if let Some(exit) = self.startup_exit.take() {
            registration.retired = Some(exit);
        }
        state.cleanup_failed |= self.failed || std::thread::panicking();
    }
}
