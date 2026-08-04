//! Coalescing of rapid configuration changes (spec 21.4).
//!
//! Spec 21.4: "The host shall coalesce rapid changes and send the latest
//! complete configuration state rather than every intermediate edit unless
//! explicitly requested."
//!
//! Someone holding a key down in an editor, or a settings dialog writing on
//! every keystroke, produces a burst of file writes. Delivering each one would
//! wake every plugin repeatedly and hand several of them a half-finished state —
//! a plugin that reconnects to a server on a configuration change would dial a
//! partially typed hostname. So a burst collapses to one publication of the
//! final state.
//!
//! # The two bounds
//!
//! A plain debounce is not enough on its own: edits that keep arriving just
//! inside the window would defer publication forever, and a plugin would never
//! see a change the user made minutes ago. So there are two bounds and the
//! earlier one wins:
//!
//! * `coalesce` — quiet time after the LAST change before publishing.
//! * `maximum_wait` — ceiling measured from the FIRST change of the burst.
//!
//! This is the same shape the query scheduler uses for debounce and maximum
//! wait, deliberately: two mechanisms for "wait for quiet but not forever"
//! should not behave differently.
//!
//! # No clock of its own
//!
//! Every method takes `now`. The caller already has a clock — the launcher's
//! event loop — and a publisher that read one would be untestable without
//! sleeping, which is not a synchronisation primitive.

use std::time::{Duration, Instant};

use crate::snapshot::ConfigurationSnapshot;

/// Collapses a burst of configuration changes into one publication.
#[derive(Debug, Clone)]
pub struct ConfigurationPublisher {
    coalesce: Duration,
    maximum_wait: Duration,
    /// The latest observed state, not yet published.
    pending: Option<ConfigurationSnapshot>,
    /// The earliest instant `pending` may be published.
    due: Option<Instant>,
    /// When the current burst began, for the `maximum_wait` ceiling.
    burst_started: Option<Instant>,
    /// How many observations this burst discarded without publishing.
    coalesced: usize,
    /// The last state actually handed to a caller, so an unchanged reload
    /// publishes nothing at all.
    published: Option<ConfigurationSnapshot>,
}

impl ConfigurationPublisher {
    /// A publisher with the given quiet time and ceiling.
    ///
    /// A zero `coalesce` is legitimate and means "publish on the next poll":
    /// useful where the caller is already the only writer and wants no delay.
    pub fn new(coalesce: Duration, maximum_wait: Duration) -> Self {
        Self {
            coalesce,
            // A ceiling below the quiet time would make the quiet time
            // unreachable and turn every burst into a fixed-interval publish.
            // Clamping here rather than refusing keeps a misconfigured pair
            // behaving sensibly instead of failing startup over a timing hint.
            maximum_wait: maximum_wait.max(coalesce),
            pending: None,
            due: None,
            burst_started: None,
            coalesced: 0,
            published: None,
        }
    }

    /// Records the latest complete state.
    ///
    /// Replaces any state still waiting: that replacement IS the coalescing, and
    /// it is why an intermediate edit can never reach a plugin. Two cases record
    /// nothing at all:
    ///
    /// * the state is identical to what is already waiting — no new information,
    ///   and restarting the quiet timer would let an unchanged file that is
    ///   re-read on a timer defer publication forever;
    /// * the state is identical to what was last published — a file whose
    ///   timestamp moved but whose content did not is not a configuration
    ///   change, and waking every plugin for it would be a lie.
    pub fn observe(&mut self, snapshot: ConfigurationSnapshot, now: Instant) {
        if self.pending.as_ref() == Some(&snapshot) {
            return;
        }
        if self.pending.is_none() && self.published.as_ref() == Some(&snapshot) {
            return;
        }
        if self.pending.is_some() {
            self.coalesced += 1;
        }
        let started = *self.burst_started.get_or_insert(now);
        self.pending = Some(snapshot);
        self.due = Some(
            now.checked_add(self.coalesce)
                .unwrap_or(now)
                .min(started.checked_add(self.maximum_wait).unwrap_or(now)),
        );
    }

    /// The state to publish now, if the quiet time or the ceiling has elapsed.
    ///
    /// `None` while a burst is still settling, which is the whole point: the
    /// caller polls, and the intermediate states it observed in between are gone.
    pub fn poll(&mut self, now: Instant) -> Option<ConfigurationSnapshot> {
        if self.due? > now {
            return None;
        }
        self.take()
    }

    /// The state to publish immediately, bypassing both bounds.
    ///
    /// The "unless explicitly requested" of spec 21.4, and the apply step of an
    /// explicit save (spec 18.8): a user who pressed Save has already told the
    /// host the edit is finished, so making them wait out a quiet time built for
    /// keystrokes would be a delay with nothing to gain.
    pub fn flush(&mut self) -> Option<ConfigurationSnapshot> {
        self.due?;
        self.take()
    }

    /// Hands over the pending state and closes the burst.
    fn take(&mut self) -> Option<ConfigurationSnapshot> {
        let snapshot = self.pending.take()?;
        self.due = None;
        self.burst_started = None;
        self.coalesced = 0;
        self.published = Some(snapshot.clone());
        Some(snapshot)
    }

