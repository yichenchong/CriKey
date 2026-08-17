use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use crikey_core::{ArgumentPolicy, Generation, Item, PluginId};
use crikey_input_scheduler::{
    ActivationPolicy, BatchAdmission, BatchCompletion, CancelledRequest, CompletionOutcome, DebouncePolicy,
    DispatchedRequest, Millis, PluginDiagnostics, PluginPolicy, QueryScheduler, QueryTraceEvent,
    SchedulerConfig, SchedulerDiagnostics, SchedulingProfile,
};
use crikey_plugin_model::{ConcurrencySection, Manifest, Runtime};
use crikey_plugin_supervisor::{
    shared_budget_from_section, BudgetKind, CircuitBreakerConfig, ConcurrencyRefusals, MemorySupervisor,
    OwnedBudgetGuard, PluginBudgetHandle, PluginHealth, Supervisor,
};
use crikey_result_aggregator::{
    BatchPriority, BatchState, DrainBudget, DrainReport, InboundBatch, InboundResultQueue, IntakePolicy,
    MemoryResultAggregator, MergedBatch, OverflowPolicy, ProducerState, QueueDepth, QueueDiagnostics,
    QueueEvent, QueueLimits, QueueReject, RejectReason, ResultBatch, ResultLimits,
};
use crikey_ui::{ResultRow, ViewModel};

use crate::plugin_icons::PluginIconResolver;

/// Bounds and fairness policy for one composed query pipeline.
#[derive(Debug, Clone, Copy)]
pub struct PipelineConfig {
    pub scheduler: SchedulerConfig,
    pub limits: ResultLimits,
    pub intake_limits: QueueLimits,
    pub default_intake_policy: IntakePolicy,
    pub drain_budget: DrainBudget,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        let scheduler = SchedulerConfig::default();
        let limits = ResultLimits::default();
        Self {
            scheduler,
            limits,
            intake_limits: QueueLimits {
                capacity_batches: scheduler.result_queue_capacity,
                capacity_items: limits.max_items_per_query,
            },
            default_intake_policy: IntakePolicy {
                capacity_batches: scheduler.result_queue_capacity,
                capacity_items: limits.max_items_per_plugin_per_query,
                pause_at_batches: scheduler.result_queue_capacity,
                resume_at_batches: scheduler.result_queue_capacity / 2,
                overflow: OverflowPolicy::PauseProducer,
            },
            drain_budget: DrainBudget {
                batches_per_plugin: 1,
                items_per_plugin: limits.max_items_per_batch,
                total_batches: scheduler.dispatch_budget_per_tick,
            },
        }
    }
}

/// Why work submitted to a query pipeline could not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    AlreadyRegistered {
        plugin: PluginId,
    },
    QueueRejected {
        plugin: PluginId,
        generation: Generation,
        reason: QueueReject,
    },
    AggregatorRejected {
        plugin: PluginId,
        generation: Generation,
        reason: RejectReason,
    },
    /// The manifest names a runtime for which this build has no host.
    UnsupportedRuntime {
        plugin: PluginId,
        runtime: Runtime,
    },
    /// One `[concurrency]` budget handle was offered for a second plugin.
    ///
    /// The §13.5 watermark in [`HealthSync`] is per plugin while the refusal
    /// counters live on the handle, so a handle shared by two ids has every
    /// refusal reconciled independently into both health records and reported
    /// twice. Registration refuses the sharing rather than letting the
    /// double-attribution happen.
    BudgetAlreadyOwned {
        plugin: PluginId,
        owner: PluginId,
    },
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRegistered { plugin } => {
                write!(formatter, "plugin `{}` is already registered", plugin.0)
            }
            Self::QueueRejected {
                plugin,
                generation,
                reason,
            } => write!(
                formatter,
                "result batch from plugin `{}` for generation {generation} was rejected: {reason:?}",
                plugin.0
            ),
            Self::AggregatorRejected {
                plugin,
                generation,
                reason,
            } => write!(
                formatter,
                "result batch from plugin `{}` for generation {generation} was rejected by the aggregator: {reason:?}",
                plugin.0
            ),
            Self::UnsupportedRuntime { plugin, runtime } => write!(
                formatter,
                "plugin `{}` declares unsupported runtime `{}`; this build deliberately refuses it because no host is available",
                plugin.0,
                runtime_name(*runtime)
            ),
            Self::BudgetAlreadyOwned { plugin, owner } => write!(
                formatter,
                "plugin `{}` was offered the concurrency budget already owned by plugin `{}`; one \
                 handle per plugin, or a refusal on it would be attributed to both",
                plugin.0, owner.0
            ),
        }
    }
}

impl std::error::Error for PipelineError {}

/// Work and diagnostics produced by one pipeline tick.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipelineTick {
    pub dispatches: Vec<DispatchedRequest>,
    pub cancellations: Vec<CancelledRequest>,
    pub drain_report: DrainReport,
    pub errors: Vec<PipelineError>,
}

#[derive(Debug, Clone, Copy, Default)]
struct HealthSync {
    stale_results: u64,
    obsolete_requests: u64,
    /// Per-kind refusal totals already pushed into the supervisor. The budget
    /// counters are cumulative and are incremented on whichever thread was
    /// refused, so reconciliation is by delta against this watermark rather
    /// than by the pipeline counting its own refusals: an action refused on
    /// the UI thread or a background task refused inside a Python worker
    /// would otherwise never reach `PluginHealth`.
    refusals: ConcurrencyRefusals,
}

