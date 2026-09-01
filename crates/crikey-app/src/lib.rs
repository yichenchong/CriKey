//! Composition root (spec 5.1).
//!
//! Wires the query scheduler, core services, plugin hosts and the platform
//! backend for the current target. Nothing else in the workspace is allowed to
//! know which backend was selected.
//!
//! [`SearchService`] is where that wiring becomes one user-visible behaviour
//! (spec 11.1, 11.3, 11.6, 25.6): it owns the [`App`], the catalog, the query
//! engine, the ranker and the result aggregator, and turns typed text into a
//! ranked answer.

mod cabi_provider;
mod legacy_provider;
mod modern_provider;
mod native_provider;
mod plugin_action;
mod plugin_icons;
mod plugin_page;
mod query_pipeline;
mod remote_catalog;
mod selection_history_store;
mod startup_recovery;
mod wasm_provider;

/// Re-exported so the composition root can report per-plugin diagnostics
/// without depending on the supervisor crate directly; the driver accessors
/// that return these live here.
pub use crikey_plugin_supervisor::{BudgetKind, ConcurrencyRefusals, PluginHealth};
pub use crikey_query::AliasTable;
/// Re-exported so a host can persist ranking history without naming the
/// ranking crate: [`SelectionHistoryStore`] speaks in exactly these types.
pub use crikey_ranking::{QueryAffinityRecord, SelectionHistorySnapshot, SelectionRecord};
pub use crikey_result_aggregator::{
    BatchPriority, BatchState, DrainBudget, DrainReport, InboundBatch, IntakePolicy, MergedBatch,
    OverflowPolicy, ProducerState, QueueDepth, QueueDiagnostics, QueueEvent, QueueEventKind, QueueLimits,
    QueueReject, RejectReason, ResultBatch, ResultLimits,
};
pub use legacy_provider::{
    LegacyDirectories, LegacyDriver, LegacyProvider, LegacyUnavailable, LegacyWorkerPool,
};
pub use modern_provider::{ModernDriver, ModernProvider, ModernUnavailable};
pub use native_provider::{NativeDriver, NativeProvider, NativeUnavailable};
pub use plugin_action::{
    ActionEffect, ActionRequestId, ActionSubmission, HostCapability, PluginActionCompletion,
    PluginActionExecutor, PluginActionRouter,
};
pub use plugin_icons::{
    IconFetch, PluginIconResolver, PluginResourceSource, MAX_PLUGIN_ICON_BYTES, PLUGIN_ICON_DEADLINE,
};
pub use plugin_page::PageUpdate;
pub use query_pipeline::{PipelineConfig, PipelineError, PipelineTick, QueryPipeline};
pub use remote_catalog::{
    fetch_source, remote_owner, CatalogFetcher, DefaultCatalogFetcher, RemoteCatalogError,
    RemoteCatalogService, RemoteManifest, RemoteOutcome, RemoteReport, RemoteSlice, RemoteSource,
    RemoteSourceStatus, MAX_MANIFEST_BYTES, MAX_SIGNATURE_BYTES, REMOTE_OWNER_PREFIX,
};
pub use selection_history_store::SelectionHistoryStore;
pub use startup_recovery::{
    admitted_plugin_roots, DisabledPlugins, StartupJournal, StartupMode, DISABLED_BY_CONFIGURATION,
    SAFE_MODE_AFTER_FAILURES,
};

/// One complete configuration state, per plugin, ready for delivery (spec 21.4).
///
/// The host resolves layering and per-plugin scoping before this point, so a
/// provider's job is only to hand each worker the map addressed to it. Keyed by
/// the namespaced plugin identity the providers register, and each inner map is
/// keyed by the field names the plugin declared — never by the host's dotted
/// configuration keys, which a plugin has no use for.
///
/// A plugin present with an EMPTY map is meaningful and different from an absent
/// one: it says "you have no settings", where absence says nothing at all and
/// would leave the plugin applying whatever it was last sent.
pub type PluginConfiguration =
    std::collections::BTreeMap<PluginId, std::collections::BTreeMap<String, String>>;

use std::{
    cmp::{Ordering, Reverse},
    collections::{BinaryHeap, HashMap, HashSet},
    sync::{Arc, Mutex},
};

use crikey_catalog::{
    CacheError, CachedSlice, CatalogCache, CatalogError, CatalogStore, CatalogUpdate, MemoryCatalog,
};
use crikey_core::{
    Action, ActionId, ArgumentPolicy, Category, ExecutionPolicy, Generation, GenerationTracker, Item, ItemId,
    PluginId, Result as CoreResult,
};
use crikey_platform::{
    application_arguments, application_items, application_working_directory, decode_target, file_items,
    Clipboard, FileSearchQuery, FileSearchResults, IconImage, IconProvider, WindowInfo,
    APPLICATION_LAUNCH_ACTION_ID, DEFAULT_ICON_SIZE, FILE_OPEN_ACTION_ID, FILE_REVEAL_ACTION_ID,
};
#[cfg(any(windows, target_os = "linux"))]
use crikey_platform::{HotkeyActivationHandler, HotkeyBinding};
use crikey_query::{
    DefaultMatcher, DefaultNormalizer, MatchMethod, MatchOutcome, MatchPolicy, Matcher, NormalizedQuery,
    Normalizer, PreparedLabel,
};
use crikey_ranking::{DefaultRanker, QueryAffinity, RankingSignals, Score, SelectionHistory};
use crikey_result_aggregator::{MemoryResultAggregator, ResultAggregator};
use crikey_ui::ResultRow;

/// How many resolved icons one session retains before the memo is dropped whole.
///
/// Bounds a walk through a very large catalog: a decoded 48x48 icon is 9 KiB, so
/// this is a few megabytes at worst, and the platform's on-disk cache is what
/// makes re-resolving after a clear cheap.
const MAX_RESOLVED_ICONS: usize = 512;

/// State-only milestones for staged startup (spec 25.6).
///
/// Completing a milestone records coordination state only. The caller remains
/// responsible for performing and verifying the corresponding startup work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupStage {
    WindowAndHotkey,
    PersistedCatalog,
    AcceptQueries,
    RequiredWorkers,
    LegacyPlugins,
    BackgroundRefresh,
    LazyModernPlugins,
}

/// A rejected acknowledgement of a startup milestone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupError {
    /// The acknowledged milestone precedes the milestone currently pending.
    StaleAcknowledgement {
        expected: StartupStage,
        pending: StartupStage,
    },
    /// The acknowledged milestone has not become pending yet.
    OutOfOrderAcknowledgement {
        expected: StartupStage,
        pending: StartupStage,
    },
    /// The eager startup sequence has already handed off to lazy activation.
    AlreadyComplete,
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleAcknowledgement { expected, pending } => write!(
                formatter,
                "startup milestone {expected:?} is stale; {pending:?} is pending"
            ),
            Self::OutOfOrderAcknowledgement { expected, pending } => write!(
                formatter,
                "startup milestone {expected:?} is out of order; {pending:?} is pending"
            ),
            Self::AlreadyComplete => formatter.write_str("eager startup coordination is already complete"),
        }
    }
}

impl std::error::Error for StartupError {}

impl StartupStage {
    /// Returns the next startup stage in specification order.
    pub const fn next(self) -> Option<Self> {
        match self {
            Self::WindowAndHotkey => Some(Self::PersistedCatalog),
            Self::PersistedCatalog => Some(Self::AcceptQueries),
            Self::AcceptQueries => Some(Self::RequiredWorkers),
            Self::RequiredWorkers => Some(Self::LegacyPlugins),
            Self::LegacyPlugins => Some(Self::BackgroundRefresh),
            Self::BackgroundRefresh => Some(Self::LazyModernPlugins),
            Self::LazyModernPlugins => None,
        }
    }

    fn precedes(self, other: Self) -> bool {
        let mut candidate = self.next();
        while let Some(stage) = candidate {
            if stage == other {
                return true;
            }
            candidate = stage.next();
        }
        false
    }
}