    /// Whether a state is waiting to be published.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// When [`Self::poll`] will next produce a state, if anything is waiting.
    ///
    /// Lets an event loop wait until then instead of spinning.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.due
    }

    /// How many observations the current burst has discarded.
    ///
    /// Reported by the launcher when it applies a change, so an operator can see
    /// that coalescing happened rather than having to trust that it did.
    pub fn coalesced(&self) -> usize {
        self.coalesced
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crikey_core::PluginId;

    use super::*;

    fn snapshot(theme: &str) -> ConfigurationSnapshot {
        ConfigurationSnapshot::new(BTreeMap::from([(
            PluginId("modern.example".to_owned()),
            BTreeMap::from([("theme".to_owned(), theme.to_owned())]),
        )]))
    }

    fn publisher() -> ConfigurationPublisher {
        ConfigurationPublisher::new(Duration::from_millis(150), Duration::from_millis(1_000))
    }

    #[test]
    fn a_burst_publishes_only_the_final_state_and_never_an_intermediate_one() {
        let mut publisher = publisher();
        let start = Instant::now();
        for (offset, theme) in [(0, "d"), (10, "da"), (20, "dar"), (30, "dark")] {
            publisher.observe(snapshot(theme), start + Duration::from_millis(offset));
            assert_eq!(
                publisher.poll(start + Duration::from_millis(offset)),
                None,
                "an edit {offset} ms into a 150 ms quiet time must not publish"
            );
        }
        assert_eq!(publisher.coalesced(), 3, "three intermediate states were dropped");
        assert_eq!(
            publisher.poll(start + Duration::from_millis(179)),
            None,
            "the quiet time is measured from the LAST edit, not the first"
        );
        let published = publisher
            .poll(start + Duration::from_millis(180))
            .expect("150 ms after the last edit the state is due");
        assert_eq!(published, snapshot("dark"), "only the final state is published");
        assert_eq!(publisher.coalesced(), 0, "the burst is closed");
        assert!(!publisher.has_pending());
    }

    #[test]
    fn nothing_is_published_when_the_burst_leaves_the_state_unchanged() {
        let mut publisher = publisher();
        let start = Instant::now();
        publisher.observe(snapshot("dark"), start);
        publisher
            .poll(start + Duration::from_millis(150))
            .expect("the first state publishes");
        publisher.observe(snapshot("dark"), start + Duration::from_secs(1));
        assert!(
            !publisher.has_pending(),
            "a file whose contents did not change is not a configuration change"
        );
        assert_eq!(publisher.poll(start + Duration::from_secs(2)), None);
    }

    #[test]
    fn an_unbroken_stream_of_edits_still_publishes_at_the_maximum_wait() {
        let mut publisher = publisher();
        let start = Instant::now();
        // An edit every 100 ms never leaves a 150 ms quiet gap, so without the
        // ceiling no plugin would ever be told anything.
        let mut now = start;
        let mut published = None;
        for step in 0..30 {
            publisher.observe(snapshot(&format!("theme-{step}")), now);
            if let Some(snapshot) = publisher.poll(now) {
                published = Some((snapshot, now));
                break;
            }
            now += Duration::from_millis(100);
        }
        let (snapshot, at) = published.expect("the ceiling forces a publication");
        assert!(
            at <= start + Duration::from_millis(1_000),
            "published {:?} after the burst began, past the 1000 ms ceiling",
            at - start
        );
        assert_eq!(
            snapshot.values_for(&PluginId("modern.example".to_owned())),
            Some(&BTreeMap::from([("theme".to_owned(), "theme-10".to_owned())])),
            "the state published at the ceiling is the latest one observed"
        );
    }

    #[test]
    fn an_explicit_save_publishes_without_waiting_out_the_quiet_time() {
        let mut publisher = publisher();
        let start = Instant::now();
        publisher.observe(snapshot("dark"), start);
        assert_eq!(publisher.poll(start), None, "the quiet time has not elapsed");
        assert_eq!(
            publisher.flush(),
            Some(snapshot("dark")),
            "an explicit apply bypasses the coalescing delay"
        );
        assert_eq!(publisher.flush(), None, "there is nothing left to publish");
    }

    #[test]
    fn a_poll_with_nothing_pending_publishes_nothing() {
        let mut publisher = publisher();
        assert_eq!(publisher.poll(Instant::now()), None);
        assert_eq!(publisher.flush(), None);
        assert_eq!(publisher.next_deadline(), None);
    }

    #[test]
    fn a_ceiling_below_the_quiet_time_is_raised_to_it() {
        let mut publisher =
            ConfigurationPublisher::new(Duration::from_millis(150), Duration::from_millis(10));
        let start = Instant::now();
        publisher.observe(snapshot("dark"), start);
        assert_eq!(
            publisher.next_deadline(),
            Some(start + Duration::from_millis(150)),
            "an impossible ceiling must not make the quiet time unreachable"
        );
    }

    #[test]
    fn a_repeated_identical_observation_does_not_defer_the_deadline() {
        let mut publisher = publisher();
        let start = Instant::now();
        publisher.observe(snapshot("dark"), start);
        // A reloader polling on a timer re-reads the same unchanged state. If
        // that reset the quiet time, the state would never become due.
        for step in 1..10 {
            publisher.observe(snapshot("dark"), start + Duration::from_millis(step * 20));
        }
        assert_eq!(
            publisher.next_deadline(),
            Some(start + Duration::from_millis(150))
        );
        assert_eq!(
            publisher.coalesced(),
            0,
            "an identical state is not an intermediate edit"
        );
    }
}
