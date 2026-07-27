//! Stateful query scheduling, cancellation, diagnostics and developer tracing.

use std::collections::{BTreeMap, VecDeque};

use crikey_core::{Generation, GenerationTracker, PluginId};

use crate::{DebouncePolicy, LegacyDispatch, Millis, SchedulingProfile};

/// Host-side activation metadata for a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActivationPolicy {
    pub supports_empty_query: bool,
    pub prefixes: Vec<String>,
    pub keywords: Vec<String>,
}

/// Overflow behavior for one plugin's undispatched request queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueuePolicy {
    /// Keep only the newest undispatched request.
    #[default]
    ReplaceOldest,
    /// Preserve queued order and refuse an arrival when the bound is full.
    RejectNewest,
    /// Preserve queued order, evicting its oldest member to admit an arrival.
    DropOldest,
}

/// Complete scheduling policy for one plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPolicy {
    pub profile: SchedulingProfile,
    pub debounce: DebouncePolicy,
    pub activation: ActivationPolicy,
    pub max_concurrent_requests: usize,
    pub queue_policy: QueuePolicy,
    pub queue_capacity: usize,
}

impl PluginPolicy {
    /// Compatibility profile: prompt, ungated, serial and newest-pending.
    pub fn legacy_strict() -> Self {
        Self {
            profile: SchedulingProfile::LegacyStrict,
            debounce: DebouncePolicy {
                debounce_ms: 0,
                maximum_wait_ms: None,
                leading_edge: true,
                trailing_edge: true,
                minimum_query_length: 0,
            },
            activation: ActivationPolicy {
                supports_empty_query: true,
                ..ActivationPolicy::default()
            },
            max_concurrent_requests: 1,
            queue_policy: QueuePolicy::ReplaceOldest,
            queue_capacity: 1,
        }
    }

    /// Opt-in legacy profile using the ordinary debounce controls.
    pub fn legacy_optimized() -> Self {
        Self {
            profile: SchedulingProfile::LegacyOptimized,
            debounce: DebouncePolicy::default(),
            activation: ActivationPolicy::default(),
            max_concurrent_requests: 1,
            queue_policy: QueuePolicy::ReplaceOldest,
            queue_capacity: 1,
        }
    }

    /// Manifest-driven modern scheduling defaults.
    pub fn modern() -> Self {
        Self {
            profile: SchedulingProfile::Modern,
            debounce: DebouncePolicy::default(),
            activation: ActivationPolicy::default(),
            max_concurrent_requests: 1,
            queue_policy: QueuePolicy::ReplaceOldest,
            queue_capacity: 1,
        }
    }
}

impl Default for PluginPolicy {
    fn default() -> Self {
        Self::legacy_strict()
    }
}

/// Bounds and fairness budgets for a scheduler instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    pub request_queue_capacity: usize,
    pub result_queue_capacity: usize,
    pub per_plugin_dispatch_budget: usize,
    pub dispatch_budget_per_tick: usize,
    pub trace_capacity: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            request_queue_capacity: 64,
            result_queue_capacity: 64,
            per_plugin_dispatch_budget: 1,
            dispatch_budget_per_tick: 16,
            trace_capacity: 1024,
        }
    }
}

/// Why a modern plugin was not relevant to a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateReason {
    Disabled,
    MinimumQueryLength,
    EmptyQueryUnsupported,
    PrefixMismatch,
    KeywordMismatch,
}

/// One observable modern debounce decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebounceDecision {
    LeadingEdge,
    Deferred { until: Millis },
    Coalesced { superseded: Generation },
    TrailingEdge,
    MaximumWait,
    Gated(GateReason),
}

/// Why outstanding work was invalidated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    QueryChanged,
    NoLongerRelevant,
    Reconfigured,
    ProfileChanged,
    Disabled,
    Shutdown,
}

/// Outcome of retiring a dispatched callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionOutcome {
    Accepted,
    Stale,
    Unknown,
}

/// Lifecycle marker attached to one result batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchCompletion {
    Partial,
    Final,
    Cancelled,
    Failed,
}

/// Admission result for an incoming result batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchAdmission {
    Accepted,
    StaleRejected,
    RejectedResultQueueFull,
}

/// Work handed to a plugin by [`QueryScheduler::tick`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchedRequest {
    pub plugin: PluginId,
    pub generation: Generation,
    pub query: String,
    pub dispatched_at: Millis,
}

/// A cancellation the host can propagate to a worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelledRequest {
    pub plugin: PluginId,
    pub generation: Generation,
    pub reason: CancelReason,
    pub cancelled_at: Millis,
}

