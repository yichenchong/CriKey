use std::collections::{HashMap, VecDeque};

use crikey_core::{Generation, PluginId};

use crate::{BatchState, RejectReason, ResultAggregator, ResultBatch};

/// Boundary-wide resident-work limits. `capacity_batches` also bounds the recent-event ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueLimits {
    pub capacity_batches: usize,
    pub capacity_items: usize,
}

/// The explicitly named overflow behavior for one producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowPolicy {
    RejectLowPriority,
    PauseProducer,
    ReplaceOldest,
    Disconnect,
}

/// Per-producer resident-work limits and normalized backpressure watermarks.
/// Registration enforces `1 <= pause_at_batches <= capacity_batches` and
/// `resume_at_batches < pause_at_batches`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntakePolicy {
    pub capacity_batches: usize,
    pub capacity_items: usize,
    pub pause_at_batches: usize,
    pub resume_at_batches: usize,
    pub overflow: OverflowPolicy,
}

/// Importance assigned by the transport to an inbound publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchPriority {
    High,
    Low,
}

/// A result batch together with its transport priority.
#[derive(Debug, Clone)]
pub struct InboundBatch {
    pub batch: ResultBatch,
    pub priority: BatchPriority,
}

/// Backpressure state exposed to a producer transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProducerState {
    Running,
    Paused,
    Disconnected,
}

/// Why an inbound publication was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueReject {
    Unregistered,
    StaleGeneration,
    StreamTerminated,
    LowPriorityShed,
    QueueFull,
    BoundaryFull,
    Disconnected,
}

/// Resident queue depth. `obsolete` is a batch count and is included in
/// `batches` and `items` until lazy reclamation removes it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueDepth {
    pub batches: usize,
    pub items: usize,
    pub obsolete: usize,
}

/// Work allowed during one fair drain pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainBudget {
    pub batches_per_plugin: usize,
    pub items_per_plugin: usize,
    pub total_batches: usize,
}

/// Identity and lifecycle metadata for a batch the aggregator actually merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedBatch {
    pub plugin: PluginId,
    /// Millisecond timestamp passed to `submit` when this batch was admitted.
    pub admitted_at_ms: u64,
    pub generation: Generation,
    pub state: BatchState,
    pub items: usize,
}

/// Outcome of one drain pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrainReport {
    pub merged: usize,
    /// Successful merges in drain order, suitable for committing downstream lifecycle traces.
    pub merged_batches: Vec<MergedBatch>,
    pub dropped_obsolete: usize,
    pub merge_rejected: Vec<(PluginId, RejectReason)>,
}

/// One timestamped intake-boundary decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEvent {
    pub at_ms: u64,
    pub plugin: PluginId,
    pub generation: Generation,
    pub kind: QueueEventKind,
}

/// Kind-specific payload for a queue event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueEventKind {
    Admitted { items: usize },
    Rejected(QueueReject),
    DroppedObsolete { batches: usize, items: usize },
    EvictedOldest { items: usize },
    Merged { items: usize },
    MergeRejected(RejectReason),
    ProducerPaused,
    ProducerResumed,
}

const QUEUE_REJECT_COUNT: usize = 7;
const MERGE_REJECT_COUNT: usize = 6;

/// Cumulative counters for intake and merge decisions.
#[derive(Debug, Clone)]
pub struct QueueDiagnostics {
    admitted: usize,
    merged: usize,
    dropped_obsolete: usize,
    evicted_oldest: usize,
    rejected: [usize; QUEUE_REJECT_COUNT],
    merge_rejected: [usize; MERGE_REJECT_COUNT],
    pauses: usize,
    resumes: usize,
    events_dropped: usize,
    peak_batches: usize,
    peak_items: usize,
}

impl Default for QueueDiagnostics {
    fn default() -> Self {
        Self {
            admitted: 0,
            merged: 0,
            dropped_obsolete: 0,
            evicted_oldest: 0,
            rejected: [0; QUEUE_REJECT_COUNT],
            merge_rejected: [0; MERGE_REJECT_COUNT],
            pauses: 0,
            events_dropped: 0,
            resumes: 0,
            peak_batches: 0,
            peak_items: 0,
        }
    }
}

impl QueueDiagnostics {
    pub fn admitted(&self) -> usize {
        self.admitted
    }

    pub fn merged(&self) -> usize {
        self.merged
    }

    pub fn dropped_obsolete(&self) -> usize {
        self.dropped_obsolete
    }

