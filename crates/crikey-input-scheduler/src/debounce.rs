//! Modern plugin debouncing (spec 8.3, 8.5, 8.6, 8.8).

use crikey_core::Generation;

use crate::Millis;

/// Per-plugin debounce policy, normally supplied by the plugin manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebouncePolicy {
    /// Quiet period after the latest query change.
    pub debounce_ms: Millis,
    /// Upper bound on postponement during continuous typing (spec 8.6).
    pub maximum_wait_ms: Option<Millis>,
    /// Dispatch immediately when the plugin becomes newly relevant.
    pub leading_edge: bool,
    /// Dispatch the latest query once typing pauses.
    pub trailing_edge: bool,
    /// Host-imposed minimum normalized query length (spec 8.10).
    pub minimum_query_length: usize,
}

impl Default for DebouncePolicy {
    fn default() -> Self {
        // Spec 8.3 / 25.4: ordinary local modern plugins sit in the 30-75 ms band.
        Self {
            debounce_ms: 50,
            maximum_wait_ms: Some(200),
            leading_edge: true,
            trailing_edge: true,
            minimum_query_length: 0,
        }
    }
}

/// What the scheduler must do with a query change or a timer tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    /// Send the pending query to the plugin now.
    Now(Generation),
    /// Nothing to send yet; wake the scheduler again at this timestamp.
    At(Millis),
    /// Nothing to do; the query is gated out or already satisfied.
    Idle,
}

/// Leading/trailing debouncer with a maximum wait.
///
/// Only the newest undispatched query is ever retained (spec 8.8).
#[derive(Debug, Clone)]
pub struct Debouncer {
    policy: DebouncePolicy,
    pending: Option<Generation>,
    /// When the current burst of typing started.
    burst_started: Option<Millis>,
    /// When the newest query arrived.
    last_change: Millis,
    last_dispatch: Option<Millis>,
}

impl Debouncer {
    pub fn new(policy: DebouncePolicy) -> Self {
        Self {
            policy,
            pending: None,
            burst_started: None,
            last_change: 0,
            last_dispatch: None,
        }
    }

    pub fn policy(&self) -> &DebouncePolicy {
        &self.policy
    }

    pub fn pending(&self) -> Option<Generation> {
        self.pending
    }

    /// Records a query change and reports the resulting dispatch decision.
    pub fn on_query(&mut self, now: Millis, generation: Generation, query_len: usize) -> Dispatch {
        if query_len < self.policy.minimum_query_length {
            // Leaving relevance must re-arm the leading edge for the next admissible
            // query, not merely discard the pending generation.
            self.reset();
            return Dispatch::Idle;
        }

        // Coalesce: the newest query replaces any older undispatched one.
        self.pending = Some(generation);
        self.last_change = now;
        let burst_start = *self.burst_started.get_or_insert(now);

        let newly_relevant = self.last_dispatch.is_none();
        if self.policy.leading_edge && newly_relevant {
            return self.dispatch(now);
        }

        if !self.policy.trailing_edge {
            return Dispatch::Idle;
        }

        let trailing_at = now.saturating_add(self.policy.debounce_ms);
        let deadline = match self.policy.maximum_wait_ms {
            Some(max_wait) => trailing_at.min(burst_start.saturating_add(max_wait)),
            None => trailing_at,
        };
        Dispatch::At(deadline)
    }

    /// Called when a previously requested wake-up time is reached.
    pub fn on_timer(&mut self, now: Millis) -> Dispatch {
        let Some(_) = self.pending else {
            return Dispatch::Idle;
        };
        let trailing_ready = now.saturating_sub(self.last_change) >= self.policy.debounce_ms;
        let max_wait_ready = match (self.policy.maximum_wait_ms, self.burst_started) {
            (Some(max_wait), Some(start)) => now.saturating_sub(start) >= max_wait,
            _ => false,
        };
        if trailing_ready || max_wait_ready {
            self.dispatch(now)
        } else {
            let trailing_at = self.last_change.saturating_add(self.policy.debounce_ms);
            let deadline = match (self.policy.maximum_wait_ms, self.burst_started) {
                (Some(max_wait), Some(start)) => trailing_at.min(start.saturating_add(max_wait)),
                _ => trailing_at,
            };
            Dispatch::At(deadline)
        }
    }