/// Bounded developer trace for a query session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryTraceEvent {
    Keystroke {
        at: Millis,
        generation: Generation,
        query_length: usize,
    },
    Debounce {
        at: Millis,
        plugin: PluginId,
        generation: Generation,
        decision: DebounceDecision,
    },
    LegacyDispatch {
        at: Millis,
        plugin: PluginId,
        generation: Generation,
        decision: LegacyDispatch,
    },
    Dispatched {
        at: Millis,
        plugin: PluginId,
        generation: Generation,
    },
    RequestDropped {
        at: Millis,
        plugin: PluginId,
        generation: Generation,
        policy: QueuePolicy,
    },
    Cancelled {
        at: Millis,
        plugin: PluginId,
        generation: Generation,
        reason: CancelReason,
    },
    FirstResult {
        at: Millis,
        plugin: PluginId,
        generation: Generation,
        latency_ms: Millis,
    },
    FinalResult {
        at: Millis,
        plugin: PluginId,
        generation: Generation,
        latency_ms: Millis,
    },
    ResultBatch {
        at: Millis,
        plugin: PluginId,
        generation: Generation,
        items: usize,
        completion: BatchCompletion,
    },
    StaleResultRejected {
        at: Millis,
        plugin: PluginId,
        generation: Generation,
    },
    Ranking {
        at: Millis,
        generation: Generation,
        ranked_items: usize,
    },
    Presentation {
        at: Millis,
        generation: Generation,
        visible_items: usize,
    },
}

/// Aggregate bounded-queue and lifecycle diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SchedulerDiagnostics {
    pub queued_requests: usize,
    pub in_flight_requests: usize,
    pub peak_queue_depth: usize,
    pub coalesced_requests: u64,
    pub dropped_obsolete_requests: u64,
    pub cancelled_requests: u64,
    pub rejected_stale_results: u64,
    pub rejected_plugin_queue_full: u64,
    pub rejected_global_queue_full: u64,
    pub dispatched_requests: u64,
    pub trace_events_dropped: u64,
}

impl SchedulerDiagnostics {
    pub fn rejected_requests(&self) -> u64 {
        self.rejected_plugin_queue_full
            .saturating_add(self.rejected_global_queue_full)
    }

    pub fn discarded_requests(&self) -> u64 {
        self.coalesced_requests
            .saturating_add(self.dropped_obsolete_requests)
            .saturating_add(self.rejected_requests())
    }
}

/// Diagnostics attributed to one registered plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PluginDiagnostics {
    pub queued_requests: usize,
    pub in_flight_requests: usize,
    pub peak_queue_depth: usize,
    pub dispatched_requests: u64,
    pub coalesced_requests: u64,
    pub dropped_obsolete_requests: u64,
    pub rejected_queue_full: u64,
    pub cancelled_requests: u64,
    pub rejected_stale_results: u64,
    pub last_dispatched_at: Option<Millis>,
    pub should_terminate: bool,
}

#[derive(Debug, Clone, Copy)]
enum DueReason {
    TrailingEdge,
    MaximumWait,
}

#[derive(Debug, Clone)]
struct PendingRequest {
    generation: Generation,
    query: String,
    ready: bool,
    due_at: Option<Millis>,
    due_reason: Option<DueReason>,
}

#[derive(Debug, Clone)]
struct InFlightRequest {
    generation: Generation,
    dispatched_at: Millis,
    cancel_reason: Option<CancelReason>,
    first_result_recorded: bool,
    final_result_recorded: bool,
}

#[derive(Debug)]
struct PluginState {
    policy: PluginPolicy,
    enabled: bool,
    relevant: bool,
    burst_started: Option<Millis>,
    queue: VecDeque<PendingRequest>,
    in_flight: Vec<InFlightRequest>,
    diagnostics: PluginDiagnostics,
}

impl PluginState {
    fn new(policy: PluginPolicy) -> Self {
        Self {
            policy,
            enabled: true,
            relevant: false,
            burst_started: None,
            queue: VecDeque::new(),
            in_flight: Vec::new(),
            diagnostics: PluginDiagnostics::default(),
        }
    }
}

#[derive(Debug)]
enum PreparedRequest {
    Legacy {
        decision: LegacyDispatch,
    },
    Modern {
        ready: bool,
        due_at: Option<Millis>,
        due_reason: Option<DueReason>,
        decision: DebounceDecision,
    },
}

/// Stateful, virtual-time query scheduler.
#[derive(Debug)]
pub struct QueryScheduler {
    config: SchedulerConfig,
    generations: GenerationTracker,
    plugins: BTreeMap<PluginId, PluginState>,
    plugin_order: Vec<PluginId>,
    round_robin_cursor: usize,
    diagnostics: SchedulerDiagnostics,
    trace: Vec<QueryTraceEvent>,
    cancellations: Vec<CancelledRequest>,
    result_queue_depth: usize,
    shutdown: bool,
}

impl Default for QueryScheduler {
    fn default() -> Self {
        Self::new(SchedulerConfig::default())
    }
}