    pub fn evicted_oldest(&self) -> usize {
        self.evicted_oldest
    }

    pub fn rejected(&self, reason: QueueReject) -> usize {
        self.rejected[queue_reject_index(reason)]
    }

    pub fn merge_rejected(&self, reason: RejectReason) -> usize {
        self.merge_rejected[merge_reject_index(reason)]
    }

    pub fn pauses(&self) -> usize {
        self.pauses
    }

    pub fn resumes(&self) -> usize {
        self.resumes
    }

    /// Events evicted from (or refused by) the bounded recent-event ring.
    pub fn events_dropped(&self) -> usize {
        self.events_dropped
    }

    pub fn peak_batches(&self) -> usize {
        self.peak_batches
    }

    pub fn peak_items(&self) -> usize {
        self.peak_items
    }
}

fn queue_reject_index(reason: QueueReject) -> usize {
    match reason {
        QueueReject::Unregistered => 0,
        QueueReject::StaleGeneration => 1,
        QueueReject::StreamTerminated => 2,
        QueueReject::LowPriorityShed => 3,
        QueueReject::QueueFull => 4,
        QueueReject::BoundaryFull => 5,
        QueueReject::Disconnected => 6,
    }
}

fn merge_reject_index(reason: RejectReason) -> usize {
    match reason {
        RejectReason::StaleGeneration => 0,
        RejectReason::QuotaExceeded => 1,
        RejectReason::PayloadTooLarge => 2,
        RejectReason::OwnerMismatch => 3,
        RejectReason::StreamTerminated => 4,
        RejectReason::PluginSuspended => 5,
    }
}

#[derive(Debug)]
struct Entry {
    admitted_at_ms: u64,
    inbound: InboundBatch,
}

#[derive(Debug)]
struct PluginQueue {
    policy: IntakePolicy,
    entries: VecDeque<Entry>,
    items: usize,
    state: ProducerState,
    terminal_generation: Option<Generation>,
}

impl PluginQueue {
    fn new(policy: IntakePolicy) -> Self {
        Self {
            policy,
            entries: VecDeque::new(),
            items: 0,
            state: ProducerState::Running,
            terminal_generation: None,
        }
    }
}

/// Bounded, per-plugin fair intake boundary in front of a result aggregator.
#[derive(Debug)]
pub struct InboundResultQueue {
    limits: QueueLimits,
    retirement_floor: Generation,
    active: Option<Generation>,
    plugins: HashMap<PluginId, PluginQueue>,
    order: Vec<PluginId>,
    drain_cursor: usize,
    total_batches: usize,
    total_items: usize,
    diagnostics: QueueDiagnostics,
    events: VecDeque<QueueEvent>,
}

impl InboundResultQueue {
    pub fn new(limits: QueueLimits) -> Self {
        Self {
            limits,
            retirement_floor: Generation::ZERO,
            active: None,
            plugins: HashMap::new(),
            order: Vec::new(),
            drain_cursor: 0,
            total_batches: 0,
            total_items: 0,
            diagnostics: QueueDiagnostics::default(),
            events: VecDeque::new(),
        }
    }

    /// Installs or replaces a normalized producer policy. Pending terminal and
    /// hysteresis state survive re-registration; only this call revives a disconnect.
    pub fn register(&mut self, plugin: PluginId, policy: IntakePolicy) {
        let policy = normalize_policy(policy);
        let active = self.active;
        if let Some(queue) = self.plugins.get_mut(&plugin) {
            let depth = queue
                .entries
                .iter()
                .filter(|entry| active == Some(entry.inbound.batch.generation))
                .count();
            queue.policy = policy;
            let connected_state = match queue.state {
                ProducerState::Disconnected => ProducerState::Running,
                state => state,
            };
            queue.state = match connected_state {
                ProducerState::Running if depth >= policy.pause_at_batches => ProducerState::Paused,
                ProducerState::Paused if depth <= policy.resume_at_batches => ProducerState::Running,
                state => state,
            };
            return;
        }

        self.order.push(plugin.clone());
        self.plugins.insert(plugin, PluginQueue::new(policy));
    }

    /// Selects the sole generation admissible at the boundary. Resident work
    /// is deliberately left in place for lazy reclamation.
    pub fn begin_generation(&mut self, generation: Generation) {
        if generation < self.retirement_floor {
            return;
        }
        if self.active.is_some_and(|active| generation <= active) {
            return;
        }
        self.active = Some(generation);
    }

