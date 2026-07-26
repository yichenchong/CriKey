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
    /// [`GenerationTracker::advance`], which is what keeps them monotonic; a
    /// value invented here has no relationship to any tracker's sequence and
    /// would make staleness answers meaningless.
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
    pub fn advance(&self) -> Generation {
        Generation(self.current.fetch_add(1, Ordering::AcqRel) + 1)
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
}
