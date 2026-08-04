//! Live `legacy-strict` dispatch and the legacy catalog lifecycle
//! (spec 7.1, 7.2, 8.1, 8.4, 8.8, 8.10, 8.11, 9.2, 9.3, 9.5, 9.6, 14.5, 14.8,
//! 14.9, 26.4; ADR-0006; acceptance 31.4, 31.7, 31.8, 31.14 - 31.18).
//!
//! [`LegacyRuntime`] owns every registered legacy plugin instance and decides,
//! per keystroke, what each one is told to do. It performs no I/O: its only
//! outbound edge is [`LegacyWorkerHandle`], and answers re-enter through
//! [`LegacyRuntime::deliver`]. That makes it a pure state machine over an
//! explicit `u64`-millisecond clock — no `Instant::now()`, no sleeps, no
//! threads (spec 25.1 measurement discipline).
//!
//! # Intake decides, `tick` dispatches
//!
//! `submit_query`, `select_item`, `catalog_rebuild`, `deliver` and `reload`
//! evaluate the per-instance serial dispatcher *immediately* and record their
//! verdict at their own timestamp. `tick(now)` is the only call that hands a
//! callback across the worker boundary, and the only place the legacy deadline
//! ladder is evaluated. The single exception is cooperative termination:
//! [`LegacyWorkerHandle::request_termination`] is raised synchronously from
//! intake, because spec 31.17 requires `should_terminate()` to be true at the
//! keystroke timestamp and raising a flag schedules nothing.
//!
//! Registration carries no timestamp, so the one-time `on_start` of a fresh
//! instance is queued by `register` and its dispatch verdict is recorded at the
//! first `tick` that dispatches it.
//!
//! # Why not `ObsoleteWorkManager`
//!
//! [`crikey_input_scheduler::ObsoleteWorkManager`] answers "given a *query*
//! change, is this instance idle or busy". It marks whatever is running as
//! obsolete, which is exactly wrong here: spec 9.2 lists obsolete queries,
//! reload, shutdown, disable and instance supersession as the reasons
//! `should_terminate()` becomes true, and a keystroke is none of those for an
//! in-flight `on_catalog()` (spec 14.8). The dispatcher below therefore keeps
//! its own state but emits [`LegacyDispatch`] verdicts verbatim — the legacy
//! layer does not get a second, parallel scheduling vocabulary.
//!
//! # Bounds
//!
//! Every retained collection has an explicit cap and documented overflow
//! behaviour: at most one undispatched suggestion request per instance
//! (spec 8.8), one cached answer per instance (spec 14.9), and the constants
//! [`TRACE_CAPACITY`], [`MAX_ITEMS_PER_PUBLICATION`] and
//! [`MAX_CATALOG_ITEMS`] below. Nothing here grows with session length.

use std::collections::BTreeMap;
use std::collections::VecDeque;

use crikey_core::{Generation, Item, ItemId, PluginId};
use crikey_input_scheduler::{LegacyDispatch, Millis, SchedulingProfile};

use crate::package::PackageId;
use crate::worker::{
    InstanceId, LegacyOutcome, LegacyRequest, LegacyRequestKind, LegacyResponse, WorkerError,
};
use crate::LegacyCallback;

/// Retained scheduling trace events. On overflow the oldest quarter is dropped
/// in one batch (amortising the shift) and counted by
/// [`LegacyRuntime::trace_dropped`]; the trace is a debugging aid, never a
/// durable log, so losing the distant past is preferable to unbounded growth.
pub const TRACE_CAPACITY: usize = 4_096;

/// Items accepted from one plugin for one suggestion generation. A plugin that
/// answers with more has the excess discarded and counted in
/// [`LegacyPluginDiagnostics::items_dropped`]: the result list is a fixed-height
/// view and an unbounded answer is a plugin defect, not a host obligation.
pub const MAX_ITEMS_PER_PUBLICATION: usize = 4_096;

/// Items retained in one plugin's live catalog. Excess is discarded and counted
/// in [`LegacyPluginDiagnostics::items_dropped`].
pub const MAX_CATALOG_ITEMS: usize = 100_000;

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

/// Why the host raised `should_terminate()` on in-flight legacy work
/// (spec 9.2). Cooperative only: the plugin may refuse, and correctness never
/// depends on its cooperation (spec 9.5, acceptance 31.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminationReason {
    /// A newer query generation superseded the running one.
    QuerySuperseded,
    /// The owning package is being reloaded.
    PackageReload,
    /// The host is shutting down.
    Shutdown,
    /// The plugin was disabled by configuration.
    PluginDisabled,
    /// A newer instance of the same plugin took over.
    InstanceSuperseded,
}

impl TerminationReason {
    /// Stable machine-readable spelling for diagnostics and the developer
    /// commands.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QuerySuperseded => "query-superseded",
            Self::PackageReload => "package-reload",
            Self::Shutdown => "shutdown",
            Self::PluginDisabled => "plugin-disabled",
            Self::InstanceSuperseded => "instance-superseded",
        }
    }
}

impl std::fmt::Display for TerminationReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outbound half of the worker surface: everything the runtime pushes
/// towards the CPython child process. Answers travel the other way through
/// [`LegacyRuntime::deliver`], which is what keeps the runtime free of I/O.
pub trait LegacyWorkerHandle: std::fmt::Debug {
    /// Hand one callback to the plugin instance. `at_ms` is the virtual
    /// timestamp of the dispatch, supplied so an implementation that needs a
    /// real subprocess deadline derives it from a value the caller owns.
    fn dispatch(&mut self, at_ms: Millis, request: &LegacyRequest) -> Result<(), WorkerError>;

    /// Raise the flag the plugin reads through `Plugin.should_terminate()`
    /// (spec 9.2). Advisory: it schedules nothing and must not block.
    fn request_termination(
        &mut self,
        at_ms: Millis,
        plugin: &PluginId,
        instance: InstanceId,
        generation: Generation,
        reason: TerminationReason,
    ) -> Result<(), WorkerError>;

    /// Lower the flag raised by [`Self::request_termination`], so a callback
    /// that has not been superseded observes `should_terminate() == false`
    /// (spec 9.2, 9.5). The host flag is sticky per-process, so fresh,
    /// non-obsolete work must clear it at dispatch time; otherwise a raise from
    /// an earlier, superseded generation would make every later callback
    /// abandon (acceptance 31.17). Advisory, like `request_termination`: it
    /// must not block.
    ///
    /// Defaulted to a no-op so a handle with no cross-process flag to lower
    /// keeps compiling untouched; the production handle overrides it to clear
    /// the shared atomic and write the authoritative `set_terminate:false`
    /// frame.
    fn lower_termination(
        &mut self,
        at_ms: Millis,
        plugin: &PluginId,
        instance: InstanceId,
        generation: Generation,
    ) -> Result<(), WorkerError> {
        let _ = (at_ms, plugin, instance, generation);
        Ok(())
    }

    /// Stop the worker, honouring `budget_ms` as the whole teardown budget
    /// (spec 9.6).
    fn stop(&mut self, at_ms: Millis, budget_ms: Millis) -> Result<(), WorkerError>;
}

/// How long a callback may run before the host reacts (spec 9.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeadlinePolicy {
    /// The modern policy: exceed the budget and the request is killed.
    HardKill { after_ms: Millis },
    /// The legacy ladder: a soft latency warning, then a far hung-worker
    /// watchdog. Legacy callbacks are never hard-killed on a query budget,
    /// because a documented `on_catalog()` legitimately takes minutes.
    Cooperative {
        soft_warning_ms: Millis,
        hung_worker_ms: Millis,
    },
}

/// The deadline ladder the legacy layer applies, and the modern budget it
/// deliberately refuses to apply to legacy callbacks (spec 9.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LegacyDeadlines {
    /// The hard per-query budget a *modern* plugin is killed on. Recorded here
    /// only so the contrast is inspectable; the legacy layer never enforces it.
    pub modern_hard_query_ms: Millis,
    /// Elapsed time past which one soft latency warning is emitted per
    /// callback.
    pub soft_warning_ms: Millis,
    /// Elapsed time past which the callback is reported as a suspected hang.
    pub hung_worker_ms: Millis,
    /// Whole budget for cooperative teardown of every live instance.
    pub teardown_ms: Millis,
}