/// Deterministic composition of query scheduling, bounded result intake,
/// aggregation and plugin supervision.
#[derive(Debug)]
pub struct QueryPipeline {
    scheduler: QueryScheduler,
    intake: InboundResultQueue,
    aggregator: MemoryResultAggregator,
    supervisor: MemorySupervisor,
    default_intake_policy: IntakePolicy,
    drain_budget: DrainBudget,
    last_intake_drain_at: Option<Millis>,
    result_trace_capacity: usize,
    /// The largest batch the aggregator will accept, kept so a caller can size
    /// its deliveries. A producer that submits more than this has its whole
    /// batch refused rather than truncated, so it has to know the ceiling.
    max_items_per_batch: usize,
    /// Items one plugin may contribute to one query in total.
    ///
    /// Retained for the same reason as the batch ceiling, and it is a different
    /// ceiling: a producer that splits correctly into legal batches still has
    /// its *next* batch refused whole once the running total would cross this,
    /// and a refused batch becomes a pipeline error rather than a truncation.
    /// A per-query producer therefore has to stop at this number, not merely
    /// batch beneath the other one.
    max_items_per_plugin_per_query: usize,
    /// Items every plugin together may contribute to one query.
    ///
    /// Lowered on its own by the launcher's `max-results` setting, so it can be
    /// smaller than the per-plugin quota above.
    max_items_per_query: usize,
    unranked_batches: usize,
    registered: Vec<PluginId>,
    health_sync: HashMap<PluginId, HealthSync>,
    /// Operator-selected scheduling profiles, applied before provider
    /// registration. Overrides are intentionally one-shot configuration state:
    /// changing one after registration cannot rewrite admitted work.
    profile_overrides: HashMap<PluginId, SchedulingProfile>,
    /// One admission gate per registered plugin, resolved from its
    /// `[concurrency]` declaration (spec 13.5). Shared behind an `Arc` so a
    /// caller can hold the same gate a dispatch site consults.
    budgets: HashMap<PluginId, PluginBudgetHandle>,
    /// Slots held by dispatched-but-unretired work, released in
    /// [`Self::finish_completion`] — the pipeline's single retirement edge.
    admitted: HashMap<(PluginId, Generation), OwnedBudgetGuard>,
    active_requests: HashSet<(PluginId, Generation)>,
    unresolved_cancellations: HashMap<(PluginId, Generation), ()>,
    pending_cancellations: Vec<CancelledRequest>,
    pending_completions: HashMap<(PluginId, Generation), Millis>,
    pending_errors: VecDeque<PipelineError>,
    error_capacity: usize,
    dropped_errors: u64,
    query: String,
    generation: Generation,
    rows: Arc<[ResultRow]>,
    visible_generation: Option<Generation>,
    presented_pending: Option<bool>,
    presentation_dirty: bool,
    /// Resolver for the icon references plugins put on their own items, set by
    /// whichever provider owns this pipeline. Absent until a provider installs
    /// one, and absent forever for a pipeline with no plugin rows.
    icons: Option<Arc<PluginIconResolver>>,
}

impl QueryPipeline {
    pub fn new(mut config: PipelineConfig) -> Self {
        config.intake_limits.capacity_batches = config.intake_limits.capacity_batches.max(1);
        config.intake_limits.capacity_items = config.intake_limits.capacity_items.max(1);
        config.drain_budget.batches_per_plugin = config.drain_budget.batches_per_plugin.max(1);
        config.drain_budget.items_per_plugin = config.drain_budget.items_per_plugin.max(1);
        config.drain_budget.total_batches = config.drain_budget.total_batches.max(1);

        let error_capacity = config.intake_limits.capacity_batches;
        let result_trace_capacity = config.scheduler.result_queue_capacity.max(1);
        Self {
            scheduler: QueryScheduler::new(config.scheduler),
            intake: InboundResultQueue::new(config.intake_limits),
            aggregator: MemoryResultAggregator::new(config.limits),
            supervisor: MemorySupervisor::new(CircuitBreakerConfig {
                failure_threshold: 0,
                cooldown: Duration::ZERO,
            }),
            default_intake_policy: config.default_intake_policy,
            drain_budget: config.drain_budget,
            last_intake_drain_at: None,
            result_trace_capacity,
            max_items_per_batch: config.limits.max_items_per_batch,
            max_items_per_plugin_per_query: config.limits.max_items_per_plugin_per_query,
            max_items_per_query: config.limits.max_items_per_query,
            unranked_batches: 0,
            registered: Vec::new(),
            health_sync: HashMap::new(),
            profile_overrides: HashMap::new(),
            budgets: HashMap::new(),
            admitted: HashMap::new(),
            active_requests: HashSet::new(),
            unresolved_cancellations: HashMap::new(),
            pending_cancellations: Vec::new(),
            pending_completions: HashMap::new(),
            pending_errors: VecDeque::with_capacity(error_capacity),
            error_capacity,
            dropped_errors: 0,
            query: String::new(),
            generation: Generation::ZERO,
            rows: Arc::from(Vec::<ResultRow>::new()),
            visible_generation: None,
            presented_pending: None,
            presentation_dirty: false,
            icons: None,
        }
    }

    /// The largest batch [`Self::deliver`] will accept for one plugin.
    ///
    /// A batch above it is refused whole, so a producer holding more items than
    /// this must split them rather than hope for truncation.
    pub fn max_items_per_batch(&self) -> usize {
        self.max_items_per_batch
    }

    /// The total items [`Self::deliver`] will accept from one plugin for one
    /// query.
    ///
    /// A producer that can find more than this must stop here. Handing over the
    /// extra does not truncate the answer, it gets the crossing batch refused
    /// whole and turns a broad-but-legal query into a pipeline error and no
    /// published frame at all.
    pub fn max_items_per_plugin_per_query(&self) -> usize {
        self.max_items_per_plugin_per_query
    }

    /// The total items [`Self::deliver`] will accept for one query across every
    /// plugin.
    ///
    /// Separate from the per-owner quota and frequently *smaller* than it: the
    /// launcher's `max-results` setting lowers this one alone, so a provider
    /// that respects only the per-owner number can still be refused here. Both
    /// bounds have to be obeyed, and a producer should cap at whichever is
    /// lower.
    pub fn max_items_per_query(&self) -> usize {
        self.max_items_per_query
    }

    /// Records the profile override configured for a plugin. Providers must
    /// call this before registering the plugin.
    pub fn override_scheduling_profile(&mut self, plugin: PluginId, profile: SchedulingProfile) {
        self.profile_overrides.insert(plugin, profile);
    }