#[derive(Debug)]
pub struct App {
    backend: Backend,
    generations: GenerationTracker,
    limits: ResultLimits,
    stage: StartupStage,
    eager_startup_complete: bool,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            backend: Backend::new(),
            generations: GenerationTracker::new(),
            limits: ResultLimits::default(),
            stage: StartupStage::WindowAndHotkey,
            eager_startup_complete: false,
        }
    }

    /// Builds an application with explicit result safety limits.
    ///
    /// Limits are fixed before [`SearchService`] constructs its aggregator, so
    /// the composition cannot disagree about the bounds for one generation.
    pub fn with_limits(limits: ResultLimits) -> Self {
        Self {
            limits,
            ..Self::new()
        }
    }

    /// Builds an application over a caller-supplied platform backend.
    ///
    /// [`Backend`] is a per-target alias, so this is not a way to run one
    /// platform's backend on another; it is how a caller substitutes a backend
    /// it has configured. The reason it exists is testing capabilities whose
    /// answer depends on the machine: file search reads the user's real home
    /// directory, and a test that asserted against that would assert against
    /// whatever happens to be on the developer's disk. The platform crates
    /// already expose the matching seams — `LinuxBackend::with_file_search`,
    /// `MacFileSearch::walking` — and this is the host-side end of them.
    pub fn with_backend(backend: Backend) -> Self {
        Self {
            backend,
            ..Self::new()
        }
    }

    pub fn generations(&self) -> &GenerationTracker {
        &self.generations
    }

    pub fn limits(&self) -> &ResultLimits {
        &self.limits
    }

    /// Returns the milestone currently awaiting acknowledgement.
    ///
    /// After eager startup completes, the terminal lazy-activation milestone
    /// remains visible for diagnostics.
    pub fn stage(&self) -> StartupStage {
        self.stage
    }

    /// Acknowledges completion of the expected current startup milestone.
    ///
    /// This method coordinates state only: it performs no window, catalog,
    /// worker, plugin, or refresh work. Intermediate acknowledgements return
    /// the next pending milestone. Acknowledging `LazyModernPlugins` returns
    /// `None` and completes the eager sequence, handing responsibility to
    /// demand-driven lazy activation without activating a plugin itself.
    pub fn complete_stage(&mut self, expected: StartupStage) -> Result<Option<StartupStage>, StartupError> {
        if self.eager_startup_complete {
            return Err(StartupError::AlreadyComplete);
        }

        if expected != self.stage {
            let error = if expected.precedes(self.stage) {
                StartupError::StaleAcknowledgement {
                    expected,
                    pending: self.stage,
                }
            } else {
                StartupError::OutOfOrderAcknowledgement {
                    expected,
                    pending: self.stage,
                }
            };
            return Err(error);
        }

        match self.stage.next() {
            Some(next) => {
                self.stage = next;
                Ok(Some(next))
            }
            None => {
                self.eager_startup_complete = true;
                Ok(None)
            }
        }
    }

    /// Whether the acknowledged milestones permit user queries.
    pub fn can_accept_queries(&self) -> bool {
        match self.stage {
            StartupStage::WindowAndHotkey | StartupStage::PersistedCatalog | StartupStage::AcceptQueries => {
                false
            }
            StartupStage::RequiredWorkers
            | StartupStage::LegacyPlugins
            | StartupStage::BackgroundRefresh
            | StartupStage::LazyModernPlugins => true,
        }
    }

    /// Whether eager startup coordination has handed off to lazy activation.
    ///
    /// This does not mean that any lazy modern plugin has been activated.
    pub fn startup_complete(&self) -> bool {
        self.eager_startup_complete
    }

    /// The platform's icon resolver, behind
    /// [`Capability::Icons`](crikey_platform::Capability::Icons).
    ///
    /// Every backend has one, so this is not an `Option`: what varies is how
    /// much of its platform's icon surface it can answer for, which each backend
    /// reports through its own capability state rather than by withholding the
    /// service.
    pub fn icon_provider(&self) -> &dyn IconProvider {
        self.backend.icon_provider()
    }

    /// The window the desktop currently focuses, when this build's backend can
    /// observe one.
    ///
    /// `None` covers every negative answer, and they are genuinely equivalent
    /// to the one caller: no window is focused, the session withholds window
    /// control (a Wayland compositor, or an X display with no EWMH manager),
    /// or this target has no window service at all. The one thing that must
    /// not happen is a positive answer nothing read, so nothing here falls
    /// back to a guess when the backend declines.
    ///
    /// A read error is folded into `None` rather than propagated: the caller
    /// is a ranking signal that runs before every query, and a broken X
    /// connection must degrade the ranking, not fail the query.
    pub fn foreground_window(&self) -> Option<WindowInfo> {
        #[cfg(target_os = "linux")]
        {
            self.backend
                .window_service()
                .and_then(|service| service.foreground_window().ok().flatten())
        }
        // Windows and macOS have no `WindowService` implementation in this
        // build, so there is nothing to ask. Reporting `None` is the honest
        // answer; synthesising one from the process list would not be.
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    /// The session's clipboard, when this build's backend has one for the
    /// session it is running in.
    ///
    /// `None` is a session fact, not a defect: a Linux unit with no display
    /// server has no clipboard to reach, a Wayland session without XWayland
    /// cannot reach the one it has, and a macOS process without access to the
    /// pasteboard server is handed none. A caller that gets `None` should say so
    /// rather than pretend a copy happened.
    ///
    /// Owned, unlike [`Self::icon_provider`] and [`Self::search_file_items`],
    /// and that is the load-bearing part of this signature: an X11 selection is
    /// served by the client that owns it, so the value only stays pasteable
    /// while somebody holds this box. The launcher holds it for the lifetime of
    /// the process. Borrowing from the backend instead would tie the user's
    /// clipboard to the lifetime of an `App` that lives on a worker thread.
    pub fn clipboard(&self) -> Option<Box<dyn Clipboard>> {
        self.backend.clipboard()
    }

    /// Discovers the current platform's applications and maps them into one
    /// catalog slice owned by `plugin`.
    pub fn discover_application_items(&self, plugin: &PluginId) -> CoreResult<Vec<Item>> {
        let discovered = self.backend.application_discovery().discover()?;
        Ok(application_items(plugin, &discovered))
    }

    /// Searches the platform's files and folders, mapping hits into items
    /// owned by `plugin`.
    ///
    /// `None` when this build's backend has no file search for the session it
    /// is running in — a Linux unit with no readable home, a Windows build off
    /// Windows. That is distinct from `Some` with no hits, which means the
    /// search ran and found nothing, and the caller must keep them apart: the
    /// first is a missing capability to report, the second is an ordinary
    /// empty answer to show.
    ///
    /// Unlike [`Self::discover_application_items`] this runs per query rather
    /// than once at startup, so the deadline in `query` is load bearing. The
    /// backends treat it as a promise; see `crikey_platform::file_search`.
    pub fn search_file_items(
        &self,
        plugin: &PluginId,
        query: &FileSearchQuery,
    ) -> Option<CoreResult<(Vec<Item>, FileSearchResults)>> {
        let service = self.backend.file_search()?;
        Some(service.search(query).map(|results| {
            let items = file_items(plugin, &results.hits);
            (items, results)
        }))
    }

    /// Registers the activation shortcut and connects it to the native UI
    /// loop's wake-up callback.
    ///
    /// Compiled for the targets whose backend has a real global-shortcut
    /// implementation behind [`Capability::GlobalHotkeys`]: Win32
    /// `RegisterHotKey`, X11 `GrabKey`, and Linux Wayland's portal-backed
    /// GlobalShortcuts service. A target without one has no method here at all,
    /// so a host that calls it fails to build rather than being handed a
    /// shortcut nothing can deliver.
    ///
    /// [`Capability::GlobalHotkeys`]: crikey_platform::Capability::GlobalHotkeys
    #[cfg(any(windows, target_os = "linux"))]
    pub fn register_activation_hotkey(
        &mut self,
        accelerator: &str,
        handler: HotkeyActivationHandler,
    ) -> CoreResult<()> {
        let binding = HotkeyBinding {
            accelerator: accelerator.to_owned(),
        };
        // The Windows backend always has a service; the Linux one has to reach
        // an X display first, and says so when it cannot.
        #[cfg(windows)]
        let hotkeys = self.backend.hotkeys();
        #[cfg(target_os = "linux")]
        let hotkeys = self.backend.hotkeys()?;
        hotkeys.set_activation_handler(Some(handler));
        if let Err(error) = hotkeys.register(&binding) {
            hotkeys.set_activation_handler(None);
            return Err(error);
        }
        Ok(())
    }

    /// Releases the activation shortcut, so a launcher that re-binds its
    /// accelerator hands the old chord back to the desktop.
    ///
    /// Same targets and same error type as
    /// [`register_activation_hotkey`](Self::register_activation_hotkey): the
    /// pair is one API, and a host able to take a grab it can never release
    /// would leave every chord it has ever bound swallowing that key press for
    /// the life of the process. The handler is left installed, because the
    /// caller re-binds rather than goes silent -- a launcher that unregisters
    /// its last accelerator has nothing left that can fire.
    #[cfg(any(windows, target_os = "linux"))]
    pub fn unregister_activation_hotkey(&mut self, accelerator: &str) -> CoreResult<()> {
        let binding = HotkeyBinding {
            accelerator: accelerator.to_owned(),
        };
        #[cfg(windows)]
        let hotkeys = self.backend.hotkeys();
        #[cfg(target_os = "linux")]
        let hotkeys = self.backend.hotkeys()?;
        hotkeys.unregister(&binding)
    }

    fn launch_application(&self, item: &Item) -> CoreResult<()> {
        let target = decode_target(&item.target).map_err(|error| {
            crikey_core::CoreError::Invalid(format!("invalid application target: {error}"))
        })?;
        let arguments = application_arguments(item)?;
        let working_directory = application_working_directory(item)?;
        self.backend
            .process_launcher()
            .launch_in(&target, &arguments, working_directory.as_ref())
    }

    /// Opens a file item's target with the desktop's default handler.
    ///
    /// The path is reconstructed from the item's own encoded target rather
    /// than from anything the renderer carried alongside the row: a target is
    /// the one lossless copy of the path (ADR-0007), and a row's description
    /// is display text that has already been through a lossy conversion.
    fn open_file_item(&self, item: &Item) -> CoreResult<()> {
        self.file_opener()?.open_path(&file_target(item)?)
    }

    /// Shows a file item's target in the desktop's file manager.
    fn reveal_file_item(&self, item: &Item) -> CoreResult<()> {
        self.file_opener()?.reveal_path(&file_target(item)?)
    }

    /// The platform opener, or the refusal that says this session has none.
    ///
    /// A session without one is reachable -- a Linux container with no
    /// xdg-utils -- and it must fail here rather than earlier: the file rows
    /// themselves are still useful, and the user who picks one is owed the
    /// reason instead of a row that silently does nothing.
    fn file_opener(&self) -> CoreResult<&dyn crikey_platform::FileOpener> {
        self.backend.file_opener().ok_or_else(|| {
            crikey_core::CoreError::Invalid(format!(
                "the {} backend has no way to open files in this session",
                Backend::NAME
            ))
        })
    }

    /// Name of the platform backend compiled into this build.
    pub fn platform_backend_name() -> &'static str {
        Backend::NAME
    }

    /// Whether this session's desktop composites window transparency.
    ///
    /// Asked before the launcher window is created, because it decides whether
    /// the window may have a shape: a rounded window on a desktop that
    /// discards alpha presents black corners rather than rounded ones. Only
    /// [`CapabilityState::Available`] is an answer to build a shape on —
    /// `Partial` and the rest describe a capability that works sometimes,
    /// which for a window that is either the right shape or visibly wrong is
    /// the same as no.
    pub fn desktop_composites(&self) -> bool {
        matches!(
            self.backend.capability(crikey_platform::Capability::Compositing),
            crikey_platform::CapabilityState::Available
        )
    }
}

/// The path a file item names, or the refusal that says why it names none.
///
/// Separate from the launch decode so the two diagnostics stay distinct: a
/// broken application target and a broken file target come from different
/// producers and are fixed in different places.
fn file_target(item: &Item) -> CoreResult<crikey_core::PlatformPath> {
    decode_target(&item.target)
        .map_err(|error| crikey_core::CoreError::Invalid(format!("invalid file target: {error}")))
}

// ---------------------------------------------------------------------------
// Composed search service (spec 11.1, 11.3, 11.6, 25.6)
// ---------------------------------------------------------------------------

/// Why a submitted query was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchError {
    /// Startup has not acknowledged the AcceptQueries milestone yet.
    NotAcceptingQueries { pending: StartupStage },
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAcceptingQueries { pending } => write!(
                formatter,
                "queries are not accepted yet; startup milestone {pending:?} is pending"
            ),
        }
    }
}

impl std::error::Error for SearchError {}

/// One catalog item that answered the current query, with the evidence that
/// placed it.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// The catalog item, exactly as the owning plugin published it.
    pub item: Item,
    /// Ordering key from the ranker. Higher is better.
    pub score: Score,
    /// Weakest interpretation the matcher had to accept, which is what fixes
    /// the coarse rank.
    pub method: MatchMethod,
    /// Byte ranges within the item label the matcher credited, ordered and
    /// disjoint.
    pub highlights: Vec<(usize, usize)>,
    /// True when this hit was merged in for the current generation only by
    /// [`SearchService::merge_query_items`], rather than coming from the
    /// catalog.
    ///
    /// Provenance is recorded on the hit rather than in a side table because
    /// the answer vector is replaced wholesale by every accepted query: a
    /// flag that lives in the answer cannot outlive it, whereas a side table
    /// would have to be cleared in every path that clears `results`.
    pub ephemeral: bool,
}

/// Match evidence for one query, keyed by owner and then by stable id.
///
/// The aggregator retains [`Item`]s alone, so the outcome that justified each
/// item is parked here for the ranking pass instead of being recomputed.
type Outcomes = HashMap<PluginId, HashMap<ItemId, MatchOutcome>>;

type PositionCache = HashMap<PluginId, Vec<usize>>;

#[derive(Debug)]
struct CandidateCache {
    normalized: String,
    by_owner: PositionCache,
}

#[derive(Clone)]
struct CatalogCacheHandle(Arc<dyn CatalogCache + Send + Sync>);

impl std::fmt::Debug for CatalogCacheHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CatalogCacheHandle(..)")
    }
}

/// Work performed by the most recently accepted local query.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchStats {
    /// Catalog items examined by bounded selection after catalog prefilters.
    pub candidates_examined: u64,
    /// Candidates the matcher accepted before result limits were applied.
    pub matches_found: u64,
}

/// A matching catalog item held by reference until bounded selection decides
/// that it belongs in the owned result set.
#[derive(Debug)]
struct RankedCandidate<'a> {
    item: &'a Item,
    prepared_label: &'a PreparedLabel,
    score: Score,
}

impl PartialEq for RankedCandidate<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for RankedCandidate<'_> {}

