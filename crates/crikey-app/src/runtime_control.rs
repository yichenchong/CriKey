//! Permanent admission fencing and physical runtime shutdown evidence.
use crikey_core::PluginId;
use std::num::NonZeroU64;
use std::sync::{Arc, Condvar, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginAdmission {
    Open,
    Closing { epoch: NonZeroU64 },
    Offline { epoch: NonZeroU64 },
    FailedClosed { epoch: NonZeroU64 },
}
#[derive(Debug, Clone)]
pub struct AdmissionGate(Arc<Mutex<PluginAdmission>>);
impl Default for AdmissionGate {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(PluginAdmission::Open)))
    }
}
impl AdmissionGate {
    /// Hold only across bounded admission/queue insertion, never work or joins.
    pub fn admit(&self) -> Result<std::sync::MutexGuard<'_, PluginAdmission>, StopError> {
        let guard = self.0.lock().map_err(|_| StopError::new("admission-poisoned"))?;
        if *guard != PluginAdmission::Open {
            return Err(StopError::new("admission-closed"));
        }
        Ok(guard)
    }
    pub fn state(&self) -> PluginAdmission {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn close(&self, epoch: NonZeroU64) -> Result<(), StopError> {
        let mut state = self.0.lock().map_err(|_| StopError::new("admission-poisoned"))?;
        match *state {
            PluginAdmission::Open => *state = PluginAdmission::Closing { epoch },
            PluginAdmission::Closing { epoch: old }
            | PluginAdmission::Offline { epoch: old }
            | PluginAdmission::FailedClosed { epoch: old }
                if old == epoch => {}
            _ => return Err(StopError::new("stop-epoch-mismatch")),
        }
        Ok(())
    }
    pub fn finish(&self, epoch: NonZeroU64, success: bool) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match *state {
            PluginAdmission::Closing { epoch: current } | PluginAdmission::Offline { epoch: current }
                if current == epoch =>
            {
                *state = if success {
                    PluginAdmission::Offline { epoch }
                } else {
                    PluginAdmission::FailedClosed { epoch }
                };
            }
            // Failure is sticky, and a different epoch cannot mint offline state.
            PluginAdmission::FailedClosed { .. } => (),
            _ => *state = PluginAdmission::FailedClosed { epoch },
        }
    }
}
/// Checked process-local epoch allocation; exhaustion never reopens admission.
#[derive(Debug, Default)]
pub struct StopEpoch(u64);
impl StopEpoch {
    pub fn advance(&mut self) -> Result<NonZeroU64, StopError> {
        self.0 = self
            .0
            .checked_add(1)
            .ok_or_else(|| StopError::new("stop-epoch-exhausted"))?;
        NonZeroU64::new(self.0).ok_or_else(|| StopError::new("stop-epoch-exhausted"))
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopError {
    pub code: &'static str,
}
impl StopError {
    pub fn new(code: &'static str) -> Self {
        Self { code }
    }
}
impl std::fmt::Display for StopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code)
    }
}
impl std::error::Error for StopError {}
/// Immutable ledger minted by a runtime only after all physical cleanup succeeds.
#[derive(Debug, Clone)]
pub struct ProcessExit {
    pub owner: PluginId,
    pub kind: crikey_native_host::ExitKind,
    pub code: Option<i32>,
    pub signal: Option<i32>,
}
#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeRetirement {
    pub catalog_threads: usize,
    pub query_threads: usize,
    pub preview_threads: usize,
    pub page_threads: usize,
    pub resource_calls: usize,
    pub resource_threads: usize,
}
#[derive(Debug, Clone)]
pub struct StoppedRuntime {
    runtime: &'static str,
    epoch: NonZeroU64,
    owners: Arc<[PluginId]>,
    native_exits: Arc<[ProcessExit]>,
    retirement: RuntimeRetirement,
}
impl StoppedRuntime {
    pub(crate) fn validated(
        runtime: &'static str,
        epoch: NonZeroU64,
        owners: impl Into<Arc<[PluginId]>>,
    ) -> Self {
        Self {
            runtime,
            epoch,
            owners: owners.into(),
            native_exits: Arc::from([]),
            retirement: RuntimeRetirement::default(),
        }
    }
    pub(crate) fn with_native_exits(
        mut self,
        exits: Vec<(PluginId, crikey_native_host::ExitRecord)>,
    ) -> Self {
        self.native_exits = exits
            .into_iter()
            .map(|(owner, exit)| ProcessExit {
                owner,
                kind: exit.kind,
                code: exit.code,
                signal: exit.signal,
            })
            .collect();
        self
    }
    pub fn native_exits(&self) -> &[ProcessExit] {
        &self.native_exits
    }
    pub(crate) fn with_retirement(mut self, retirement: RuntimeRetirement) -> Self {
        self.retirement = retirement;
        self
    }
    pub fn retirement(&self) -> RuntimeRetirement {
        self.retirement
    }
    pub fn runtime(&self) -> &'static str {
        self.runtime
    }
    pub fn epoch(&self) -> NonZeroU64 {
        self.epoch
    }
    pub fn owners(&self) -> &[PluginId] {
        &self.owners
    }
}
type Terminal = Result<StoppedRuntime, StopError>;
type Wait = Box<dyn FnOnce(NonZeroU64) -> Terminal + Send>;
struct State {
    epoch: Option<NonZeroU64>,
    waiting: bool,
    wait: Option<Wait>,
    terminal: Option<Terminal>,
    signal_failed: bool,
}
struct Shared {
    runtime: &'static str,
    owners: Arc<[PluginId]>,
    gate: AdmissionGate,
    signal: Box<dyn Fn() + Send + Sync>,
    state: Mutex<State>,
    changed: Condvar,
}
#[derive(Clone)]
pub struct DriverControl(Arc<Shared>);
impl std::fmt::Debug for DriverControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverControl").finish_non_exhaustive()
    }
}
impl DriverControl {
    pub(crate) fn new(
        runtime: &'static str,
        owners: Arc<[PluginId]>,
        gate: AdmissionGate,
        signal: impl Fn() + Send + Sync + 'static,
        wait: impl FnOnce(NonZeroU64) -> Terminal + Send + 'static,
    ) -> Self {
        Self(Arc::new(Shared {
            runtime,
            owners,
            gate,
            signal: Box::new(signal),
            state: Mutex::new(State {
                epoch: None,
                waiting: false,
                wait: Some(Box::new(wait)),
                terminal: None,
                signal_failed: false,
            }),
            changed: Condvar::new(),
        }))
    }
    pub fn admission(&self) -> AdmissionGate {
        self.0.gate.clone()
    }
    pub fn runtime(&self) -> &'static str {
        self.0.runtime
    }
    pub fn startup_owners(&self) -> &[PluginId] {
        &self.0.owners
    }
    pub fn request_stop(&self, epoch: NonZeroU64) -> Result<StopTicket, StopError> {
        self.0.gate.close(epoch)?;
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| StopError::new("stop-state-poisoned"))?;
        if state.epoch.is_some_and(|old| old != epoch) {
            return Err(StopError::new("stop-epoch-mismatch"));
        }
        if state.epoch.is_none() {
            state.epoch = Some(epoch);
            state.signal_failed =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.0.signal)())).is_err();
        }
        Ok(StopTicket(self.clone(), epoch))
    }
}
pub struct StopTicket(DriverControl, NonZeroU64);
impl std::fmt::Debug for StopTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StopTicket")
            .field("epoch", &self.1)
            .finish_non_exhaustive()
    }
}
impl StopTicket {
    /// Blocking physical settlement: call only on the package/shutdown worker.
    pub fn wait(self) -> Terminal {
        let shared = &self.0 .0;
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(result) = &state.terminal {
                if result.is_err() {
                    return result.clone();
                }
                return match shared.gate.state() {
                    PluginAdmission::Offline { epoch } if epoch == self.1 => result.clone(),
                    _ => Err(StopError::new("runtime-failed-closed")),
                };
            }
            if !state.waiting {
                state.waiting = true;
                let wait = state.wait.take().expect("single shutdown owner");
                drop(state);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wait(self.1)))
                    .unwrap_or_else(|_| Err(StopError::new("stop-worker-panicked")));
                let result = result.and_then(|proof| {
                    if proof.epoch != self.1 {
                        return Err(StopError::new("stop-epoch-mismatch"));
                    }
                    if proof.runtime != shared.runtime {
                        return Err(StopError::new("stop-runtime-mismatch"));
                    }
                    if proof.owners.len() != shared.owners.len()
                        || !shared.owners.iter().all(|owner| proof.owners.contains(owner))
                        || !proof.owners.iter().all(|owner| shared.owners.contains(owner))
                    {
                        return Err(StopError::new("stop-owner-census-mismatch"));
                    }
                    Ok(proof)
                });
                shared.gate.finish(self.1, result.is_ok());
                let result = if result.is_ok()
                    && shared.gate.state() != (PluginAdmission::Offline { epoch: self.1 })
                {
                    Err(StopError::new("runtime-failed-closed"))
                } else {
                    result
                };
                state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                let result = if state.signal_failed {
                    Err(StopError::new("stop-signal-panicked"))
                } else {
                    result
                };
                if result.is_err() {
                    shared.gate.finish(self.1, false);
                }
                state.terminal = Some(result.clone());
                shared.changed.notify_all();
                return result;
            }
            state = shared.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Counts admitted escaped resource calls, including callers waiting for a worker.