impl Default for LegacyDeadlines {
    fn default() -> Self {
        Self {
            modern_hard_query_ms: 250,
            soft_warning_ms: 5_000,
            hung_worker_ms: 120_000,
            teardown_ms: 2_000,
        }
    }
}

impl LegacyDeadlines {
    /// What a *modern* plugin's query budget is. Never applied to a legacy
    /// callback; exposed so the difference is auditable rather than folklore.
    pub fn modern_policy(self) -> DeadlinePolicy {
        DeadlinePolicy::HardKill {
            after_ms: self.modern_hard_query_ms,
        }
    }

    /// The ladder every legacy callback runs under (spec 9.6, 14.8).
    pub fn legacy_policy(self) -> DeadlinePolicy {
        DeadlinePolicy::Cooperative {
            soft_warning_ms: self.soft_warning_ms,
            hung_worker_ms: self.hung_worker_ms,
        }
    }
}

/// Whether this plugin's dynamic suggestions may be cached, and on whose
/// authority (spec 14.9). `Refused` is the only default: an unchanged legacy
/// plugin computes suggestions from state the host cannot see, so replaying a
/// previous answer is not equivalent to asking again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DynamicCachePolicy {
    /// Never cached. The default for every unchanged plugin.
    Refused,
    /// Permitted by explicit per-plugin compatibility metadata.
    CompatibilityMetadata,
    /// Permitted by a user-enabled `legacy-optimized` scheduling override.
    LegacyOptimizedOverride,
}

impl DynamicCachePolicy {
    pub fn permits_caching(self) -> bool {
        !matches!(self, Self::Refused)
    }
}

/// Per-plugin compatibility metadata. Deliberately minimal: the only
/// documented opt-in it carries is the dynamic-suggestion cache of spec 14.9,
/// and nothing else in the layer may enable caching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct LegacyCompatibility {
    /// Declares the plugin's `on_suggest()` pure with respect to its query, so
    /// an identical query may be answered from the previous result.
    pub dynamic_suggestion_cache: bool,
}

/// Everything the runtime needs to instantiate one legacy plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyRegistration {
    pub plugin: PluginId,
    pub package: PackageId,
    /// `legacy-strict` unless a user explicitly overrode it (spec 7.1, 7.2).
    pub profile: SchedulingProfile,
    pub compatibility: LegacyCompatibility,
}

impl LegacyRegistration {
    /// An unchanged legacy plugin: `legacy-strict`, no opt-ins.
    pub fn new(plugin: PluginId, package: PackageId) -> Self {
        Self {
            plugin,
            package,
            profile: SchedulingProfile::LegacyStrict,
            compatibility: LegacyCompatibility::default(),
        }
    }
}

/// Why a catalog update was refused (spec 14.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CatalogRejectReason {
    /// It came from an instance a reload has already superseded.
    ObsoleteInstance,
    /// The plugin is not registered.
    UnknownPlugin,
    /// No `on_catalog()` was in flight for this instance.
    NoCatalogBuildInFlight,
}

/// What the runtime did with one answer (spec 14.5, 14.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// A callback completed with nothing to display or catalogue.
    Accepted,
    /// Suggestions for the current generation are now displayed.
    Published { items: usize },
    /// The plugin's live catalog was updated; `total` is its new size.
    CatalogUpdated { total: usize },
    /// The answer belongs to a superseded query generation. Refused at the
    /// intake boundary however late it arrives (acceptance 31.7).
    RejectedStale {
        generation: Generation,
        current: Generation,
    },
    /// The answer belongs to an instance a reload has superseded (spec 14.8).
    RejectedObsoleteInstance {
        instance: InstanceId,
        current: InstanceId,
    },
    /// Nothing was waiting for this answer: an unregistered plugin, or a reply
    /// to work that is no longer in flight. Discarded without side effects.
    Ignored,
}

/// Observable scheduling state of one legacy plugin instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyInstanceState {
    pub instance: InstanceId,
    pub profile: SchedulingProfile,
    /// Whether the one-time `on_start` already completed (spec 14.8).
    pub started: bool,
    pub running: Option<Generation>,
    pub running_callback: Option<LegacyCallback>,
    pub pending: Option<Generation>,
    pub pending_query: Option<String>,
    /// Undispatched requests retained for this instance. Never above two — at
    /// most one suggestion request (spec 8.8) plus at most one coalesced
    /// catalog rebuild.
    pub pending_depth: usize,
}

/// Per-plugin scheduling counters (spec 26.4). Survives a reload: they describe
/// the plugin's behaviour across the session, not one instance's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct LegacyPluginDiagnostics {
    /// Callbacks that actually crossed the worker boundary.
    pub dispatched: u64,
    /// Undispatched requests discarded because a newer one arrived (spec 8.8).
    pub replaced: u64,
    /// High-water mark of the retained-request depth (acceptance 31.4).
    pub max_pending_depth: usize,
    /// Cooperative termination requests issued (spec 9.2).
    pub terminations_requested: u64,
    /// Answers refused because their generation was superseded (spec 31.7).
    pub stale_rejected: u64,
    /// Answers that arrived after an unheeded cooperative request: exactly the
    /// reportable "long callback that does not check `should_terminate()`"
    /// (spec 9.5, 26.2).
    pub late_answers_after_termination_request: u64,
    /// Identical queries re-dispatched because caching is refused (spec 14.9).
    pub cache_refusals: u64,
    /// Identical queries answered from the retained answer under an opt-in.
    pub cache_hits: u64,
    /// Accepted `on_catalog()` requests (spec 14.8).
    pub catalog_rebuilds: u64,
    /// Catalog updates refused, e.g. from a superseded instance.
    pub catalog_updates_rejected: u64,
    /// Soft latency warnings emitted; at most one per callback (spec 9.6).
    pub soft_latency_warnings: u64,
    /// Callbacks that honoured a cooperative termination request and published
    /// nothing. The counterpart of
    /// `late_answers_after_termination_request`: together they say whether a
    /// plugin cooperates (spec 9.5, acceptance 31.7).
    pub callbacks_abandoned: u64,
    /// Suspected hangs reported by the far watchdog (spec 9.6).
    pub hung_workers_suspected: u64,
    /// Items discarded by the publication and catalog caps.
    pub items_dropped: u64,
    /// Dispatch attempts the worker refused (transport failures, spec 24.1).
    pub dispatch_failures: u64,
}