    fn effective_policy(&self, plugin: &PluginId, policy: PluginPolicy) -> PluginPolicy {
        let Some(profile) = self.profile_overrides.get(plugin).copied() else {
            return policy;
        };
        if profile == policy.profile {
            return policy;
        }
        let mut effective = match profile {
            SchedulingProfile::LegacyStrict => PluginPolicy::legacy_strict(),
            SchedulingProfile::LegacyOptimized => PluginPolicy::legacy_optimized(),
            SchedulingProfile::Modern => PluginPolicy::modern(),
        };
        effective.activation = policy.activation;
        effective
    }

    /// Applies a configured profile to an already-registered plugin. Provider
    /// discovery often learns the final id only during loading, so callers may
    /// set the override after registration; queued work is invalidated by the
    /// scheduler's normal policy transition.
    pub fn set_scheduling_profile(&mut self, plugin: &PluginId, profile: SchedulingProfile) -> bool {
        let Some(current) = self.scheduler.plugin_policy(plugin).cloned() else {
            return false;
        };
        let mut policy = match profile {
            SchedulingProfile::LegacyStrict => PluginPolicy::legacy_strict(),
            SchedulingProfile::LegacyOptimized => PluginPolicy::legacy_optimized(),
            SchedulingProfile::Modern => PluginPolicy::modern(),
        };
        policy.activation = current.activation;
        self.scheduler.set_policy(plugin, policy, 0);
        self.profile_overrides.insert(plugin.clone(), profile);
        true
    }

    pub fn register_plugin(&mut self, plugin: PluginId, policy: PluginPolicy) -> Result<(), PipelineError> {
        self.register_plugin_with_intake(plugin, policy, self.default_intake_policy)
    }

    pub fn register_plugin_with_intake(
        &mut self,
        plugin: PluginId,
        policy: PluginPolicy,
        intake_policy: IntakePolicy,
    ) -> Result<(), PipelineError> {
        self.register_plugin_with_concurrency(plugin, policy, intake_policy, &ConcurrencySection::default())
    }

    /// Registers a plugin together with the `[concurrency]` declaration that
    /// bounds its simultaneous work (spec 13.5).
    ///
    /// This convenience path resolves and owns a budget for callers that do
    /// not have a provider runtime owner. Production providers use
    /// [`Self::register_namespaced_manifest`], which returns the exact handle
    /// retained by the pipeline so the runtime can clone it.
    pub fn register_plugin_with_concurrency(
        &mut self,
        plugin: PluginId,
        policy: PluginPolicy,
        intake_policy: IntakePolicy,
        concurrency: &ConcurrencySection,
    ) -> Result<(), PipelineError> {
        if self.health_sync.contains_key(&plugin) {
            return Err(PipelineError::AlreadyRegistered { plugin });
        }
        let policy = self.effective_policy(&plugin, policy);
        let budget = resolved_budget_for_policy(&policy, concurrency);
        self.register_plugin_with_budget(plugin, policy, intake_policy, budget)
    }

    /// Registers a plugin against an already-resolved shared budget handle.
    ///
    /// The handle is the ownership boundary for all four §13.5 kinds. The
    /// pipeline stores this exact `Arc`; dispatch owners must clone the handle
    /// rather than resolve another budget from the manifest.
    pub fn register_plugin_with_budget(
        &mut self,
        plugin: PluginId,
        policy: PluginPolicy,
        intake_policy: IntakePolicy,
        budget: PluginBudgetHandle,
    ) -> Result<(), PipelineError> {
        if self.health_sync.contains_key(&plugin) {
            return Err(PipelineError::AlreadyRegistered { plugin });
        }
        if let Some(owner) = self
            .budgets
            .iter()
            .find_map(|(owner, existing)| Arc::ptr_eq(existing, &budget).then(|| owner.clone()))
        {
            return Err(PipelineError::BudgetAlreadyOwned { plugin, owner });
        }
        let policy = self.effective_policy(&plugin, policy);
        self.supervisor
            .register(&plugin)
            .expect("a pipeline registers each plugin with its supervisor once");
        self.scheduler.register_plugin(plugin.clone(), policy);
        self.intake.register(plugin.clone(), intake_policy);
        self.health_sync.insert(plugin.clone(), HealthSync::default());
        self.budgets.insert(plugin.clone(), budget);
        self.registered.push(plugin);
        Ok(())
    }

    /// Registers against the pipeline's default bounded intake policy while
    /// preserving an explicit provider-owned budget handle.
    pub fn register_plugin_with_budget_default_intake(
        &mut self,
        plugin: PluginId,
        policy: PluginPolicy,
        budget: PluginBudgetHandle,
    ) -> Result<(), PipelineError> {
        self.register_plugin_with_budget(plugin, policy, self.default_intake_policy, budget)
    }

    pub fn register_manifest(&mut self, manifest: &Manifest) -> Result<PluginId, PipelineError> {
        let plugin = PluginId(manifest.plugin.id.clone());
        self.register_namespaced_manifest(plugin.clone(), manifest)?;
        Ok(plugin)
    }

    /// Registers a manifest against a provider-created shared budget.
    pub fn register_manifest_with_budget(
        &mut self,
        manifest: &Manifest,
        budget: PluginBudgetHandle,
    ) -> Result<PluginId, PipelineError> {
        let plugin = PluginId(manifest.plugin.id.clone());
        self.register_namespaced_manifest_with_budget(plugin.clone(), manifest, budget)?;
        Ok(plugin)
    }

    /// [`Self::register_manifest`] for a provider that namespaces the plugin
    /// id it exposes (`native.*`, `modern.*`). The scheduling policy and the
    /// concurrency budget still come from the manifest, so a namespacing
    /// provider cannot accidentally drop the author's declaration.
    ///
    /// The returned handle is the same `Arc` stored by this pipeline. A
    /// provider retains it and passes clones to every non-query dispatch seam.
    pub fn register_namespaced_manifest(
        &mut self,
        plugin: PluginId,
        manifest: &Manifest,
    ) -> Result<PluginBudgetHandle, PipelineError> {
        if self.health_sync.contains_key(&plugin) {
            return Err(PipelineError::AlreadyRegistered { plugin });
        }
        ensure_supported_runtime(&plugin, manifest.plugin.runtime)?;
        let policy = self.effective_policy(&plugin, plugin_policy_from_manifest(manifest));
        let budget = resolved_budget_for_policy(&policy, &manifest.concurrency);
        self.register_plugin_with_budget(plugin.clone(), policy, self.default_intake_policy, budget.clone())?;
        self.aggregator.set_plugin_limits(
            plugin,
            manifest.performance.maximum_results_per_query,
            manifest.performance.maximum_results_per_batch,
        );
        Ok(budget)
    }