    /// The plugin is no longer relevant to the query; the next relevant query
    /// is treated as a leading edge again.
    pub fn reset(&mut self) {
        self.pending = None;
        self.burst_started = None;
        self.last_dispatch = None;
    }

    fn dispatch(&mut self, now: Millis) -> Dispatch {
        let generation = self.pending.take().expect("dispatch requires a pending query");
        self.burst_started = None;
        self.last_dispatch = Some(now);
        Dispatch::Now(generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen(n: u64) -> Generation {
        let tracker = crikey_core::GenerationTracker::new();
        let mut g = Generation::ZERO;
        for _ in 0..n {
            g = tracker.advance();
        }
        g
    }

    #[test]
    fn leading_edge_dispatches_immediately() {
        let mut d = Debouncer::new(DebouncePolicy::default());
        assert_eq!(d.on_query(0, gen(1), 3), Dispatch::Now(gen(1)));
    }

    #[test]
    fn subsequent_keystrokes_are_coalesced_to_the_newest_query() {
        let mut d = Debouncer::new(DebouncePolicy::default());
        d.on_query(0, gen(1), 1);
        d.on_query(10, gen(2), 2);
        d.on_query(20, gen(3), 3);
        assert_eq!(d.pending(), Some(gen(3)));
        assert_eq!(d.on_timer(70), Dispatch::Now(gen(3)));
    }

    #[test]
    fn trailing_edge_waits_for_the_quiet_period() {
        let mut d = Debouncer::new(DebouncePolicy::default());
        d.on_query(0, gen(1), 1);
        d.on_query(10, gen(2), 2);
        assert_eq!(d.on_timer(30), Dispatch::At(60));
        assert_eq!(d.on_timer(60), Dispatch::Now(gen(2)));
    }

    #[test]
    fn maximum_wait_bounds_continuous_typing() {
        let policy = DebouncePolicy {
            debounce_ms: 50,
            maximum_wait_ms: Some(120),
            ..Default::default()
        };
        let mut d = Debouncer::new(policy);
        d.on_query(0, gen(1), 1); // leading edge consumes generation 1
        let mut now = 10;
        let mut latest = gen(1);
        for step in 2..=8u64 {
            latest = gen(step);
            d.on_query(now, latest, step as usize);
            now += 10;
        }
        // Typing never paused for 50 ms, but the maximum wait forces dispatch.
        assert_eq!(d.on_timer(130), Dispatch::Now(latest));
    }
    #[test]
    fn minimum_query_length_gates_dispatch() {
        let policy = DebouncePolicy {
            minimum_query_length: 2,
            ..Default::default()
        };
        let mut d = Debouncer::new(policy);
        assert_eq!(d.on_query(0, gen(1), 1), Dispatch::Idle);
        assert_eq!(d.on_query(5, gen(2), 2), Dispatch::Now(gen(2)));
    }

    #[test]
    fn gating_resets_relevance_for_the_next_leading_query() {
        let policy = DebouncePolicy {
            minimum_query_length: 2,
            ..Default::default()
        };
        let mut d = Debouncer::new(policy);
        let first = gen(1);
        assert_eq!(d.on_query(0, first, 2), Dispatch::Now(first));
        assert_eq!(d.on_query(10, gen(2), 1), Dispatch::Idle);

        let next = gen(3);
        assert_eq!(
            d.on_query(20, next, 2),
            Dispatch::Now(next),
            "a query becoming relevant again must take the leading edge"
        );
    }

    #[test]
    fn maximum_wait_deadline_is_not_lost_when_timer_wakes_early() {
        let policy = DebouncePolicy {
            debounce_ms: 100,
            maximum_wait_ms: Some(50),
            leading_edge: false,
            trailing_edge: true,
            minimum_query_length: 0,
        };
        let mut d = Debouncer::new(policy);
        let latest = gen(1);
        assert_eq!(d.on_query(0, latest, 1), Dispatch::At(50));
        assert_eq!(
            d.on_timer(25),
            Dispatch::At(50),
            "an early wake-up must preserve the earlier maximum-wait deadline"
        );
        assert_eq!(d.on_timer(50), Dispatch::Now(latest));
    }
}