    /// Retires every generation below `generation` without walking queues.
    pub fn retire_before(&mut self, generation: Generation) {
        if generation <= self.retirement_floor {
            return;
        }
        self.retirement_floor = generation;
        if self.active.is_some_and(|active| active < generation) {
            self.active = None;
        }
    }

    pub fn depth(&self) -> QueueDepth {
        let obsolete = self
            .plugins
            .values()
            .map(|queue| self.obsolete_batches(queue))
            .sum();
        QueueDepth {
            batches: self.total_batches,
            items: self.total_items,
            obsolete,
        }
    }

    pub fn plugin_depth(&self, plugin: &PluginId) -> QueueDepth {
        let Some(queue) = self.plugins.get(plugin) else {
            return QueueDepth::default();
        };
        QueueDepth {
            batches: queue.entries.len(),
            items: queue.items,
            obsolete: self.obsolete_batches(queue),
        }
    }

    pub fn producer_state(&self, plugin: &PluginId) -> Option<ProducerState> {
        self.plugins.get(plugin).map(|queue| queue.state)
    }

    pub fn diagnostics(&self) -> &QueueDiagnostics {
        &self.diagnostics
    }

    /// Removes the retained recent events in decision order.
    pub fn take_events(&mut self) -> Vec<QueueEvent> {
        self.events.drain(..).collect()
    }

    /// Atomically admits a whole batch or reports the transport-side reason it
    /// was refused. Staleness is checked before terminal or overflow state.
    pub fn submit(&mut self, at_ms: u64, inbound: InboundBatch) -> Result<ProducerState, QueueReject> {
        let plugin = inbound.batch.plugin.clone();
        let generation = inbound.batch.generation;
        let item_count = inbound.batch.items.len();

        if !self.plugins.contains_key(&plugin) {
            return Err(self.reject(at_ms, plugin, generation, QueueReject::Unregistered));
        }
        if self.active != Some(generation) {
            return Err(self.reject(at_ms, plugin, generation, QueueReject::StaleGeneration));
        }

        let (state, terminal) = {
            let queue = self.plugins.get(&plugin).expect("registered plugin disappeared");
            (queue.state, queue.terminal_generation)
        };
        if state == ProducerState::Disconnected {
            return Err(self.reject(at_ms, plugin, generation, QueueReject::Disconnected));
        }
        if terminal == Some(generation) {
            return Err(self.reject(at_ms, plugin, generation, QueueReject::StreamTerminated));
        }

        if self.admission_needs_reclamation(&plugin, &inbound) {
            self.reclaim_obsolete(at_ms);
        }

        let low_at_watermark = {
            let queue = self.plugins.get(&plugin).expect("registered plugin disappeared");
            queue.policy.overflow == OverflowPolicy::RejectLowPriority
                && inbound.priority == BatchPriority::Low
                && queue.entries.len() >= queue.policy.pause_at_batches
        };
        if low_at_watermark {
            return Err(self.reject(at_ms, plugin, generation, QueueReject::LowPriorityShed));
        }

        let (per_plugin_full, boundary_full) = self.capacity_violations(&plugin, item_count);
        if per_plugin_full || boundary_full {
            let overflow = self
                .plugins
                .get(&plugin)
                .expect("registered plugin disappeared")
                .policy
                .overflow;

            match overflow {
                OverflowPolicy::ReplaceOldest => {
                    if let Err(reason) = self.replacement_preflight(&plugin, item_count) {
                        return Err(self.reject(at_ms, plugin, generation, reason));
                    }
                    while {
                        let (plugin_full, all_full) = self.capacity_violations(&plugin, item_count);
                        plugin_full || all_full
                    } {
                        self.evict_oldest(at_ms, &plugin);
                    }
                }
                OverflowPolicy::Disconnect if per_plugin_full => {
                    self.plugins
                        .get_mut(&plugin)
                        .expect("registered plugin disappeared")
                        .state = ProducerState::Disconnected;
                    return Err(self.reject(at_ms, plugin, generation, QueueReject::Disconnected));
                }
                OverflowPolicy::PauseProducer => {
                    let reason = if per_plugin_full {
                        QueueReject::QueueFull
                    } else {
                        QueueReject::BoundaryFull
                    };
                    let rejection = self.reject(at_ms, plugin.clone(), generation, reason);
                    self.force_pause(at_ms, &plugin, generation);
                    return Err(rejection);
                }
                OverflowPolicy::RejectLowPriority | OverflowPolicy::Disconnect => {
                    let reason = if per_plugin_full {
                        QueueReject::QueueFull
                    } else {
                        QueueReject::BoundaryFull
                    };
                    return Err(self.reject(at_ms, plugin, generation, reason));
                }
            }
        }

        let terminal = inbound.batch.state.is_terminal();
        {
            let queue = self
                .plugins
                .get_mut(&plugin)
                .expect("registered plugin disappeared");
            queue.items += item_count;
            if terminal {
                queue.terminal_generation = Some(generation);
            }
            queue.entries.push_back(Entry {
                admitted_at_ms: at_ms,
                inbound,
            });
        }
        self.total_batches += 1;
        self.total_items += item_count;
        self.diagnostics.admitted = self.diagnostics.admitted.saturating_add(1);
        self.diagnostics.peak_batches = self.diagnostics.peak_batches.max(self.total_batches);
        self.diagnostics.peak_items = self.diagnostics.peak_items.max(self.total_items);
        self.push_event(QueueEvent {
            at_ms,
            plugin: plugin.clone(),
            generation,
            kind: QueueEventKind::Admitted { items: item_count },
        });
        self.reconcile_producer(at_ms, &plugin, generation);

        Ok(self
            .plugins
            .get(&plugin)
            .expect("registered plugin disappeared")
            .state)
    }