    /// Registers a namespaced manifest against an already-created shared
    /// budget. Use this when the provider creates the handle before starting
    /// its worker; no budget is reconstructed here.
    pub fn register_namespaced_manifest_with_budget(
        &mut self,
        plugin: PluginId,
        manifest: &Manifest,
        budget: PluginBudgetHandle,
    ) -> Result<(), PipelineError> {
        if self.health_sync.contains_key(&plugin) {
            return Err(PipelineError::AlreadyRegistered { plugin });
        }
        ensure_supported_runtime(&plugin, manifest.plugin.runtime)?;
        let policy = self.effective_policy(&plugin, plugin_policy_from_manifest(manifest));
        self.register_plugin_with_budget(plugin.clone(), policy, self.default_intake_policy, budget)?;
        self.aggregator.set_plugin_limits(
            plugin,
            manifest.performance.maximum_results_per_query,
            manifest.performance.maximum_results_per_batch,
        );
        Ok(())
    }

    /// Rolls back a plugin registration whose runtime failed to start.
    ///
    /// This is intentionally a hard removal, not a disable: no scheduler,
    /// intake, supervisor or budget state survives to become a ghost
    /// registration, and any admitted guards are dropped at this edge.
    pub fn unregister_plugin(&mut self, plugin: &PluginId) -> bool {
        if !self.health_sync.contains_key(plugin) {
            return false;
        }

        let admitted = self
            .admitted
            .keys()
            .filter(|(owner, _)| owner == plugin)
            .cloned()
            .collect::<Vec<_>>();
        for key in admitted {
            self.admitted.remove(&key);
        }

        self.active_requests.retain(|(owner, _)| owner != plugin);
        self.unresolved_cancellations
            .retain(|(owner, _), _| owner != plugin);
        self.pending_completions.retain(|(owner, _), _| owner != plugin);
        self.pending_cancellations
            .retain(|cancellation| &cancellation.plugin != plugin);
        self.pending_errors.retain(|error| match error {
            PipelineError::AlreadyRegistered { plugin: owner }
            | PipelineError::QueueRejected { plugin: owner, .. }
            | PipelineError::AggregatorRejected { plugin: owner, .. }
            | PipelineError::UnsupportedRuntime { plugin: owner, .. } => owner != plugin,
            PipelineError::BudgetAlreadyOwned {
                plugin: attempted,
                owner,
            } => attempted != plugin && owner != plugin,
        });

        self.scheduler.unregister_plugin(plugin);
        self.intake.unregister(plugin);
        self.supervisor.unregister(plugin);
        self.aggregator.remove_plugin_limits(plugin);
        self.health_sync.remove(plugin);
        self.budgets.remove(plugin);
        self.registered.retain(|registered| registered != plugin);
        true
    }

    /// Mints and opens a generation. Worker dispatch remains exclusively a
    /// responsibility of [`Self::tick`]. Rows from the preceding generation
    /// are cleared synchronously, before any worker can answer.
    pub fn keystroke(&mut self, text: &str, now: Millis) -> Generation {
        let generation = self.scheduler.submit_query(text, now);
        self.query.clear();
        self.query.push_str(text);
        self.generation = generation;
        self.unranked_batches = 0;
        self.last_intake_drain_at = None;
        self.capture_cancellations();

        self.intake.begin_generation(generation);
        self.aggregator.begin_generation(generation);
        self.rows = Arc::from(Vec::<ResultRow>::new());
        self.visible_generation = Some(generation);
        self.presented_pending = None;
        self.presentation_dirty = true;

        // Generation rollover reclaims obsolete resident work immediately.
        // A zero merge budget is intentional here: a keystroke never executes
        // plugin work or consumes the new generation's intake.
        let _ = self.intake.drain_into(
            now,
            &mut self.aggregator,
            DrainBudget {
                batches_per_plugin: 0,
                items_per_plugin: 0,
                total_batches: 0,
            },
        );
        self.flush_ready_completions(&[]);
        self.sync_health();
        generation
    }

    pub fn tick(&mut self, now: Millis) -> PipelineTick {
        let (drain_report, mut errors) = self.drain_intake(now);
        errors.splice(0..0, self.pending_errors.drain(..));

        // Admission is the production seam for spec 13.5: a plugin already at
        // its declared suggestion budget does not receive a second request.
        // The refusal is counted and the request is retired here, so the
        // scheduler is not left holding an in-flight entry nobody will answer.
        let mut dispatches = self.scheduler.tick(now);
        let mut refused = Vec::new();
        dispatches.retain(|dispatch| {
            let Some(budget) = self.budgets.get(&dispatch.plugin) else {
                return true;
            };
            match budget.try_acquire_owned(BudgetKind::Suggestion) {
                Some(guard) => {
                    self.admitted
                        .insert((dispatch.plugin.clone(), dispatch.generation), guard);
                    self.active_requests
                        .insert((dispatch.plugin.clone(), dispatch.generation));
                    true
                }
                None => {
                    refused.push((dispatch.plugin.clone(), dispatch.generation));
                    false
                }
            }
        });
        // The refusal itself was already counted on the shared budget by the
        // failed admission above; `sync_health` is the single writer that
        // carries it into `PluginHealth`. Retire the request here so the
        // scheduler is not left holding an in-flight entry nobody will answer.
        for (plugin, generation) in refused {
            let _ = self.finish_completion(&plugin, generation, now);
        }
        self.capture_cancellations();
        let cancellations = std::mem::take(&mut self.pending_cancellations);
        self.sync_health();

        PipelineTick {
            dispatches,
            cancellations,
            drain_report,
            errors,
        }
    }

