//! Enforcement of declared per-plugin concurrency budgets (spec 13.5).
//!
//! The manifest layer records what the author wrote; this layer resolves that
//! declaration into effective limits and hands out slots. The two stay
//! separate on purpose: a future change to a host default must never be
//! mistaken for an author's intent.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use crikey_plugin_model::ConcurrencySection;

/// The four independent kinds of simultaneous work a plugin may declare
/// (spec 13.5). Kept separate so suggestion pressure can never stall an
/// unrelated catalog build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BudgetKind {
    Suggestion,
    Action,
    Background,
    Catalog,
}

impl BudgetKind {
    /// Index into the fixed per-kind slot arrays. A dense index keeps the
    /// counters in one cache-friendly array instead of a map lookup on every
    /// admission decision.
    fn index(self) -> usize {
        match self {
            BudgetKind::Suggestion => 0,
            BudgetKind::Action => 1,
            BudgetKind::Background => 2,
            BudgetKind::Catalog => 3,
        }
    }
}

const KIND_COUNT: usize = 4;

// An undeclared budget is NOT unlimited. The scheduler already normalises an
// absent `max-concurrent-requests` to one because §8.12 fairness depends on
// every plugin being bounded.
// Treating silence as unbounded here would silently uncap every manifest that
// never mentions concurrency, so the conservative host default is one
// concurrent unit per kind. An author who wants more says so; an author who
// wants none writes `0`.

/// Effective suggestion-request limit when the manifest declares none.
pub const DEFAULT_SUGGESTION_BUDGET: u32 = 1;
/// Effective action-request limit when the manifest declares none.
pub const DEFAULT_ACTION_BUDGET: u32 = 1;
/// Effective background-task limit when the manifest declares none.
pub const DEFAULT_BACKGROUND_BUDGET: u32 = 1;
/// Effective catalog-task limit when the manifest declares none.
pub const DEFAULT_CATALOG_BUDGET: u32 = 1;

/// Shareable admission handle for one loaded plugin runtime.
///
/// A handle is created once while a manifest is registered and then cloned by
/// every dispatch owner (query, action, background and catalog). Keeping the
/// alias public makes that ownership edge explicit without exposing another
/// budget implementation that could drift from [`ConcurrencyBudget`].
pub type PluginBudgetHandle = Arc<ConcurrencyBudget>;

/// Constructs the one shared budget handle resolved from a manifest section.
///
/// This is the preferred constructor for a registration owner. Tests that
/// exercise the value type directly may continue to use
/// [`ConcurrencyBudget::from_section`].
pub fn shared_budget_from_section(section: &ConcurrencySection) -> PluginBudgetHandle {
    Arc::new(ConcurrencyBudget::from_section(section))
}

/// Shared admission gate for one plugin's worker pool.
///
/// Every method takes `&self` so the dispatch threads of a single plugin can
/// share one gate through an `Arc`. Admission is a compare-and-swap rather
/// than a check-then-increment: under contention the latter admits more
/// contenders than the declared limit, which is exactly the bound this type
/// exists to hold.
#[derive(Debug)]
pub struct ConcurrencyBudget {
    limits: [u32; KIND_COUNT],
    in_flight: [AtomicU32; KIND_COUNT],
    refusals: [AtomicU64; KIND_COUNT],
}

impl ConcurrencyBudget {
    /// Resolves a declaration into effective limits, substituting the host
    /// default for each budget the author left undeclared.
    pub fn from_section(section: &ConcurrencySection) -> Self {
        Self {
            limits: [
                section
                    .max_suggestion_requests
                    .unwrap_or(DEFAULT_SUGGESTION_BUDGET),
                section.max_action_requests.unwrap_or(DEFAULT_ACTION_BUDGET),
                section.max_background_tasks.unwrap_or(DEFAULT_BACKGROUND_BUDGET),
                section.max_catalog_tasks.unwrap_or(DEFAULT_CATALOG_BUDGET),
            ],
            in_flight: Default::default(),
            refusals: Default::default(),
        }
    }