/// One observable legacy scheduling decision (spec 26.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyTraceEvent {
    /// A serial-dispatcher verdict, in the shared `LegacyDispatch` vocabulary.
    Decision {
        at_ms: Millis,
        plugin: PluginId,
        dispatch: LegacyDispatch,
    },
    /// A callback crossed the worker boundary.
    Dispatched {
        at_ms: Millis,
        plugin: PluginId,
        generation: Generation,
        callback: LegacyCallback,
    },
    /// An initial suggestion request went to every loaded legacy plugin, in
    /// registration order (spec 14.5, acceptance 31.15).
    Broadcast {
        at_ms: Millis,
        generation: Generation,
        plugins: Vec<PluginId>,
    },
    /// An argument-suggestion request went to the owner of the selected item.
    Routed {
        at_ms: Millis,
        generation: Generation,
        plugin: PluginId,
        owner_of: ItemId,
    },
    /// `should_terminate()` was raised on in-flight work (spec 9.2).
    TerminationRequested {
        at_ms: Millis,
        plugin: PluginId,
        generation: Generation,
        reason: TerminationReason,
    },
    /// An undispatched request was discarded in favour of a newer one.
    Replaced {
        at_ms: Millis,
        plugin: PluginId,
        discarded: Generation,
        retained: Generation,
    },
    /// A superseded generation's answer was refused (acceptance 31.7).
    StaleRejected {
        at_ms: Millis,
        plugin: PluginId,
        generation: Generation,
        current: Generation,
    },
    /// Suggestions were published for the current generation.
    Published {
        at_ms: Millis,
        plugin: PluginId,
        generation: Generation,
        items: usize,
    },
    /// An identical query was answered from the retained answer (spec 14.9).
    CacheServed {
        at_ms: Millis,
        plugin: PluginId,
        query: String,
    },
    /// An identical query was re-dispatched because caching is refused.
    CacheRefused {
        at_ms: Millis,
        plugin: PluginId,
        query: String,
    },
    /// `set_catalog()` replaced the live catalog.
    CatalogReplaced {
        at_ms: Millis,
        plugin: PluginId,
        items: usize,
    },
    /// `merge_catalog()` extended the live catalog in place.
    CatalogMerged {
        at_ms: Millis,
        plugin: PluginId,
        added: usize,
        total: usize,
    },
    /// A catalog update was refused without mutating the live catalog.
    CatalogRejected {
        at_ms: Millis,
        plugin: PluginId,
        instance: InstanceId,
        reason: CatalogRejectReason,
    },
    /// A reload minted a new instance; the old one is obsolete from here on.
    InstanceSuperseded {
        at_ms: Millis,
        plugin: PluginId,
        previous: InstanceId,
        replacement: InstanceId,
    },
    /// One soft latency warning for a long-running callback (spec 9.6).
    SoftLatencyWarning {
        at_ms: Millis,
        plugin: PluginId,
        callback: LegacyCallback,
        elapsed_ms: Millis,
    },
    /// The far watchdog fired: this callback is probably hung (spec 9.6).
    HungWorkerSuspected {
        at_ms: Millis,
        plugin: PluginId,
        callback: LegacyCallback,
        elapsed_ms: Millis,
    },
    /// The worker refused a dispatch. The instance is freed so one transport
    /// failure cannot wedge a plugin forever (spec 24.1).
    DispatchFailed {
        at_ms: Millis,
        plugin: PluginId,
        callback: LegacyCallback,
        detail: String,
    },
    /// Cooperative teardown finished, within budget, cooperation or not.
    ShutdownCompleted {
        at_ms: Millis,
        instances: usize,
        abandoned: usize,
    },
}

/// Accounting for one cooperative teardown (spec 9.6, 14.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShutdownReport {
    pub requested_at_ms: Millis,
    /// Never later than `requested_at_ms + LegacyDeadlines::teardown_ms`.
    pub completed_at_ms: Millis,
    /// Live instances accounted for.
    pub instances: usize,
    /// Instances still inside a callback when the budget expired.
    pub abandoned: usize,
}

/// Refusals at the runtime's intake boundary. Every variant names what it
/// concerns: an error that cannot be attributed cannot become an actionable
/// diagnostic (spec 26.2).
#[derive(Debug, thiserror::Error)]
pub enum LegacyRuntimeError {
    #[error("legacy plugin `{}` is not registered", .0.0)]
    UnknownPlugin(PluginId),
    #[error("no displayed item has the stable id `{}`", .0.0)]
    UnknownItem(ItemId),
    #[error("the legacy runtime has shut down and accepts no further work")]
    ShuttingDown,
    #[error("legacy worker refused the request: {0}")]
    Worker(#[from] WorkerError),
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// The identity of a suggestion request, and therefore the dynamic cache key
/// (spec 14.9). The selected item is part of it: `""` typed against a selection
/// is a different question from `""` typed with nothing selected.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheKey {
    query: String,
    selected: Option<ItemId>,
}

/// The one answer retained per instance. `items` is empty unless the instance's
/// [`DynamicCachePolicy`] permits caching: the key alone is what a refusal
/// needs, so a refusing plugin's results are never held (spec 14.9).
#[derive(Debug)]
struct RetainedAnswer {
    key: CacheKey,
    items: Vec<Item>,
}

/// Work that can be queued behind an in-flight callback. One-time
/// initialization is not here: it is owed exactly once per instance and is
/// tracked by `queued_start`, so it can never be queued twice (spec 14.8).
#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkKind {
    Catalog,
    Suggest { query: String, selected: Option<ItemId> },
}

impl WorkKind {
    fn callback(&self) -> LegacyCallback {
        match self {
            Self::Catalog => LegacyCallback::OnCatalog,
            Self::Suggest { .. } => LegacyCallback::OnSuggest,
        }
    }

    fn into_request_kind(self) -> LegacyRequestKind {
        match self {
            Self::Catalog => LegacyRequestKind::Catalog,
            Self::Suggest {
                query,
                selected: None,
            } => LegacyRequestKind::InitialSuggest { query },
            Self::Suggest {
                query,
                selected: Some(selected),
            } => LegacyRequestKind::ArgumentSuggest { query, selected },
        }
    }

    fn cache_key(&self) -> Option<CacheKey> {
        match self {
            Self::Suggest { query, selected } => Some(CacheKey {
                query: query.clone(),
                selected: selected.clone(),
            }),
            Self::Catalog => None,
        }
    }

    fn query_text(&self) -> Option<&str> {
        match self {
            Self::Suggest { query, .. } => Some(query.as_str()),
            Self::Catalog => None,
        }
    }
}

#[derive(Debug)]
struct PendingWork {
    generation: Generation,
    work: WorkKind,
}

#[derive(Debug)]
struct RunningWork {
    generation: Generation,
    callback: LegacyCallback,
    /// The question this callback answers, kept so its result can be retained
    /// under the right cache key when it completes.
    key: Option<CacheKey>,
    /// `None` until the request actually crossed the worker boundary. The
    /// deadline ladder measures from the boundary crossing, never from intake.
    dispatched_at_ms: Option<Millis>,
    termination_requested: bool,
    soft_warned: bool,
    hung_reported: bool,
}

#[derive(Debug)]
struct Instance {
    id: InstanceId,
    profile: SchedulingProfile,
    cache_policy: DynamicCachePolicy,
    /// The one-time `on_start` this instance owes, not yet given a timestamp.
    queued_start: bool,
    started: bool,
    /// An initialization failure permanently excludes this instance. A reload
    /// or re-registration creates a fresh instance and may try again.
    disabled: bool,
    running: Option<RunningWork>,
    /// At most one undispatched suggestion request (spec 8.8).
    pending: Option<PendingWork>,
    /// Repeated rebuild requests against a busy instance collapse into this one
    /// bit: `on_catalog()` is idempotent, so a queue of them would be waste.
    pending_catalog: bool,
    last_answer: Option<RetainedAnswer>,
}

impl Instance {
    fn new(id: InstanceId, profile: SchedulingProfile, compatibility: LegacyCompatibility) -> Self {
        Self {
            id,
            profile,
            cache_policy: cache_policy_of(profile, compatibility),
            queued_start: true,
            started: false,
            disabled: false,
            running: None,
            pending: None,
            pending_catalog: false,
            last_answer: None,
        }
    }

    /// A queued-but-undispatched `on_start` counts as busy: nothing may run
    /// before one-time initialization (spec 14.8). A failed initialization is
    /// not busy work; it is an excluded instance and never runs again.
    fn busy(&self) -> bool {
        !self.disabled && (self.running.is_some() || self.queued_start)
    }

    fn pending_depth(&self) -> usize {
        usize::from(self.pending.is_some()) + usize::from(self.pending_catalog)
    }

    fn state(&self) -> LegacyInstanceState {
        LegacyInstanceState {
            instance: self.id,
            profile: self.profile,
            started: self.started,
            running: self.running.as_ref().map(|work| work.generation),
            running_callback: self.running.as_ref().map(|work| work.callback),
            pending: self.pending.as_ref().map(|work| work.generation),
            pending_query: self
                .pending
                .as_ref()
                .and_then(|work| work.work.query_text())
                .map(str::to_owned),
            pending_depth: self.pending_depth(),
        }
    }
}