impl PartialOrd for RankedCandidate<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedCandidate<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .cmp(&other.score)
            // A smaller identity wins a tie, so reverse the identity
            // comparisons while keeping a stronger candidate `Greater`.
            .then_with(|| other.item.stable_id.cmp(&self.item.stable_id))
            .then_with(|| other.item.plugin_id.cmp(&self.item.plugin_id))
    }
}

/// Presentation order for a published answer: strongest score first, then a
/// stable identity so equal scores order the same way on every host.
///
/// The single definition is what lets a merged item and a catalog item share
/// one ordering instead of being interleaved by two comparators.
fn by_rank(left: &SearchHit, right: &SearchHit) -> Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| left.item.stable_id.cmp(&right.item.stable_id))
        .then_with(|| left.item.plugin_id.cmp(&right.item.plugin_id))
}

#[derive(Debug, Clone, Copy)]
struct SearchPlan<'a> {
    matcher: &'a DefaultMatcher,
    ranker: &'a DefaultRanker,
    non_prefix_upper: &'a HashMap<PluginId, Score>,
    query: &'a NormalizedQuery,
    previous: Option<&'a PositionCache>,
    history: &'a SelectionHistory,
    foreground_category: Option<&'a Category>,
    now_secs: u64,
    per_plugin_limit: usize,
    global_limit: usize,
    batch_limit: usize,
}

#[derive(Debug)]
struct PluginSelection<'items, 'query> {
    matcher: &'query DefaultMatcher,
    ranker: &'query DefaultRanker,
    query: &'query NormalizedQuery,
    history: &'query SelectionHistory,
    /// Per-item affinity for this query, resolved once for the whole sweep.
    affinity: QueryAffinity<'query>,
    foreground_category: Option<&'query Category>,
    now_secs: u64,
    match_spans: Vec<(usize, usize)>,
    matched_positions: Vec<usize>,
    stats: SearchStats,
    retained: BinaryHeap<Reverse<RankedCandidate<'items>>>,
    limit: usize,
}

impl<'items, 'query> PluginSelection<'items, 'query> {
    /// Takes the whole plan rather than eight positional arguments: every field
    /// it needs is already grouped there, and `SearchPlan` is `Copy`.
    fn new(plan: SearchPlan<'query>, candidate_capacity: usize) -> Self {
        Self {
            matcher: plan.matcher,
            ranker: plan.ranker,
            query: plan.query,
            history: plan.history,
            affinity: plan.history.affinities_for(plan.query),
            foreground_category: plan.foreground_category,
            now_secs: plan.now_secs,
            match_spans: Vec::new(),
            matched_positions: Vec::with_capacity(candidate_capacity),
            stats: SearchStats::default(),
            retained: BinaryHeap::with_capacity(plan.per_plugin_limit),
            limit: plan.per_plugin_limit,
        }
    }

    fn consider(&mut self, position: usize, item: &'items Item, prepared_label: &'items PreparedLabel) {
        self.stats.candidates_examined = self.stats.candidates_examined.saturating_add(1);
        let Some(summary) =
            self.matcher
                .score_prepared(self.query, item, prepared_label, &mut self.match_spans)
        else {
            return;
        };
        self.matched_positions.push(position);
        self.stats.matches_found = self.stats.matches_found.saturating_add(1);
        let mut signals = RankingSignals::default();
        self.history.augment(
            item,
            &self.affinity,
            self.now_secs,
            self.foreground_category,
            &mut signals,
        );
        retain_best(
            &mut self.retained,
            RankedCandidate {
                score: self.ranker.score_match_with_signals(item, summary, signals),
                item,
                prepared_label,
            },
            self.limit,
        );
    }

    /// Records a candidate that was skipped for scoring but must still narrow
    /// the next keystroke.
    ///
    /// The cache is a superset of the match set, so membership is decided by
    /// `may_match` under the policy this selection *scores* with. Deciding that
    /// at the call site invited the two to drift: a strict test beside a
    /// subsequence-scoring matcher discards precisely the candidates that
    /// matcher exists to find, and because the cached set only ever shrinks,
    /// they never come back. Keeping it here leaves one policy source instead
    /// of two that have to agree.
    ///
    /// `already_filtered` says the source applied the same test — the catalog
    /// does when revisiting cached positions — so it is not repeated.
    fn record_pruned(
        &mut self,
        position: usize,
        prepared_label: &'items PreparedLabel,
        already_filtered: bool,
    ) {
        self.stats.candidates_examined = self.stats.candidates_examined.saturating_add(1);
        if already_filtered || prepared_label.may_match_with(self.query, self.matcher.policy()) {
            self.matched_positions.push(position);
        }
    }
}

/// What one pass over the persistent cache made of it (spec 22.1, 25.6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CatalogLoad {
    /// Items the live catalog retained, and so the count that became
    /// searchable.
    pub items: usize,
    /// Owners the cache advertised that this pass could not admit: the read
    /// faulted, the stored bytes were not trustworthy, or the live catalog
    /// refused the slice.
    ///
    /// Cached items are a rebuildable artifact, so each of those costs one
    /// owner its cached state and nothing else. Counting them is what keeps a
    /// half-loaded catalog distinguishable from a complete one while startup
    /// stage 2 could still rebuild the difference.
    pub skipped: usize,
}
/// A catalog produced by a provider worker and authorized for one plugin
/// instance. Providers attach the instance and source generation before
/// publishing; [`SearchService::replace_catalog`] remains the sole live
/// catalog publication edge and rejects stale instances.
#[derive(Debug)]
pub struct CatalogBuild {
    pub plugin: PluginId,
    pub instance: u64,
    pub generation: Generation,
    pub items: Vec<Item>,
    /// Whether this slice may be written to the persistent catalog cache
    /// (spec 22.1). Carried with the build rather than looked up at the
    /// publication edge because the manifest that declares it is held by the
    /// provider, and the edge is reached from three of them.
    pub persist: bool,
}

/// A catalog request that completed without publishing because it was
/// superseded by a newer request for the same plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObsoleteCatalogBuild {
    pub plugin: PluginId,
    pub instance: u64,
    pub generation: Generation,
}

/// Result of one bounded provider catalog task.
#[derive(Debug)]
pub enum CatalogBuildResult {
    Complete(CatalogBuild),
    Failed {
        plugin: PluginId,
        instance: u64,
        generation: Generation,
        reason: String,
    },
    Obsolete(ObsoleteCatalogBuild),
}

/// Why a provider refused a catalog-build request before starting a worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogDispatchError {
    UnknownPlugin { plugin: PluginId },
    BudgetRefused { plugin: PluginId },
    QueueFull { plugin: PluginId },
    ThreadSpawn { plugin: PluginId, reason: String },
}

impl std::fmt::Display for CatalogDispatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownPlugin { plugin } => write!(formatter, "unknown plugin {}", plugin.0),
            Self::BudgetRefused { plugin } => {
                write!(formatter, "catalog budget is full for plugin {}", plugin.0)
            }
            Self::QueueFull { plugin } => {
                write!(formatter, "catalog result queue is full for plugin {}", plugin.0)
            }
            Self::ThreadSpawn { plugin, reason } => {
                write!(
                    formatter,
                    "catalog worker for plugin {} could not start: {reason}",
                    plugin.0
                )
            }
        }
    }
}

impl std::error::Error for CatalogDispatchError {}

impl CatalogBuild {
    /// Publishes through the existing owner/instance-safe catalog edge.
    ///
    /// The declaration is applied first, so that a plugin refusing persistence
    /// has any slice an earlier run left on disk withdrawn even on the pass
    /// where it publishes a replacement.
    pub fn publish(self, search: &mut SearchService) -> Result<usize, CatalogError> {
        search.set_catalog_persistence(&self.plugin, self.persist);
        search.replace_catalog(&self.plugin, self.instance, self.items)
    }
}

/// The catalog, query engine, ranker and aggregator as one behaviour.
///
/// Owning the [`App`] is what lets a single object answer both "may I query?"
/// and "here are the results": startup gating decides *when* a query is legal,
/// generations decide *which* answer is current, and ranking decides *what
/// order* the answer arrives in.
///
/// Every stage is delegated. Nothing here normalizes, matches, scores or
/// enforces a result limit on its own.
#[derive(Debug)]
pub struct SearchService {
    app: App,
    catalog: MemoryCatalog,
    /// Owners holding a retained catalog slice, ascending, so the sweep visits
    /// them in an order that does not depend on hash iteration.
    owners: Vec<PluginId>,
    aggregator: MemoryResultAggregator,
    normalizer: DefaultNormalizer,
    /// User-defined query rewrites, applied to every normalized query.
    aliases: AliasTable,
    matcher: DefaultMatcher,
    /// Plugins whose manifest refuses catalog persistence (spec 22.1, 22.4).
    volatile: HashSet<PluginId>,
    /// Plugins whose live slice came from the persisted cache rather than from
    /// the plugin itself during this run.
    cache_sourced: HashSet<PluginId>,
    ranker: DefaultRanker,
    results: Vec<SearchHit>,
    catalog_cache: Option<CatalogCacheHandle>,
    cache_error: Option<CacheError>,
    last_query_stats: SearchStats,
    candidate_cache: Option<CandidateCache>,
    non_prefix_upper: HashMap<PluginId, Score>,
    /// Successful selections used to improve future generations.
    history: SelectionHistory,
    /// Optional foreground application category supplied by the platform layer.
    foreground_category: Option<Category>,
    /// Deterministic clock value used for recency scoring.
    now_secs: u64,
    /// Query that was active when the current results were published.
    last_query: Option<NormalizedQuery>,
    /// Exact-owner runtime endpoints for plugin-owned actions.
    plugin_actions: Option<Arc<PluginActionRouter>>,
    /// The plugin and page id of the one open plugin-drawn page.
    ///
    /// One at a time, and the host remembers which: every later page call
    /// names no plugin, so without this the launcher could not tell which
    /// runtime a keystroke on the visible surface belongs to.
    page: Option<(PluginId, String)>,
    /// Icons already resolved for this session, keyed by reference.
    ///
    /// A miss is recorded too. A themed name no installed theme carries costs a
    /// walk of every directory of every theme in the chain, and re-walking it
    /// for the same reference on every publication would make an item with a
    /// broken icon the most expensive kind of item to display.
    icons: Mutex<HashMap<String, Option<Arc<IconImage>>>>,
}

impl SearchService {
    /// Wraps an [`App`], sharing its result limits with the aggregator.
    pub fn new(app: App) -> Self {
        let aggregator = MemoryResultAggregator::new(*app.limits());
        Self {
            app,
            catalog: MemoryCatalog::new(),
            owners: Vec::new(),
            aggregator,
            normalizer: DefaultNormalizer::default(),
            aliases: AliasTable::default(),
            matcher: DefaultMatcher::default(),
            volatile: HashSet::new(),
            cache_sourced: HashSet::new(),
            ranker: DefaultRanker::new(crikey_ranking::HistoryPolicy { enabled: true }),
            results: Vec::new(),
            catalog_cache: None,
            cache_error: None,
            last_query_stats: SearchStats::default(),
            candidate_cache: None,
            non_prefix_upper: HashMap::new(),
            history: SelectionHistory::default(),
            foreground_category: None,
            now_secs: 0,
            last_query: None,
            plugin_actions: None,
            page: None,
            icons: Mutex::new(HashMap::new()),
        }
    }

