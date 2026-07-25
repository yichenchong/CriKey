//! Legacy obsolete-work replacement (spec 3.6, 8.4, 14.5).
//!
//! `legacy-strict` plugins are never time debounced. Instead the host
//! dispatches promptly, flips `should_terminate()` on obsolete in-flight work,
//! keeps only the newest undispatched request, and serializes callbacks so no
//! two lifecycle callbacks ever run concurrently on one plugin instance.

use crikey_core::Generation;

/// What the host must do with a legacy plugin right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyDispatch {
    /// Deliver this query to the plugin immediately; it is idle.
    Now(Generation),
    /// The plugin is busy. Its running work is now obsolete and the newest
    /// query is queued until the current callback returns.
    QueuedBehindRunning {
        obsolete: Generation,
        queued: Generation,
    },
    /// Nothing to dispatch.
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Running { generation: Generation, obsolete: bool },
}

/// Serial dispatcher for one legacy plugin instance.
#[derive(Debug, Clone)]
pub struct ObsoleteWorkManager {
    state: State,
    /// At most one pending request: older undispatched queries are discarded.
    pending: Option<Generation>,
}

impl Default for ObsoleteWorkManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ObsoleteWorkManager {
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            pending: None,
        }
    }

    /// A relevant query change arrived.
    pub fn on_query(&mut self, generation: Generation) -> LegacyDispatch {
        match self.state {
            State::Idle => {
                self.state = State::Running {
                    generation,
                    obsolete: false,
                };
                self.pending = None;
                LegacyDispatch::Now(generation)
            }
            State::Running {
                generation: running, ..
            } => {
                // Running work becomes obsolete: should_terminate() flips true.
                self.state = State::Running {
                    generation: running,
                    obsolete: true,
                };
                // Only the newest pending request is retained.
                self.pending = Some(generation);
                LegacyDispatch::QueuedBehindRunning {
                    obsolete: running,
                    queued: generation,
                }
            }
        }
    }

    /// The plugin's callback returned. Dispatch the newest pending request, if any.
    pub fn on_callback_finished(&mut self) -> LegacyDispatch {
        self.state = State::Idle;
        match self.pending.take() {
            Some(generation) => {
                self.state = State::Running {
                    generation,
                    obsolete: false,
                };
                LegacyDispatch::Now(generation)
            }
            None => LegacyDispatch::Idle,
        }
    }

    /// Backing value of the legacy `Plugin.should_terminate()` API (spec 9.2).
    pub fn should_terminate(&self) -> bool {
        matches!(self.state, State::Running { obsolete: true, .. })
    }

    /// Cooperative termination for reload, shutdown, disable or supersession.
    pub fn invalidate(&mut self) {
        if let State::Running { generation, .. } = self.state {
            self.state = State::Running {
                generation,
                obsolete: true,
            };
        }
        self.pending = None;
    }

    pub fn running_generation(&self) -> Option<Generation> {
        match self.state {
            State::Running { generation, .. } => Some(generation),
            State::Idle => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crikey_core::GenerationTracker;

    #[test]
    fn idle_plugin_receives_the_query_immediately() {
        let t = GenerationTracker::new();
        let mut m = ObsoleteWorkManager::new();
        let g1 = t.advance();
        assert_eq!(m.on_query(g1), LegacyDispatch::Now(g1));
        assert!(!m.should_terminate());
    }

    #[test]
    fn busy_plugin_gets_should_terminate_and_keeps_only_the_newest_pending() {
        let t = GenerationTracker::new();
        let mut m = ObsoleteWorkManager::new();
        let g1 = t.advance();
        let g2 = t.advance();
        let g3 = t.advance();
        m.on_query(g1);
        m.on_query(g2);
        assert!(m.should_terminate());
        assert_eq!(
            m.on_query(g3),
            LegacyDispatch::QueuedBehindRunning {
                obsolete: g1,
                queued: g3
            }
        );
        // g2 was discarded; the newest pending request runs next.
        assert_eq!(m.on_callback_finished(), LegacyDispatch::Now(g3));
        assert!(!m.should_terminate());
    }

    #[test]
    fn callbacks_never_overlap() {
        let t = GenerationTracker::new();
        let mut m = ObsoleteWorkManager::new();
        let g1 = t.advance();
        let g2 = t.advance();
        m.on_query(g1);
        // A second query does not start a second callback.
        assert!(matches!(
            m.on_query(g2),
            LegacyDispatch::QueuedBehindRunning { .. }
        ));
        assert_eq!(m.running_generation(), Some(g1));
    }

    #[test]
    fn invalidate_requests_cooperative_termination() {
        let t = GenerationTracker::new();
        let mut m = ObsoleteWorkManager::new();
        m.on_query(t.advance());
        m.invalidate();
        assert!(m.should_terminate());
        assert_eq!(m.on_callback_finished(), LegacyDispatch::Idle);
    }
}
