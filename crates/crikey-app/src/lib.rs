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

mod legacy_provider;
mod modern_provider;
mod native_provider;
mod query_pipeline;
mod startup_recovery;

pub use crikey_result_aggregator::{
    BatchPriority, BatchState, DrainBudget, DrainReport, InboundBatch, IntakePolicy, MergedBatch,
    OverflowPolicy, ProducerState, QueueDepth, QueueDiagnostics, QueueEvent, QueueEventKind, QueueLimits,
    QueueReject, RejectReason, ResultBatch, ResultLimits,
};
pub use legacy_provider::{LegacyDriver, LegacyProvider, LegacyUnavailable, LegacyWorkerPool};
pub use modern_provider::{ModernDriver, ModernProvider, ModernUnavailable};
pub use native_provider::{NativeDriver, NativeProvider, NativeUnavailable};
pub use query_pipeline::{PipelineConfig, PipelineError, PipelineTick, QueryPipeline};
pub use startup_recovery::{admitted_plugin_roots, StartupJournal, StartupMode, SAFE_MODE_AFTER_FAILURES};

use std::{
    cmp::{Ordering, Reverse},
    collections::{BinaryHeap, HashMap},
};

use crikey_catalog::{
    CacheError, CachedSlice, CatalogCache, CatalogError, CatalogStore, CatalogUpdate, MemoryCatalog,
};
use crikey_core::{
    ActionId, ArgumentPolicy, ExecutionPolicy, Generation, GenerationTracker, Item, ItemId, PluginId,
    Result as CoreResult,
};
use crikey_input_scheduler::SchedulingProfile;
use crikey_platform::{
    application_arguments, application_items, decode_target, APPLICATION_LAUNCH_ACTION_ID,
};
#[cfg(any(windows, target_os = "linux"))]
use crikey_platform::{HotkeyActivationHandler, HotkeyBinding};
use crikey_query::{
    DefaultMatcher, DefaultNormalizer, MatchMethod, MatchOutcome, NormalizedQuery, Normalizer, PreparedLabel,
};
use crikey_ranking::{DefaultRanker, Ranker, Score};
use crikey_result_aggregator::{MemoryResultAggregator, ResultAggregator};
use crikey_ui::ResultRow;

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
    default_legacy_profile: SchedulingProfile,
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
            default_legacy_profile: SchedulingProfile::LegacyStrict,
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

    pub fn generations(&self) -> &GenerationTracker {
        &self.generations
    }

    pub fn limits(&self) -> &ResultLimits {
        &self.limits
    }

    pub fn default_legacy_profile(&self) -> SchedulingProfile {
        self.default_legacy_profile
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

    /// Discovers the current platform's applications and maps them into one
    /// catalog slice owned by `plugin`.
    pub fn discover_application_items(&self, plugin: &PluginId) -> CoreResult<Vec<Item>> {
        let discovered = self.backend.application_discovery().discover()?;
        Ok(application_items(plugin, &discovered))
    }

    /// Registers the activation shortcut and connects it to the native UI
    /// loop's wake-up callback.
    ///
    /// Compiled for the targets whose backend has a real global-shortcut
    /// implementation behind [`Capability::GlobalHotkeys`]: Win32
    /// `RegisterHotKey` and X11 `GrabKey`. A target without one has no method
    /// here at all, so a host that calls it fails to build rather than being
    /// handed a shortcut nothing can deliver.
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

    fn launch_application(&self, item: &Item) -> CoreResult<()> {
        let target = decode_target(&item.target).map_err(|error| {
            crikey_core::CoreError::Invalid(format!("invalid application target: {error}"))
        })?;
        let arguments = application_arguments(item)?;
        self.backend.process_launcher().launch(&target, &arguments)
    }

    /// Name of the platform backend compiled into this build.
    pub fn platform_backend_name() -> &'static str {
        Backend::NAME
    }
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

#[derive(Debug, Clone, Copy)]
struct SearchPlan<'a> {
    matcher: &'a DefaultMatcher,
    ranker: &'a DefaultRanker,
    non_prefix_upper: &'a HashMap<PluginId, Score>,
    query: &'a NormalizedQuery,
    previous: Option<&'a PositionCache>,
    per_plugin_limit: usize,
    global_limit: usize,
    batch_limit: usize,
}

#[derive(Debug)]
struct PluginSelection<'items, 'query> {
    matcher: &'query DefaultMatcher,
    ranker: &'query DefaultRanker,
    query: &'query NormalizedQuery,
    match_spans: Vec<(usize, usize)>,
    matched_positions: Vec<usize>,
    stats: SearchStats,
    retained: BinaryHeap<Reverse<RankedCandidate<'items>>>,
    limit: usize,
}