    /// Drains one rotating round-robin pass. Obsolete work is removed before
    /// any batch can reach the aggregator and does not consume merge budget.
    pub fn drain_into(
        &mut self,
        at_ms: u64,
        aggregator: &mut impl ResultAggregator,
        budget: DrainBudget,
    ) -> DrainReport {
        let mut report = DrainReport {
            dropped_obsolete: self.reclaim_obsolete(at_ms),
            ..DrainReport::default()
        };
        let plugin_count = self.order.len();
        if plugin_count == 0 {
            return report;
        }

        let start = self.drain_cursor % plugin_count;
        let mut total_handled = 0usize;
        let mut last_handled = None;

        for offset in 0..plugin_count {
            if total_handled >= budget.total_batches {
                break;
            }
            let index = (start + offset) % plugin_count;
            let plugin = self.order[index].clone();
            let mut plugin_handled = 0usize;
            let mut plugin_items = 0usize;

            loop {
                if total_handled >= budget.total_batches || plugin_handled >= budget.batches_per_plugin {
                    break;
                }

                let Some(next_items) = self
                    .plugins
                    .get(&plugin)
                    .and_then(|queue| queue.entries.front())
                    .map(|entry| entry.inbound.batch.items.len())
                else {
                    break;
                };
                if plugin_handled > 0 && next_items > budget.items_per_plugin.saturating_sub(plugin_items) {
                    break;
                }

                let entry = {
                    let queue = self
                        .plugins
                        .get_mut(&plugin)
                        .expect("registered plugin disappeared");
                    let entry = queue.entries.pop_front().expect("front entry disappeared");
                    queue.items -= next_items;
                    entry
                };
                self.total_batches -= 1;
                self.total_items -= next_items;
                plugin_handled += 1;
                plugin_items = plugin_items.saturating_add(next_items);
                total_handled += 1;
                last_handled = Some(index);

                let generation = entry.inbound.batch.generation;
                let state = entry.inbound.batch.state;
                match aggregator.accept(entry.inbound.batch) {
                    Ok(()) => {
                        report.merged += 1;
                        report.merged_batches.push(MergedBatch {
                            plugin: plugin.clone(),
                            admitted_at_ms: entry.admitted_at_ms,
                            generation,
                            state,
                            items: next_items,
                        });
                        self.diagnostics.merged = self.diagnostics.merged.saturating_add(1);
                        self.push_event(QueueEvent {
                            at_ms,
                            plugin: plugin.clone(),
                            generation,
                            kind: QueueEventKind::Merged { items: next_items },
                        });
                    }
                    Err(reason) => {
                        // A terminal is provisional until the retained merge commits.
                        if state.is_terminal() {
                            let queue = self
                                .plugins
                                .get_mut(&plugin)
                                .expect("registered plugin disappeared");
                            if queue.terminal_generation == Some(generation) {
                                queue.terminal_generation = None;
                            }
                        }
                        report.merge_rejected.push((plugin.clone(), reason));
                        let slot = merge_reject_index(reason);
                        self.diagnostics.merge_rejected[slot] =
                            self.diagnostics.merge_rejected[slot].saturating_add(1);
                        self.push_event(QueueEvent {
                            at_ms,
                            plugin: plugin.clone(),
                            generation,
                            kind: QueueEventKind::MergeRejected(reason),
                        });
                    }
                }
            }
        }

        if let Some(index) = last_handled {
            self.drain_cursor = if total_handled >= budget.total_batches {
                (index + 1) % plugin_count
            } else {
                (start + 1) % plugin_count
            };
        } else if budget.total_batches > 0 {
            self.drain_cursor = (start + 1) % plugin_count;
        }

        let transition_generation = self.active.unwrap_or(self.retirement_floor);
        for index in 0..plugin_count {
            let plugin = self.order[index].clone();
            self.reconcile_producer(at_ms, &plugin, transition_generation);
        }

        report
    }

