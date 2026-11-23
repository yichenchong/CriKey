//! A speculative preview yields its existing action slot to one foreground intent.
use crikey_core::{CoreError, Result};
use crikey_plugin_supervisor::{BudgetKind, OwnedBudgetGuard, PluginBudgetHandle};
use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex, Weak,
    },
    time::{Duration, Instant},
};

pub(crate) trait PreviewCancellation: Send + Sync {
    fn cancel_preview(&self);
}
impl PreviewCancellation for AtomicBool {
    fn cancel_preview(&self) {
        self.store(true, Ordering::Release);
    }
}
struct State {
    preview: Option<Weak<dyn PreviewCancellation>>,
    foreground_waiting: bool,
    foreground_active: usize,
}
pub(crate) struct PreviewActionGate {
    budget: PluginBudgetHandle,
    state: Mutex<State>,
    settled: Condvar,
}
impl fmt::Debug for PreviewActionGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreviewActionGate").finish_non_exhaustive()
    }
}
impl PreviewActionGate {
    pub(crate) fn new(budget: PluginBudgetHandle) -> Arc<Self> {
        Arc::new(Self {
            budget,
            state: Mutex::new(State {
                preview: None,
                foreground_waiting: false,
                foreground_active: 0,
            }),
            settled: Condvar::new(),
        })
    }
    pub(crate) fn budget(&self) -> &PluginBudgetHandle {
        &self.budget
    }
    pub(crate) fn has_foreground(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.foreground_waiting || state.foreground_active != 0
    }
    pub(crate) fn try_preview(
        self: &Arc<Self>,
        cancellation: Arc<dyn PreviewCancellation>,
    ) -> Option<PreviewBudgetGuard> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.foreground_waiting || state.foreground_active != 0 || state.preview.is_some() {
            cancellation.cancel_preview();
            return None;
        }
        let guard = self.budget.try_acquire_owned(BudgetKind::Action)?;
        state.preview = Some(Arc::downgrade(&cancellation));
        Some(PreviewBudgetGuard {
            guard: Some(guard),
            gate: Arc::clone(self),
            _cancellation: cancellation,
        })
    }
    /// At most one intent can wait for the currently running preview. A real
    /// foreground holder still refuses according to the unchanged action quota.
    pub(crate) fn reserve_foreground(self: &Arc<Self>) -> Result<ForegroundActionAdmission> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.foreground_waiting {
            return Err(CoreError::Invalid(
                "foreground action handoff is already pending".into(),
            ));
        }
        if let Some(preview) = state.preview.as_ref().and_then(Weak::upgrade) {
            preview.cancel_preview();
            // The live preview guard proves that exactly one occupied slot
            // is speculative. Cancelling it can transfer that slot without
            // increasing the quota or displacing another foreground holder.
            if self.budget.in_flight(BudgetKind::Action) >= self.budget.limit(BudgetKind::Action) {
                state.foreground_waiting = true;
                return Ok(ForegroundActionAdmission::AfterPreview(ForegroundHandoff {
                    gate: Arc::clone(self),
                    armed: true,
                }));
            }
        }
        let guard = self
            .budget
            .try_acquire_owned(BudgetKind::Action)
            .ok_or_else(|| CoreError::Invalid("action budget is full".into()))?;
        state.foreground_active += 1;
        Ok(ForegroundActionAdmission::Ready(ForegroundBudgetGuard {
            guard: Some(guard),
            gate: Arc::clone(self),
        }))
    }
}
pub(crate) struct PreviewBudgetGuard {
    guard: Option<OwnedBudgetGuard>,
    gate: Arc<PreviewActionGate>,
    _cancellation: Arc<dyn PreviewCancellation>,
}
impl fmt::Debug for PreviewBudgetGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreviewBudgetGuard").finish_non_exhaustive()
    }
}
impl Drop for PreviewBudgetGuard {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap_or_else(|error| error.into_inner());
        drop(self.guard.take());
        state.preview = None;
        self.gate.settled.notify_all();
    }
}
#[derive(Debug)]
pub(crate) enum ForegroundActionAdmission {
    Ready(ForegroundBudgetGuard),
    AfterPreview(ForegroundHandoff),
}
impl ForegroundActionAdmission {
    /// Existing provider workers call this; the event loop never waits here.
    pub(crate) fn acquire(self, deadline: Instant, cancelled: &AtomicBool) -> Result<ForegroundBudgetGuard> {
        if cancelled.load(Ordering::Acquire) {
            return Err(CoreError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(CoreError::Invalid(
                "foreground action timed out before admission".into(),
            ));
        }
        match self {
            Self::Ready(guard) => Ok(guard),
            Self::AfterPreview(mut handoff) => {
                let mut state = handoff
                    .gate
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                while state.preview.is_some() {
                    if cancelled.load(Ordering::Acquire) {
                        return Err(CoreError::Cancelled);
                    }
                    let remaining = deadline.checked_duration_since(Instant::now()).ok_or_else(|| {
                        CoreError::Invalid(
                            "foreground action timed out waiting for preview retirement".into(),
                        )
                    })?;
                    state = handoff
                        .gate
                        .settled
                        .wait_timeout(state, remaining.min(Duration::from_millis(10)))
                        .unwrap_or_else(|error| error.into_inner())
                        .0;
                }
                if cancelled.load(Ordering::Acquire) {
                    return Err(CoreError::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Err(CoreError::Invalid(
                        "foreground action timed out waiting for preview retirement".into(),
                    ));
                }
                let guard = handoff
                    .gate
                    .budget
                    .try_acquire_owned(BudgetKind::Action)
                    .ok_or_else(|| CoreError::Invalid("action budget is full".into()))?;
                state.foreground_waiting = false;
                state.foreground_active += 1;
                handoff.armed = false;
                Ok(ForegroundBudgetGuard {
                    guard: Some(guard),
                    gate: Arc::clone(&handoff.gate),
                })
            }
        }
    }
}
#[derive(Debug)]
pub(crate) struct ForegroundHandoff {
    gate: Arc<PreviewActionGate>,
    armed: bool,
}
impl Drop for ForegroundHandoff {
    fn drop(&mut self) {
        if self.armed {
            let mut state = self.gate.state.lock().unwrap_or_else(|error| error.into_inner());
            state.foreground_waiting = false;
            self.gate.settled.notify_all();
        }
    }
}
#[derive(Debug)]
pub(crate) struct ForegroundBudgetGuard {
    guard: Option<OwnedBudgetGuard>,
    gate: Arc<PreviewActionGate>,
}
impl Drop for ForegroundBudgetGuard {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap_or_else(|error| error.into_inner());
        drop(self.guard.take());
        state.foreground_active -= 1;
        self.gate.settled.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crikey_plugin_supervisor::shared_budget_from_section;

    #[test]
    fn preview_yields_one_bounded_foreground_reservation_without_changing_quota() {
        let budget = shared_budget_from_section(&Default::default());
        let gate = PreviewActionGate::new(Arc::clone(&budget));
        let cancelled = Arc::new(AtomicBool::new(false));
        let preview = gate.try_preview(cancelled.clone()).unwrap();
        let foreground = gate.reserve_foreground().unwrap();
        assert!(cancelled.load(Ordering::Acquire));
        assert_eq!(budget.in_flight(BudgetKind::Action), 1);
        assert_eq!(budget.refusals(BudgetKind::Action), 0);
        assert!(gate.reserve_foreground().is_err());
        assert!(gate.try_preview(Arc::new(AtomicBool::new(false))).is_none());
        drop(preview);
        let admitted = foreground
            .acquire(Instant::now() + Duration::from_secs(1), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(budget.in_flight(BudgetKind::Action), 1);
        assert!(
            gate.reserve_foreground().is_err(),
            "a genuine foreground holder still saturates the declared quota"
        );
        drop(admitted);
        assert_eq!(budget.in_flight(BudgetKind::Action), 0);
        assert_eq!(budget.refusals(BudgetKind::Action), 1);
    }

    #[test]
    fn cancelled_handoff_cannot_leak_or_release_the_still_running_preview_slot() {
        let budget = shared_budget_from_section(&Default::default());
        let gate = PreviewActionGate::new(Arc::clone(&budget));
        let preview = gate.try_preview(Arc::new(AtomicBool::new(false))).unwrap();
        let foreground = gate.reserve_foreground().unwrap();
        assert!(matches!(
            foreground.acquire(Instant::now() + Duration::from_secs(1), &AtomicBool::new(true)),
            Err(CoreError::Cancelled)
        ));
        assert_eq!(budget.in_flight(BudgetKind::Action), 1);
        let next = gate.reserve_foreground().unwrap();
        drop(preview);
        drop(
            next.acquire(Instant::now() + Duration::from_secs(1), &AtomicBool::new(false))
                .unwrap(),
        );
        assert_eq!(budget.in_flight(BudgetKind::Action), 0);
    }

    #[test]
    fn dropped_handoff_releases_only_its_bounded_waiting_reservation() {
        let budget = shared_budget_from_section(&Default::default());
        let gate = PreviewActionGate::new(Arc::clone(&budget));
        let preview = gate.try_preview(Arc::new(AtomicBool::new(false))).unwrap();
        drop(gate.reserve_foreground().unwrap());
        assert_eq!(budget.in_flight(BudgetKind::Action), 1);
        let next = gate.reserve_foreground().unwrap();
        drop(preview);
        drop(
            next.acquire(Instant::now() + Duration::from_secs(1), &AtomicBool::new(false))
                .unwrap(),
        );
        assert_eq!(budget.in_flight(BudgetKind::Action), 0);
    }
}