    /// Chooses which readings the matcher credits (spec 11.1).
    ///
    /// Defaults to [`MatchPolicy::Strict`]. [`MatchPolicy::Subsequence`]
    /// additionally admits ordered-subsequence matches, which cannot be made
    /// selective — `manic` reaches `Memory Diagnostic Tool` that way — so it is
    /// off unless a caller asks for it.
    ///
    /// Changing the policy invalidates the candidate cache: it was narrowed
    /// under the previous policy, and a stricter narrowing has already discarded
    /// the looser candidates the new policy would want.
    pub fn set_match_policy(&mut self, policy: MatchPolicy) {
        if self.matcher.policy() == policy {
            return;
        }
        self.matcher = match policy {
            MatchPolicy::Strict => DefaultMatcher::new(),
            MatchPolicy::Subsequence => DefaultMatcher::with_subsequence(),
        };
        self.candidate_cache = None;
    }

    /// Installs the user's query aliases (spec 21.2).
    ///
    /// The candidate cache is dropped, but unlike [`Self::set_match_policy`]
    /// this is housekeeping rather than a correctness requirement. Reuse is
    /// gated on the *rewritten* text prefix-extending the rewritten text the
    /// cache was built from, and matching is prefix-closed, so a cached set
    /// stays a valid superset no matter which table produced either string.
    /// Dropping it keeps the cache attributable to the configuration in force,
    /// and costs one cold pass on an event that happens when a file is edited.
    pub fn set_aliases(&mut self, aliases: AliasTable) {
        if self.aliases == aliases {
            return;
        }
        self.aliases = aliases;
        self.candidate_cache = None;
    }

    /// The aliases currently in force.
    #[must_use]
    pub const fn aliases(&self) -> &AliasTable {
        &self.aliases
    }

    /// Records whether a plugin permits its catalog to be persisted (spec 22.1).
    ///
    /// Called when a manifest is registered, which is *after* the persisted
    /// catalog has already been loaded and made searchable - that ordering is
    /// the whole reason this does more than set a flag. A plugin whose
    /// declaration changed since the last run, or that never got to correct a
    /// slice written by an older version of itself, would otherwise have that
    /// slice answering queries for the rest of the session.
    ///
    /// So refusing persistence withdraws the cached slice immediately, both
    /// from disk and from the live catalog - but only when the live slice is
    /// the one that came from the cache. A plugin that has already published
    /// for itself this run keeps what it published; those items are current by
    /// definition, and dropping them would delete a working catalog.
    pub fn set_catalog_persistence(&mut self, plugin: &PluginId, persist: bool) {
        if persist {
            self.volatile.remove(plugin);
            return;
        }
        if !self.volatile.insert(plugin.clone()) {
            return;
        }
        if let Some(cache) = &self.catalog_cache {
            if let Err(error) = cache.0.invalidate(plugin) {
                self.cache_error = Some(error);
            }
        }
        if self.cache_sourced.remove(plugin) {
            self.withdraw_slice(plugin);
        }
    }

    /// Removes a plugin's live catalog slice and everything derived from it.
    fn withdraw_slice(&mut self, plugin: &PluginId) {
        self.catalog.invalidate(plugin);
        self.non_prefix_upper.remove(plugin);
        if let Ok(at) = self.owners.binary_search(plugin) {
            self.owners.remove(at);
        }
        // The visible answer may name an item that no longer exists, and a
        // narrowed candidate set indexes positions that have just moved.
        self.results.clear();
        self.candidate_cache = None;
    }

    /// Sets the clock used for deterministic recency scoring.
    pub fn set_history_time(&mut self, now_secs: u64) {
        self.now_secs = now_secs;
    }

    /// Supplies the foreground category used by context-aware ranking.
    ///
    /// # Where this signal is inert
    ///
    /// The context term is only ever non-neutral where the platform can name
    /// the focused window, and today that is X11 alone. Wayland sessions
    /// withhold window control by design, and neither the Windows nor the
    /// macOS backend implements [`WindowService`] at all, so
    /// [`Self::refresh_foreground_category`] resolves to `None` on all three
    /// and every candidate scores with `context_match` false. Ranking is
    /// therefore *correct* on those platforms and *no better than
    /// context-free* — read a context-aware ranking claim as X11-only until a
    /// backend there grows a focused-window query.
    ///
    /// [`WindowService`]: crikey_platform::WindowService
    pub fn set_foreground_category(&mut self, category: Option<Category>) {
        self.foreground_category = category;
    }

    /// The category context-aware ranking is currently scoring against.
    pub fn foreground_category(&self) -> Option<&Category> {
        self.foreground_category.as_ref()
    }

    /// Reads the foreground window from the platform backend and sets the
    /// context signal from it.
    ///
    /// A no-op in effect on any backend that cannot name the focused window —
    /// see [`Self::set_foreground_category`] for which those are. The read and
    /// the interpretation are separate methods because only the read depends
    /// on the host's desktop: this one is what `crikey run` calls and cannot
    /// be pinned by a test that must pass on a headless builder, while
    /// [`Self::set_foreground_from_window`] is a pure function of a window and
    /// the catalog and is pinned exhaustively.
    pub fn refresh_foreground_category(&mut self) {
        let window = self.app.foreground_window();
        self.set_foreground_from_window(window.as_ref());
    }

    /// Sets the context signal to the category of the catalog item `window`'s
    /// owning program names.
    ///
    /// The catalog is the only thing in the process that knows what a window
    /// belongs to. A window reports the program that owns it (`WM_CLASS` on
    /// X11), and the launcher already holds a catalog of programs, so the two
    /// are matched by name and the *item's* category is the answer. Deriving a
    /// category from the window alone would mean hard-coding a category for
    /// every program in the world, and getting it wrong silently.
    ///
    /// Every unknown resolves to `None`, which switches the context term off
    /// rather than pointing it somewhere: no window at all (a backend that
    /// cannot answer, or an empty desktop), a window whose owner the window
    /// system will not name, and an owner that matches nothing in the catalog.
    /// Guessing [`Category::Application`] because "windows belong to
    /// applications" would promote every application row on every query on a
    /// desktop this launcher understands nothing about.
    pub fn set_foreground_from_window(&mut self, window: Option<&WindowInfo>) {
        // Resolved into a local first: the closure borrows `self` to read the
        // catalog, and that borrow must be released before the field is written.
        let category = window.and_then(|window| self.category_of(window));
        self.foreground_category = category;
    }

    /// The category of the catalog item `window`'s owning program names.
    ///
    /// Case-insensitive equality, and nothing looser: `WM_CLASS` carries a
    /// program name and catalog labels are program names, so an exact match is
    /// available and a substring rule would let "Files" claim "Files
    /// (Nautilus)" and every other row containing the word. Search terms are
    /// consulted alongside the label because a plugin declares them for
    /// exactly this — the aliases its item is also known by.
    fn category_of(&self, window: &WindowInfo) -> Option<Category> {
        let application = window.application.as_ref()?;
        self.owners
            .iter()
            .flat_map(|owner| self.catalog.items(owner))
            .find(|item| {
                std::iter::once(&item.label)
                    .chain(item.search_terms.iter())
                    .any(|name| name.eq_ignore_ascii_case(application))
            })
            .map(|item| item.category.clone())
    }

    /// A lossless copy of the ranking history, for a caller that persists it.
    pub fn selection_history_snapshot(&self) -> SelectionHistorySnapshot {
        self.history.snapshot()
    }

    /// Replaces the ranking history with a previously taken snapshot.
    ///
    /// Replaces rather than merges: the snapshot is the whole history, and a
    /// merge would double every count on a host that restored twice. Callers
    /// restore once, before queries are accepted.
    pub fn restore_selection_history(&mut self, snapshot: SelectionHistorySnapshot) {
        self.history = SelectionHistory::from_snapshot(snapshot);
    }

    /// Clears all selection and query affinity records.
    pub fn clear_selection_history(&mut self) {
        self.history.clear();
    }

    /// Records a successful execution of the currently visible item.
    ///
    /// Selection is recorded only after the caller confirms execution
    /// succeeded; stale or non-visible item ids are ignored.
    pub fn record_selection(&mut self, item_id: &ItemId) -> bool {
        let Some(item) = self
            .results
            .iter()
            .find(|hit| &hit.item.stable_id == item_id)
            .map(|hit| hit.item.clone())
        else {
            return false;
        };
        let Some(query) = self.last_query.as_ref() else {
            return false;
        };
        self.history.record(&item, query, self.now_secs);
        true
    }

    /// Attaches the persistent catalog cache used for completed publications.
    pub fn set_catalog_cache(&mut self, cache: Arc<dyn CatalogCache + Send + Sync>) {
        self.catalog_cache = Some(CatalogCacheHandle(cache));
        self.cache_error = None;
    }

    /// The most recent non-fatal cache write failure, if any.
    pub fn catalog_cache_error(&self) -> Option<&CacheError> {
        self.cache_error.as_ref()
    }

    /// The startup milestone currently awaiting acknowledgement.
    pub fn stage(&self) -> StartupStage {
        self.app.stage()
    }

    /// Acknowledges the expected startup milestone; see [`App::complete_stage`].
    pub fn complete_stage(&mut self, expected: StartupStage) -> Result<Option<StartupStage>, StartupError> {
        self.app.complete_stage(expected)
    }
    /// Discovers application items through the selected platform backend.
    pub fn discover_application_items(&self, plugin: &PluginId) -> CoreResult<Vec<Item>> {
        self.app.discover_application_items(plugin)
    }

    /// The session's clipboard through the selected platform backend; see
    /// [`App::clipboard`].
    pub fn clipboard(&self) -> Option<Box<dyn Clipboard>> {
        self.app.clipboard()
    }

    /// Searches files through the selected platform backend; see
    /// [`App::search_file_items`].
    pub fn search_file_items(
        &self,
        plugin: &PluginId,
        query: &FileSearchQuery,
    ) -> Option<CoreResult<(Vec<Item>, FileSearchResults)>> {
        self.app.search_file_items(plugin, query)
    }

    /// Connects the platform global shortcut to a native UI event-loop wake-up.
    #[cfg(any(windows, target_os = "linux"))]
    pub fn register_activation_hotkey(
        &mut self,
        accelerator: &str,
        handler: HotkeyActivationHandler,
    ) -> CoreResult<()> {
        self.app.register_activation_hotkey(accelerator, handler)
    }

    /// Releases a global shortcut this service registered.
    #[cfg(any(windows, target_os = "linux"))]
    pub fn unregister_activation_hotkey(&mut self, accelerator: &str) -> CoreResult<()> {
        self.app.unregister_activation_hotkey(accelerator)
    }

    /// Admits every readable cached slice into the live catalog and reports
    /// what became searchable and what had to be skipped (spec 22.1, 25.6).
    ///
    /// A cold cache is a miss, not a startup failure: a root that was never
    /// written contributes zero items and still returns `Ok`. Otherwise the
    /// launcher could not start until it had already run once.
    ///
    /// Fault isolation is per slice for the same reason. A filesystem fault
    /// while reading one owner's archive is that owner's miss: the remaining
    /// owners still load and the casualty is counted in
    /// [`CatalogLoad::skipped`]. Only a fault enumerating the cache root is
    /// propagated - it names no owner, so there is no slice to isolate and no
    /// other owner that could be loaded instead.
    ///
    /// Every admitted slice raises its plugin's instance high-water mark to
    /// the instance recorded on disk, and that mark never falls. A worker that
    /// goes on to publish for the same plugin must therefore claim an instance
    /// at least as high as the cached one, or the catalog refuses it as stale
    /// (spec 14.8).
    pub fn load_persisted_catalog(&mut self, cache: &dyn CatalogCache) -> Result<CatalogLoad, CacheError> {
        self.candidate_cache = None;
        self.non_prefix_upper.clear();
        let mut owners = cache.plugins()?;
        owners.sort_unstable();
        owners.dedup();

        let mut load = CatalogLoad::default();
        for plugin in owners {
            let admitted = match cache.load_slice(&plugin) {
                Ok(Some(slice)) => self.admit(slice),
                // An owner the cache still advertises but cannot deliver: a
                // torn, foreign or unreadable archive is the same miss as a
                // fault, and neither may cost the owners not yet visited.
                Ok(None) | Err(_) => None,
            };
            match admitted {
                Some(items) => {
                    load.items = load.items.saturating_add(items);
                    // Provenance, so that a plugin which later declares itself
                    // non-persistable can have exactly this slice withdrawn
                    // without disturbing one it has published live.
                    self.cache_sourced.insert(plugin.clone());
                }
                None => load.skipped = load.skipped.saturating_add(1),
            }
        }
        Ok(load)
    }