    fn obsolete_batches(&self, queue: &PluginQueue) -> usize {
        queue
            .entries
            .iter()
            .filter(|entry| self.active != Some(entry.inbound.batch.generation))
            .count()
    }

    fn admission_needs_reclamation(&self, plugin: &PluginId, inbound: &InboundBatch) -> bool {
        let queue = self.plugins.get(plugin).expect("registered plugin disappeared");
        let item_count = inbound.batch.items.len();
        let low_at_watermark = queue.policy.overflow == OverflowPolicy::RejectLowPriority
            && inbound.priority == BatchPriority::Low
            && queue.entries.len() >= queue.policy.pause_at_batches;
        let (plugin_full, boundary_full) = self.capacity_violations(plugin, item_count);
        low_at_watermark || plugin_full || boundary_full
    }

    fn capacity_violations(&self, plugin: &PluginId, item_count: usize) -> (bool, bool) {
        let queue = self.plugins.get(plugin).expect("registered plugin disappeared");
        let plugin_full = exceeds(queue.entries.len(), 1, queue.policy.capacity_batches)
            || exceeds(queue.items, item_count, queue.policy.capacity_items);
        let boundary_full = exceeds(self.total_batches, 1, self.limits.capacity_batches)
            || exceeds(self.total_items, item_count, self.limits.capacity_items);
        (plugin_full, boundary_full)
    }

    fn replacement_preflight(&self, plugin: &PluginId, item_count: usize) -> Result<(), QueueReject> {
        let queue = self.plugins.get(plugin).expect("registered plugin disappeared");
        if exceeds(0, 1, queue.policy.capacity_batches) || exceeds(0, item_count, queue.policy.capacity_items)
        {
            return Err(QueueReject::QueueFull);
        }

        let other_batches = self.total_batches - queue.entries.len();
        let other_items = self.total_items - queue.items;
        if exceeds(other_batches, 1, self.limits.capacity_batches)
            || exceeds(other_items, item_count, self.limits.capacity_items)
        {
            return Err(QueueReject::BoundaryFull);
        }
        Ok(())
    }

    fn evict_oldest(&mut self, at_ms: u64, plugin: &PluginId) {
        let entry = {
            let queue = self
                .plugins
                .get_mut(plugin)
                .expect("registered plugin disappeared");
            let entry = queue
                .entries
                .pop_front()
                .expect("replacement preflight guaranteed an evictable entry");
            queue.items -= entry.inbound.batch.items.len();
            entry
        };
        let item_count = entry.inbound.batch.items.len();
        self.total_batches -= 1;
        self.total_items -= item_count;
        self.diagnostics.evicted_oldest = self.diagnostics.evicted_oldest.saturating_add(1);
        self.push_event(QueueEvent {
            at_ms,
            plugin: plugin.clone(),
            generation: entry.inbound.batch.generation,
            kind: QueueEventKind::EvictedOldest { items: item_count },
        });
    }

