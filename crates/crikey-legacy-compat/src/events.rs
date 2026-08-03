//! Legacy event semantics and activation/deactivation coalescing (spec 14.6,
//! 14.7, 13.3-13.4, 18.7, 26.2).
//!
//! The shape of this module follows from one asymmetry stated in spec 18.7:
//! "For legacy plugins, low-level event noise may be coalesced before
//! translation, but semantic legacy events shall not receive arbitrary
//! additional debounce delays." So [`EventCoalescer`] has exactly **one**
//! time-driven stage, in front of translation - the raw filesystem window with
//! its maximum-wait flush - and everything downstream of translation is
//! immediate. Reusing the modern `Debouncer` on semantic events would violate
//! spec 14.6 and spec 8.4 (`legacy-strict` is never time debounced).
//!
//! Three invariants shape the API:
//!
//! * **Time is explicit.** Every timestamp is a [`Millis`] argument. Nothing
//!   here reads a wall clock, so the component is deterministic and testable at
//!   millisecond resolution.
//! * **Recording is not delivery.** [`EventCoalescer::post_event`],
//!   [`EventCoalescer::broadcast_event`],
//!   [`EventCoalescer::note_raw_filesystem`],
//!   [`EventCoalescer::note_activated`] and
//!   [`EventCoalescer::note_deactivated`] only mutate pending state.
//!   [`EventCoalescer::tick`] performs everything due at or before `now`.
//! * **A delivery is an in-flight callback.** `tick` returns at most one
//!   [`EventDelivery`] per plugin instance, because callbacks are serialized per
//!   instance (spec 13.4). The host reports the end with
//!   [`EventCoalescer::end_callback`]; only then can that instance receive the
//!   next notice.
//!
//! [`EventCoalescer::next_wakeup`] reports *time-driven* deadlines only. Work
//! held back purely by callback serialization is not a deadline: it becomes
//! deliverable the instant `end_callback` runs, and the host ticks after every
//! callback end. Reporting it would fabricate the debounce window spec 14.6
//! forbids.
//!
//! All retained state is bounded. Pending flags are a fixed-width bitset per
//! instance, the raw-notification buffer is capped at
//! [`MAX_RETAINED_RAW_NOTIFICATIONS`] with the overflow counted (spec 18.7,
//! "overflow detection"), and the instance registry mirrors the host's loaded
//! plugin set through `register_plugin` / `unregister_plugin`.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::{BitAnd, BitOr, BitOrAssign};
use std::path::PathBuf;

use crikey_core::PluginId;
use crikey_input_scheduler::Millis;

use crate::LegacyCallback;

/// Upper bound on the raw notifications retained for one open burst.
///
/// The burst's translated flag union is folded in *before* this cap is
/// consulted, so an overflow never changes what a plugin observes; it only stops
/// the host-side inspection buffer from growing without limit under a
/// pathological watcher storm. Overflow is counted in
/// [`CoalescerDiagnostics::raw_notifications_dropped`] (spec 18.7).
pub const MAX_RETAINED_RAW_NOTIFICATIONS: usize = 256;

// ---------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------

/// The documented `keypirinha.Events` flag set (spec 14.2, 14.4).
///
/// The bit values are an ABI: `python/keypirinha.py` exposes exactly these
/// integers as an `enum.IntFlag`, and unchanged plugin source compares against
/// them. They must never be renumbered.
///
/// `PACKAGES` and `FILESYSTEM` are CriKey extensions - the installed package set
/// (spec 14.3) and a watched path (spec 18.7) have no documented legacy flag of
/// their own - and take bits above the documented ones so those keep their
/// values.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct LegacyEventFlags(u32);

impl LegacyEventFlags {
    /// The application configuration changed.
    pub const APP_CONFIG: Self = Self(0x01);
    /// A package's configuration changed.
    pub const PACKAGE_CONFIG: Self = Self(0x02);
    /// The network options changed.
    pub const NETWORK_OPTIONS: Self = Self(0x04);
    /// The installed package set changed (CriKey extension, spec 14.3).
    pub const PACKAGES: Self = Self(0x08);
    /// A watched path changed (CriKey extension, spec 18.7).
    pub const FILESYSTEM: Self = Self(0x10);
    /// The desktop contents changed.
    pub const DESKTOP: Self = Self(0x20);
    /// The start menu contents changed.
    pub const START_MENU: Self = Self(0x40);