impl QueryScheduler {
    pub fn new(mut config: SchedulerConfig) -> Self {
        normalize_config(&mut config);
        Self {
            config,
            generations: GenerationTracker::new(),
            plugins: BTreeMap::new(),
            plugin_order: Vec::new(),
            round_robin_cursor: 0,
            diagnostics: SchedulerDiagnostics::default(),
            trace: Vec::with_capacity(config.trace_capacity),
            cancellations: Vec::new(),
            result_queue_depth: 0,
            shutdown: false,
        }
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// Returns the normalized policy that the scheduler applies to `plugin`.
    pub fn plugin_policy(&self, plugin: &PluginId) -> Option<&PluginPolicy> {
        self.plugins.get(plugin).map(|state| &state.policy)
    }

    pub fn register_plugin(&mut self, plugin: PluginId, mut policy: PluginPolicy) {
        normalize_policy(&mut policy);
        if !self.plugins.contains_key(&plugin) {
            self.plugin_order.push(plugin.clone());
        }
        self.plugins.insert(plugin, PluginState::new(policy));
    }

    pub fn set_policy(&mut self, plugin: &PluginId, mut policy: PluginPolicy, now: Millis) {
        let Some(old_profile) = self.plugins.get(plugin).map(|state| state.policy.profile) else {
            return;
        };
        let reason = if old_profile == policy.profile {
            CancelReason::Reconfigured
        } else {
            CancelReason::ProfileChanged
        };
        self.invalidate_plugin(plugin, reason, now, true);
        normalize_policy(&mut policy);
        if let Some(state) = self.plugins.get_mut(plugin) {
            state.policy = policy;
            state.relevant = false;
            state.burst_started = None;
        }
    }

    pub fn disable_plugin(&mut self, plugin: &PluginId, now: Millis) {
        self.invalidate_plugin(plugin, CancelReason::Disabled, now, true);
        if let Some(state) = self.plugins.get_mut(plugin) {
            state.enabled = false;
            state.relevant = false;
            state.burst_started = None;
            state.in_flight.clear();
        }
    }

    pub fn enable_plugin(&mut self, plugin: &PluginId, _now: Millis) {
        if let Some(state) = self.plugins.get_mut(plugin) {
            state.enabled = true;
            state.relevant = false;
            state.burst_started = None;
        }
    }

    /// Records a query state. Dispatch remains exclusively a [`tick`](Self::tick) side effect.
    pub fn submit_query(&mut self, query: &str, now: Millis) -> Generation {
        let generation = self.generations.advance();
        let normalized = query.trim().to_lowercase();
        self.result_queue_depth = 0;
        self.push_trace(QueryTraceEvent::Keystroke {
            at: now,
            generation,
            query_length: normalized.chars().count(),
        });

        if self.shutdown {
            return generation;
        }

        // Explicit FIFO overflow policies may retain more than one pending
        // request, but a query change makes every older pending generation
        // unusable. Reclaim those slots before any global or plugin overflow
        // decision so an obsolete request can never displace the current one.
        self.reclaim_obsolete_queued(generation, now, false);

        let plugins = self.plugin_order.clone();
        // A newest-wins slot is reclaimed by the ordinary replacement path so
        // its coalescing diagnostics stay intact. Service those stale slots
        // first; otherwise one could transiently occupy global capacity while
        // an earlier explicit-policy plugin is making its overflow decision.
        let reclaims_replace_slot = plugins
            .iter()
            .map(|plugin| {
                self.plugins.get(plugin).is_some_and(|state| {
                    effective_queue_policy(&state.policy) == QueuePolicy::ReplaceOldest
                        && state.queue.iter().any(|request| request.generation != generation)
                })
            })
            .collect::<Vec<_>>();
        for reclaiming_first in [true, false] {
            for (plugin, reclaims_slot) in plugins.iter().zip(&reclaims_replace_slot) {
                if *reclaims_slot == reclaiming_first {
                    self.process_query_for_plugin(plugin, generation, query, &normalized, now);
                }
            }
        }
        generation
    }

    pub fn current_generation(&self) -> Generation {
        self.generations.current()
    }

    pub fn pending(&self, plugin: &PluginId) -> Option<Generation> {
        self.plugins
            .get(plugin)
            .and_then(|state| state.queue.back())
            .map(|request| request.generation)
    }

    pub fn queued(&self, plugin: &PluginId) -> usize {
        self.plugins.get(plugin).map_or(0, |state| state.queue.len())
    }

    pub fn in_flight(&self, plugin: &PluginId) -> usize {
        self.plugins.get(plugin).map_or(0, |state| state.in_flight.len())
    }

    pub fn should_terminate(&self, plugin: &PluginId) -> bool {
        self.plugins.get(plugin).is_some_and(|state| {
            state.policy.profile == SchedulingProfile::LegacyStrict
                && state
                    .in_flight
                    .iter()
                    .any(|request| request.cancel_reason.is_some())
        })
    }

    pub fn next_wakeup(&self) -> Option<Millis> {
        if self.shutdown {
            return None;
        }
        self.plugins
            .values()
            .filter(|state| state.enabled)
            .flat_map(|state| state.queue.iter())
            .filter_map(|request| (!request.ready).then_some(request.due_at).flatten())
            .min()
    }

    /// Applies elapsed deadlines and dispatches ready work within rotating budgets.
    pub fn tick(&mut self, now: Millis) -> Vec<DispatchedRequest> {
        if self.shutdown {
            return Vec::new();
        }

        // Admission normally reclaims explicit queues at the keystroke. Keep a
        // dispatch-side guard as the invariant boundary: tick must never hand
        // a worker a request from an obsolete generation.
        let current = self.current_generation();
        self.reclaim_obsolete_queued(current, now, true);

        let plugins = self.plugin_order.clone();
        for plugin in &plugins {
            let due = {
                let Some(state) = self.plugins.get_mut(plugin) else {
                    continue;
                };
                let mut due = Vec::new();
                for request in &mut state.queue {
                    if !request.ready && request.due_at.is_some_and(|deadline| deadline <= now) {
                        request.ready = true;
                        request.due_at = None;
                        if let Some(reason) = request.due_reason.take() {
                            due.push((request.generation, reason));
                        }
                    }
                }
                if !due.is_empty() {
                    state.burst_started = None;
                }
                due
            };
            for (generation, reason) in due {
                let decision = match reason {
                    DueReason::TrailingEdge => DebounceDecision::TrailingEdge,
                    DueReason::MaximumWait => DebounceDecision::MaximumWait,
                };
                self.push_trace(QueryTraceEvent::Debounce {
                    at: now,
                    plugin: plugin.clone(),
                    generation,
                    decision,
                });
            }
        }

        self.record_explicit_queue_peaks();

        let plugin_count = plugins.len();
        if plugin_count == 0
            || self.config.dispatch_budget_per_tick == 0
            || self.config.per_plugin_dispatch_budget == 0
        {
            self.record_remaining_queue_peaks();
            return Vec::new();
        }

        let start = self.round_robin_cursor % plugin_count;
        let mut dispatched = Vec::new();
        let mut last_served = None;

        for offset in 0..plugin_count {
            if dispatched.len() >= self.config.dispatch_budget_per_tick {
                break;
            }
            let index = (start + offset) % plugin_count;
            let plugin = &plugins[index];
            let mut own_budget = 0usize;

            while own_budget < self.config.per_plugin_dispatch_budget
                && dispatched.len() < self.config.dispatch_budget_per_tick
            {
                let request = {
                    let Some(state) = self.plugins.get_mut(plugin) else {
                        break;
                    };
                    let limit = effective_concurrency_limit(&state.policy);
                    if state.in_flight.len() >= limit
                        || !state.queue.front().is_some_and(|request| request.ready)
                    {
                        break;
                    }
                    let pending = state.queue.pop_front().expect("front was ready");
                    state.in_flight.push(InFlightRequest {
                        generation: pending.generation,
                        dispatched_at: now,
                        cancel_reason: None,
                        first_result_recorded: false,
                        final_result_recorded: false,
                    });
                    state.diagnostics.dispatched_requests =
                        state.diagnostics.dispatched_requests.saturating_add(1);
                    state.diagnostics.last_dispatched_at = Some(now);
                    DispatchedRequest {
                        plugin: plugin.clone(),
                        generation: pending.generation,
                        query: pending.query,
                        dispatched_at: now,
                    }
                };

                self.diagnostics.dispatched_requests = self.diagnostics.dispatched_requests.saturating_add(1);
                self.push_trace(QueryTraceEvent::Dispatched {
                    at: now,
                    plugin: plugin.clone(),
                    generation: request.generation,
                });
                dispatched.push(request);
                own_budget += 1;
                last_served = Some(index);
            }
        }

        if let Some(index) = last_served {
            self.round_robin_cursor = (index + 1) % plugin_count;
        } else {
            self.round_robin_cursor = (start + 1) % plugin_count;
        }
        self.record_remaining_queue_peaks();
        dispatched
    }

    pub fn complete(&mut self, plugin: &PluginId, generation: Generation, now: Millis) -> CompletionOutcome {
        let Some(position) = self.plugins.get(plugin).and_then(|state| {
            state
                .in_flight
                .iter()
                .position(|request| request.generation == generation)
        }) else {
            return CompletionOutcome::Unknown;
        };

        let request = self
            .plugins
            .get_mut(plugin)
            .expect("plugin was found")
            .in_flight
            .remove(position);
        let stale = request.cancel_reason.is_some() || generation != self.current_generation();
        if stale {
            return CompletionOutcome::Stale;
        }
        if !request.final_result_recorded {
            self.push_trace(QueryTraceEvent::FinalResult {
                at: now,
                plugin: plugin.clone(),
                generation,
                latency_ms: now.saturating_sub(request.dispatched_at),
            });
        }
        CompletionOutcome::Accepted
    }

    /// Invalidates all of a plugin's in-flight and queued generations.
    pub fn cancel_plugin(&mut self, plugin: &PluginId, reason: CancelReason, now: Millis) -> Vec<Generation> {
        self.invalidate_plugin(plugin, reason, now, true)
    }

    pub fn drain_cancellations(&mut self) -> Vec<CancelledRequest> {
        std::mem::take(&mut self.cancellations)
    }

    pub fn shutdown(&mut self, now: Millis) {
        if self.shutdown {
            return;
        }
        let plugins = self.plugin_order.clone();
        for plugin in &plugins {
            self.invalidate_plugin(plugin, CancelReason::Shutdown, now, false);
            if let Some(state) = self.plugins.get_mut(plugin) {
                state.in_flight.clear();
                state.queue.clear();
                state.relevant = false;
                state.burst_started = None;
            }
        }
        self.result_queue_depth = 0;
        self.shutdown = true;
    }

    pub fn record_result_batch(
        &mut self,
        plugin: &PluginId,
        generation: Generation,
        items: usize,
        completion: BatchCompletion,
        now: Millis,
    ) -> BatchAdmission {
        let current = generation == self.current_generation();
        let usable = self.plugins.get(plugin).is_some_and(|state| {
            state
                .in_flight
                .iter()
                .any(|request| request.generation == generation && request.cancel_reason.is_none())
        });
        if !current || !usable {
            self.diagnostics.rejected_stale_results =
                self.diagnostics.rejected_stale_results.saturating_add(1);
            if let Some(state) = self.plugins.get_mut(plugin) {
                state.diagnostics.rejected_stale_results =
                    state.diagnostics.rejected_stale_results.saturating_add(1);
            }
            self.push_trace(QueryTraceEvent::StaleResultRejected {
                at: now,
                plugin: plugin.clone(),
                generation,
            });
            return BatchAdmission::StaleRejected;
        }

        if self.result_queue_depth >= self.config.result_queue_capacity {
            return BatchAdmission::RejectedResultQueueFull;
        }
        self.result_queue_depth = self.result_queue_depth.saturating_add(1);

        let (first, final_result, latency) = {
            let state = self.plugins.get_mut(plugin).expect("usable plugin exists");
            let request = state
                .in_flight
                .iter_mut()
                .find(|request| request.generation == generation)
                .expect("usable request exists");
            let first = !request.first_result_recorded;
            if first {
                request.first_result_recorded = true;
            }
            let is_final = !matches!(completion, BatchCompletion::Partial);
            let final_result = is_final && !request.final_result_recorded;
            if final_result {
                request.final_result_recorded = true;
            }
            (first, final_result, now.saturating_sub(request.dispatched_at))
        };

        if first {
            self.push_trace(QueryTraceEvent::FirstResult {
                at: now,
                plugin: plugin.clone(),
                generation,
                latency_ms: latency,
            });
        }
        self.push_trace(QueryTraceEvent::ResultBatch {
            at: now,
            plugin: plugin.clone(),
            generation,
            items,
            completion,
        });
        if final_result {
            self.push_trace(QueryTraceEvent::FinalResult {
                at: now,
                plugin: plugin.clone(),
                generation,
                latency_ms: latency,
            });
        }
        BatchAdmission::Accepted
    }

    pub fn record_ranking(&mut self, generation: Generation, ranked_items: usize, now: Millis) {
        self.result_queue_depth = 0;
        self.push_trace(QueryTraceEvent::Ranking {
            at: now,
            generation,
            ranked_items,
        });
    }

    pub fn record_presentation(&mut self, generation: Generation, visible_items: usize, now: Millis) {
        self.result_queue_depth = 0;
        self.push_trace(QueryTraceEvent::Presentation {
            at: now,
            generation,
            visible_items,
        });
    }

    pub fn diagnostics(&self) -> SchedulerDiagnostics {
        let mut diagnostics = self.diagnostics;
        diagnostics.queued_requests = self.total_queued();
        diagnostics.in_flight_requests = self.plugins.values().map(|state| state.in_flight.len()).sum();
        diagnostics
    }

    pub fn plugin_diagnostics(&self, plugin: &PluginId) -> Option<PluginDiagnostics> {
        let state = self.plugins.get(plugin)?;
        let mut diagnostics = state.diagnostics;
        diagnostics.queued_requests = state.queue.len();
        diagnostics.in_flight_requests = state.in_flight.len();
        diagnostics.should_terminate = state.policy.profile == SchedulingProfile::LegacyStrict
            && state
                .in_flight
                .iter()
                .any(|request| request.cancel_reason.is_some());
        Some(diagnostics)
    }

    pub fn trace(&self) -> &[QueryTraceEvent] {
        &self.trace
    }

    fn process_query_for_plugin(
        &mut self,
        plugin: &PluginId,
        generation: Generation,
        query: &str,
        normalized: &str,
        now: Millis,
    ) {
        let gate = self
            .plugins
            .get(plugin)
            .and_then(|state| gate_reason(state, normalized));
        let cancellation_reason = if gate.is_some() {
            CancelReason::NoLongerRelevant
        } else {
            CancelReason::QueryChanged
        };

        let cancelled = {
            let Some(state) = self.plugins.get_mut(plugin) else {
                return;
            };
            let mut cancelled = Vec::new();
            for request in &mut state.in_flight {
                if request.cancel_reason.is_none() {
                    request.cancel_reason = Some(cancellation_reason);
                    cancelled.push((request.generation, true));
                }
            }
            if gate.is_some() {
                cancelled.extend(state.queue.drain(..).map(|request| (request.generation, true)));
                state.relevant = false;
                state.burst_started = None;
            }
            cancelled
        };
        for (cancelled_generation, newly_cancelled) in cancelled {
            self.emit_cancellation(
                plugin,
                cancelled_generation,
                cancellation_reason,
                now,
                newly_cancelled,
            );
        }

        if let Some(reason) = gate {
            self.push_trace(QueryTraceEvent::Debounce {
                at: now,
                plugin: plugin.clone(),
                generation,
                decision: DebounceDecision::Gated(reason),
            });
            return;
        }

        let prepared = {
            let state = self.plugins.get_mut(plugin).expect("registered plugin");
            let was_relevant = state.relevant;
            state.relevant = true;
            if state.policy.profile == SchedulingProfile::LegacyStrict {
                let decision = if let Some(running) = state.in_flight.first() {
                    LegacyDispatch::QueuedBehindRunning {
                        obsolete: running.generation,
                        queued: generation,
                    }
                } else if let Some(pending) = state.queue.front() {
                    LegacyDispatch::QueuedBehindRunning {
                        obsolete: pending.generation,
                        queued: generation,
                    }
                } else {
                    LegacyDispatch::Now(generation)
                };
                Some(PreparedRequest::Legacy { decision })
            } else if !was_relevant && state.policy.debounce.leading_edge {
                state.burst_started = None;
                Some(PreparedRequest::Modern {
                    ready: true,
                    due_at: None,
                    due_reason: None,
                    decision: DebounceDecision::LeadingEdge,
                })
            } else if state.policy.debounce.trailing_edge {
                let burst = *state.burst_started.get_or_insert(now);
                let trailing_at = now.saturating_add(state.policy.debounce.debounce_ms);
                let maximum_at = state
                    .policy
                    .debounce
                    .maximum_wait_ms
                    .map(|wait| burst.saturating_add(wait));
                let due_at = maximum_at.map_or(trailing_at, |maximum| trailing_at.min(maximum));
                let due_reason = if maximum_at.is_some_and(|maximum| maximum < trailing_at) {
                    DueReason::MaximumWait
                } else {
                    DueReason::TrailingEdge
                };
                Some(PreparedRequest::Modern {
                    ready: false,
                    due_at: Some(due_at),
                    due_reason: Some(due_reason),
                    decision: DebounceDecision::Deferred { until: due_at },
                })
            } else {
                None
            }
        };
        let Some(prepared) = prepared else {
            return;
        };

        let (queue_policy, queue_capacity, queue_len) = {
            let state = self.plugins.get(plugin).expect("registered plugin");
            (
                effective_queue_policy(&state.policy),
                effective_queue_capacity(&state.policy),
                state.queue.len(),
            )
        };
        let can_reuse_slot = match queue_policy {
            QueuePolicy::ReplaceOldest => queue_len > 0,
            QueuePolicy::DropOldest => queue_capacity > 0 && queue_len >= queue_capacity,
            QueuePolicy::RejectNewest => false,
        };
        if self.total_queued() >= self.config.request_queue_capacity && !can_reuse_slot {
            self.diagnostics.rejected_global_queue_full =
                self.diagnostics.rejected_global_queue_full.saturating_add(1);
            if let Some(state) = self.plugins.get_mut(plugin) {
                state.diagnostics.rejected_queue_full =
                    state.diagnostics.rejected_queue_full.saturating_add(1);
            }
            self.push_trace(QueryTraceEvent::RequestDropped {
                at: now,
                plugin: plugin.clone(),
                generation,
                policy: queue_policy,
            });
            return;
        }
        if queue_capacity == 0 || (queue_policy == QueuePolicy::RejectNewest && queue_len >= queue_capacity) {
            self.diagnostics.rejected_plugin_queue_full =
                self.diagnostics.rejected_plugin_queue_full.saturating_add(1);
            if let Some(state) = self.plugins.get_mut(plugin) {
                state.diagnostics.rejected_queue_full =
                    state.diagnostics.rejected_queue_full.saturating_add(1);
            }
            self.push_trace(QueryTraceEvent::RequestDropped {
                at: now,
                plugin: plugin.clone(),
                generation,
                policy: queue_policy,
            });
            return;
        }

        let (ready, due_at, due_reason, mut decision) = match prepared {
            PreparedRequest::Legacy { decision } => (true, None, None, EitherDecision::Legacy(decision)),
            PreparedRequest::Modern {
                ready,
                due_at,
                due_reason,
                decision,
            } => (ready, due_at, due_reason, EitherDecision::Modern(decision)),
        };

        let (superseded, evicted) = {
            let state = self.plugins.get_mut(plugin).expect("registered plugin");
            let mut superseded = None;
            let mut evicted = None;
            match queue_policy {
                QueuePolicy::ReplaceOldest => {
                    if let Some(old) = state.queue.pop_back() {
                        superseded = Some(old.generation);
                    }
                    state.queue.clear();
                }
                QueuePolicy::RejectNewest => {}
                QueuePolicy::DropOldest if state.queue.len() >= queue_capacity => {
                    evicted = state.queue.pop_front().map(|request| request.generation);
                }
                QueuePolicy::DropOldest => {}
            }
            state.queue.push_back(PendingRequest {
                generation,
                query: query.to_owned(),
                ready,
                due_at,
                due_reason,
            });
            (superseded, evicted)
        };

        if let Some(old) = superseded {
            self.diagnostics.coalesced_requests = self.diagnostics.coalesced_requests.saturating_add(1);
            if let Some(state) = self.plugins.get_mut(plugin) {
                state.diagnostics.coalesced_requests = state.diagnostics.coalesced_requests.saturating_add(1);
            }
            self.push_trace(QueryTraceEvent::RequestDropped {
                at: now,
                plugin: plugin.clone(),
                generation: old,
                policy: QueuePolicy::ReplaceOldest,
            });
            if matches!(decision, EitherDecision::Modern(_)) {
                decision = EitherDecision::Modern(DebounceDecision::Coalesced { superseded: old });
            }
        }
        if let Some(old) = evicted {
            self.diagnostics.dropped_obsolete_requests =
                self.diagnostics.dropped_obsolete_requests.saturating_add(1);
            if let Some(state) = self.plugins.get_mut(plugin) {
                state.diagnostics.dropped_obsolete_requests =
                    state.diagnostics.dropped_obsolete_requests.saturating_add(1);
            }
            self.push_trace(QueryTraceEvent::RequestDropped {
                at: now,
                plugin: plugin.clone(),
                generation: old,
                policy: QueuePolicy::DropOldest,
            });
        }

        match decision {
            EitherDecision::Legacy(decision) => self.push_trace(QueryTraceEvent::LegacyDispatch {
                at: now,
                plugin: plugin.clone(),
                generation,
                decision,
            }),
            EitherDecision::Modern(decision) => self.push_trace(QueryTraceEvent::Debounce {
                at: now,
                plugin: plugin.clone(),
                generation,
                decision,
            }),
        }
    }

    fn invalidate_plugin(
        &mut self,
        plugin: &PluginId,
        reason: CancelReason,
        now: Millis,
        include_already_cancelled: bool,
    ) -> Vec<Generation> {
        let records = {
            let Some(state) = self.plugins.get_mut(plugin) else {
                return Vec::new();
            };
            let mut records = Vec::new();
            for request in &mut state.in_flight {
                if include_already_cancelled || request.cancel_reason.is_none() {
                    let newly_cancelled = request.cancel_reason.is_none();
                    if newly_cancelled {
                        request.cancel_reason = Some(reason);
                    }
                    records.push((request.generation, newly_cancelled));
                }
            }
            records.extend(state.queue.drain(..).map(|request| (request.generation, true)));
            state.relevant = false;
            state.burst_started = None;
            records
        };

        let generations = records
            .iter()
            .map(|(generation, _)| *generation)
            .collect::<Vec<_>>();
        for (generation, newly_cancelled) in records {
            self.emit_cancellation(plugin, generation, reason, now, newly_cancelled);
        }
        generations
    }

    fn emit_cancellation(
        &mut self,
        plugin: &PluginId,
        generation: Generation,
        reason: CancelReason,
        now: Millis,
        newly_cancelled: bool,
    ) {
        if !newly_cancelled {
            return;
        }

        self.diagnostics.cancelled_requests = self.diagnostics.cancelled_requests.saturating_add(1);
        if let Some(state) = self.plugins.get_mut(plugin) {
            state.diagnostics.cancelled_requests = state.diagnostics.cancelled_requests.saturating_add(1);
        }
        self.cancellations.push(CancelledRequest {
            plugin: plugin.clone(),
            generation,
            reason,
            cancelled_at: now,
        });
        self.push_trace(QueryTraceEvent::Cancelled {
            at: now,
            plugin: plugin.clone(),
            generation,
            reason,
        });
    }

    fn reclaim_obsolete_queued(&mut self, current: Generation, now: Millis, include_replace_oldest: bool) {
        let plugins = self.plugin_order.clone();
        for plugin in plugins {
            let Some((policy, obsolete)) = self.plugins.get_mut(&plugin).and_then(|state| {
                let policy = effective_queue_policy(&state.policy);
                if !include_replace_oldest && policy == QueuePolicy::ReplaceOldest {
                    return None;
                }

                let mut obsolete = Vec::new();
                state.queue.retain(|request| {
                    if request.generation == current {
                        true
                    } else {
                        obsolete.push(request.generation);
                        false
                    }
                });
                if obsolete.is_empty() {
                    return None;
                }

                let dropped = u64::try_from(obsolete.len()).unwrap_or(u64::MAX);
                state.diagnostics.dropped_obsolete_requests = state
                    .diagnostics
                    .dropped_obsolete_requests
                    .saturating_add(dropped);
                Some((policy, obsolete))
            }) else {
                continue;
            };

            let dropped = u64::try_from(obsolete.len()).unwrap_or(u64::MAX);
            self.diagnostics.dropped_obsolete_requests =
                self.diagnostics.dropped_obsolete_requests.saturating_add(dropped);
            for generation in obsolete {
                self.push_trace(QueryTraceEvent::RequestDropped {
                    at: now,
                    plugin: plugin.clone(),
                    generation,
                    policy,
                });
            }
        }
    }

    fn record_explicit_queue_peaks(&mut self) {
        let mut explicit_depth = 0usize;
        let mut has_explicit = false;
        for state in self.plugins.values_mut() {
            if effective_queue_policy(&state.policy) != QueuePolicy::ReplaceOldest {
                has_explicit = true;
                state.diagnostics.peak_queue_depth =
                    state.diagnostics.peak_queue_depth.max(state.queue.len());
            }
            explicit_depth = explicit_depth.saturating_add(state.queue.len());
        }
        if has_explicit {
            self.diagnostics.peak_queue_depth = self.diagnostics.peak_queue_depth.max(explicit_depth);
        }
    }

    fn record_remaining_queue_peaks(&mut self) {
        let mut depth = 0usize;
        for state in self.plugins.values_mut() {
            state.diagnostics.peak_queue_depth = state.diagnostics.peak_queue_depth.max(state.queue.len());
            depth = depth.saturating_add(state.queue.len());
        }
        self.diagnostics.peak_queue_depth = self.diagnostics.peak_queue_depth.max(depth);
    }

    fn total_queued(&self) -> usize {
        self.plugins
            .values()
            .fold(0usize, |total, state| total.saturating_add(state.queue.len()))
    }

    fn push_trace(&mut self, event: QueryTraceEvent) {
        if self.config.trace_capacity == 0 {
            self.diagnostics.trace_events_dropped = self.diagnostics.trace_events_dropped.saturating_add(1);
            return;
        }
        if self.trace.len() == self.config.trace_capacity {
            self.trace.remove(0);
            self.diagnostics.trace_events_dropped = self.diagnostics.trace_events_dropped.saturating_add(1);
        }
        self.trace.push(event);
    }
}

#[derive(Debug)]
enum EitherDecision {
    Legacy(LegacyDispatch),
    Modern(DebounceDecision),
}

fn normalize_config(config: &mut SchedulerConfig) {
    config.request_queue_capacity = config.request_queue_capacity.max(1);
    config.result_queue_capacity = config.result_queue_capacity.max(1);
    config.per_plugin_dispatch_budget = config.per_plugin_dispatch_budget.max(1);
    config.dispatch_budget_per_tick = config.dispatch_budget_per_tick.max(1);
    config.trace_capacity = config.trace_capacity.max(1);
}

fn normalize_policy(policy: &mut PluginPolicy) {
    if policy.profile == SchedulingProfile::LegacyStrict {
        *policy = PluginPolicy::legacy_strict();
        return;
    }

    policy.max_concurrent_requests = policy.max_concurrent_requests.max(1);
    policy.queue_capacity = policy.queue_capacity.max(1);
    if !policy.debounce.leading_edge && !policy.debounce.trailing_edge {
        policy.debounce.trailing_edge = true;
    }
    let debounce_ms = policy.debounce.debounce_ms;
    if debounce_ms == 0 {
        policy.debounce.maximum_wait_ms = None;
    } else if let Some(maximum_wait) = &mut policy.debounce.maximum_wait_ms {
        *maximum_wait = (*maximum_wait).max(debounce_ms);
    }
    policy.activation.prefixes.retain_mut(|prefix| {
        *prefix = prefix.trim().to_lowercase();
        !prefix.is_empty()
    });
    policy.activation.keywords.retain_mut(|keyword| {
        *keyword = keyword.trim().to_lowercase();
        !keyword.is_empty()
    });
}

fn effective_concurrency_limit(policy: &PluginPolicy) -> usize {
    if policy.profile == SchedulingProfile::LegacyStrict {
        1
    } else {
        policy.max_concurrent_requests
    }
}

fn effective_queue_policy(policy: &PluginPolicy) -> QueuePolicy {
    if policy.profile == SchedulingProfile::LegacyStrict {
        QueuePolicy::ReplaceOldest
    } else {
        policy.queue_policy
    }
}

fn effective_queue_capacity(policy: &PluginPolicy) -> usize {
    if policy.profile == SchedulingProfile::LegacyStrict {
        1
    } else {
        policy.queue_capacity
    }
}

fn gate_reason(state: &PluginState, normalized: &str) -> Option<GateReason> {
    if !state.enabled {
        return Some(GateReason::Disabled);
    }
    if !state.policy.profile.allows_host_gating() {
        return None;
    }
    if normalized.is_empty() {
        return (!state.policy.activation.supports_empty_query).then_some(GateReason::EmptyQueryUnsupported);
    }
    if normalized.chars().count() < state.policy.debounce.minimum_query_length {
        return Some(GateReason::MinimumQueryLength);
    }

    let activation = &state.policy.activation;
    if activation.prefixes.is_empty() && activation.keywords.is_empty() {
        return None;
    }
    let prefix_matches = activation
        .prefixes
        .iter()
        .any(|prefix| normalized.starts_with(prefix));
    let keyword_matches = normalized
        .split_whitespace()
        .next()
        .is_some_and(|first| activation.keywords.iter().any(|keyword| first == keyword));
    if prefix_matches || keyword_matches {
        None
    } else if activation.prefixes.is_empty() {
        Some(GateReason::KeywordMismatch)
    } else {
        Some(GateReason::PrefixMismatch)
    }
}