    fn reclaim_obsolete(&mut self, at_ms: u64) -> usize {
        let active = self.active;
        let mut records: Vec<(PluginId, Generation, usize, usize)> = Vec::new();
        let mut total_dropped = 0usize;

        for index in 0..self.order.len() {
            let plugin = self.order[index].clone();
            loop {
                let obsolete = self
                    .plugins
                    .get(&plugin)
                    .and_then(|queue| queue.entries.front())
                    .is_some_and(|entry| active != Some(entry.inbound.batch.generation));
                if !obsolete {
                    break;
                }

                let entry = {
                    let queue = self
                        .plugins
                        .get_mut(&plugin)
                        .expect("registered plugin disappeared");
                    let entry = queue
                        .entries
                        .pop_front()
                        .expect("obsolete front entry disappeared");
                    queue.items -= entry.inbound.batch.items.len();
                    entry
                };
                let generation = entry.inbound.batch.generation;
                let item_count = entry.inbound.batch.items.len();
                self.total_batches -= 1;
                self.total_items -= item_count;
                total_dropped += 1;

                if let Some((last_plugin, last_generation, batches, items)) = records.last_mut() {
                    if last_plugin == &plugin && *last_generation == generation {
                        *batches += 1;
                        *items += item_count;
                        continue;
                    }
                }
                records.push((plugin.clone(), generation, 1, item_count));
            }
        }

        self.diagnostics.dropped_obsolete = self.diagnostics.dropped_obsolete.saturating_add(total_dropped);
        for (plugin, generation, batches, items) in records {
            self.push_event(QueueEvent {
                at_ms,
                plugin,
                generation,
                kind: QueueEventKind::DroppedObsolete { batches, items },
            });
        }
        total_dropped
    }

    fn push_event(&mut self, event: QueueEvent) {
        let capacity = self.limits.capacity_batches;
        if capacity == 0 {
            self.diagnostics.events_dropped = self.diagnostics.events_dropped.saturating_add(1);
            return;
        }
        if self.events.len() >= capacity {
            self.events.pop_front();
            self.diagnostics.events_dropped = self.diagnostics.events_dropped.saturating_add(1);
        }
        self.events.push_back(event);
    }

    fn reject(
        &mut self,
        at_ms: u64,
        plugin: PluginId,
        generation: Generation,
        reason: QueueReject,
    ) -> QueueReject {
        let slot = queue_reject_index(reason);
        self.diagnostics.rejected[slot] = self.diagnostics.rejected[slot].saturating_add(1);
        self.push_event(QueueEvent {
            at_ms,
            plugin,
            generation,
            kind: QueueEventKind::Rejected(reason),
        });
        reason
    }

    fn force_pause(&mut self, at_ms: u64, plugin: &PluginId, generation: Generation) {
        let queue = self
            .plugins
            .get_mut(plugin)
            .expect("registered plugin disappeared");
        if queue.state != ProducerState::Running {
            return;
        }
        queue.state = ProducerState::Paused;
        self.diagnostics.pauses = self.diagnostics.pauses.saturating_add(1);
        self.push_event(QueueEvent {
            at_ms,
            plugin: plugin.clone(),
            generation,
            kind: QueueEventKind::ProducerPaused,
        });
    }

    fn reconcile_producer(&mut self, at_ms: u64, plugin: &PluginId, generation: Generation) {
        let transition = {
            let queue = self.plugins.get(plugin).expect("registered plugin disappeared");
            match queue.state {
                ProducerState::Running if queue.entries.len() >= queue.policy.pause_at_batches => {
                    Some(ProducerState::Paused)
                }
                ProducerState::Paused if queue.entries.len() <= queue.policy.resume_at_batches => {
                    Some(ProducerState::Running)
                }
                ProducerState::Running | ProducerState::Paused | ProducerState::Disconnected => None,
            }
        };
        let Some(state) = transition else {
            return;
        };

        self.plugins
            .get_mut(plugin)
            .expect("registered plugin disappeared")
            .state = state;
        let kind = match state {
            ProducerState::Paused => {
                self.diagnostics.pauses = self.diagnostics.pauses.saturating_add(1);
                QueueEventKind::ProducerPaused
            }
            ProducerState::Running => {
                self.diagnostics.resumes = self.diagnostics.resumes.saturating_add(1);
                QueueEventKind::ProducerResumed
            }
            ProducerState::Disconnected => unreachable!("disconnect is not a watermark transition"),
        };
        self.push_event(QueueEvent {
            at_ms,
            plugin: plugin.clone(),
            generation,
            kind,
        });
    }
}

fn normalize_policy(mut policy: IntakePolicy) -> IntakePolicy {
    policy.capacity_batches = policy.capacity_batches.max(1);
    policy.pause_at_batches = policy.pause_at_batches.clamp(1, policy.capacity_batches);
    policy.resume_at_batches = policy.resume_at_batches.min(policy.pause_at_batches - 1);
    policy
}

fn exceeds(current: usize, additional: usize, limit: usize) -> bool {
    additional > limit.saturating_sub(current)
}