    /// Replaces one live owner's catalog slice and returns the retained count.
    ///
    /// The caller supplies the worker instance that owns the publication.
    /// Replacing a slice invalidates incremental-query state before touching the
    /// catalog, and a rejected publication retires the newly activated instance
    /// so failed startup work cannot retain publishing authority.
    pub fn replace_catalog(
        &mut self,
        plugin: &PluginId,
        instance: u64,
        items: Vec<Item>,
    ) -> Result<usize, CatalogError> {
        self.candidate_cache = None;
        self.non_prefix_upper.remove(plugin);
        self.catalog.activate_instance(plugin, instance)?;
        if let Err(error) = self
            .catalog
            .apply(plugin, instance, CatalogUpdate::Replace, items)
        {
            let _ = self.catalog.retire_instance(plugin, instance);
            return Err(error);
        }
        // A successful catalog replacement invalidates the visible answer.
        // Keeping old hits would let a user execute an item no longer in the catalog.
        self.results.clear();
        // And it is the one event that can change what an icon reference means:
        // an install, an upgrade or a removal all arrive as a replaced slice.
        // `icon` answers from this memo without consulting the platform loader,
        // so this is where a reference gets to resolve differently.
        self.icons
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();

        let retained = self.catalog.plugin_len(plugin);
        if let Some(upper) = self
            .catalog
            .items(plugin)
            .iter()
            .map(|item| self.ranker.non_prefix_upper_bound(item))
            .max()
        {
            self.non_prefix_upper.insert(plugin.clone(), upper);
        }
        match (retained, self.owners.binary_search(plugin)) {
            (0, Ok(at)) => {
                self.owners.remove(at);
            }
            (0, Err(_)) | (_, Ok(_)) => {}
            (_, Err(at)) => self.owners.insert(at, plugin.clone()),
        }
        // The plugin has published for itself, so whatever the cache holds for
        // it is superseded and its slice is no longer cache-sourced.
        self.cache_sourced.remove(plugin);
        if let Some(cache) = &self.catalog_cache {
            // A plugin that refuses persistence is not merely skipped: any
            // slice an earlier declaration left on disk is withdrawn here, so
            // the refusal takes effect for the *next* launch and not only for
            // this one.
            if retained > 0 && !self.volatile.contains(plugin) {
                let slice = CachedSlice {
                    plugin: plugin.clone(),
                    instance,
                    generation: Generation::ZERO,
                    items: self.catalog.items(plugin).to_vec(),
                };
                if let Err(error) = cache.0.store_slice(&slice) {
                    self.cache_error = Some(error);
                }
            } else if let Err(error) = cache.0.invalidate(plugin) {
                self.cache_error = Some(error);
            }
        }
        Ok(retained)
    }

    /// Publishes one cached slice, returning the items the catalog retained,
    /// or `None` when the slice never became catalog state.
    ///
    /// A slice the live catalog refuses - a regressed instance number, an item
    /// claiming another owner, a breached catalog limit - is discarded exactly
    /// like an unreadable one. Cached items are a rebuildable artifact, so a
    /// bad slice costs its own plugin its cached state and nothing else
    /// (ADR-0008, spec 22.4).
    ///
    /// Authorizing the instance is the part that would outlive the failure:
    /// [`CatalogStore::activate_instance`] raises a high-water mark that never
    /// falls. A refused slice therefore retires the instance it just
    /// activated. The worker that wrote those bytes belongs to a previous run,
    /// so leaving its number authorized would hand publishing rights to an
    /// instance that published nothing, on the strength of a load that failed.
    /// The mark itself stands either way: a live publisher for this plugin
    /// must claim an instance at least as high as the cached one.
    fn admit(&mut self, slice: CachedSlice) -> Option<usize> {
        let CachedSlice {
            plugin,
            instance,
            items,
            ..
        } = slice;
        self.replace_catalog(&plugin, instance, items).ok()
    }

    /// Accepts a query and allocates the generation that makes its answer the
    /// current one (spec 8.1, 11.6).
    ///
    /// A query refused by startup gating allocates nothing and leaves the
    /// visible results untouched. An accepted query always replaces them, even
    /// when the new answer is empty: an empty answer is still an answer.
    pub fn submit_query(&mut self, raw: &str) -> Result<Generation, SearchError> {
        self.submit_query_at(raw, self.now_secs)
    }

    /// Accepts a query at an explicit clock value for deterministic ranking.
    pub fn submit_query_at(&mut self, raw: &str, now_secs: u64) -> Result<Generation, SearchError> {
        self.now_secs = now_secs;
        if !self.app.can_accept_queries() {
            return Err(SearchError::NotAcceptingQueries {
                pending: self.app.stage(),
            });
        }

        let generation = self.app.generations().advance();
        self.aggregator.begin_generation(generation);

        let query = self.aliases.expand(self.normalizer.normalize(raw));
        self.last_query = Some(query.clone());
        let per_plugin_limit = self
            .app
            .limits()
            .max_items_per_plugin_per_query
            .min(self.app.limits().max_items_per_query);
        let global_limit = self.app.limits().max_items_per_query;
        let batch_limit = self.app.limits().max_items_per_batch;

        let previous = self.candidate_cache.take();
        let incremental = previous.as_ref().filter(|cached| {
            query.normalized.chars().count() > 2
                && !cached.normalized.is_empty()
                && query.normalized.starts_with(&cached.normalized)
        });
        let (selected, stats, matched_positions, cache_complete) = select_best(
            &self.catalog,
            &self.owners,
            SearchPlan {
                matcher: &self.matcher,
                ranker: &self.ranker,
                non_prefix_upper: &self.non_prefix_upper,
                query: &query,
                previous: incremental.map(|cached| &cached.by_owner),
                history: &self.history,
                foreground_category: self.foreground_category.as_ref(),
                now_secs: self.now_secs,
                per_plugin_limit,
                global_limit,
                batch_limit,
            },
        );
        self.candidate_cache = cache_complete.then(|| CandidateCache {
            normalized: query.normalized.clone(),
            by_owner: matched_positions,
        });
        self.last_query_stats = stats;

        let mut outcomes = Outcomes::with_capacity(self.owners.len());
        let mut selected_by_owner: HashMap<PluginId, Vec<Item>> = HashMap::new();
        for candidate in selected {
            let outcome = self
                .matcher
                .match_prepared(&query, candidate.item, candidate.prepared_label)
                .expect("a selected match summary must materialize identically");
            outcomes
                .entry(candidate.item.plugin_id.clone())
                .or_default()
                .insert(candidate.item.stable_id.clone(), outcome);
            selected_by_owner
                .entry(candidate.item.plugin_id.clone())
                .or_default()
                .push(candidate.item.clone());
        }

        for plugin in &self.owners {
            let items = selected_by_owner.remove(plugin).unwrap_or_default();
            publish_selected(&mut self.aggregator, generation, plugin, items, batch_limit);
        }

        let retained = self.aggregator.items();
        let mut hits = Vec::with_capacity(retained.len());
        // One resolution for the whole publication, as in the candidate sweep.
        let affinity = self.history.affinities_for(&query);
        for item in retained {
            let Some(outcome) = outcomes
                .get_mut(&item.plugin_id)
                .and_then(|by_id| by_id.remove(&item.stable_id))
            else {
                continue;
            };
            hits.push(self.rank_hit(item.clone(), outcome, &affinity, false));
        }

        hits.sort_unstable_by(by_rank);

        self.results = hits;
        Ok(generation)
    }

    /// Scores one matched item exactly as the catalog pass does.
    ///
    /// Shared by [`Self::submit_query_at`] and [`Self::merge_query_items`] so
    /// that a merged item and a catalog item cannot be placed by two
    /// independently maintained formulas.
    fn rank_hit(
        &self,
        item: Item,
        outcome: MatchOutcome,
        affinity: &QueryAffinity<'_>,
        ephemeral: bool,
    ) -> SearchHit {
        let mut signals = RankingSignals::default();
        self.history.augment(
            &item,
            affinity,
            self.now_secs,
            self.foreground_category.as_ref(),
            &mut signals,
        );
        let score = self.ranker.score_outcome_with_signals(&item, &outcome, signals);
        SearchHit {
            item,
            score,
            method: outcome.method,
            highlights: outcome.highlights,
            ephemeral,
        }
    }

    /// Ranks `items` into the answer for `generation` alongside the catalog
    /// hits. Returns false when `generation` is not the current one (a stale
    /// batch).
    ///
    /// This is how a provider that answers asynchronously - file search, above
    /// all - gets its items *ranked against* the catalog rather than appended
    /// after it. A file whose name the query prefixes therefore outranks an
    /// application the query merely occurs inside, because both are placed by
    /// the same match method, history and ranker.
    ///
    /// The items are ephemeral: they never reach the catalog, the catalog
    /// cache or the candidate cache, and the next accepted query replaces the
    /// answer that holds them. They do become selectable history, which is the
    /// point - [`Self::record_selection`] resolves ids against the answer, so
    /// before this existed selecting a file recorded nothing at all.
    ///
    /// Calling this twice for one generation and plugin replaces that plugin's
    /// previous batch; other plugins' merged items and the catalog hits stand.
    ///
    /// An item whose stable id already appears in the answer is dropped. Ids
    /// address rows for selection and for history, so a duplicated id would
    /// make both arbitrary, and the incumbent - a catalog item, or an earlier
    /// batch's item - is the one already on screen.
    pub fn merge_query_items(&mut self, generation: Generation, plugin: &PluginId, items: Vec<Item>) -> bool {
        if !self.app.generations().is_current(generation) {
            return false;
        }
        let Some(query) = self.last_query.clone() else {
            return false;
        };
        // Replacing this plugin's batch before matching keeps the collision
        // check from rejecting a re-sent item against its own earlier copy.
        self.results
            .retain(|hit| !(hit.ephemeral && hit.item.plugin_id == *plugin));

        let affinity = self.history.affinities_for(&query);
        let mut merged = Vec::new();
        for item in items {
            let Some(outcome) = self.matcher.match_item(&query, &item) else {
                continue;
            };
            if self
                .results
                .iter()
                .chain(merged.iter())
                .any(|hit: &SearchHit| hit.item.stable_id == item.stable_id)
            {
                continue;
            }
            merged.push(self.rank_hit(item, outcome, &affinity, true));
        }
        self.results.append(&mut merged);
        self.results.sort_unstable_by(by_rank);
        // The catalog pass bounds its answer inside `select_best`; appending to
        // it re-opens that bound, so it is re-applied here. Sorting first is
        // what makes this a truncation of the *worst* rows rather than of
        // whichever provider happened to answer last -- a merged file that
        // outranks a catalog hit displaces it, which is the whole point of
        // ranking the two together.
        self.results.truncate(self.app.limits().max_items_per_query);
        true
    }