    /// The effective limit enforced for `kind`, after default resolution.
    pub fn limit(&self, kind: BudgetKind) -> u32 {
        self.limits[kind.index()]
    }

    /// Claims one slot of `kind`, or records a refusal and returns `None`.
    pub fn try_acquire(&self, kind: BudgetKind) -> Option<BudgetGuard<'_>> {
        self.admit(kind).then(|| BudgetGuard {
            slot: &self.in_flight[kind.index()],
        })
    }

    /// [`Self::try_acquire`] for a budget shared through an `Arc`.
    ///
    /// A borrowing guard cannot be stored beside the budget that issued it, so
    /// a host that keeps admitted work in a map (the query pipeline keys its
    /// guards by plugin and generation) needs an owning handle. The admission
    /// decision is the same compare-and-swap; only the release edge differs.
    pub fn try_acquire_owned(self: &Arc<Self>, kind: BudgetKind) -> Option<OwnedBudgetGuard> {
        self.admit(kind).then(|| OwnedBudgetGuard {
            budget: Arc::clone(self),
            kind,
        })
    }

    /// The single admission decision: claims a slot of `kind` and reports
    /// whether it was granted, counting a refusal when it was not.
    fn admit(&self, kind: BudgetKind) -> bool {
        let index = kind.index();
        let limit = self.limits[index];
        let in_flight = &self.in_flight[index];

        let mut current = in_flight.load(Ordering::Acquire);
        loop {
            if current >= limit {
                // A refusal is the operator's only evidence that a plugin is
                // being throttled rather than broken, so it is always counted.
                // Keep this diagnostic monotonic at the integer boundary;
                // wrapping to zero would hide a plugin that has been refused
                // for its entire lifetime.
                let _ = self.refusals[index].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                    Some(count.saturating_add(1))
                });
                return false;
            }
            match in_flight.compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    /// Live occupancy of `kind`.
    pub fn in_flight(&self, kind: BudgetKind) -> u32 {
        self.in_flight[kind.index()].load(Ordering::Acquire)
    }

    /// Cumulative refusals recorded for `kind`. Never reset by a release: the
    /// history is the diagnostic.
    pub fn refusals(&self, kind: BudgetKind) -> u64 {
        self.refusals[kind.index()].load(Ordering::Relaxed)
    }
}

/// Ownership of one admitted unit of work.
///
/// Release is by `Drop`, which also runs while unwinding, so a panicking unit
/// of work cannot permanently shrink the plugin's capacity.
#[derive(Debug)]
pub struct BudgetGuard<'a> {
    slot: &'a AtomicU32,
}

impl Drop for BudgetGuard<'_> {
    fn drop(&mut self) {
        self.slot.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Ownership of one admitted unit of work against a shared budget.
///
/// Behaves exactly like [`BudgetGuard`] but keeps the budget alive itself, so
/// it can be parked in a host-side registry until the work retires.
#[derive(Debug)]
pub struct OwnedBudgetGuard {
    budget: Arc<ConcurrencyBudget>,
    kind: BudgetKind,
}

impl OwnedBudgetGuard {
    /// The kind of work this slot was admitted for.
    pub fn kind(&self) -> BudgetKind {
        self.kind
    }
}

impl Drop for OwnedBudgetGuard {
    fn drop(&mut self) {
        self.budget.in_flight[self.kind.index()].fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_counter_saturates_at_u64_max() {
        let budget = ConcurrencyBudget::from_section(&ConcurrencySection {
            max_suggestion_requests: Some(0),
            ..ConcurrencySection::default()
        });
        budget.refusals[BudgetKind::Suggestion.index()].store(u64::MAX, Ordering::Relaxed);

        assert!(budget.try_acquire(BudgetKind::Suggestion).is_none());
        assert_eq!(budget.refusals(BudgetKind::Suggestion), u64::MAX);
    }
}