    pub fn next_wakeup(&self) -> Option<Millis> {
        self.scheduler.next_wakeup()
    }

    /// Admits a high-priority worker publication to the bounded intake queue.
    /// Aggregation and successful result tracing happen only during a fair
    /// [`tick`](Self::tick) or [`present`](Self::present) drain.
    pub fn deliver(&mut self, batch: ResultBatch, now: Millis) -> Result<(), PipelineError> {
        self.deliver_with_priority(batch, BatchPriority::High, now)
    }

    pub fn deliver_with_priority(
        &mut self,
        batch: ResultBatch,
        priority: BatchPriority,
        now: Millis,
    ) -> Result<(), PipelineError> {
        let plugin = batch.plugin.clone();
        let generation = batch.generation;
        let state = batch.state;
        let item_count = batch.items.len();

        if self.health_sync.contains_key(&plugin)
            && !self.active_requests.contains(&(plugin.clone(), generation))
        {
            let _ = self.scheduler.record_result_batch(
                &plugin,
                generation,
                item_count,
                batch_completion(state),
                now,
            );
            self.classify_cancelled_response(&plugin, generation, state);
            self.sync_health();
            return Err(PipelineError::QueueRejected {
                plugin,
                generation,
                reason: QueueReject::StaleGeneration,
            });
        }

        match self.intake.submit(now, InboundBatch { batch, priority }) {
            Ok(_) => Ok(()),
            Err(reason) => {
                if reason == QueueReject::StaleGeneration {
                    let _ = self.scheduler.record_result_batch(
                        &plugin,
                        generation,
                        item_count,
                        batch_completion(state),
                        now,
                    );
                    self.classify_cancelled_response(&plugin, generation, state);
                    self.sync_health();
                }
                Err(PipelineError::QueueRejected {
                    plugin,
                    generation,
                    reason,
                })
            }
        }
    }

    pub fn complete(&mut self, plugin: &PluginId, generation: Generation, now: Millis) -> CompletionOutcome {
        let key = (plugin.clone(), generation);
        if generation == self.generation
            && self.active_requests.contains(&key)
            && self.intake.plugin_depth(plugin).batches != 0
        {
            self.pending_completions.insert(key, now);
            return CompletionOutcome::Accepted;
        }
        self.finish_completion(plugin, generation, now)
    }
    /// Aborts a request whose provider can no longer answer.
    ///
    /// This is the explicit death path for a provider. It retires the
    /// scheduler request, any parked completion and the admitted suggestion
    /// slot without advancing the current query generation. Repeating the
    /// call after retirement is a harmless no-op.
    pub fn abort_request(&mut self, plugin: &PluginId, generation: Generation, now: Millis) -> bool {
        let key = (plugin.clone(), generation);
        let had_request = self.active_requests.contains(&key)
            || self.admitted.contains_key(&key)
            || self.pending_completions.contains_key(&key)
            || self.unresolved_cancellations.contains_key(&key);
        if !had_request {
            return false;
        }

        self.pending_completions.remove(&key);
        self.pending_cancellations
            .retain(|cancellation| &cancellation.plugin != plugin || cancellation.generation != generation);
        let cancellation_pending = self.unresolved_cancellations.remove(&key).is_some();
        let _ = self.finish_completion(plugin, generation, now);
        if cancellation_pending {
            self.supervisor
                .record_cancellation(plugin, false, 1)
                .expect("scheduler and supervisor plugin registries stay in lockstep");
        }
        true
    }

    /// Fair-drains intake, preserves plugin publication order and publishes at
    /// most one coalesced frame. The empty opening frame of every generation is
    /// visible, so rows can never outlive the query generation that produced them.
    /// Presents after draining everything a synchronous producer has queued.
    ///
    /// [`Self::present`] drains one batch per plugin per timestamp: that budget
    /// paces a stream of plugin results against the frame rate, and the
    /// once-per-timestamp gate stops a repainting UI from re-draining. Neither
    /// applies to a producer that has already handed over its whole answer
    /// before returning, and pacing it against frames is actively wrong -- the
    /// built-in application catalog queued three chunks of a common-letter
    /// match, one drained, the rest sat in the queue, and the request stayed
    /// unsettled, so the launcher said "Providers are still responding" until
    /// the next keystroke.
    ///
    /// Bounded by the queue depth observed on entry plus a margin, so a batch
    /// that cannot drain ends the loop instead of spinning in it.
    pub fn present_drained(&mut self, now: Millis) -> Option<ViewModel> {
        let mut frame = self.present(now);
        let mut rounds = self.intake.depth().batches.saturating_add(2);
        while rounds > 0 && self.intake.depth().batches > 0 {
            rounds -= 1;
            let before = self.intake.depth().batches;
            // Re-arm the gate deliberately: this is the same instant, and the
            // whole point is to take the rest of what is already queued.
            self.last_intake_drain_at = None;
            if let Some(next) = self.present(now) {
                frame = Some(next);
            }
            if self.intake.depth().batches >= before {
                // No progress; something is refusing to drain and another pass
                // would only repeat it.
                break;
            }
        }
        frame
    }

    pub fn present(&mut self, now: Millis) -> Option<ViewModel> {
        let (_, errors) = self.drain_intake(now);
        for error in errors {
            self.push_error(error);
        }

        self.aggregator.begin_frame();
        let update = self.aggregator.take_ui_update();
        let mut rows_changed = false;

        if let Some(items) = update {
            let icons = self.icons.clone();
            self.rows = items
                .into_iter()
                .map(|item| result_row(item, icons.as_ref()))
                .collect::<Vec<_>>()
                .into();
            self.visible_generation = Some(self.generation);
            rows_changed = true;
        } else if self.fill_pending_icons() {
            rows_changed = true;
        }

        if self.visible_generation != Some(self.generation) {
            return None;
        }

        let pending_plugins = self.has_pending_work();
        if !rows_changed && !self.presentation_dirty && self.presented_pending == Some(pending_plugins) {
            return None;
        }
        self.presented_pending = Some(pending_plugins);

        self.scheduler
            .record_ranking(self.generation, self.rows.len(), now);
        self.scheduler
            .record_presentation(self.generation, self.rows.len(), now);
        self.unranked_batches = 0;
        self.presentation_dirty = false;

        Some(ViewModel {
            generation: self.generation,
            query: self.query.clone(),
            rows: Arc::clone(&self.rows),
            selected: 0,
            pending_plugins,
            actions_open: false,
            // A pipeline frame carries results, never the launcher's own
            // settings: the panel is host state that no query can open or fill.
            settings_open: false,
            settings: Arc::default(),
            settings_focus: None,
        })
    }