    /// The ranked answer to the most recently accepted query.
    pub fn results(&self) -> &[SearchHit] {
        &self.results
    }

    /// Materializes the current ranked answer into the renderer's row model.
    ///
    /// Search hits stay authoritative for ordering and highlights. The first
    /// declared item action is the default; remaining actions are alternates.
    ///
    /// Icons are resolved here, and this is the only place they are: the
    /// renderer must not touch a filesystem, so a row reaches it either with
    /// pixels or without (spec 6.4). Rows are built once per publication rather
    /// than once per frame, every reference is resolved at most once per session,
    /// and the platform's own cache means a reference already seen on this
    /// machine costs a `stat` and a copy rather than a decode (spec 22.1).
    pub fn result_rows(&self) -> Vec<ResultRow> {
        self.results
            .iter()
            .map(|hit| {
                let mut actions = hit.item.actions.iter();
                let default_action = actions.next().cloned();
                let alternate_actions = actions.cloned().collect();
                let argument_hint = match hit.item.argument_policy {
                    ArgumentPolicy::Forbidden => None,
                    ArgumentPolicy::Optional => Some("optional argument".to_owned()),
                    ArgumentPolicy::Required => Some("argument required".to_owned()),
                };
                ResultRow {
                    item: hit.item.stable_id.clone(),
                    label: hit.item.label.clone(),
                    description: hit.item.description.clone(),
                    icon: hit
                        .item
                        .icon_reference
                        .as_deref()
                        .and_then(|reference| self.icon(reference)),
                    icon_reference: hit.item.icon_reference.clone(),
                    category: hit.item.category.as_str().to_owned(),
                    plugin_name: hit.item.plugin_id.0.clone(),
                    highlights: hit.highlights.clone(),
                    argument_hint,
                    status: None,
                    default_action,
                    alternate_actions,
                }
            })
            .collect()
    }

    /// The pixels behind one icon reference, resolved at most once per session.
    ///
    /// This runs on the UI thread for every row of every keystroke, between the
    /// spinner going up and the results going out, so what it costs is what the
    /// user waits. Resolving a reference is not cheap: the platform loader
    /// searches an icon theme chain (or, on Windows, resolves a shortcut and
    /// extracts a resource from an executable) and then decodes an image. A miss
    /// is the most expensive case of all, because it walks the whole chain
    /// before concluding there is nothing. Measured against a real theme that
    /// costs about 3 ms per reference, and the loader keeps only the single most
    /// recent icon, so a list of distinct references never hits it: thirty rows
    /// cost thirty resolutions on every keystroke, and the launcher sits there
    /// saying "Providers are still responding" while it does them.
    ///
    /// So the memo answers first and the loader is consulted only for a
    /// reference this session has not seen. The freshness that costs bought --
    /// noticing that a reference started resolving, or that the file behind it
    /// was replaced in place -- is preserved by dropping the memo whenever a
    /// catalog slice is replaced, which is what a plugin install, upgrade or
    /// removal does. Nothing else can change what a reference means.
    fn icon(&self, reference: &str) -> Option<Arc<IconImage>> {
        let mut resolved = self.icons.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(memo) = resolved.get(reference) {
            return memo.clone();
        }
        if resolved.len() >= MAX_RESOLVED_ICONS {
            resolved.clear();
        }
        let loaded = self
            .app
            .icon_provider()
            .load(reference, DEFAULT_ICON_SIZE)
            .ok()
            .flatten()
            .map(Arc::new);
        resolved.insert(reference.to_owned(), loaded.clone());
        loaded
    }

    pub fn set_plugin_action_router(&mut self, router: Arc<PluginActionRouter>) {
        self.plugin_actions = Some(router);
    }

    pub fn execute(&self, item_id: &ItemId, action_id: &ActionId) -> CoreResult<ActionSubmission> {
        self.execute_with_argument(item_id, action_id, None)
    }

    pub fn execute_with_argument(
        &self,
        item_id: &ItemId,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> CoreResult<ActionSubmission> {
        let mut matching_hits = self.results.iter().filter(|hit| &hit.item.stable_id == item_id);
        let Some(hit) = matching_hits.next() else {
            return self
                .plugin_actions
                .as_ref()
                .ok_or_else(|| {
                    crikey_core::CoreError::Invalid("selected result is no longer current".to_owned())
                })?
                .submit_by_item_id(item_id, action_id, argument)
                .map(ActionSubmission::Pending);
        };
        if matching_hits.next().is_some() {
            return Err(crikey_core::CoreError::Invalid(
                "selected result has ambiguous item ownership".to_owned(),
            ));
        }
        let action = hit
            .item
            .actions
            .iter()
            .find(|action| &action.action_id == action_id)
            .ok_or_else(|| {
                crikey_core::CoreError::Invalid("selected action is no longer available".to_owned())
            })?;
        if !action.applicable_categories.is_empty()
            && !action.applicable_categories.contains(&hit.item.category)
        {
            return Err(crikey_core::CoreError::Invalid(format!(
                "action `{}` is not applicable to item category `{}`",
                action.action_id.0,
                hit.item.category.as_str()
            )));
        }
        match hit.item.argument_policy {
            ArgumentPolicy::Forbidden if argument.is_some() => {
                return Err(crikey_core::CoreError::Invalid(
                    "this item does not accept an argument".to_owned(),
                ))
            }
            ArgumentPolicy::Required if argument.is_none_or(str::is_empty) => {
                return Err(crikey_core::CoreError::Invalid(
                    "this item requires an argument".to_owned(),
                ))
            }
            _ => {}
        }
        match action.execution_policy {
            ExecutionPolicy::HostMediated => self.dispatch_host_mediated(&hit.item, action, argument),
            ExecutionPolicy::Plugin => self
                .plugin_actions
                .as_ref()
                .ok_or_else(|| {
                    crikey_core::CoreError::Invalid("plugin-owned action runtime is unavailable".to_owned())
                })?
                .submit(&hit.item.plugin_id, &hit.item, action_id, argument)
                .map(ActionSubmission::Pending),
        }
    }

    /// The one place the host performs an action on its own behalf.
    ///
    /// Three questions in a fixed order, and the order is the point. *Is this
    /// an action the host implements at all?* -- an unknown id is refused
    /// rather than dispatched somewhere plausible. *May this owner have it
    /// done?* -- per owner, through the one grant map; a build with no action
    /// registry has no plugin runtimes either, so every item it can hold was
    /// produced by the host, while a build that has one refuses an owner it
    /// does not know rather than assuming it is host-owned. That is why the
    /// composition root registers its own builtin catalogs there too. Only
    /// then is anything run.
    fn dispatch_host_mediated(
        &self,
        item: &Item,
        action: &Action,
        argument: Option<&str>,
    ) -> CoreResult<ActionSubmission> {
        if argument.is_some() {
            return Err(crikey_core::CoreError::Invalid(
                "host-mediated actions do not accept an argument".to_owned(),
            ));
        }
        // The capability, not just the operation: launching an application and
        // opening a document are different authority questions even though
        // today's grant map answers both from the same declaration.
        let (capability, run): (_, fn(&App, &Item) -> CoreResult<()>) = match action.action_id.0.as_str() {
            APPLICATION_LAUNCH_ACTION_ID => (HostCapability::ProcessLaunch, App::launch_application),
            FILE_OPEN_ACTION_ID => (HostCapability::DocumentOpen, App::open_file_item),
            FILE_REVEAL_ACTION_ID => (HostCapability::DocumentOpen, App::reveal_file_item),
            unsupported => {
                return Err(crikey_core::CoreError::Invalid(format!(
                    "unsupported host-mediated action {unsupported:?}"
                )))
            }
        };
        if let Some(router) = self.plugin_actions.as_ref() {
            router.authorize(&item.plugin_id, capability)?;
        }
        run(&self.app, item).map(|()| ActionSubmission::Completed)
    }

    pub fn poll_action_completions(&self) -> Vec<PluginActionCompletion> {
        self.plugin_actions
            .as_ref()
            .map_or_else(Vec::new, |router| router.poll())
    }

    pub fn cancel_action(&self, request_id: &ActionRequestId) -> bool {
        self.plugin_actions
            .as_ref()
            .is_some_and(|router| router.cancel(request_id))
    }

    /// Opens a plugin-drawn page and starts asking its plugin for frames
    /// (spec 32.2).
    ///
    /// A plugin with no loaded runtime is refused: nothing would ever answer
    /// a frame request, and an empty surface no one draws into is worse than
    /// a refused action the caller can report.
    ///
    /// A page opened while another is already open replaces it, and the one
    /// being replaced is closed first. A launcher shows one surface at a
    /// time, so the alternatives are worse: refusing would strand whichever
    /// plugin asked second, and replacing silently would leave the first
    /// still believing it owns the screen — still being asked for frames,
    /// still holding whatever the user typed into it.
    pub fn open_page(
        &mut self,
        plugin: &PluginId,
        page_id: &str,
        width: u32,
        height: u32,
        palette: crikey_core::PagePalette,
    ) -> CoreResult<()> {
        // A launcher shows one surface at a time, so the page already open is
        // ended before the new one starts rather than refused (spec 32.2).
        // Refusing would strand whichever plugin asked second, and silently
        // replacing would leave the first believing it still owns the screen;
        // closing tells it, and the transport orders the two so the old page's
        // `Closed` cannot overtake the new page's `Opened`.
        if self.page.is_some() {
            self.close_page();
        }
        let router = self.plugin_actions.as_ref().ok_or_else(|| {
            crikey_core::CoreError::Invalid("plugin page runtime is unavailable".to_owned())
        })?;
        router.open_page(plugin, page_id, width, height, palette)?;
        self.page = Some((plugin.clone(), page_id.to_owned()));
        Ok(())
    }

    /// Hands one host-hit-tested event to the open page's plugin.
    pub fn send_page_input(&mut self, input: crikey_core::PageInput) -> CoreResult<()> {
        let (plugin, _) = self
            .page
            .as_ref()
            .ok_or_else(|| crikey_core::CoreError::Invalid("no plugin page is open".to_owned()))?;
        let router = self.plugin_actions.as_ref().ok_or_else(|| {
            crikey_core::CoreError::Invalid("plugin page runtime is unavailable".to_owned())
        })?;
        router.send_page_input(plugin, input)
    }

    /// Tells the open page its viewport changed.
    pub fn resize_page(&mut self, width: u32, height: u32) {
        let (Some((plugin, _)), Some(router)) = (self.page.as_ref(), self.plugin_actions.as_ref()) else {
            return;
        };
        router.resize_page(plugin, width, height);
    }

    /// Closes the open page, telling its plugin the surface is gone.
    pub fn close_page(&mut self) {
        let Some((plugin, _)) = self.page.take() else {
            return;
        };
        if let Some(router) = self.plugin_actions.as_ref() {
            router.close_page(&plugin);
        }
    }

    /// Takes at most one finished page update, without waiting for a plugin.
    ///
    /// A [`PageUpdate::closed`] update is the last one a page produces, so
    /// the host forgets the owner as it hands that update over: the caller
    /// dismisses the surface and every later page call correctly reports that
    /// nothing is open.
    pub fn poll_page(&mut self) -> Option<PageUpdate> {
        let (plugin, _) = self.page.as_ref()?;
        let update = self.plugin_actions.as_ref()?.poll_page(plugin)?;
        if update.closed {
            self.page = None;
        }
        Some(update)
    }

    /// The plugin and page id of the open page, if there is one.
    pub fn page_owner(&self) -> Option<(PluginId, String)> {
        self.page.clone()
    }

    pub fn last_query_stats(&self) -> SearchStats {
        self.last_query_stats
    }
}
/// Selects the strongest legal result set without cloning every match.
///
/// Each owner is first reduced to its per-plugin quota, then those survivors
/// compete for the generation-wide quota. Both heaps keep the weakest retained
/// candidate at the root, making each replacement $O(\log k)$ while catalog
/// items stay borrowed.
fn select_best<'items>(
    catalog: &'items MemoryCatalog,
    owners: &[PluginId],
    plan: SearchPlan<'_>,
) -> (Vec<RankedCandidate<'items>>, SearchStats, PositionCache, bool) {
    let SearchPlan {
        matcher,
        ranker,
        non_prefix_upper,
        query,
        previous,
        per_plugin_limit,
        global_limit,
        batch_limit,
        ..
    } = plan;
    let mut stats = SearchStats::default();
    let mut global = BinaryHeap::with_capacity(global_limit);
    let mut matched_by_owner = PositionCache::with_capacity(owners.len());
    let mut cache_complete = true;

    // A zero batch limit means the aggregator cannot legally accept an item.
    if per_plugin_limit == 0 || global_limit == 0 || batch_limit == 0 {
        return (Vec::new(), stats, matched_by_owner, cache_complete);
    }
    for plugin in owners {
        let prior = previous.and_then(|by_owner| by_owner.get(plugin));
        let mut selection = PluginSelection::new(plan, prior.map_or(0, |positions| positions.len()));
        // The leading token, when the prefix index can answer it.
        //
        // Two characters, not one, and that is a measured floor rather than an
        // arbitrary one. A single-character prefix match cannot outscore the
        // best possible non-prefix match, so `skip_remaining` below can never
        // fire for it, and admitting one-character tokens here buys a full
        // `starts_with` scan of the catalog that changes no answer and skips no
        // work: measured at 200k items it moved the median one-character
        // keystroke from 44.5 ms to 53.8 ms.
        let prefix_token = query.tokens.first().filter(|token| {
            token.chars().take(2).count() == 2
                && token
                    .chars()
                    .take(2)
                    .all(|character| character.is_ascii_alphanumeric())
        });

        if let Some(token) = prefix_token {
            catalog.visit_label_prefixes(plugin, token, |position, item, prepared_label| {
                selection.consider(position, item, prepared_label);
            });
        }

        let source_is_filtered = prior.is_some();
        let skip_remaining = prefix_token.is_some()
            && selection.retained.len() == per_plugin_limit
            && non_prefix_upper.get(plugin).is_some_and(|upper| {
                selection
                    .retained
                    .peek()
                    .is_some_and(|Reverse(weakest)| *upper < weakest.score)
            });
        if skip_remaining {
            cache_complete = false;
        } else {
            let mut visit_remaining = |position, item, prepared_label: &'items PreparedLabel| {
                if prefix_token.is_some_and(|token| prepared_label.normalized().starts_with(token)) {
                    return;
                }

                if prefix_token.is_some() && selection.retained.len() == per_plugin_limit {
                    let upper = ranker.non_prefix_upper_bound(item);
                    if selection
                        .retained
                        .peek()
                        .is_some_and(|Reverse(weakest)| upper < weakest.score)
                    {
                        selection.record_pruned(position, prepared_label, source_is_filtered);
                        return;
                    }
                }

                selection.consider(position, item, prepared_label);
            };
            if let Some(positions) = prior {
                // The warm filter must use the policy the matcher scores with.
                // Narrowing more strictly than the matcher matches would drop
                // candidates it would have accepted, so a subsequence-enabled
                // search would lose its subsequence-only hits on the second
                // keystroke.
                catalog.visit_prepared_positions_with(
                    plugin,
                    positions,
                    query,
                    matcher.policy(),
                    &mut visit_remaining,
                );
            } else {
                catalog.visit_prepared_candidates(plugin, query, &mut visit_remaining);
            }
        }

        stats.candidates_examined = stats
            .candidates_examined
            .saturating_add(selection.stats.candidates_examined);
        stats.matches_found = stats.matches_found.saturating_add(selection.stats.matches_found);
        matched_by_owner.insert(plugin.clone(), selection.matched_positions);
        for Reverse(candidate) in selection.retained {
            retain_best(&mut global, candidate, global_limit);
        }
    }

    let mut selected: Vec<_> = global.into_iter().map(|Reverse(candidate)| candidate).collect();
    selected.sort_unstable_by(|left, right| right.cmp(left));
    (selected, stats, matched_by_owner, cache_complete)
}