impl<'items, 'query> PluginSelection<'items, 'query> {
    fn new(
        matcher: &'query DefaultMatcher,
        ranker: &'query DefaultRanker,
        query: &'query NormalizedQuery,
        limit: usize,
        candidate_capacity: usize,
    ) -> Self {
        Self {
            matcher,
            ranker,
            query,
            match_spans: Vec::new(),
            matched_positions: Vec::with_capacity(candidate_capacity),
            stats: SearchStats::default(),
            retained: BinaryHeap::with_capacity(limit),
            limit,
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
        retain_best(
            &mut self.retained,
            RankedCandidate {
                score: self.ranker.score_match(item, summary),
                item,
                prepared_label,
            },
            self.limit,
        );
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
    matcher: DefaultMatcher,
    ranker: DefaultRanker,
    results: Vec<SearchHit>,
    last_query_stats: SearchStats,
    candidate_cache: Option<CandidateCache>,
    non_prefix_upper: HashMap<PluginId, Score>,
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
            matcher: DefaultMatcher::default(),
            ranker: DefaultRanker::default(),
            results: Vec::new(),
            last_query_stats: SearchStats::default(),
            candidate_cache: None,
            non_prefix_upper: HashMap::new(),
        }
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

    /// Connects the platform global shortcut to a native UI event-loop wake-up.
    #[cfg(any(windows, target_os = "linux"))]
    pub fn register_activation_hotkey(
        &mut self,
        accelerator: &str,
        handler: HotkeyActivationHandler,
    ) -> CoreResult<()> {
        self.app.register_activation_hotkey(accelerator, handler)
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
                Some(items) => load.items = load.items.saturating_add(items),
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
        if !self.app.can_accept_queries() {
            return Err(SearchError::NotAcceptingQueries {
                pending: self.app.stage(),
            });
        }

        let generation = self.app.generations().advance();
        self.aggregator.begin_generation(generation);

        let query = self.normalizer.normalize(raw);
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
        for item in retained {
            let Some(outcome) = outcomes
                .get_mut(&item.plugin_id)
                .and_then(|by_id| by_id.remove(&item.stable_id))
            else {
                continue;
            };
            let score = self.ranker.score(&query, item, &outcome);
            hits.push(SearchHit {
                item: item.clone(),
                score,
                method: outcome.method,
                highlights: outcome.highlights,
            });
        }

        hits.sort_unstable_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.item.stable_id.cmp(&right.item.stable_id))
                .then_with(|| left.item.plugin_id.cmp(&right.item.plugin_id))
        });

        self.results = hits;
        Ok(generation)
    }

    /// The ranked answer to the most recently accepted query.
    pub fn results(&self) -> &[SearchHit] {
        &self.results
    }

    /// Materializes the current ranked answer into the renderer's row model.
    ///
    /// Search hits stay authoritative for ordering and highlights. The first
    /// declared item action is the default; remaining actions are alternates.
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

    /// Executes an action from the currently presented result set.
    ///
    /// Host-mediated application launches are routed through the selected
    /// platform backend. Plugin-owned actions remain outside the M1 runtime and
    /// fail explicitly instead of becoming a silent Enter key.
    pub fn execute(&self, item_id: &ItemId, action_id: &ActionId) -> CoreResult<()> {
        let hit = self
            .results
            .iter()
            .find(|hit| &hit.item.stable_id == item_id)
            .ok_or_else(|| {
                crikey_core::CoreError::Invalid("selected result is no longer current".to_owned())
            })?;
        let action = hit
            .item
            .actions
            .iter()
            .find(|action| &action.action_id == action_id)
            .ok_or_else(|| {
                crikey_core::CoreError::Invalid("selected action is no longer available".to_owned())
            })?;
        if action.execution_policy != ExecutionPolicy::HostMediated {
            return Err(crikey_core::CoreError::Invalid(
                "plugin-owned action execution is not wired in the M1 runtime".to_owned(),
            ));
        }
        if action.action_id.0 != APPLICATION_LAUNCH_ACTION_ID {
            return Err(crikey_core::CoreError::Invalid(format!(
                "unsupported host-mediated action {:?}",
                action.action_id.0
            )));
        }
        self.app.launch_application(&hit.item)
    }

    /// Work performed by the most recently accepted query.
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
        let mut selection = PluginSelection::new(
            matcher,
            ranker,
            query,
            per_plugin_limit,
            prior.map_or(0, |positions| positions.len()),
        );
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
                        selection.stats.candidates_examined =
                            selection.stats.candidates_examined.saturating_add(1);
                        if source_is_filtered || prepared_label.may_match(query) {
                            selection.matched_positions.push(position);
                        }
                        return;
                    }
                }

                selection.consider(position, item, prepared_label);
            };
            if let Some(positions) = prior {
                catalog.visit_prepared_positions(plugin, positions, query, &mut visit_remaining);
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
    use std::collections::BTreeMap;

    use crikey_catalog::CatalogError;
    use crikey_core::{ArgumentPolicy, Category, HitPolicy};

    use super::*;

    const OWNER: &str = "dev.crikey.app-tests";
    const OTHER: &str = "dev.crikey.app-tests.other";

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
    fn unchanged_legacy_plugins_default_to_legacy_strict() {
        assert_eq!(
            App::new().default_legacy_profile(),
            SchedulingProfile::LegacyStrict
        );
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
}