    /// Installs the resolver for plugin-supplied icon references.
    ///
    /// Called once by the provider that owns this pipeline, after discovery,
    /// so the resolver knows every plugin that actually loaded.
    pub fn set_plugin_icons(&mut self, icons: Arc<PluginIconResolver>) {
        self.icons = Some(icons);
    }

    /// Fills in icons that had not arrived when their rows were built.
    ///
    /// A native plugin's icon comes over the protocol, so a row is published
    /// without pixels rather than holding a finished frame. This is the edge
    /// that lets the icon appear on a later frame instead of only after the
    /// next query. Returns whether any row changed.
    fn fill_pending_icons(&mut self) -> bool {
        let Some(icons) = self.icons.clone() else {
            return false;
        };
        if !self
            .rows
            .iter()
            .any(|row| row.icon.is_none() && row.icon_reference.is_some())
        {
            return false;
        }
        let mut rows = self.rows.to_vec();
        let mut changed = false;
        for row in &mut rows {
            if row.icon.is_some() {
                continue;
            }
            let Some(reference) = row.icon_reference.as_deref() else {
                continue;
            };
            if let Some(image) = icons.resolve(&row.plugin_name, reference) {
                row.icon = Some(image);
                changed = true;
            }
        }
        if changed {
            self.rows = rows.into();
        }
        changed
    }

    pub fn rows(&self) -> &[ResultRow] {
        self.rows.as_ref()
    }

    pub fn visible_generation(&self) -> Option<Generation> {
        self.visible_generation
    }

    pub fn diagnostics(&self) -> SchedulerDiagnostics {
        self.scheduler.diagnostics()
    }

    pub fn plugin_diagnostics(&self, plugin: &PluginId) -> Option<PluginDiagnostics> {
        self.scheduler.plugin_diagnostics(plugin)
    }

    pub fn plugin_policy(&self, plugin: &PluginId) -> Option<&PluginPolicy> {
        self.scheduler.plugin_policy(plugin)
    }

    /// The admission gate enforcing this plugin's `[concurrency]` limits.
    ///
    /// Exposed so a dispatch site outside the pipeline can consult the same
    /// gate, and so operators can read per-kind `refusals` and live
    /// `in_flight` occupancy alongside [`Self::health`].
    pub fn plugin_budget(&self, plugin: &PluginId) -> Option<&PluginBudgetHandle> {
        self.budgets.get(plugin)
    }

    pub fn intake_depth(&self) -> QueueDepth {
        self.intake.depth()
    }

    pub fn plugin_intake_depth(&self, plugin: &PluginId) -> QueueDepth {
        self.intake.plugin_depth(plugin)
    }

    pub fn producer_state(&self, plugin: &PluginId) -> Option<ProducerState> {
        self.intake.producer_state(plugin)
    }

    pub fn intake_diagnostics(&self) -> &QueueDiagnostics {
        self.intake.diagnostics()
    }

    pub fn take_intake_events(&mut self) -> Vec<QueueEvent> {
        self.intake.take_events()
    }

    pub fn take_errors(&mut self) -> Vec<PipelineError> {
        self.pending_errors.drain(..).collect()
    }

    pub fn dropped_errors(&self) -> u64 {
        self.dropped_errors
    }

    /// Current diagnostics for one registered plugin (spec 24.3).
    ///
    /// Takes `&mut self` because reading is also a reconciliation point:
    /// action, catalog and background refusals are raised on threads that
    /// never touch the supervisor, so the shared budget counters are folded
    /// in here. Without that, `crikey run` would report zero refusals for
    /// three of the four §13.5 kinds no matter how hard a plugin was throttled.
    pub fn health(&mut self, plugin: &PluginId) -> PluginHealth {
        self.sync_concurrency_refusals(plugin);
        self.supervisor.health(plugin)
    }

    /// Diagnostics for every registered plugin, in registration order.
    ///
    /// The composition root has no independent roster of what a provider
    /// loaded, so iterating the pipeline's own registry is what lets an
    /// operator-facing report name a throttled plugin it never asked about.
    pub fn plugin_health_report(&mut self) -> Vec<(PluginId, PluginHealth)> {
        let registered = self.registered.clone();
        registered
            .into_iter()
            .map(|plugin| {
                let health = self.health(&plugin);
                (plugin, health)
            })
            .collect()
    }

    pub fn trace(&self) -> &[QueryTraceEvent] {
        self.scheduler.trace()
    }

