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
use crikey_plugin_model::Manifest;
use crikey_plugin_supervisor::{CircuitBreakerConfig, MemorySupervisor, PluginHealth, Supervisor};
use crikey_result_aggregator::{
    BatchPriority, BatchState, DrainBudget, DrainReport, InboundBatch, InboundResultQueue, IntakePolicy,
    MemoryResultAggregator, MergedBatch, OverflowPolicy, ProducerState, QueueDepth, QueueDiagnostics,
    QueueEvent, QueueLimits, QueueReject, RejectReason, ResultBatch, ResultLimits,
};
use crikey_ui::{ResultRow, ViewModel};

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
}

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
    unranked_batches: usize,
    registered: Vec<PluginId>,
    health_sync: HashMap<PluginId, HealthSync>,
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
            unranked_batches: 0,
            registered: Vec::new(),
            health_sync: HashMap::new(),
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
        }
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
        if self.health_sync.contains_key(&plugin) {
            return Err(PipelineError::AlreadyRegistered { plugin });
        }

        self.supervisor
            .register(&plugin)
            .expect("a pipeline registers each plugin with its supervisor once");
        self.scheduler.register_plugin(plugin.clone(), policy);
        self.intake.register(plugin.clone(), intake_policy);
        self.health_sync.insert(plugin.clone(), HealthSync::default());
        self.registered.push(plugin);
        Ok(())
    }

    pub fn register_manifest(&mut self, manifest: &Manifest) -> Result<PluginId, PipelineError> {
        let plugin = PluginId(manifest.plugin.id.clone());
        let policy = plugin_policy_from_manifest(manifest);
        self.register_plugin(plugin.clone(), policy)?;
        Ok(plugin)
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

        let dispatches = self.scheduler.tick(now);
        for dispatch in &dispatches {
            self.active_requests
                .insert((dispatch.plugin.clone(), dispatch.generation));
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

    /// Fair-drains intake, ranks the aggregated set and publishes at most one
    /// coalesced frame. The empty opening frame of every generation is visible,
    /// so rows can never outlive the query generation that produced them.
    pub fn present(&mut self, now: Millis) -> Option<ViewModel> {
        let (_, errors) = self.drain_intake(now);
        for error in errors {
            self.push_error(error);
        }

        self.aggregator.begin_frame();
        let update = self.aggregator.take_ui_update();
        let mut rows_changed = false;

        if let Some(mut items) = update {
            items.sort_by_key(|item| std::cmp::Reverse(item.score_hint));
            self.rows = items.into_iter().map(result_row).collect::<Vec<_>>().into();
            self.visible_generation = Some(self.generation);
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
        })
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

    pub fn health(&self, plugin: &PluginId) -> PluginHealth {
        self.supervisor.health(plugin)
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
        }
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
        },
        max_concurrent_requests: resolved.max_concurrent_requests as usize,
        queue_policy: defaults.queue_policy,
        queue_capacity: defaults.queue_capacity,
    }
}

fn result_row(item: Item) -> ResultRow {
    let argument_hint = match item.argument_policy {
        ArgumentPolicy::Forbidden => None,
        ArgumentPolicy::Optional => Some("optional argument".to_owned()),
        ArgumentPolicy::Required => Some("argument required".to_owned()),
    };
    let mut actions = item.actions.into_iter();
    let default_action = actions.next();

    ResultRow {
        item: item.stable_id,
        label: item.label,
        description: item.description,
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