/// Per-plugin state that outlives any one instance: the live catalog and the
/// session counters both survive a reload (spec 14.8, 26.4).
#[derive(Debug)]
struct PluginRecord {
    package: PackageId,
    /// Retained so a reload can mint a fresh instance with the same declared
    /// scheduling and compatibility metadata.
    profile: SchedulingProfile,
    compatibility: LegacyCompatibility,
    instance: Instance,
    catalog: Vec<Item>,
    diagnostics: LegacyPluginDiagnostics,
}

fn cache_policy_of(profile: SchedulingProfile, compatibility: LegacyCompatibility) -> DynamicCachePolicy {
    // Metadata wins over the profile so a plugin that declares its
    // `on_suggest()` cacheable keeps that guarantee under any profile.
    if compatibility.dynamic_suggestion_cache {
        DynamicCachePolicy::CompatibilityMetadata
    } else if matches!(profile, SchedulingProfile::LegacyStrict) {
        DynamicCachePolicy::Refused
    } else {
        DynamicCachePolicy::LegacyOptimizedOverride
    }
}

/// Appends to the bounded trace ring. Free-standing so callers can push while
/// holding a disjoint mutable borrow of the plugin table.
fn push_trace(trace: &mut Vec<LegacyTraceEvent>, dropped: &mut u64, event: LegacyTraceEvent) {
    if trace.len() >= TRACE_CAPACITY {
        let batch = TRACE_CAPACITY / 4;
        trace.drain(..batch);
        *dropped = dropped.saturating_add(batch as u64);
    }
    trace.push(event);
}

// ---------------------------------------------------------------------------
// The runtime
// ---------------------------------------------------------------------------

/// Owns every registered legacy plugin instance and decides what each one is
/// told to do, per keystroke, under `legacy-strict` rules (spec 14.5).
#[derive(Debug)]
pub struct LegacyRuntime<W> {
    worker: W,
    deadlines: LegacyDeadlines,
    /// Registration order, which is broadcast order (spec 14.5).
    order: Vec<PluginId>,
    plugins: BTreeMap<PluginId, PluginRecord>,
    /// Requests decided but not yet across the boundary. Bounded by one per
    /// instance, because an instance with work in flight accepts none.
    outbox: VecDeque<LegacyRequest>,
    trace: Vec<LegacyTraceEvent>,
    trace_dropped: u64,
    next_generation: u64,
    next_instance: u64,
    current_generation: Generation,
    query: String,
    selected: Option<(PluginId, ItemId)>,
    visible: Vec<Item>,
    visible_generation: Generation,
    shut_down: bool,
    shutdown_report: Option<ShutdownReport>,
}

impl<W: LegacyWorkerHandle> LegacyRuntime<W> {
    pub fn new(worker: W, deadlines: LegacyDeadlines) -> Self {
        Self {
            worker,
            deadlines,
            order: Vec::new(),
            plugins: BTreeMap::new(),
            outbox: VecDeque::new(),
            trace: Vec::new(),
            trace_dropped: 0,
            next_generation: 0,
            next_instance: 0,
            current_generation: Generation::ZERO,
            query: String::new(),
            selected: None,
            visible: Vec::new(),
            visible_generation: Generation::ZERO,
            shut_down: false,
            shutdown_report: None,
        }
    }

    // -- registration -------------------------------------------------------

    /// Registers an unchanged legacy plugin: `legacy-strict`, no cache opt-in.
    pub fn register(&mut self, plugin: PluginId, package: PackageId) -> InstanceId {
        self.register_with(LegacyRegistration::new(plugin, package))
    }

    /// Registers a plugin with explicit scheduling and compatibility metadata.
    /// Re-registering a live plugin supersedes its instance and keeps its
    /// catalog and counters, exactly as a reload does.
    pub fn register_with(&mut self, registration: LegacyRegistration) -> InstanceId {
        let LegacyRegistration {
            plugin,
            package,
            profile,
            compatibility,
        } = registration;
        if self.plugins.contains_key(&plugin) {
            self.invalidate_plugin_display(&plugin);
        }
        let id = self.mint_instance();

        // Registration is also the instance-supersession path used by package
        // reloads. Drop work that has not crossed the boundary, and raise the
        // cooperative flag on work that has. This method has no timestamp, so
        // zero is the registration-time origin.
        let termination = self.plugins.get_mut(&plugin).and_then(|record| {
            let running = record.instance.running.as_mut()?;
            if running.termination_requested {
                return None;
            }
            running.termination_requested = true;
            record.diagnostics.terminations_requested += 1;
            Some((record.instance.id, running.generation))
        });
        if let Some((instance, generation)) = termination {
            let _ = self.worker.request_termination(
                0,
                &plugin,
                instance,
                generation,
                TerminationReason::InstanceSuperseded,
            );
            push_trace(
                &mut self.trace,
                &mut self.trace_dropped,
                LegacyTraceEvent::TerminationRequested {
                    at_ms: 0,
                    plugin: plugin.clone(),
                    generation,
                    reason: TerminationReason::InstanceSuperseded,
                },
            );
        }

        if let Some(previous) = self.plugins.get(&plugin).map(|record| record.instance.id) {
            self.outbox.retain(|request| request.instance != previous);
        }

        let instance = Instance::new(id, profile, compatibility);
        match self.plugins.get_mut(&plugin) {
            Some(record) => {
                record.package = package;
                record.profile = profile;
                record.compatibility = compatibility;
                record.instance = instance;
            }
            None => {
                self.order.push(plugin.clone());
                self.plugins.insert(
                    plugin,
                    PluginRecord {
                        package,
                        profile,
                        compatibility,
                        instance,
                        catalog: Vec::new(),
                        diagnostics: LegacyPluginDiagnostics::default(),
                    },
                );
            }
        }
        id
    }

    // -- intake -------------------------------------------------------------