    fn drain_intake(&mut self, now: Millis) -> (DrainReport, Vec<PipelineError>) {
        if self.last_intake_drain_at == Some(now) {
            return (DrainReport::default(), Vec::new());
        }
        // An empty pass does not spend this turn's budget: synchronous
        // providers may publish immediately after `tick` dispatches them and
        // must still be drainable by `present` at the same timestamp.
        if self.intake.depth().batches == 0 {
            return (DrainReport::default(), Vec::new());
        }
        self.last_intake_drain_at = Some(now);

        let available_trace_slots = self.result_trace_capacity.saturating_sub(self.unranked_batches);
        let budget = DrainBudget {
            batches_per_plugin: self.drain_budget.batches_per_plugin,
            items_per_plugin: self.drain_budget.items_per_plugin,
            total_batches: self.drain_budget.total_batches.min(available_trace_slots),
        };
        let report = self.intake.drain_into(now, &mut self.aggregator, budget);
        let mut errors = Vec::with_capacity(report.merge_rejected.len());

        for merged in &report.merged_batches {
            match self.scheduler.record_result_batch(
                &merged.plugin,
                merged.generation,
                merged.items,
                batch_completion(merged.state),
                merged.admitted_at_ms,
            ) {
                BatchAdmission::Accepted => {
                    self.unranked_batches = self.unranked_batches.saturating_add(1);
                    self.presentation_dirty = true;
                }
                BatchAdmission::StaleRejected => {
                    errors.push(PipelineError::AggregatorRejected {
                        plugin: merged.plugin.clone(),
                        generation: merged.generation,
                        reason: RejectReason::StaleGeneration,
                    });
                }
                BatchAdmission::RejectedResultQueueFull => {
                    errors.push(PipelineError::QueueRejected {
                        plugin: merged.plugin.clone(),
                        generation: merged.generation,
                        reason: QueueReject::BoundaryFull,
                    });
                }
            }
        }

        errors.extend(report.merge_rejected.iter().map(|(plugin, reason)| {
            PipelineError::AggregatorRejected {
                plugin: plugin.clone(),
                generation: self.generation,
                reason: *reason,
            }
        }));

        self.flush_ready_completions(&report.merged_batches);
        self.sync_health();
        (report, errors)
    }

    fn finish_completion(
        &mut self,
        plugin: &PluginId,
        generation: Generation,
        now: Millis,
    ) -> CompletionOutcome {
        let outcome = self.scheduler.complete(plugin, generation, now);
        self.active_requests.remove(&(plugin.clone(), generation));
        // The retirement edge for the admitted slot. Dropping the guard here
        // — rather than at any earlier caller — keeps occupancy equal to the
        // work the scheduler still believes is running.
        self.admitted.remove(&(plugin.clone(), generation));
        if outcome == CompletionOutcome::Stale
            && self
                .unresolved_cancellations
                .remove(&(plugin.clone(), generation))
                .is_some()
        {
            self.supervisor
                .record_cancellation(plugin, true, 1)
                .expect("scheduler and supervisor plugin registries stay in lockstep");
        }
        self.sync_health();
        outcome
    }