    /// The union of every defined flag. Any bit outside `ALL` is undefined and
    /// is rejected by [`LegacyEventFlags::from_bits`].
    pub const ALL: Self = Self(0x7f);

    /// Every defined flag with its shim-visible name, in ascending bit order.
    /// The `Debug` rendering derives from this list, so there is exactly one
    /// place to extend when a flag is added.
    pub const NAMED: [(Self, &'static str); 7] = [
        (Self::APP_CONFIG, "APP_CONFIG"),
        (Self::PACKAGE_CONFIG, "PACKAGE_CONFIG"),
        (Self::NETWORK_OPTIONS, "NETWORK_OPTIONS"),
        (Self::PACKAGES, "PACKAGES"),
        (Self::FILESYSTEM, "FILESYSTEM"),
        (Self::DESKTOP, "DESKTOP"),
        (Self::START_MENU, "START_MENU"),
    ];

    /// The identity of the union (spec 14.6).
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Whether no flag is set.
    ///
    /// An empty set is never delivered: `on_events` describes *what changed*, so
    /// a callback carrying nothing would only force the plugin to re-read
    /// everything (spec 14.6).
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The wire form shared with the Python shim (spec 14.2).
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Parses a wire value, rejecting any undefined bit rather than silently
    /// keeping it: a flag CriKey does not understand must not reach a plugin
    /// dressed up as one that it does (spec 14.2).
    #[must_use]
    pub const fn from_bits(bits: u32) -> Option<Self> {
        if bits & !Self::ALL.0 == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// Whether every flag in `other` is set here.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether any flag in `other` is set here.
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Adds every flag in `other`. Idempotent, which is what makes accumulating
    /// into a pending delivery safe (spec 14.6).
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    /// Clears every flag in `other`, leaving the rest untouched.
    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    /// Translates a watched filesystem scope into legacy flags (spec 18.7).
    ///
    /// `FILESYSTEM` is always present so a plugin that only cares that "some
    /// watched path changed" needs one flag test, while a plugin written against
    /// the documented legacy flags still sees its specific one.
    #[must_use]
    pub const fn for_watch_scope(scope: WatchScope) -> Self {
        let specific = match scope {
            WatchScope::ApplicationConfig => Self::APP_CONFIG.0,
            WatchScope::PackageConfig => Self::PACKAGE_CONFIG.0,
            WatchScope::PackageFiles => Self::PACKAGES.0,
            WatchScope::Desktop => Self::DESKTOP.0,
            WatchScope::StartMenu => Self::START_MENU.0,
            // A plain watched path has no documented legacy flag of its own.
            WatchScope::PluginData => 0,
        };
        Self(specific | Self::FILESYSTEM.0)
    }
}

impl BitOr for LegacyEventFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for LegacyEventFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for LegacyEventFlags {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl fmt::Debug for LegacyEventFlags {
    // Renders flag names rather than an opaque integer: these values surface in
    // assertion failures and in compatibility diagnostics, where
    // `LegacyEventFlags(18)` would cost the reader a lookup.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LegacyEventFlags(")?;
        if self.is_empty() {
            f.write_str("empty")?;
        } else {
            let mut separator = "";
            for (flag, name) in Self::NAMED {
                if self.contains(flag) {
                    f.write_str(separator)?;
                    f.write_str(name)?;
                    separator = " | ";
                }
            }
        }
        f.write_str(")")
    }
}

// ---------------------------------------------------------------------------
// Raw filesystem input
// ---------------------------------------------------------------------------

/// What a watcher is watching, and therefore which legacy flags its
/// notifications translate to (spec 18.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WatchScope {
    /// The application configuration tree.
    ApplicationConfig,
    /// A package's configuration file.
    PackageConfig,
    /// The installed package files themselves (installs and removals).
    PackageFiles,
    /// The user's desktop.
    Desktop,
    /// The start menu / application entries.
    StartMenu,
    /// A path a plugin asked to watch, with no documented legacy meaning.
    PluginData,
}

/// The low-level change kind reported by the platform watcher.
///
/// This is deliberately *not* forwarded to plugins: the documented legacy API
/// exposes flags, not per-path change kinds. It exists so identical raw
/// notifications can be recognised and collapsed before translation
/// ("event-type coalescing", spec 18.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RawFilesystemKind {
    /// The path appeared.
    Created,
    /// The path's contents or metadata changed.
    Modified,
    /// The path disappeared.
    Removed,
    /// The path was renamed (either endpoint).
    Renamed,
}