/// Inserts `candidate` when it belongs in the strongest `limit` entries.
fn retain_best<'a>(
    retained: &mut BinaryHeap<Reverse<RankedCandidate<'a>>>,
    candidate: RankedCandidate<'a>,
    limit: usize,
) {
    if retained.len() < limit {
        retained.push(Reverse(candidate));
    } else if retained
        .peek()
        .is_some_and(|Reverse(weakest)| candidate > *weakest)
    {
        retained.pop();
        retained.push(Reverse(candidate));
    }
}

/// Publishes one owner's selected rows in legal batches and always terminates
/// the owner's stream for the generation.
fn publish_selected(
    aggregator: &mut MemoryResultAggregator,
    generation: Generation,
    plugin: &PluginId,
    items: Vec<Item>,
    batch_limit: usize,
) {
    if items.is_empty() || batch_limit == 0 {
        let _ = merge(aggregator, generation, plugin, Vec::new(), BatchState::Final);
        return;
    }

    let chunk_count = items.len().div_ceil(batch_limit);
    let mut remaining = items;
    for chunk_index in 0..chunk_count {
        let take = batch_limit.min(remaining.len());
        let tail = remaining.split_off(take);
        let chunk = std::mem::replace(&mut remaining, tail);
        let state = if chunk_index + 1 == chunk_count {
            BatchState::Final
        } else {
            BatchState::Partial
        };
        if merge(aggregator, generation, plugin, chunk, state).is_err() && state == BatchState::Final {
            let _ = merge(aggregator, generation, plugin, Vec::new(), BatchState::Final);
        }
    }
}

/// Merges one generation-tagged batch, reporting whether it was refused.
///
/// Every rejection reason is a normal operating condition - a safety limit of
/// spec 11.7, a stream already ended - and costs its own batch only, so the
/// sweep keeps going. The caller still has to look: a refused *closing* batch
/// takes the owner's terminal state down with its items.
fn merge(
    aggregator: &mut MemoryResultAggregator,
    generation: Generation,
    plugin: &PluginId,
    items: Vec<Item>,
    state: BatchState,
) -> Result<(), RejectReason> {
    aggregator.accept(ResultBatch {
        generation,
        plugin: plugin.clone(),
        state,
        items,
    })
}

/// One file search a caller can own, so the search runs somewhere other than
/// the thread that asked for it.
///
/// [`App::search_file_items`] is the whole implementation; this trait exists
/// because the launcher's file provider runs its searches on a worker thread
/// and a backend is not required to be `Send`. The provider therefore builds
/// its own implementation *on* that thread and keeps it there — only the query
/// and the resulting items cross the boundary, and both already are `Send`.
/// A test substitutes a recording or scripted implementation through the same
/// seam without going near a filesystem.
pub trait FileItemSearch {
    /// Files and folders matching `query`, as items owned by `plugin`.
    ///
    /// `None` carries exactly [`App::search_file_items`]'s meaning: this
    /// session has no file search at all, which is not the same answer as a
    /// search that ran and matched nothing.
    fn search_file_items(
        &self,
        plugin: &PluginId,
        query: &FileSearchQuery,
    ) -> Option<CoreResult<(Vec<Item>, FileSearchResults)>>;
}

impl FileItemSearch for App {
    fn search_file_items(
        &self,
        plugin: &PluginId,
        query: &FileSearchQuery,
    ) -> Option<CoreResult<(Vec<Item>, FileSearchResults)>> {
        // Named rather than called through `self`: the inherent method of the
        // same name is what this forwards to, and spelling it out is what
        // keeps the forward from silently becoming a recursion.
        App::search_file_items(self, plugin, query)
    }
}