    fn flush_ready_completions(&mut self, merged_batches: &[MergedBatch]) {
        let ready = self
            .pending_completions
            .keys()
            .filter(|(plugin, generation)| {
                *generation != self.generation
                    || merged_batches.iter().any(|merged| {
                        &merged.plugin == plugin
                            && merged.generation == *generation
                            && merged.state != BatchState::Partial
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        for (plugin, generation) in ready {
            let now = self
                .pending_completions
                .remove(&(plugin.clone(), generation))
                .expect("ready completion disappeared");
            let _ = self.finish_completion(&plugin, generation, now);
        }
    }

    fn capture_cancellations(&mut self) {
        for cancellation in self.scheduler.drain_cancellations() {
            let key = (cancellation.plugin.clone(), cancellation.generation);
            if !self.active_requests.contains(&key) {
                continue;
            }
            self.unresolved_cancellations.insert(key, ());
            self.pending_cancellations.push(cancellation);
        }
    }

    fn push_error(&mut self, error: PipelineError) {
        if self.pending_errors.len() == self.error_capacity {
            self.pending_errors.pop_front();
            self.dropped_errors = self.dropped_errors.saturating_add(1);
        }
        self.pending_errors.push_back(error);
    }

    fn has_pending_work(&self) -> bool {
        let diagnostics = self.scheduler.diagnostics();
        diagnostics.queued_requests != 0
            || diagnostics.in_flight_requests != 0
            || self.intake.depth().batches != 0
            || self.scheduler.next_wakeup().is_some()
    }

    fn classify_cancelled_response(&mut self, plugin: &PluginId, generation: Generation, state: BatchState) {
        if self
            .unresolved_cancellations
            .remove(&(plugin.clone(), generation))
            .is_none()
        {
            return;
        }

        self.supervisor
            .record_cancellation(plugin, state == BatchState::Cancelled, 1)
            .expect("scheduler and supervisor plugin registries stay in lockstep");
    }

    fn sync_health(&mut self) {
        for plugin in &self.registered {
            let Some(diagnostics) = self.scheduler.plugin_diagnostics(plugin) else {
                continue;
            };

            let synced = self
                .health_sync
                .get_mut(plugin)
                .expect("registered plugins have health synchronization state");

            let intake_depth = self.intake.plugin_depth(plugin).batches;
            self.supervisor
                .record_queue_depth(plugin, u32::try_from(intake_depth).unwrap_or(u32::MAX))
                .expect("scheduler and supervisor plugin registries stay in lockstep");

            let stale_delta = diagnostics
                .rejected_stale_results
                .saturating_sub(synced.stale_results);
            if stale_delta != 0 {
                self.supervisor
                    .record_stale_result_rejected(plugin, stale_delta)
                    .expect("scheduler and supervisor plugin registries stay in lockstep");
                synced.stale_results = diagnostics.rejected_stale_results;
            }

            let obsolete_delta = diagnostics
                .dropped_obsolete_requests
                .saturating_sub(synced.obsolete_requests);
            if obsolete_delta != 0 {
                self.supervisor
                    .record_obsolete_request_dropped(plugin, obsolete_delta)
                    .expect("scheduler and supervisor plugin registries stay in lockstep");
                synced.obsolete_requests = diagnostics.dropped_obsolete_requests;
            }

            Self::reconcile_refusals(
                &mut self.supervisor,
                &mut synced.refusals,
                self.budgets.get(plugin),
                plugin,
            );
        }
    }

    /// Folds one plugin's live budget refusal counters into its health record.
    fn sync_concurrency_refusals(&mut self, plugin: &PluginId) {
        let Some(synced) = self.health_sync.get_mut(plugin) else {
            return;
        };
        Self::reconcile_refusals(
            &mut self.supervisor,
            &mut synced.refusals,
            self.budgets.get(plugin),
            plugin,
        );
    }

    /// Carries the difference between the budget's cumulative refusals and
    /// what has already been reported into the supervisor, one kind at a time.
    ///
    /// Free of `self` so the caller can hold a mutable borrow of the health
    /// watermark and an immutable borrow of the budget registry at once.
    fn reconcile_refusals(
        supervisor: &mut MemorySupervisor,
        synced: &mut ConcurrencyRefusals,
        budget: Option<&PluginBudgetHandle>,
        plugin: &PluginId,
    ) {
        let Some(budget) = budget else {
            return;
        };
        let observed = budget.refusals_snapshot();
        for kind in BudgetKind::ALL {
            let delta = observed.of(kind).saturating_sub(synced.of(kind));
            if delta != 0 {
                supervisor
                    .record_concurrency_refusal(plugin, kind, delta)
                    .expect("scheduler and supervisor plugin registries stay in lockstep");
            }
        }
        *synced = observed;
    }
}

/// Resolves the shared suggestion budget from both manifest declarations.
///
/// `query.max-concurrent-requests` is the scheduler's hard ceiling and
/// defaults to one when omitted. `[concurrency].max-suggestion-requests` may
/// tighten that ceiling, but cannot raise it; a plugin requesting more
/// suggestion slots must repeat the higher value in the query policy.
fn resolved_budget_for_policy(policy: &PluginPolicy, concurrency: &ConcurrencySection) -> PluginBudgetHandle {
    let scheduler_limit = u32::try_from(policy.max_concurrent_requests).unwrap_or(u32::MAX);
    let mut resolved = concurrency.clone();
    resolved.max_suggestion_requests = Some(match resolved.max_suggestion_requests {
        Some(declared) => declared.min(scheduler_limit),
        None => scheduler_limit,
    });
    shared_budget_from_section(&resolved)
}

/// Refuses a runtime this build has no host for.
///
/// Exhaustive on purpose: a new `Runtime` variant must decide here, so adding
/// one can never make it silently supported. WASM is accepted only because
/// `native_provider` registers it as a supervised `crikey-wasm-host` worker;
/// the launcher itself never instantiates a module (ADR-0014).
fn ensure_supported_runtime(_plugin: &PluginId, runtime: Runtime) -> Result<(), PipelineError> {
    match runtime {
        Runtime::LegacyPython | Runtime::Python | Runtime::Native | Runtime::Builtin => Ok(()),
        Runtime::Wasm => Ok(()),
        // `c-abi` is served by the supervised `crikey-cabi-host` executable
        // and registered by `native_provider` (ADR-0015).
        Runtime::CAbi => Ok(()),
    }
}

fn runtime_name(runtime: Runtime) -> &'static str {
    match runtime {
        Runtime::LegacyPython => "legacy-python",
        Runtime::Python => "python",
        Runtime::Native => "native",
        Runtime::Wasm => "wasm",
        Runtime::CAbi => "c-abi",
        Runtime::Builtin => "builtin",
    }
}

fn batch_completion(state: BatchState) -> BatchCompletion {
    match state {
        BatchState::Partial => BatchCompletion::Partial,
        BatchState::Final => BatchCompletion::Final,
        BatchState::Cancelled => BatchCompletion::Cancelled,
        BatchState::Failed => BatchCompletion::Failed,
    }
}

pub(crate) fn plugin_policy_from_manifest(manifest: &Manifest) -> PluginPolicy {
    let resolved = manifest.query_policy();
    let defaults = match resolved.profile {
        crikey_plugin_model::SchedulingProfile::LegacyStrict => PluginPolicy::legacy_strict(),
        crikey_plugin_model::SchedulingProfile::LegacyOptimized => PluginPolicy::legacy_optimized(),
        crikey_plugin_model::SchedulingProfile::Modern => PluginPolicy::modern(),
    };

    PluginPolicy {
        profile: match resolved.profile {
            crikey_plugin_model::SchedulingProfile::LegacyStrict => SchedulingProfile::LegacyStrict,
            crikey_plugin_model::SchedulingProfile::LegacyOptimized => SchedulingProfile::LegacyOptimized,
            crikey_plugin_model::SchedulingProfile::Modern => SchedulingProfile::Modern,
        },
        debounce: DebouncePolicy {
            debounce_ms: resolved.debounce_ms,
            maximum_wait_ms: resolved.maximum_wait_ms,
            leading_edge: resolved.leading_edge,
            trailing_edge: resolved.trailing_edge,
            minimum_query_length: resolved.minimum_query_length,
        },
        activation: ActivationPolicy {
            supports_empty_query: resolved.empty_query,
            prefixes: resolved.prefixes,
            keywords: resolved.keywords,
            patterns: resolved.patterns,
        },
        max_concurrent_requests: resolved.max_concurrent_requests as usize,
        queue_policy: defaults.queue_policy,
        queue_capacity: defaults.queue_capacity,
    }
}

/// Builds one renderer row from a plugin item.
///
/// The icon reference is resolved through `icons` rather than the platform's
/// icon provider. A plugin's reference is not a platform reference -- it names
/// a file inside the plugin's own package, or a resource only a native plugin
/// can produce -- and handing it to the desktop's theme lookup would find
/// nothing or, worse, an unrelated icon of the same name. Catalog rows, whose
/// references *are* platform references, get their pixels in
/// `SearchService::result_rows`.
fn result_row(item: Item, icons: Option<&Arc<PluginIconResolver>>) -> ResultRow {
    let argument_hint = match item.argument_policy {
        ArgumentPolicy::Forbidden => None,
        ArgumentPolicy::Optional => Some("optional argument".to_owned()),
        ArgumentPolicy::Required => Some("argument required".to_owned()),
    };
    let mut actions = item.actions.into_iter();
    let default_action = actions.next();
    let icon = icons
        .zip(item.icon_reference.as_deref())
        .and_then(|(resolver, reference)| resolver.resolve(&item.plugin_id.0, reference));

    ResultRow {
        item: item.stable_id,
        label: item.label,
        description: item.description,
        icon,
        icon_reference: item.icon_reference,
        category: item.category.as_str().to_owned(),
        plugin_name: item.plugin_id.0,
        highlights: Vec::new(),
        argument_hint,
        status: None,
        default_action,
        alternate_actions: actions.collect(),
    }
}