/// One raw notification as delivered by a platform watcher, before translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFilesystemNotification {
    /// The watcher that produced it, which decides the translated flags.
    pub scope: WatchScope,
    /// The path that changed.
    pub path: PathBuf,
    /// The low-level change kind.
    pub kind: RawFilesystemKind,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Timing configuration for the one time-driven stage (spec 18.7).
///
/// None of these windows may ever be applied to a semantic legacy event; they
/// exist strictly in front of translation (spec 14.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoalescerConfig {
    /// Sliding quiet period: a burst closes this long after its last raw
    /// notification.
    pub raw_filesystem_window_ms: Millis,
    /// Hard bound measured from the burst's first raw notification, so a
    /// sustained stream cannot postpone translation forever (spec 18.7,
    /// "maximum-wait flushes").
    pub raw_filesystem_maximum_wait_ms: Millis,
    /// How long a deactivation is held so a later activation can supersede it
    /// (spec 14.7).
    pub activation_window_ms: Millis,
}

impl Default for CoalescerConfig {
    fn default() -> Self {
        // Short enough to stay below the perception of "the launcher noticed",
        // long enough to absorb the editor-writes-a-temp-file pattern that emits
        // three to six raw notifications for a single logical save.
        Self {
            raw_filesystem_window_ms: 20,
            raw_filesystem_maximum_wait_ms: 200,
            activation_window_ms: 50,
        }
    }
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

/// The activation state a plugin instance has been told about.
///
/// This is the *delivered* state, not the recorded intent: while a deactivation
/// waits out its coalescing window the instance still reports the last state the
/// plugin actually observed, which is exactly the state that survives if the
/// deactivation is superseded (spec 14.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum ActivationState {
    /// The instance has been activated and not since deactivated.
    Activated,
    /// The instance has never been activated, or was last deactivated.
    #[default]
    Deactivated,
}

/// How an in-flight callback finished (spec 13.4, 26.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CallbackOutcome {
    /// The plugin returned normally.
    Completed,
    /// The plugin raised. Diagnosed, never fatal, never sticky (spec 31.9).
    Raised,
}