#[derive(Debug, Clone, Default)]
pub(crate) struct PendingWork(Arc<(Mutex<PendingWorkState>, Condvar)>);
#[derive(Debug, Default)]
struct PendingWorkState {
    active: usize,
    completed: usize,
    failed: bool,
}
pub(crate) struct PendingWorkGuard(PendingWork);
impl PendingWork {
    pub(crate) fn begin(&self) -> PendingWorkGuard {
        self.0 .0.lock().unwrap_or_else(|e| e.into_inner()).active += 1;
        PendingWorkGuard(self.clone())
    }
    pub(crate) fn settle(&self) -> Result<usize, StopError> {
        let mut state = self.0 .0.lock().unwrap_or_else(|e| e.into_inner());
        while state.active != 0 {
            state = self.0 .1.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        if state.failed {
            Err(StopError::new("escaped-work-panicked"))
        } else {
            Ok(state.completed)
        }
    }
}
impl Drop for PendingWorkGuard {
    fn drop(&mut self) {
        let mut count = self.0 .0 .0.lock().unwrap_or_else(|e| e.into_inner());
        count.active -= 1;
        count.completed = count.completed.saturating_add(1);
        count.failed |= std::thread::panicking();
        self.0 .0 .1.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn queue_insertion_and_close_share_one_fence() {
        let gate = AdmissionGate::default();
        let queue = Arc::new(Mutex::new(Vec::new()));
        let admitted = gate.admit().unwrap();
        let closer_gate = gate.clone();
        let closer = std::thread::spawn(move || closer_gate.close(NonZeroU64::MIN));
        queue.lock().unwrap().push(1);
        drop(admitted);
        closer.join().unwrap().unwrap();
        assert!(gate.admit().is_err());
        assert_eq!(*queue.lock().unwrap(), [1]);
    }

    #[test]
    fn concurrent_tickets_physically_settle_once_and_reuse_same_epoch() {
        let calls = Arc::new(AtomicUsize::new(0));
        let settled = Arc::clone(&calls);
        let control = DriverControl::new(
            "fixture",
            Arc::from([PluginId("native.fixture".into())]),
            AdmissionGate::default(),
            || (),
            move |epoch| {
                let worker = std::thread::spawn(move || {
                    settled.fetch_add(1, Ordering::AcqRel);
                });
                worker.join().unwrap();
                Ok(StoppedRuntime::validated(
                    "fixture",
                    epoch,
                    vec![PluginId("native.fixture".into())],
                ))
            },
        );
        let first = control.request_stop(NonZeroU64::MIN).unwrap();
        let second = control.request_stop(NonZeroU64::MIN).unwrap();
        let waiter = std::thread::spawn(move || first.wait().unwrap());
        let proof = second.wait().unwrap();
        assert_eq!(waiter.join().unwrap().epoch(), proof.epoch());
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert!(control.request_stop(NonZeroU64::new(2).unwrap()).is_err());
        assert_eq!(
            control
                .request_stop(proof.epoch())
                .unwrap()
                .wait()
                .unwrap()
                .owners(),
            proof.owners()
        );
    }

    #[test]
    fn panic_is_retained_failed_closed_and_epochs_never_wrap() {
        let control = DriverControl::new(
            "fixture",
            Arc::from([]),
            AdmissionGate::default(),
            || (),
            |_| panic!("fixture panic"),
        );
        assert!(control.request_stop(NonZeroU64::MIN).unwrap().wait().is_err());
        assert!(matches!(
            control.admission().state(),
            PluginAdmission::FailedClosed { .. }
        ));
        assert!(control.request_stop(NonZeroU64::MIN).unwrap().wait().is_err());
        let mut epoch = StopEpoch(u64::MAX);
        assert!(epoch.advance().is_err());
        assert!(epoch.advance().is_err());
    }
}

#[cfg(test)]
mod census_tests {
    use super::*;

    #[test]
    fn an_incomplete_terminal_ledger_cannot_replace_the_startup_census() {
        let owners: Arc<[PluginId]> = Arc::from([PluginId("native.one".into()), PluginId("cabi.two".into())]);
        let control = DriverControl::new(
            "native",
            Arc::clone(&owners),
            AdmissionGate::default(),
            || (),
            |epoch| {
                Ok(StoppedRuntime::validated(
                    "native",
                    epoch,
                    vec![PluginId("native.one".into())],
                ))
            },
        );
        assert_eq!(control.runtime(), "native");
        assert_eq!(control.startup_owners(), owners.as_ref());
        let error = control.request_stop(NonZeroU64::MIN).unwrap().wait().unwrap_err();
        assert_eq!(error.code, "stop-owner-census-mismatch");
        assert!(matches!(
            control.admission().state(),
            PluginAdmission::FailedClosed { .. }
        ));
    }
}