/// The platform backend selected for this target.
///
/// Per ADR-0001 this is the only place in the workspace that names a backend
/// crate. Resolving the alias is what makes the `cfg` target dependencies
/// load-bearing: a mis-gated or renamed backend fails the build here rather
/// than silently falling back.
#[cfg(windows)]
pub type Backend = crikey_platform_windows::WindowsBackend;
#[cfg(target_os = "macos")]
pub type Backend = crikey_platform_macos::MacOsBackend;
#[cfg(target_os = "linux")]
pub type Backend = crikey_platform_linux::LinuxBackend;

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
compile_error!(
    "CriKey has no platform backend for this target; \
     implement one behind the crikey-platform traits (spec 18)"
);

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, sync::Mutex};

    use crikey_catalog::{CatalogCache, CatalogError, FileCatalogCache};
    use crikey_core::{Action, ActionId, ArgumentPolicy, Category, ExecutionPolicy, HitPolicy};

    use super::*;

    const OWNER: &str = "dev.crikey.app-tests";
    const OTHER: &str = "dev.crikey.app-tests.other";

    #[derive(Debug, Default)]
    struct RecordingCache {
        writes: Mutex<Vec<CachedSlice>>,
        invalidated: Mutex<Vec<PluginId>>,
    }

    impl CatalogCache for RecordingCache {
        fn load_slice(&self, _plugin: &PluginId) -> Result<Option<CachedSlice>, CacheError> {
            Ok(None)
        }

        fn store_slice(&self, slice: &CachedSlice) -> Result<(), CacheError> {
            self.writes
                .lock()
                .expect("recording cache lock")
                .push(slice.clone());
            Ok(())
        }

        fn invalidate(&self, plugin: &PluginId) -> Result<(), CacheError> {
            self.invalidated
                .lock()
                .expect("recording cache lock")
                .push(plugin.clone());
            Ok(())
        }

        fn plugins(&self) -> Result<Vec<PluginId>, CacheError> {
            Ok(Vec::new())
        }
    }
    fn plugin(id: &str) -> PluginId {
        PluginId(id.to_owned())
    }

    /// An item owned by `owner` whose label answers the query `fire`.
    fn burning(id: &str, owner: &str) -> Item {
        Item {
            stable_id: ItemId(id.to_owned()),
            plugin_id: plugin(owner),
            category: Category::Application,
            label: format!("Fire {id}"),
            description: String::new(),
            target: format!("app://{id}"),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: HitPolicy::Recorded,
            score_hint: 0,
            metadata: BTreeMap::new(),
            actions: Vec::new(),
        }
    }

    fn slice(owner: &str, instance: u64, items: Vec<Item>) -> CachedSlice {
        CachedSlice {
            plugin: plugin(owner),
            instance,
            generation: Generation::ZERO,
            items,
        }
    }

    /// A service accepting queries under `limits`.
    ///
    /// The limits are set before the service is built: the aggregator is
    /// handed a copy of them at construction.
    fn accepting(limits: ResultLimits) -> SearchService {
        let mut app = App::new();
        app.limits = limits;

        let mut service = SearchService::new(app);
        for stage in [
            StartupStage::WindowAndHotkey,
            StartupStage::PersistedCatalog,
            StartupStage::AcceptQueries,
        ] {
            service
                .complete_stage(stage)
                .expect("acknowledge a startup milestone");
        }
        assert_eq!(service.stage(), StartupStage::RequiredWorkers);
        service
    }

    #[test]
    fn the_selected_backend_identifies_itself() {
        let _backend = Backend::new();
        assert!(
            matches!(App::platform_backend_name(), "windows" | "macos" | "linux"),
            "backend NAME must be a known platform id, got {:?}",
            App::platform_backend_name()
        );
    }
    #[test]
    fn duplicate_item_ids_are_refused_for_action_execution() {
        let mut first = burning("same", OWNER);
        first.actions = vec![Action {
            action_id: ActionId("run".to_owned()),
            label: "Run".to_owned(),
            description: String::new(),
            applicable_categories: vec![Category::Application],
            icon_reference: None,
            execution_policy: ExecutionPolicy::Plugin,
        }];
        let mut second = burning("same", OTHER);
        second.actions = first.actions.clone();

        let mut service = accepting(ResultLimits::default());
        assert_eq!(service.replace_catalog(&plugin(OWNER), 1, vec![first]), Ok(1));
        assert_eq!(service.replace_catalog(&plugin(OTHER), 1, vec![second]), Ok(1));
        service.submit_query("fire").expect("query is accepted");
        assert_eq!(service.results().len(), 2);

        let error = service
            .execute(&ItemId("same".to_owned()), &ActionId("run".to_owned()))
            .expect_err("an ambiguous item id must not select an arbitrary owner");
        assert!(error.to_string().contains("ambiguous item ownership"));
    }

    #[test]
    fn a_quota_refused_closing_batch_still_ends_the_owners_stream() {
        // Three matches, batches of two, and room for two retained items per
        // plugin: the first batch spends the whole quota, so the closing batch
        // is refused whole (spec 11.7) - its items and its terminal state
        // together.
        let mut service = accepting(ResultLimits {
            max_items_per_batch: 2,
            max_items_per_plugin_per_query: 2,
            ..ResultLimits::default()
        });
        let owner = plugin(OWNER);
        let items = vec![burning("a", OWNER), burning("b", OWNER), burning("c", OWNER)];
        assert_eq!(
            service.admit(slice(OWNER, 1, items)),
            Some(3),
            "the catalog retains every item of an accepted slice"
        );

        service.submit_query("fire").expect("queries are legal");

        assert_eq!(
            service.aggregator.plugin_state(&owner),
            Some(BatchState::Final),
            "a closing batch refused by a quota must still end the stream for this generation"
        );

        let mut found: Vec<&str> = service
            .results()
            .iter()
            .map(|hit| hit.item.stable_id.0.as_str())
            .collect();
        found.sort_unstable();
        assert_eq!(
            found,
            ["a", "b"],
            "the refused batch costs its own items only; what the quota already bought stands"
        );
    }

    #[test]
    fn a_refused_cached_slice_leaves_no_authorized_publisher() {
        let mut service = SearchService::new(App::new());
        let owner = plugin(OWNER);

        // Readable, and refused by the live catalog: the item it carries
        // claims a different owner. The instance was authorized to get here.
        let refused = slice(OWNER, 7, vec![burning("impostor", OTHER)]);
        assert_eq!(
            service.admit(refused),
            None,
            "a slice whose items claim another owner is refused"
        );
        assert_eq!(
            service.catalog.plugin_len(&owner),
            0,
            "a refused slice retains nothing"
        );

        assert_eq!(
            service
                .catalog
                .apply(&owner, 7, CatalogUpdate::Replace, vec![burning("a", OWNER)]),
            Err(CatalogError::StaleInstance),
            "an instance whose slice failed to apply must hold no publishing rights"
        );

        // Nor is the live publisher starved. The high-water mark rose to the
        // cached instance, and claiming it is all it takes to publish.
        assert_eq!(service.catalog.activate_instance(&owner, 7), Ok(()));
        assert_eq!(
            service
                .catalog
                .apply(&owner, 7, CatalogUpdate::Replace, vec![burning("a", OWNER)]),
            Ok(()),
            "a publisher that claims the instance may publish over a refused slice"
        );
        assert_eq!(service.catalog.plugin_len(&owner), 1);
    }
    #[test]
    fn cache_invalidates_an_empty_replacement() {
        let cache = Arc::new(RecordingCache::default());
        let mut service = accepting(ResultLimits::default());
        service.set_catalog_cache(cache.clone());

        let owner = plugin(OWNER);
        assert_eq!(
            service.replace_catalog(&owner, 1, vec![burning("a", OWNER)]),
            Ok(1)
        );
        assert_eq!(cache.writes.lock().expect("recording cache lock").len(), 1);
        assert_eq!(
            cache.writes.lock().expect("recording cache lock")[0].items.len(),
            1
        );

        assert_eq!(service.replace_catalog(&owner, 2, Vec::new()), Ok(0));
        assert_eq!(
            cache.invalidated.lock().expect("recording cache lock").as_slice(),
            [owner],
            "an empty replacement must delete the previous cached slice"
        );
    }
    #[test]
    fn file_cache_round_trips_a_published_catalog_between_services() {
        let root = std::env::temp_dir().join(format!("crikey-app-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let cache = Arc::new(FileCatalogCache::new(root.clone()));
        let owner = plugin(OWNER);

        let mut writer = accepting(ResultLimits::default());
        writer.set_catalog_cache(cache.clone());
        assert_eq!(
            writer.replace_catalog(&owner, 1, vec![burning("a", OWNER)]),
            Ok(1)
        );

        let mut reader = accepting(ResultLimits::default());
        reader.set_catalog_cache(cache.clone());
        let loaded = reader
            .load_persisted_catalog(cache.as_ref())
            .expect("the next service can load the completed slice");
        assert_eq!(loaded.items, 1);
        assert_eq!(loaded.skipped, 0);
        assert_eq!(reader.catalog.plugin_len(&owner), 1);
        assert_eq!(writer.replace_catalog(&owner, 2, Vec::new()), Ok(0));
        let mut empty_reader = accepting(ResultLimits::default());
        let loaded = empty_reader
            .load_persisted_catalog(cache.as_ref())
            .expect("an empty replacement leaves no stale slice to reload");
        assert_eq!(loaded.items, 0);
        assert_eq!(loaded.skipped, 0);
        assert_eq!(empty_reader.catalog.plugin_len(&owner), 0);

        fs::remove_dir_all(root).expect("remove the round-trip cache");
    }
}

/// Page ownership at the service seam: which plugin the host believes is
/// drawing, and when it stops believing it.
#[cfg(test)]
mod page_ownership {
    use std::sync::{Arc, Mutex};

    use crikey_core::{PageFrame, PageInput, PageInputKind};

    use super::*;

    const OWNER: &str = "dev.crikey.page-tests";

    /// A runtime that records what the service asked it to do, with no plugin
    /// behind it: what is under test here is the service's own bookkeeping.
    #[derive(Debug, Default)]
    struct PageStub {
        opened: Mutex<Vec<(PluginId, String)>>,
        inputs: Mutex<Vec<PageInputKind>>,
        closes: Mutex<u32>,
        next: Mutex<Option<PageUpdate>>,
    }

    impl PluginActionExecutor for PageStub {
        fn submit_plugin_action(
            &self,
            _plugin: &PluginId,
            _item: &Item,
            _action_id: &ActionId,
            _argument: Option<&str>,
        ) -> CoreResult<ActionRequestId> {
            Err(crikey_core::CoreError::Invalid("not under test".to_owned()))
        }

        fn open_plugin_page(
            &self,
            plugin: &PluginId,
            page_id: &str,
            _width: u32,
            _height: u32,
            _palette: crikey_core::PagePalette,
        ) -> CoreResult<()> {
            self.opened
                .lock()
                .expect("stub state")
                .push((plugin.clone(), page_id.to_owned()));
            Ok(())
        }

        fn send_plugin_page_input(&self, input: PageInput) -> CoreResult<()> {
            self.inputs.lock().expect("stub state").push(input.kind);
            Ok(())
        }

        fn close_plugin_page(&self) {
            *self.closes.lock().expect("stub state") += 1;
        }

        fn poll_plugin_page(&self) -> Option<PageUpdate> {
            self.next.lock().expect("stub state").take()
        }
    }

    fn service(stub: &Arc<PageStub>) -> SearchService {
        let mut router = PluginActionRouter::default();
        router
            .register(
                [PluginId(OWNER.to_owned())],
                Arc::clone(stub) as Arc<dyn PluginActionExecutor>,
            )
            .expect("the stub registers once");
        let mut service = SearchService::new(App::new());
        service.set_plugin_action_router(Arc::new(router));
        service
    }

    /// A launcher shows one surface at a time, and the replaced plugin has to
    /// be told (spec 32.2). Asserting the close reached the runtime is what
    /// separates this from silently dropping the first page's session.
    #[test]
    fn opening_a_page_closes_the_one_already_open() {
        let stub = Arc::new(PageStub::default());
        let mut service = service(&stub);
        let owner = PluginId(OWNER.to_owned());
        service
            .open_page(&owner, "first", 800, 600, crikey_core::PagePalette::default())
            .expect("the first page opens");
        service
            .open_page(&owner, "second", 800, 600, crikey_core::PagePalette::default())
            .expect("the second page opens over the first");
        assert_eq!(
            *stub.closes.lock().expect("stub state"),
            1,
            "the replaced page was closed rather than abandoned"
        );
        assert_eq!(
            stub.opened
                .lock()
                .expect("stub state")
                .iter()
                .map(|(_, page)| page.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"],
            "both pages reached the runtime, in order"
        );
        assert_eq!(
            service.page_owner(),
            Some((owner, "second".to_owned())),
            "the newest page owns the screen"
        );
    }

    #[test]
    fn a_plugin_with_no_runtime_cannot_open_a_page() {
        let stub = Arc::new(PageStub::default());
        let mut service = service(&stub);
        service
            .open_page(
                &PluginId("dev.crikey.absent".to_owned()),
                "page",
                800,
                600,
                crikey_core::PagePalette::default(),
            )
            .expect_err("a plugin that is not loaded has nothing to draw a page");
        assert_eq!(service.page_owner(), None);
        assert!(stub.opened.lock().expect("stub state").is_empty());
    }

    #[test]
    fn a_closing_update_releases_the_page_owner() {
        let stub = Arc::new(PageStub::default());
        let mut service = service(&stub);
        let owner = PluginId(OWNER.to_owned());
        service
            .open_page(&owner, "page", 800, 600, crikey_core::PagePalette::default())
            .expect("the page opens");
        service
            .send_page_input(PageInput::new(PageInputKind::KeyPressed))
            .expect("input reaches the open page");
        assert_eq!(
            stub.inputs.lock().expect("stub state").as_slice(),
            [PageInputKind::KeyPressed]
        );

        *stub.next.lock().expect("stub state") = Some(PageUpdate {
            frame: PageFrame {
                generation: 4,
                ..PageFrame::default()
            },
            closed: true,
        });
        let update = service.poll_page().expect("the closing update is delivered");
        assert!(update.closed);
        assert_eq!(
            update.frame.generation, 4,
            "the last drawable frame comes with the close"
        );
        assert_eq!(service.page_owner(), None);
        service
            .send_page_input(PageInput::new(PageInputKind::KeyPressed))
            .expect_err("input after the page closed has nowhere to go");
        // A page the host already saw close needs no second close call.
        service.close_page();
        assert_eq!(*stub.closes.lock().expect("stub state"), 0);
    }
}
