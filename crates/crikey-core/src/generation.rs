//! Query generation identifiers (spec 3.4, 8.1).
//!
//! Every user-visible query state receives a monotonically increasing
//! generation. Results carrying an obsolete generation are never displayed.

use std::sync::atomic::{AtomicU64, Ordering};

/// A monotonically increasing identifier for one complete launcher search state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(u64);

impl Generation {
    pub const ZERO: Generation = Generation(0);

    /// Rebuilds a generation from a value that was previously produced by
    /// [`Generation::get`] and carried across a process boundary: a persisted
    /// catalog slice, an IPC frame, a stored query state.
    ///
    /// Decoding only. Live generations are minted exclusively by
    /// [`GenerationTracker::advance`] or [`GenerationTracker::try_advance`],
    /// which is what keeps them monotonic; a value invented here has no
    /// relationship to any tracker's sequence and would make staleness answers
    /// meaningless.
    #[inline]
    pub const fn from_raw(value: u64) -> Generation {
        Generation(value)
    }

    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for Generation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "g{}", self.0)
    }
}

/// Allocates generations and answers staleness questions.
///
/// Cheap to share: the tracker is lock free so the UI thread can bump the
/// generation without ever waiting on plugin traffic.
#[derive(Debug, Default)]
pub struct GenerationTracker {
    current: AtomicU64,
}

impl GenerationTracker {
    pub const fn new() -> Self {
        Self {
            current: AtomicU64::new(0),
        }
    }

    /// Allocates the generation for a new query state.
    ///
    /// There is no safe successor to `u64::MAX`; callers that can report
    /// exhaustion should use [`GenerationTracker::try_advance`] instead.
    /// This convenience method is retained for existing infallible callers;
    /// it panics only when the process has exhausted every possible
    /// generation. New long-lived services should prefer the fallible method.
    pub fn advance(&self) -> Generation {
        self.try_advance()
            .expect("query generation counter exhausted at u64::MAX")
    }

    /// Fallibly allocates the generation for a new query state.
    ///
    /// The counter never wraps. Once it reaches `u64::MAX`, this returns
    /// [`CoreError::CapacityExceeded`] and leaves the current generation at
    /// the maximum value.
    pub fn try_advance(&self) -> crate::Result<Generation> {
        let previous = self
            .current
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| crate::CoreError::CapacityExceeded("query generations"))?;
        Ok(Generation(previous + 1))
    }

    /// Checks that work belongs to the currently visible generation.
    pub fn ensure_current(&self, generation: Generation) -> crate::Result<()> {
        let current = self.current();
        if generation == current {
            Ok(())
        } else {
            Err(crate::CoreError::StaleGeneration {
                got: generation.get(),
                current: current.get(),
            })
        }
    }

    pub fn current(&self) -> Generation {
        Generation(self.current.load(Ordering::Acquire))
    }

    /// True when work tagged `generation` still belongs to the visible query.
    pub fn is_current(&self, generation: Generation) -> bool {
        generation == self.current()
    }

    /// True when a result batch must be rejected without being displayed.
    pub fn is_stale(&self, generation: Generation) -> bool {
        !self.is_current(generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generations_increase_monotonically() {
        let tracker = GenerationTracker::new();
        assert_eq!(tracker.current(), Generation::ZERO);
        let first = tracker.advance();
        let second = tracker.advance();
        assert!(second > first);
        assert_eq!(tracker.current(), second);
    }

    #[test]
    fn older_generations_are_stale() {
        let tracker = GenerationTracker::new();
        let old = tracker.advance();
        let new = tracker.advance();
        assert!(tracker.is_stale(old));
        assert!(tracker.is_current(new));
    }

    #[test]
    fn stale_generation_returns_the_typed_error() {
        let tracker = GenerationTracker::new();
        let old = tracker.advance();
        let current = tracker.advance();

        assert!(matches!(
            tracker.ensure_current(old),
            Err(crate::CoreError::StaleGeneration { got, current: seen })
                if got == old.get() && seen == current.get()
        ));
        assert!(tracker.ensure_current(current).is_ok());
    }

    #[test]
    fn fallible_advance_reports_exhaustion_without_wrapping() {
        let tracker = GenerationTracker::new();
        tracker.current.store(u64::MAX, Ordering::Relaxed);

        assert!(matches!(
            tracker.try_advance(),
            Err(crate::CoreError::CapacityExceeded("query generations"))
        ));
        assert_eq!(tracker.current(), Generation::from_raw(u64::MAX));
    }

    #[test]
    fn generation_exhaustion_does_not_wrap_to_zero() {
        let tracker = GenerationTracker::new();
        tracker.current.store(u64::MAX, Ordering::Relaxed);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tracker.advance()));
        assert!(result.is_err(), "overflow must fail closed");
        assert_eq!(tracker.current(), Generation::from_raw(u64::MAX));
    }
}