    /// A query change. Broadcast to every loaded legacy plugin when nothing is
    /// selected, routed to the owning plugin when something is (spec 14.5).
    ///
    /// There is no minimum length and no prefix or keyword gating: the
    /// `legacy-strict` profile refuses host gating outright, so the empty query
    /// and a single character reach every plugin verbatim (acceptance 31.15).
    /// Nor is there any debouncing (acceptance 31.14).
    pub fn submit_query(&mut self, text: &str, at_ms: Millis) -> Generation {
        if self.shut_down {
            return self.current_generation;
        }
        let generation = self.mint_generation();
        self.query = text.to_owned();
        self.begin_generation(generation, at_ms);

        match self.selected.clone() {
            Some((owner, item)) => {
                push_trace(
                    &mut self.trace,
                    &mut self.trace_dropped,
                    LegacyTraceEvent::Routed {
                        at_ms,
                        generation,
                        plugin: owner.clone(),
                        owner_of: item.clone(),
                    },
                );
                self.intake_suggest(&owner, at_ms, generation, text, Some(&item));
            }
            None => {
                let recipients = self
                    .order
                    .iter()
                    .filter(|plugin| {
                        self.plugins
                            .get(*plugin)
                            .is_some_and(|record| !record.instance.disabled)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                push_trace(
                    &mut self.trace,
                    &mut self.trace_dropped,
                    LegacyTraceEvent::Broadcast {
                        at_ms,
                        generation,
                        plugins: recipients.clone(),
                    },
                );
                for plugin in &recipients {
                    self.intake_suggest(plugin, at_ms, generation, text, None);
                }
            }
        }
        generation
    }

    /// Selects a displayed item. Every later suggestion request is routed to
    /// the plugin that owns it and reaches no other plugin (spec 14.5).
    pub fn select_item(&mut self, item: &ItemId, at_ms: Millis) -> Result<Generation, LegacyRuntimeError> {
        if self.shut_down {
            return Err(LegacyRuntimeError::ShuttingDown);
        }
        let owner = self
            .visible
            .iter()
            .find(|candidate| &candidate.stable_id == item)
            .map(|candidate| candidate.plugin_id.clone())
            .ok_or_else(|| LegacyRuntimeError::UnknownItem(item.clone()))?;
        if self
            .plugins
            .get(&owner)
            .is_none_or(|record| record.instance.disabled)
        {
            return Err(LegacyRuntimeError::UnknownPlugin(owner));
        }

        // A selection restarts the argument text: what follows is an argument
        // to the selected item, not a continuation of the initial query.
        self.query = String::new();
        self.selected = Some((owner.clone(), item.clone()));
        let generation = self.mint_generation();
        self.begin_generation(generation, at_ms);
        push_trace(
            &mut self.trace,
            &mut self.trace_dropped,
            LegacyTraceEvent::Routed {
                at_ms,
                generation,
                plugin: owner.clone(),
                owner_of: item.clone(),
            },
        );
        self.intake_suggest(&owner, at_ms, generation, "", Some(item));
        Ok(generation)
    }

    /// Forgets the current selection, so the next query broadcasts again.
    pub fn clear_selection(&mut self) {
        self.selected = None;
    }

    /// Requests a catalog rebuild. `on_catalog()` may be called repeatedly and
    /// carries no query generation: catalog work is not query work and is never
    /// subject to query staleness (spec 14.8).
    pub fn catalog_rebuild(&mut self, plugin: &PluginId, at_ms: Millis) -> Result<(), LegacyRuntimeError> {
        if self.shut_down {
            return Err(LegacyRuntimeError::ShuttingDown);
        }
        let Some(record) = self.plugins.get_mut(plugin) else {
            return Err(LegacyRuntimeError::UnknownPlugin(plugin.clone()));
        };
        if record.instance.disabled {
            return Ok(());
        }
        record.diagnostics.catalog_rebuilds += 1;
        self.enqueue(plugin, at_ms, Generation::ZERO, WorkKind::Catalog);
        Ok(())
    }

    /// Reloads a package: the live instance is superseded, its in-flight work
    /// is asked to stop, and a *new* instance re-runs one-time initialization.
    /// A rebuild never does that (spec 14.8); a reload always does.
    ///
    /// The plugin's live catalog and counters survive, because they describe
    /// the plugin, not the instance.
    pub fn reload(&mut self, plugin: &PluginId, at_ms: Millis) -> Result<InstanceId, LegacyRuntimeError> {
        if self.shut_down {
            return Err(LegacyRuntimeError::ShuttingDown);
        }
        if !self.plugins.contains_key(plugin) {
            return Err(LegacyRuntimeError::UnknownPlugin(plugin.clone()));
        }
        self.invalidate_plugin_display(plugin);
        let replacement = self.mint_instance();
        let Some(record) = self.plugins.get_mut(plugin) else {
            return Err(LegacyRuntimeError::UnknownPlugin(plugin.clone()));
        };
        let previous = record.instance.id;
        let in_flight = record
            .instance
            .running
            .as_ref()
            .map(|work| (work.generation, work.termination_requested));
        if let Some((generation, already_requested)) = in_flight {
            if !already_requested {
                record.diagnostics.terminations_requested += 1;
                // Advisory (spec 9.2); see the note in `enqueue`.
                let _ = self.worker.request_termination(
                    at_ms,
                    plugin,
                    previous,
                    generation,
                    TerminationReason::PackageReload,
                );
                push_trace(
                    &mut self.trace,
                    &mut self.trace_dropped,
                    LegacyTraceEvent::TerminationRequested {
                        at_ms,
                        plugin: plugin.clone(),
                        generation,
                        reason: TerminationReason::PackageReload,
                    },
                );
            }
        }
        record.instance = Instance::new(replacement, record.profile, record.compatibility);

        // Anything the superseded instance still has in the outbox must never
        // reach the worker: it belongs to an instance that no longer exists.
        self.outbox.retain(|request| request.instance != previous);
        push_trace(
            &mut self.trace,
            &mut self.trace_dropped,
            LegacyTraceEvent::InstanceSuperseded {
                at_ms,
                plugin: plugin.clone(),
                previous,
                replacement,
            },
        );
        Ok(replacement)
    }

    // -- dispatch -----------------------------------------------------------

    /// Hands every decided callback across the worker boundary and evaluates
    /// the legacy deadline ladder. The only call that performs a dispatch.
    pub fn tick(&mut self, now: Millis) -> Vec<LegacyRequest> {
        if self.shut_down {
            return Vec::new();
        }

        // Registration carries no timestamp, so the one-time `on_start` gets
        // its dispatch verdict here, at the first tick that can run it.
        for plugin in &self.order {
            let Some(record) = self.plugins.get_mut(plugin) else {
                continue;
            };
            let instance = &mut record.instance;
            if instance.disabled || !instance.queued_start || instance.running.is_some() {
                continue;
            }
            instance.queued_start = false;
            instance.running = Some(RunningWork {
                generation: Generation::ZERO,
                callback: LegacyCallback::OnStart,
                key: None,
                dispatched_at_ms: None,
                termination_requested: false,
                soft_warned: false,
                hung_reported: false,
            });
            self.outbox.push_back(LegacyRequest {
                plugin: plugin.clone(),
                instance: instance.id,
                generation: Generation::ZERO,
                kind: LegacyRequestKind::Start,
            });
            push_trace(
                &mut self.trace,
                &mut self.trace_dropped,
                LegacyTraceEvent::Decision {
                    at_ms: now,
                    plugin: plugin.clone(),
                    dispatch: LegacyDispatch::Now(Generation::ZERO),
                },
            );
        }

        let mut dispatched = Vec::with_capacity(self.outbox.len());
        while let Some(request) = self.outbox.pop_front() {
            let callback = request.callback();
            // A callback that has not been superseded MUST observe
            // `should_terminate() == false`. The host termination flag is
            // sticky per-process (a QuerySuperseded/reload/shutdown raise from
            // an earlier generation stays set on the child), so fresh,
            // non-obsolete work lowers it here, before the request crosses the
            // boundary — otherwise gen2 and every later generation would read a
            // leaked raise and abandon forever (spec 9.2, 9.5, acceptance
            // 31.17). This is what keeps `should_terminate()` (which reports
            // from `RunningWork.termination_requested`) in agreement with what
            // the child reads: a not-yet-superseded callback disagrees with
            // neither.
            let non_obsolete = self
                .plugins
                .get(&request.plugin)
                .and_then(|record| record.instance.running.as_ref())
                .is_some_and(|running| !running.termination_requested);
            if non_obsolete {
                // Advisory (spec 9.2), discarded like every other cooperative
                // signal; the deadline ladder reports an unreachable worker.
                let _ =
                    self.worker
                        .lower_termination(now, &request.plugin, request.instance, request.generation);
            }
            match self.worker.dispatch(now, &request) {
                Ok(()) => {
                    if let Some(record) = self.plugins.get_mut(&request.plugin) {
                        record.diagnostics.dispatched += 1;
                        if let Some(running) = record.instance.running.as_mut() {
                            running.dispatched_at_ms = Some(now);
                        }
                    }
                    push_trace(
                        &mut self.trace,
                        &mut self.trace_dropped,
                        LegacyTraceEvent::Dispatched {
                            at_ms: now,
                            plugin: request.plugin.clone(),
                            generation: request.generation,
                            callback,
                        },
                    );
                    dispatched.push(request);
                }
                Err(error) => {
                    // The callback never started, so the instance must not stay
                    // wedged behind work that will never answer (spec 24.1).
                    let detail = error.to_string();
                    if let Some(record) = self.plugins.get_mut(&request.plugin) {
                        record.diagnostics.dispatch_failures += 1;
                        record.instance.running = None;
                        if callback == LegacyCallback::OnStart {
                            record.instance.disabled = true;
                            record.instance.pending = None;
                            record.instance.pending_catalog = false;
                        }
                    }
                    push_trace(
                        &mut self.trace,
                        &mut self.trace_dropped,
                        LegacyTraceEvent::DispatchFailed {
                            at_ms: now,
                            plugin: request.plugin.clone(),
                            callback,
                            detail,
                        },
                    );
                    self.finish_callback(&request.plugin, now);
                }
            }
        }

        self.evaluate_deadlines(now);
        dispatched
    }

    /// Applies the legacy ladder (spec 9.6): one soft warning per callback and
    /// a far watchdog. Never the modern hard query deadline — a documented
    /// `on_catalog()` legitimately runs for minutes and killing it would break
    /// conforming plugins (spec 14.8).
    fn evaluate_deadlines(&mut self, now: Millis) {
        let soft = self.deadlines.soft_warning_ms;
        let hung = self.deadlines.hung_worker_ms;
        for plugin in &self.order {
            let Some(record) = self.plugins.get_mut(plugin) else {
                continue;
            };
            let Some(running) = record.instance.running.as_mut() else {
                continue;
            };
            let Some(dispatched_at) = running.dispatched_at_ms else {
                continue;
            };
            let elapsed = now.saturating_sub(dispatched_at);
            let callback = running.callback;
            if !running.soft_warned && elapsed >= soft {
                running.soft_warned = true;
                record.diagnostics.soft_latency_warnings += 1;
                push_trace(
                    &mut self.trace,
                    &mut self.trace_dropped,
                    LegacyTraceEvent::SoftLatencyWarning {
                        at_ms: now,
                        plugin: plugin.clone(),
                        callback,
                        elapsed_ms: elapsed,
                    },
                );
            }
            let Some(running) = record.instance.running.as_mut() else {
                continue;
            };
            if !running.hung_reported && elapsed >= hung {
                running.hung_reported = true;
                record.diagnostics.hung_workers_suspected += 1;
                push_trace(
                    &mut self.trace,
                    &mut self.trace_dropped,
                    LegacyTraceEvent::HungWorkerSuspected {
                        at_ms: now,
                        plugin: plugin.clone(),
                        callback,
                        elapsed_ms: elapsed,
                    },
                );
            }
        }
    }

    // -- inbound ------------------------------------------------------------

    /// Accepts one answer. Retires the in-flight callback whether or not the
    /// answer is accepted: a plugin that answers a superseded query has its
    /// result refused, but its instance still becomes free — otherwise an
    /// uncooperative plugin could starve every later query (spec 9.5, 31.7).
    pub fn deliver(&mut self, response: LegacyResponse, at_ms: Millis) -> Delivery {
        // After shutdown the runtime is dead but `shutdown` deliberately leaves
        // `instance.running = Some(..)`, so a late worker answer would still
        // land here. Publishing into `self.visible` or overwriting
        // `record.catalog` would let a dead runtime mutate state the UI still
        // shows, and `finish_callback` would re-promote pending work into an
        // outbox no `tick` will ever drain (`tick` early-returns once
        // `shut_down`). Match every other entry point and drop it (Finding 2).
        if self.shut_down {
            return Delivery::Ignored;
        }
        let plugin = response.plugin.clone();
        let Some(record) = self.plugins.get_mut(&plugin) else {
            return Delivery::Ignored;
        };
        let live = record.instance.id;

        if response.instance != live {
            // A reload superseded this instance. Nothing it says may touch live
            // state (spec 14.8): not the catalog, not the display, not the
            // serial dispatcher of the instance that replaced it.
            if matches!(
                response.outcome,
                LegacyOutcome::SetCatalog(_) | LegacyOutcome::MergeCatalog(_)
            ) {
                record.diagnostics.catalog_updates_rejected += 1;
                push_trace(
                    &mut self.trace,
                    &mut self.trace_dropped,
                    LegacyTraceEvent::CatalogRejected {
                        at_ms,
                        plugin,
                        instance: response.instance,
                        reason: CatalogRejectReason::ObsoleteInstance,
                    },
                );
            }
            return Delivery::RejectedObsoleteInstance {
                instance: response.instance,
                current: live,
            };
        }

        // Only an answer to the callback actually in flight retires it. A reply
        // to work already retired is a protocol duplicate, not a completion.
        let answers_running = record.instance.running.as_ref().is_some_and(|running| {
            running.generation == response.generation && running.callback == response.callback
        });
        if !answers_running {
            return Delivery::Ignored;
        }
        let running = record
            .instance
            .running
            .take()
            .expect("running work was just observed");
        if running.termination_requested {
            record.diagnostics.late_answers_after_termination_request += 1;
        }
        let initialization_failed = running.callback == LegacyCallback::OnStart
            && matches!(&response.outcome, LegacyOutcome::Failed(_));
        if running.callback == LegacyCallback::OnStart {
            if initialization_failed {
                // A plugin that cannot complete on_start is excluded. Keeping
                // it registered but allowing later callbacks would violate
                // initialization-before-use and leave a half-live instance.
                record.instance.disabled = true;
                record.instance.pending = None;
                record.instance.pending_catalog = false;
            } else {
                record.instance.started = true;
            }
        }

        let current = self.current_generation;
        let delivery = match response.outcome {
            LegacyOutcome::SetCatalog(items) => {
                let items = truncate(items, MAX_CATALOG_ITEMS, &mut record.diagnostics);
                let total = items.len();
                record.catalog = items;
                push_trace(
                    &mut self.trace,
                    &mut self.trace_dropped,
                    LegacyTraceEvent::CatalogReplaced {
                        at_ms,
                        plugin: plugin.clone(),
                        items: total,
                    },
                );
                Delivery::CatalogUpdated { total }
            }
            LegacyOutcome::MergeCatalog(items) => {
                // A merge updates an existing item in place by stable id rather
                // than duplicating it (spec 10.2), and appends the rest.
                let mut added = 0usize;
                for item in items {
                    // Looked up by index rather than `iter_mut().find()`: the
                    // `None` arm of a match on `Option<&mut Item>` still holds
                    // the borrow the push below needs.
                    let existing = record
                        .catalog
                        .iter()
                        .position(|candidate| candidate.stable_id == item.stable_id);
                    match existing {
                        Some(index) => record.catalog[index] = item,
                        None => {
                            if record.catalog.len() >= MAX_CATALOG_ITEMS {
                                record.diagnostics.items_dropped += 1;
                                continue;
                            }
                            record.catalog.push(item);
                            added += 1;
                        }
                    }
                }
                let total = record.catalog.len();
                push_trace(
                    &mut self.trace,
                    &mut self.trace_dropped,
                    LegacyTraceEvent::CatalogMerged {
                        at_ms,
                        plugin: plugin.clone(),
                        added,
                        total,
                    },
                );
                Delivery::CatalogUpdated { total }
            }
            LegacyOutcome::Suggestions(items) => {
                if response.generation == current {
                    let items = truncate(items, MAX_ITEMS_PER_PUBLICATION, &mut record.diagnostics);
                    // Retain the key unconditionally: a refusal has to be able
                    // to say *which* identical query it refused to replay. The
                    // items themselves are held only under an opt-in (14.9).
                    if let Some(key) = running.key {
                        let retained = if record.instance.cache_policy.permits_caching() {
                            items.clone()
                        } else {
                            Vec::new()
                        };
                        record.instance.last_answer = Some(RetainedAnswer { key, items: retained });
                    }
                    let count = items.len();
                    push_trace(
                        &mut self.trace,
                        &mut self.trace_dropped,
                        LegacyTraceEvent::Published {
                            at_ms,
                            plugin: plugin.clone(),
                            generation: response.generation,
                            items: count,
                        },
                    );
                    // Inlined rather than calling `replace_visible`: a `&mut
                    // self` call here would conflict with the live `record`
                    // borrow, while these two fields are disjoint from it.
                    self.visible.retain(|item| item.plugin_id != plugin);
                    self.visible.extend(items);
                    Delivery::Published { items: count }
                } else {
                    record.diagnostics.stale_rejected += 1;
                    push_trace(
                        &mut self.trace,
                        &mut self.trace_dropped,
                        LegacyTraceEvent::StaleRejected {
                            at_ms,
                            plugin: plugin.clone(),
                            generation: response.generation,
                            current,
                        },
                    );
                    Delivery::RejectedStale {
                        generation: response.generation,
                        current,
                    }
                }
            }
            // A callback that saw `should_terminate()` and deliberately
            // published nothing. Distinct from `Suggestions(vec![])` on
            // purpose: an abandoned callback must not clobber the live list, so
            // it neither publishes nor updates the retained answer. Counted,
            // because a plugin that abandons is the cooperative one (spec 9.5).
            LegacyOutcome::Abandoned => {
                record.diagnostics.callbacks_abandoned += 1;
                Delivery::Accepted
            }
            // An acknowledgement, an execution and a plugin exception are all
            // healthy-worker outcomes: the instance frees and the session
            // continues (spec 24.1).
            _ => Delivery::Accepted,
        };

        self.finish_callback(&plugin, at_ms);
        delivery
    }

    // -- observation --------------------------------------------------------

    pub fn worker(&self) -> &W {
        &self.worker
    }

    pub fn worker_mut(&mut self) -> &mut W {
        &mut self.worker
    }

    pub fn deadlines(&self) -> LegacyDeadlines {
        self.deadlines
    }

    /// Every recorded scheduling decision, oldest first (spec 26.4). Bounded by
    /// [`TRACE_CAPACITY`]; see [`Self::trace_dropped`].
    pub fn trace(&self) -> &[LegacyTraceEvent] {
        &self.trace
    }

    /// Trace events discarded to keep the ring bounded.
    pub fn trace_dropped(&self) -> u64 {
        self.trace_dropped
    }

    pub fn diagnostics(&self, plugin: &PluginId) -> Option<LegacyPluginDiagnostics> {
        self.plugins.get(plugin).map(|record| record.diagnostics)
    }

    pub fn instance_state(&self, plugin: &PluginId) -> Option<LegacyInstanceState> {
        self.plugins.get(plugin).map(|record| record.instance.state())
    }

    /// The package the live instance was created from.
    pub fn package(&self, plugin: &PluginId) -> Option<&PackageId> {
        self.plugins.get(plugin).map(|record| &record.package)
    }

    /// What a plugin running inside the worker would read from
    /// `Plugin.should_terminate()` right now (spec 9.2).
    pub fn should_terminate(&self, plugin: &PluginId) -> bool {
        self.plugins.get(plugin).is_some_and(|record| {
            record
                .instance
                .running
                .as_ref()
                .is_some_and(|running| running.termination_requested)
        })
    }

    pub fn dynamic_cache_policy(&self, plugin: &PluginId) -> Option<DynamicCachePolicy> {
        self.plugins
            .get(plugin)
            .map(|record| record.instance.cache_policy)
    }

    /// The deadline ladder a given legacy callback runs under. Always the
    /// cooperative one: no legacy callback is hard-killed on a query budget.
    pub fn deadline_policy(&self, plugin: &PluginId, _callback: LegacyCallback) -> Option<DeadlinePolicy> {
        self.plugins.get(plugin).map(|_| self.deadlines.legacy_policy())
    }

    pub fn catalog(&self, plugin: &PluginId) -> &[Item] {
        match self.plugins.get(plugin) {
            Some(record) => &record.catalog,
            None => &[],
        }
    }

    /// Exactly what is displayed right now: only the current generation's
    /// answers ever appear here (acceptance 31.7).
    pub fn visible_items(&self) -> &[Item] {
        &self.visible
    }

    pub fn visible_generation(&self) -> Generation {
        self.visible_generation
    }

    /// The current query generation. Monotonic; every keystroke mints its own
    /// (spec 8.1).
    pub fn current_generation(&self) -> Generation {
        self.current_generation
    }

    pub fn current_query(&self) -> &str {
        &self.query
    }

    pub fn selected_item(&self) -> Option<&ItemId> {
        self.selected.as_ref().map(|(_, item)| item)
    }

    /// Registered plugins in registration order, which is broadcast order.
    pub fn plugins(&self) -> &[PluginId] {
        &self.order
    }

    // -- teardown -----------------------------------------------------------

    /// Cooperative teardown, bounded by [`LegacyDeadlines::teardown_ms`] even
    /// when no plugin cooperates (spec 9.6, 14.8). After this, `tick` is empty.
    pub fn shutdown(&mut self, at_ms: Millis) -> ShutdownReport {
        if let Some(report) = self.shutdown_report {
            return report;
        }
        let mut abandoned = 0usize;
        let plugins = self.order.clone();
        for plugin in &plugins {
            let Some(record) = self.plugins.get_mut(plugin) else {
                continue;
            };
            let Some(running) = record.instance.running.as_mut() else {
                continue;
            };
            abandoned += 1;
            if running.termination_requested {
                continue;
            }
            running.termination_requested = true;
            let (instance, generation) = (record.instance.id, running.generation);
            record.diagnostics.terminations_requested += 1;
            // Advisory (spec 9.2); see the note in `enqueue`.
            let _ = self.worker.request_termination(
                at_ms,
                plugin,
                instance,
                generation,
                TerminationReason::Shutdown,
            );
            push_trace(
                &mut self.trace,
                &mut self.trace_dropped,
                LegacyTraceEvent::TerminationRequested {
                    at_ms,
                    plugin: plugin.clone(),
                    generation,
                    reason: TerminationReason::Shutdown,
                },
            );
        }

        let instances = self.plugins.len();
        // Nothing decided before shutdown may still cross the boundary.
        self.outbox.clear();
        self.shut_down = true;
        // Advisory, like every other cooperative signal: the bounded budget
        // below is what makes teardown terminate, not the worker's agreement.
        let _ = self.worker.stop(at_ms, self.deadlines.teardown_ms);

        // A plugin that never honoured the request is waited out for the whole
        // budget and then abandoned; teardown is bounded either way.
        let completed_at_ms = if abandoned == 0 {
            at_ms
        } else {
            at_ms.saturating_add(self.deadlines.teardown_ms)
        };
        push_trace(
            &mut self.trace,
            &mut self.trace_dropped,
            LegacyTraceEvent::ShutdownCompleted {
                at_ms: completed_at_ms,
                instances,
                abandoned,
            },
        );
        let report = ShutdownReport {
            requested_at_ms: at_ms,
            completed_at_ms,
            instances,
            abandoned,
        };
        self.shutdown_report = Some(report);
        report
    }

    // -- internals ----------------------------------------------------------

    fn mint_generation(&mut self) -> Generation {
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("legacy query generation counter exhausted at u64::MAX");
        Generation::from_raw(self.next_generation)
    }

    fn mint_instance(&mut self) -> InstanceId {
        self.next_instance = self
            .next_instance
            .checked_add(1)
            .expect("legacy instance counter exhausted at u64::MAX");
        InstanceId(self.next_instance)
    }

    /// A new query generation displays nothing until somebody answers it: the
    /// previous generation's items are not what the user asked for any more,
    /// and replaying them is the caching that spec 14.9 refuses by default.
    fn begin_generation(&mut self, generation: Generation, at_ms: Millis) {
        self.current_generation = generation;
        self.visible_generation = generation;
        self.visible.clear();

        let mut replaced = Vec::new();
        let mut terminations = Vec::new();
        for (plugin, record) in &mut self.plugins {
            if let Some(pending) = record.instance.pending.take() {
                record.diagnostics.replaced += 1;
                replaced.push((plugin.clone(), pending.generation));
            }

            let Some(running) = record.instance.running.as_mut() else {
                continue;
            };
            if running.callback == LegacyCallback::OnSuggest && !running.termination_requested {
                running.termination_requested = true;
                record.diagnostics.terminations_requested += 1;
                terminations.push((plugin.clone(), record.instance.id, running.generation));
            }
        }

        for (plugin, discarded) in replaced {
            push_trace(
                &mut self.trace,
                &mut self.trace_dropped,
                LegacyTraceEvent::Replaced {
                    at_ms,
                    plugin,
                    discarded,
                    retained: generation,
                },
            );
        }
        for (plugin, instance, obsolete_generation) in terminations {
            let _ = self.worker.request_termination(
                at_ms,
                &plugin,
                instance,
                obsolete_generation,
                TerminationReason::QuerySuperseded,
            );
            push_trace(
                &mut self.trace,
                &mut self.trace_dropped,
                LegacyTraceEvent::TerminationRequested {
                    at_ms,
                    plugin,
                    generation: obsolete_generation,
                    reason: TerminationReason::QuerySuperseded,
                },
            );
        }
    }

    /// Intake for one suggestion request against one plugin: dynamic cache
    /// first (spec 14.9), then the serial dispatcher (spec 14.5).
    fn intake_suggest(
        &mut self,
        plugin: &PluginId,
        at_ms: Millis,
        generation: Generation,
        query: &str,
        selected: Option<&ItemId>,
    ) {
        let key = CacheKey {
            query: query.to_owned(),
            selected: selected.cloned(),
        };

        enum Replay {
            Serve(Vec<Item>),
            Refuse,
            Ask,
        }
        let replay = match self.plugins.get(plugin) {
            None => return,
            Some(record) if record.instance.disabled => return,
            Some(record) => match &record.instance.last_answer {
                Some(answer) if answer.key == key => {
                    if record.instance.cache_policy.permits_caching() {
                        Replay::Serve(answer.items.clone())
                    } else {
                        Replay::Refuse
                    }
                }
                _ => Replay::Ask,
            },
        };

        match replay {
            Replay::Serve(items) => {
                let count = items.len();
                if let Some(record) = self.plugins.get_mut(plugin) {
                    record.diagnostics.cache_hits += 1;
                }
                push_trace(
                    &mut self.trace,
                    &mut self.trace_dropped,
                    LegacyTraceEvent::CacheServed {
                        at_ms,
                        plugin: plugin.clone(),
                        query: query.to_owned(),
                    },
                );
                push_trace(
                    &mut self.trace,
                    &mut self.trace_dropped,
                    LegacyTraceEvent::Published {
                        at_ms,
                        plugin: plugin.clone(),
                        generation,
                        items: count,
                    },
                );
                // A cache hit is published under the *current* generation, never
                // under the generation that produced it (spec 8.1).
                self.replace_visible(plugin, items);
            }
            Replay::Refuse => {
                if let Some(record) = self.plugins.get_mut(plugin) {
                    record.diagnostics.cache_refusals += 1;
                }
                push_trace(
                    &mut self.trace,
                    &mut self.trace_dropped,
                    LegacyTraceEvent::CacheRefused {
                        at_ms,
                        plugin: plugin.clone(),
                        query: query.to_owned(),
                    },
                );
                self.enqueue(
                    plugin,
                    at_ms,
                    generation,
                    WorkKind::Suggest {
                        query: query.to_owned(),
                        selected: selected.cloned(),
                    },
                );
            }
            Replay::Ask => self.enqueue(
                plugin,
                at_ms,
                generation,
                WorkKind::Suggest {
                    query: query.to_owned(),
                    selected: selected.cloned(),
                },
            ),
        }
    }

    /// The serial dispatcher for one instance (spec 8.4, 14.5). Emits its
    /// verdict in the shared [`LegacyDispatch`] vocabulary.
    fn enqueue(&mut self, plugin: &PluginId, at_ms: Millis, generation: Generation, work: WorkKind) {
        let Some(record) = self.plugins.get_mut(plugin) else {
            return;
        };
        let instance = &mut record.instance;

        if !instance.busy() {
            let callback = work.callback();
            let key = work.cache_key();
            instance.running = Some(RunningWork {
                generation,
                callback,
                key,
                dispatched_at_ms: None,
                termination_requested: false,
                soft_warned: false,
                hung_reported: false,
            });
            self.outbox.push_back(LegacyRequest {
                plugin: plugin.clone(),
                instance: instance.id,
                generation,
                kind: work.into_request_kind(),
            });
            push_trace(
                &mut self.trace,
                &mut self.trace_dropped,
                LegacyTraceEvent::Decision {
                    at_ms,
                    plugin: plugin.clone(),
                    dispatch: LegacyDispatch::Now(generation),
                },
            );
            return;
        }

        // Busy: no second callback may start on this instance (acceptance
        // 31.16). The running generation is what the newer request is queued
        // behind; `Generation::ZERO` when the running work is not query work.
        let obsolete = instance
            .running
            .as_ref()
            .map_or(Generation::ZERO, |running| running.generation);

        match work {
            WorkKind::Catalog => {
                // Idempotent, so repeated rebuild requests coalesce into one
                // bit rather than forming a queue.
                instance.pending_catalog = true;
            }
            suggest @ WorkKind::Suggest { .. } => {
                if let Some(previous) = instance.pending.replace(PendingWork {
                    generation,
                    work: suggest,
                }) {
                    record.diagnostics.replaced += 1;
                    push_trace(
                        &mut self.trace,
                        &mut self.trace_dropped,
                        LegacyTraceEvent::Replaced {
                            at_ms,
                            plugin: plugin.clone(),
                            discarded: previous.generation,
                            retained: generation,
                        },
                    );
                }
            }
        }

        let depth = record.instance.pending_depth();
        if depth > record.diagnostics.max_pending_depth {
            record.diagnostics.max_pending_depth = depth;
        }
        push_trace(
            &mut self.trace,
            &mut self.trace_dropped,
            LegacyTraceEvent::Decision {
                at_ms,
                plugin: plugin.clone(),
                dispatch: LegacyDispatch::QueuedBehindRunning {
                    obsolete,
                    queued: generation,
                },
            },
        );
    }

    /// The in-flight callback returned. Promotes the retained request, if any.
    fn finish_callback(&mut self, plugin: &PluginId, at_ms: Millis) {
        let Some(record) = self.plugins.get_mut(plugin) else {
            return;
        };
        let instance = &mut record.instance;
        if instance.running.is_some() {
            return;
        }
        if instance.disabled {
            instance.pending = None;
            instance.pending_catalog = false;
            return;
        }

        // A rebuild goes first: it is a one-shot data build the queued query
        // may well want to search, and the query is retained either way.
        let promoted = if instance.pending_catalog {
            instance.pending_catalog = false;
            Some(PendingWork {
                generation: Generation::ZERO,
                work: WorkKind::Catalog,
            })
        } else {
            instance.pending.take()
        };

        let Some(PendingWork { generation, work }) = promoted else {
            push_trace(
                &mut self.trace,
                &mut self.trace_dropped,
                LegacyTraceEvent::Decision {
                    at_ms,
                    plugin: plugin.clone(),
                    dispatch: LegacyDispatch::Idle,
                },
            );
            return;
        };

        let callback = work.callback();
        let key = work.cache_key();
        instance.running = Some(RunningWork {
            generation,
            callback,
            key,
            dispatched_at_ms: None,
            termination_requested: false,
            soft_warned: false,
            hung_reported: false,
        });
        self.outbox.push_back(LegacyRequest {
            plugin: plugin.clone(),
            instance: instance.id,
            generation,
            kind: work.into_request_kind(),
        });
        push_trace(
            &mut self.trace,
            &mut self.trace_dropped,
            LegacyTraceEvent::Decision {
                at_ms,
                plugin: plugin.clone(),
                dispatch: LegacyDispatch::Now(generation),
            },
        );
    }

    /// A reload or duplicate registration invalidates results produced by the
    /// superseded instance. Keeping them selectable would route an old item to
    /// a new package instance.
    fn invalidate_plugin_display(&mut self, plugin: &PluginId) {
        self.visible.retain(|item| &item.plugin_id != plugin);
        if self.selected.as_ref().is_some_and(|(owner, _)| owner == plugin) {
            self.selected = None;
        }
    }
    /// One plugin's contribution to the display replaces its previous one; the
    /// other plugins' answers for the same generation are untouched.
    fn replace_visible(&mut self, plugin: &PluginId, items: Vec<Item>) {
        self.visible.retain(|item| &item.plugin_id != plugin);
        self.visible.extend(items);
    }
}

/// Enforces a retained-collection cap, counting what it discards. Silent
/// truncation is the bug this exists to make visible.
fn truncate(mut items: Vec<Item>, cap: usize, diagnostics: &mut LegacyPluginDiagnostics) -> Vec<Item> {
    if items.len() > cap {
        let dropped = items.len() - cap;
        diagnostics.items_dropped = diagnostics.items_dropped.saturating_add(dropped as u64);
        items.truncate(cap);
    }
    items
}