/// One callback the host must now run against one plugin instance.
///
/// A returned delivery *is* an in-flight callback: the host runs it and reports
/// completion with [`EventCoalescer::end_callback`] (spec 13.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventDelivery {
    /// The instance to call.
    pub plugin: PluginId,
    /// Which documented callback to invoke.
    pub callback: LegacyCallback,
    /// The flag union for `on_events`; empty for the activation callbacks, which
    /// take no flags in the documented API.
    pub flags: LegacyEventFlags,
    /// The tick that produced the delivery. This is the moment the instance
    /// became free to receive it, not the moment the notice was recorded.
    pub at: Millis,
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Counters exposed by [`EventCoalescer::diagnostics`] (spec 26.2).
///
/// Raw-noise reduction has no other observable form - the whole point is that
/// the plugin cannot tell - so acceptance 31.28 is asserted through these
/// counters plus the unchanged semantic behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CoalescerDiagnostics {
    /// `on_events` callbacks delivered.
    pub events_delivered: u64,
    /// Notices that merged into an already-pending delivery instead of becoming
    /// a second callback (spec 14.6).
    pub flag_unions_merged: u64,
    /// Notices discarded because their flag set was empty (spec 14.6). A
    /// broadcast counts once, not once per registered plugin.
    pub empty_events_discarded: u64,
    /// Notices that arrived while the target instance had a callback in flight
    /// and were therefore held (spec 13.4). This is serialization, not debounce.
    pub deferred_by_serialization: u64,
    /// Callbacks that ended with [`CallbackOutcome::Raised`].
    pub failed_deliveries: u64,
    /// Raw filesystem notifications accepted from the watchers.
    pub raw_notifications_seen: u64,
    /// Raw notifications not retained in the inspection buffer because the burst
    /// hit [`MAX_RETAINED_RAW_NOTIFICATIONS`]. Their flags were folded into the
    /// burst first, so this never changes semantics.
    pub raw_notifications_dropped: u64,
    /// Raw notifications that never became a logical event of their own, derived
    /// as `raw_notifications_seen - logical_events_translated`. This is the
    /// reduction acceptance 31.28 requires.
    pub raw_notifications_collapsed: u64,
    /// Bursts that crossed the translation boundary as one logical event.
    pub logical_events_translated: u64,
    /// `on_activated` / `on_deactivated` callbacks delivered.
    pub activation_callbacks_delivered: u64,
    /// Pending deactivations dropped because a later activation superseded them
    /// (spec 14.7). Diagnosable rather than invisible.
    pub superseded_deactivations: u64,
    /// Pending activations dropped because a later deactivation superseded them.
    /// Spec 14.7 names only the first direction, but the net-state rule is
    /// symmetric and this half must not be silent either.
    pub superseded_activations: u64,
    /// Undelivered notices absorbed by a repeat of the same kind.
    pub coalesced_repeat_notices: u64,
    /// Callbacks the layer invented rather than observed. Spec 14.7 forbids
    /// synthesizing the missing half of an alternation, so this stays zero by
    /// construction; it is published as the proof, not as a variable.
    pub synthesized_callbacks: u64,
    /// Longest observed callback duration, in milliseconds. Feeds the host's
    /// spec 13.4 watchdog reporting.
    pub longest_callback_ms: Millis,
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoticeKind {
    Activated,
    Deactivated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingNotice {
    kind: NoticeKind,
    /// Earliest tick that may deliver this notice. Activations are due at once -
    /// spec 14.7 coalesces deactivations, never activations - and deactivations
    /// are due one activation window after they were recorded.
    due_at: Millis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InFlight {
    callback: LegacyCallback,
    since: Millis,
}

impl InFlight {
    fn new(callback: LegacyCallback, since: Millis) -> Self {
        Self { callback, since }
    }
}

#[derive(Debug, Default)]
struct PluginSlot {
    /// Fixed-width bitset: accumulating flags can never grow this instance's
    /// footprint, however long the instance stays blocked.
    pending: LegacyEventFlags,
    /// When the oldest still-undelivered notice was recorded.
    pending_since: Option<Millis>,
    notice: Option<PendingNotice>,
    in_flight: Option<InFlight>,
    state: ActivationState,
}

#[derive(Debug)]
struct RawBurst {
    /// Translated union, folded on arrival so retention limits can never change
    /// what the plugin ends up seeing.
    flags: LegacyEventFlags,
    first_at: Millis,
    last_at: Millis,
    retained: Vec<RawFilesystemNotification>,
}

impl RawBurst {
    fn open(at: Millis) -> Self {
        Self {
            flags: LegacyEventFlags::empty(),
            first_at: at,
            last_at: at,
            retained: Vec::new(),
        }
    }

    fn absorb(
        &mut self,
        notification: RawFilesystemNotification,
        flags: LegacyEventFlags,
        at: Millis,
        counters: &mut CoalescerDiagnostics,
    ) {
        self.flags.insert(flags);
        // Keep both ends of the burst ordered: watcher delivery can arrive
        // slightly out of order, and the maximum-wait deadline is measured from
        // the earliest notification rather than whichever one arrived first.
        self.first_at = self.first_at.min(at);
        self.last_at = self.last_at.max(at);

        // Path- and event-type coalescing (spec 18.7): an identical notification
        // adds nothing an inspector could use.
        if self.retained.contains(&notification) {
            return;
        }
        if self.retained.len() >= MAX_RETAINED_RAW_NOTIFICATIONS {
            counters.raw_notifications_dropped = counters.raw_notifications_dropped.saturating_add(1);
            return;
        }
        self.retained.push(notification);
    }

    /// The sliding window, clamped by the maximum wait so a sustained stream
    /// still resolves (spec 18.7).
    fn deadline(&self, config: &CoalescerConfig) -> Millis {
        let sliding = self.last_at.saturating_add(config.raw_filesystem_window_ms);
        let maximum = self
            .first_at
            .saturating_add(config.raw_filesystem_maximum_wait_ms);
        sliding.min(maximum)
    }
}

// ---------------------------------------------------------------------------
// The coalescer
// ---------------------------------------------------------------------------

/// Turns raw operating-system notifications and host notices into the logical
/// legacy events a plugin observes, honouring per-instance callback
/// serialization (spec 14.6, 14.7, 13.3-13.4, 18.7).
#[derive(Debug)]
pub struct EventCoalescer {
    config: CoalescerConfig,
    /// Ordered by plugin id so a tick's fan-out is deterministic across runs.
    plugins: BTreeMap<PluginId, PluginSlot>,
    /// At most one burst is open at a time: bursts are global, because a
    /// translated filesystem event is broadcast to every instance anyway.
    raw_burst: Option<RawBurst>,
    /// `raw_notifications_collapsed` is derived in [`Self::diagnostics`] and is
    /// deliberately not maintained in this copy.
    counters: CoalescerDiagnostics,
}

impl EventCoalescer {
    /// Creates a coalescer with no registered instances.
    #[must_use]
    pub fn new(config: CoalescerConfig) -> Self {
        Self {
            config,
            plugins: BTreeMap::new(),
            raw_burst: None,
            counters: CoalescerDiagnostics::default(),
        }
    }

    /// Registers an instance. Idempotent: re-registering a known id keeps its
    /// pending state, so a duplicate call cannot drop events.
    pub fn register_plugin(&mut self, plugin: PluginId) {
        self.plugins.entry(plugin).or_default();
    }

    /// Forgets an instance and everything pending for it. The registry only
    /// grows through `register_plugin`, so unloading a package must call this or
    /// the map would retain dead instances for the life of the process.
    pub fn unregister_plugin(&mut self, plugin: &PluginId) {
        self.plugins.remove(plugin);
    }

    /// The active configuration.
    #[must_use]
    pub fn config(&self) -> CoalescerConfig {
        self.config
    }

    // -- Recording ----------------------------------------------------------

    /// Records a semantic event for one instance. No delivery side effect.
    ///
    /// An empty flag set is discarded here, at the source, rather than
    /// travelling to a callback that would tell the plugin nothing (spec 14.6).
    pub fn post_event(&mut self, plugin: &PluginId, flags: LegacyEventFlags, at: Millis) {
        if flags.is_empty() {
            self.counters.empty_events_discarded = self.counters.empty_events_discarded.saturating_add(1);
            return;
        }
        let Some(slot) = self.plugins.get_mut(plugin) else {
            return;
        };
        merge_pending(slot, &mut self.counters, flags, at);
    }

    /// Records a semantic event for every registered instance - spec 14.5's
    /// broadcast rule applied to events. No delivery side effect.
    pub fn broadcast_event(&mut self, flags: LegacyEventFlags, at: Millis) {
        if flags.is_empty() {
            // Counted once: the notice was discarded, not discarded per plugin.
            self.counters.empty_events_discarded = self.counters.empty_events_discarded.saturating_add(1);
            return;
        }
        let Self {
            plugins, counters, ..
        } = self;
        for slot in plugins.values_mut() {
            merge_pending(slot, counters, flags, at);
        }
    }

    /// Records one raw watcher notification into the open burst, opening one if
    /// needed. Nothing is translated or delivered here (spec 18.7).
    pub fn note_raw_filesystem(&mut self, notification: RawFilesystemNotification, at: Millis) {
        self.counters.raw_notifications_seen = self.counters.raw_notifications_seen.saturating_add(1);
        let flags = LegacyEventFlags::for_watch_scope(notification.scope);
        let burst = self.raw_burst.get_or_insert_with(|| RawBurst::open(at));
        burst.absorb(notification, flags, at, &mut self.counters);
    }

    /// Records that the instance was activated.
    ///
    /// A pending deactivation is superseded: the host observed a flap, and the
    /// plugin must see the net state without a bogus `on_deactivated`
    /// (spec 14.7).
    pub fn note_activated(&mut self, plugin: &PluginId, at: Millis) {
        self.note(plugin, NoticeKind::Activated, at, at);
    }

    /// Records that the instance was deactivated.
    ///
    /// Held for [`CoalescerConfig::activation_window_ms`] so a later activation
    /// can supersede it (spec 14.7). This window covers lifecycle notices only;
    /// it must never touch a semantic event.
    pub fn note_deactivated(&mut self, plugin: &PluginId, at: Millis) {
        let due_at = at.saturating_add(self.config.activation_window_ms);
        self.note(plugin, NoticeKind::Deactivated, at, due_at);
    }

    fn note(&mut self, plugin: &PluginId, kind: NoticeKind, at: Millis, due_at: Millis) {
        let Some(slot) = self.plugins.get_mut(plugin) else {
            return;
        };
        if let Some(existing) = slot.notice {
            // Whatever was pending is replaced by the newer notice, because the
            // plugin is owed the net state and spec 14.7 promises no strict
            // alternation. Which case it was stays diagnosable.
            let counters = &mut self.counters;
            let counter = if existing.kind == kind {
                &mut counters.coalesced_repeat_notices
            } else if kind == NoticeKind::Activated {
                &mut counters.superseded_deactivations
            } else {
                &mut counters.superseded_activations
            };
            *counter = counter.saturating_add(1);
        }
        slot.notice = Some(PendingNotice { kind, due_at });
        if slot.pending_since.is_none() {
            slot.pending_since = Some(at);
        }
    }

    /// Records that the host started a callback it did not get from `tick` -
    /// `on_suggest`, `on_catalog`, `on_execute` and friends. Events must not
    /// interleave with it (spec 13.4).
    ///
    /// A duplicate start is ignored. The first callback is still running, so
    /// replacing its record would let a later `end_callback` clear the wrong
    /// callback and release events too early.
    pub fn begin_callback(&mut self, plugin: &PluginId, callback: LegacyCallback, at: Millis) {
        if let Some(slot) = self.plugins.get_mut(plugin) {
            if slot.in_flight.is_none() {
                slot.in_flight = Some(InFlight::new(callback, at));
            }
        }
    }

    /// Records that the instance's in-flight callback finished.
    ///
    /// A raise is diagnosed and dropped: re-queueing the failed delivery's flags
    /// would make a raising plugin see phantom events forever (spec 26.2, 31.9).
    pub fn end_callback(&mut self, plugin: &PluginId, outcome: CallbackOutcome, at: Millis) {
        let Some(slot) = self.plugins.get_mut(plugin) else {
            return;
        };
        let Some(in_flight) = slot.in_flight.take() else {
            return;
        };
        let elapsed = at.saturating_sub(in_flight.since);
        self.counters.longest_callback_ms = self.counters.longest_callback_ms.max(elapsed);
        if outcome == CallbackOutcome::Raised {
            self.counters.failed_deliveries = self.counters.failed_deliveries.saturating_add(1);
        }
    }

    // -- Inspection ---------------------------------------------------------

    /// The flags accumulated for one instance and not yet delivered.
    #[must_use]
    pub fn pending_flags(&self, plugin: &PluginId) -> LegacyEventFlags {
        match self.plugins.get(plugin) {
            Some(slot) => slot.pending,
            None => LegacyEventFlags::empty(),
        }
    }

    /// When the oldest undelivered notice for one instance was recorded. The
    /// host reports from it how long events waited behind a slow callback.
    #[must_use]
    pub fn pending_since(&self, plugin: &PluginId) -> Option<Millis> {
        self.plugins.get(plugin)?.pending_since
    }

    /// The instance's in-flight callback and the time it started, for the host's
    /// spec 13.4 watchdog.
    #[must_use]
    pub fn in_flight(&self, plugin: &PluginId) -> Option<(LegacyCallback, Millis)> {
        let in_flight = self.plugins.get(plugin)?.in_flight?;
        Some((in_flight.callback, in_flight.since))
    }

    /// The activation state the instance has actually been told about
    /// (spec 14.7).
    #[must_use]
    pub fn activation_state(&self, plugin: &PluginId) -> ActivationState {
        match self.plugins.get(plugin) {
            Some(slot) => slot.state,
            None => ActivationState::Deactivated,
        }
    }

    /// The raw notifications retained for the open burst: deduplicated, and
    /// capped at [`MAX_RETAINED_RAW_NOTIFICATIONS`].
    #[must_use]
    pub fn pending_raw_notifications(&self) -> &[RawFilesystemNotification] {
        match &self.raw_burst {
            Some(burst) => &burst.retained,
            None => &[],
        }
    }

    /// The next *time-driven* deadline, if any.
    ///
    /// Only the raw filesystem window and pending deactivations qualify. A
    /// semantic event and a pending activation are already deliverable, and work
    /// held by callback serialization becomes deliverable at `end_callback`, so
    /// reporting either would invent the debounce window spec 14.6 forbids.
    #[must_use]
    pub fn next_wakeup(&self) -> Option<Millis> {
        let burst = self.raw_burst.as_ref().map(|burst| burst.deadline(&self.config));
        self.plugins
            .values()
            .filter_map(|slot| slot.notice)
            .filter(|notice| notice.kind == NoticeKind::Deactivated)
            .map(|notice| notice.due_at)
            .chain(burst)
            .min()
    }

    /// A snapshot of the diagnostic counters (spec 26.2).
    #[must_use]
    pub fn diagnostics(&self) -> CoalescerDiagnostics {
        // "Collapsed" is precisely the raw traffic that did not become a logical
        // event of its own, so it is derived rather than stored: there is no way
        // for the two halves to drift apart (acceptance 31.28).
        let seen = self.counters.raw_notifications_seen;
        let translated = self.counters.logical_events_translated;
        CoalescerDiagnostics {
            raw_notifications_collapsed: seen.saturating_sub(translated),
            ..self.counters
        }
    }

    // -- Delivery -----------------------------------------------------------

    /// Performs every delivery due at or before `now`.
    ///
    /// Returns at most one delivery per instance, because each returned delivery
    /// is an in-flight callback the host must close with [`Self::end_callback`]
    /// before that instance can receive the next one (spec 13.4).
    pub fn tick(&mut self, now: Millis) -> Vec<EventDelivery> {
        self.flush_due_raw_burst(now);

        let Self {
            plugins, counters, ..
        } = self;
        let mut deliveries = Vec::new();
        for (id, slot) in plugins.iter_mut() {
            if slot.in_flight.is_some() {
                continue;
            }

            // Lifecycle first: a plugin should learn that it is activated before
            // being asked to react to what changed while it was not.
            if let Some(notice) = slot.notice.filter(|notice| notice.due_at <= now) {
                slot.notice = None;
                let callback = match notice.kind {
                    NoticeKind::Activated => {
                        slot.state = ActivationState::Activated;
                        LegacyCallback::OnActivated
                    }
                    NoticeKind::Deactivated => {
                        slot.state = ActivationState::Deactivated;
                        LegacyCallback::OnDeactivated
                    }
                };
                if slot.pending.is_empty() {
                    slot.pending_since = None;
                }
                slot.in_flight = Some(InFlight::new(callback, now));
                counters.activation_callbacks_delivered =
                    counters.activation_callbacks_delivered.saturating_add(1);
                deliveries.push(EventDelivery {
                    plugin: id.clone(),
                    callback,
                    // The documented activation callbacks take no flags.
                    flags: LegacyEventFlags::empty(),
                    at: now,
                });
                continue;
            }

            if slot.pending.is_empty() {
                continue;
            }
            // Everything pending goes out as ONE union (spec 14.6), and the set
            // is cleared now rather than at `end_callback`: events arriving
            // during the callback belong to the next delivery, and a raise must
            // not resurrect the flags this one already carried (spec 31.9).
            let flags = slot.pending;
            slot.pending = LegacyEventFlags::empty();
            if slot.notice.is_none() {
                slot.pending_since = None;
            }
            slot.in_flight = Some(InFlight::new(LegacyCallback::OnEvents, now));
            counters.events_delivered = counters.events_delivered.saturating_add(1);
            deliveries.push(EventDelivery {
                plugin: id.clone(),
                callback: LegacyCallback::OnEvents,
                flags,
                at: now,
            });
        }
        deliveries
    }

    /// Translates the open burst once its deadline has passed: the single point
    /// where raw noise becomes one logical legacy event (spec 18.7).
    fn flush_due_raw_burst(&mut self, now: Millis) {
        let due = matches!(&self.raw_burst, Some(burst) if burst.deadline(&self.config) <= now);
        if !due {
            return;
        }
        let Some(burst) = self.raw_burst.take() else {
            return;
        };
        self.counters.logical_events_translated = self.counters.logical_events_translated.saturating_add(1);

        let Self {
            plugins, counters, ..
        } = self;
        for slot in plugins.values_mut() {
            merge_pending(slot, counters, burst.flags, burst.last_at);
        }
    }
}

/// Folds a non-empty notice into an instance's pending set, counting why it did
/// not become a callback of its own.
fn merge_pending(
    slot: &mut PluginSlot,
    counters: &mut CoalescerDiagnostics,
    flags: LegacyEventFlags,
    at: Millis,
) {
    if !slot.pending.is_empty() {
        counters.flag_unions_merged = counters.flag_unions_merged.saturating_add(1);
    }
    if slot.in_flight.is_some() {
        counters.deferred_by_serialization = counters.deferred_by_serialization.saturating_add(1);
    }
    slot.pending.insert(flags);
    if slot.pending_since.is_none() {
        slot.pending_since = Some(at);
    }
}
