//! Result aggregation (spec 11.5, 11.6, 11.7, 12).
//!
//! M1 holds exactly one query generation on screen. [`MemoryResultAggregator`]
//! merges every batch tagged with that generation into a single retained list:
//! deduplicated by `(PluginId, ItemId)`, ordered by first acceptance, bounded by
//! safety limits of spec 11.7, and published to the UI as coalesced whole-list
//! snapshots subject to the configured per-frame update budget.
//!
//! No ranking score reaches this boundary yet, so the aggregator never sorts.
//! Incremental reranking (spec 11.5) layers on top of this order later; until
//! then arrival order is the deterministic tie-break that keeps rows from
//! moving under the user's selection (spec 11.6).

mod intake;

pub use intake::{
    BatchPriority, DrainBudget, DrainReport, InboundBatch, InboundResultQueue, IntakePolicy, MergedBatch,
    OverflowPolicy, ProducerState, QueueDepth, QueueDiagnostics, QueueEvent, QueueEventKind, QueueLimits,
    QueueReject,
};

use std::collections::{HashMap, HashSet};

use crikey_core::{Generation, Item, ItemId, PluginId};

/// Completion state reported by a modern plugin batch (spec 12.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchState {
    Partial,
    Final,
    Cancelled,
    Failed,
}

impl BatchState {
    fn is_terminal(self) -> bool {
        !matches!(self, Self::Partial)
    }
}

/// One inbound contribution to a query generation.
#[derive(Debug, Clone)]
pub struct ResultBatch {
    pub generation: Generation,
    pub plugin: PluginId,
    pub state: BatchState,
    pub items: Vec<Item>,
}

/// Why a batch was not merged. All of these are normal operating conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Result belongs to an obsolete query generation (spec 8.1).
    StaleGeneration,
    /// Plugin exceeded an item-count safety limit (spec 11.7).
    QuotaExceeded,
    /// An icon-reference or metadata payload exceeded its byte limit.
    PayloadTooLarge,
    /// An item claimed an owner other than the plugin submitting the batch.
    OwnerMismatch,
    /// The plugin already ended its stream for this generation.
    StreamTerminated,
    /// The plugin is suspended by the circuit breaker.
    PluginSuspended,
}

/// Safety limits. Legacy limits are configured separately and set high enough
/// to preserve ordinary compatibility (spec 11.7).
#[derive(Debug, Clone, Copy)]
pub struct ResultLimits {
    pub max_items_per_batch: usize,
    pub max_items_per_plugin_per_query: usize,
    pub max_items_per_query: usize,
    /// Combined icon-reference bytes accepted in one batch.
    pub max_icon_reference_bytes_per_batch: usize,
    /// Combined metadata key and value bytes accepted in one batch.
    pub max_metadata_bytes_per_batch: usize,
    pub max_ui_updates_per_frame: usize,
}

impl Default for ResultLimits {
    fn default() -> Self {
        Self {
            max_items_per_batch: 50,
            max_items_per_plugin_per_query: 250,
            max_items_per_query: 2_000,
            max_icon_reference_bytes_per_batch: 256 * 1024,
            max_metadata_bytes_per_batch: 1024 * 1024,
            max_ui_updates_per_frame: 1,
        }
    }
}

pub trait ResultAggregator {
    /// Merges a batch, or reports why it was discarded.
    fn accept(&mut self, batch: ResultBatch) -> Result<(), RejectReason>;
    /// Drops all state for generations older than `generation`.
    fn retire_before(&mut self, generation: Generation);
}

/// In-memory aggregator for the one query generation currently on screen
/// (spec 8.1, 11.5 - 11.7).
///
/// Merging is append-or-replace: a `(PluginId, ItemId)` pair seen for the first
/// time is appended, while an already retained pair is overwritten in place.
/// The vector therefore stays in first-acceptance order for the whole
/// generation, and the hash index keeps duplicate lookup O(1) average without
/// ever dictating that order.
///
/// Every limit of spec 11.7 is checked against the *whole* batch before
/// anything is mutated, so a rejected batch leaves items, quota counters,
/// stream state and the pending snapshot exactly as they were.
#[derive(Debug)]
pub struct MemoryResultAggregator {
    limits: ResultLimits,
    /// Generations below this exclusive retirement bound can never reactivate.
    retirement_floor: Generation,
    /// The only generation whose batches may merge. `None` before the first
    /// `begin_generation`, and again once the active generation is retired.
    active: Option<Generation>,
    /// Retained items, in first-acceptance order.
    items: Vec<Item>,
    /// Submitting plugin and stable id to their position in `items`.
    index: HashMap<PluginId, HashMap<ItemId, usize>>,
    /// Composite identities each submitting plugin caused to be retained this
    /// generation. Enrichment of an existing identity consumes no extra quota.
    per_plugin: HashMap<PluginId, usize>,
    /// Most recent accepted stream state for each plugin this generation.
    batch_states: HashMap<PluginId, BatchState>,
    /// At most one whole-list repaint is ever owed to the UI (spec 11.7).
    pending_ui_update: bool,
    /// Number of snapshots the UI may still take in the current frame.
    ui_updates_remaining: usize,
}

impl MemoryResultAggregator {
    /// Builds an aggregator with no active generation: every batch is
    /// [`RejectReason::StaleGeneration`] until
    /// [`begin_generation`](Self::begin_generation) names one.
    pub fn new(limits: ResultLimits) -> Self {
        Self {
            ui_updates_remaining: limits.max_ui_updates_per_frame,
            limits,
            retirement_floor: Generation::ZERO,
            active: None,
            items: Vec::new(),
            index: HashMap::new(),
            per_plugin: HashMap::new(),
            batch_states: HashMap::new(),
            pending_ui_update: false,
        }
    }

    /// Makes `generation` the one mergeable generation and drops everything a
    /// previous generation left behind: items, quota counters, stream states
    /// and any snapshot the UI never drained. Obsolete results are never
    /// displayed (spec 8.1).
    ///
    /// Emptying the list is itself a visible change, so exactly one empty
    /// snapshot is scheduled; it replaces any snapshot still pending, which
    /// could otherwise leak retired items into the new query.
    ///
    /// Repeating the active generation is idempotent. A generation older than
    /// either the active generation or the retirement floor is ignored, so
    /// stale work can never erase or reactivate visible state.
    pub fn begin_generation(&mut self, generation: Generation) {
        if generation < self.retirement_floor {
            return;
        }
        if let Some(active) = self.active {
            if generation <= active {
                return;
            }
        }

        self.active = Some(generation);
        self.reset();
        self.pending_ui_update = true;
    }

    /// The retained set, in first-acceptance order. An enrichment update
    /// replaces its item in place and never moves it (spec 11.5, 11.6).
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// Most recent accepted state for `plugin` in the active generation.
    pub fn plugin_state(&self, plugin: &PluginId) -> Option<BatchState> {
        self.batch_states.get(plugin).copied()
    }

    /// Starts a UI frame and replenishes its snapshot budget.
    ///
    /// A pending repaint withheld after the preceding frame exhausted its
    /// budget remains coalesced and becomes available in this frame.
    pub fn begin_frame(&mut self) {
        self.ui_updates_remaining = self.limits.max_ui_updates_per_frame;
    }

    /// Takes the whole-list snapshot owed to the UI, if there is one and this
    /// frame still has update budget.
    ///
    /// Accepts coalesce: however many batches merged since the last successful
    /// call, the UI is offered a single newest snapshot, equal to
    /// [`items`](Self::items) at the moment it is taken. A withheld repaint
    /// remains pending until a later [`begin_frame`](Self::begin_frame).
    pub fn take_ui_update(&mut self) -> Option<Vec<Item>> {
        if !self.pending_ui_update || self.ui_updates_remaining == 0 {
            return None;
        }
        self.ui_updates_remaining -= 1;
        self.pending_ui_update = false;
        Some(self.items.clone())
    }

    /// Drops all state belonging to the active generation.
    fn reset(&mut self) {
        self.items.clear();
        self.index.clear();
        self.per_plugin.clear();
        self.batch_states.clear();
    }

    /// Validates ownership and computes bounded payload byte totals without
    /// mutating any aggregator state.
    fn validate_payloads(&self, plugin: &PluginId, items: &[Item]) -> Result<(), RejectReason> {
        if items.iter().any(|item| &item.plugin_id != plugin) {
            return Err(RejectReason::OwnerMismatch);
        }

        let mut icon_reference_bytes = 0usize;
        let mut metadata_bytes = 0usize;
        for item in items {
            icon_reference_bytes = icon_reference_bytes
                .checked_add(item.icon_reference.as_ref().map_or(0, String::len))
                .ok_or(RejectReason::PayloadTooLarge)?;
            if icon_reference_bytes > self.limits.max_icon_reference_bytes_per_batch {
                return Err(RejectReason::PayloadTooLarge);
            }

            for (key, value) in &item.metadata {
                metadata_bytes = metadata_bytes
                    .checked_add(key.len())
                    .and_then(|bytes| bytes.checked_add(value.len()))
                    .ok_or(RejectReason::PayloadTooLarge)?;
                if metadata_bytes > self.limits.max_metadata_bytes_per_batch {
                    return Err(RejectReason::PayloadTooLarge);
                }
            }
        }

        Ok(())
    }

    /// Counts composite identities this batch would newly retain. Repeats
    /// within the batch and identities already retained are enrichment, so
    /// they consume no additional retained-identity quota.
    fn count_new_identities(&self, plugin: &PluginId, items: &[Item]) -> usize {
        let retained = self.index.get(plugin);
        let mut seen = HashSet::with_capacity(items.len());
        let mut new_identities = 0;

        for item in items {
            if seen.insert(&item.stable_id)
                && retained.is_none_or(|index| !index.contains_key(&item.stable_id))
            {
                new_identities += 1;
            }
        }

        new_identities
    }

    /// Applies a batch whose ownership, payload sizes and quotas have already
    /// been cleared.
    fn merge(&mut self, plugin: &PluginId, items: Vec<Item>) {
        let index = self.index.entry(plugin.clone()).or_default();
        for item in items {
            if let Some(slot) = index.get(&item.stable_id).copied() {
                if let Some(retained) = self.items.get_mut(slot) {
                    // Overwrite in place: a slower enrichment pass must not
                    // reorder a row the user may already have selected.
                    *retained = item;
                }
            } else {
                index.insert(item.stable_id.clone(), self.items.len());
                self.items.push(item);
            }
        }
    }
}

impl ResultAggregator for MemoryResultAggregator {
    /// Merges a batch belonging to the active generation.
    ///
    /// A terminal batch merges its items before ending that plugin's stream.
    /// Later traffic from the plugin is rejected until a newer generation
    /// begins. Completion never discards results already retained (spec 12.5).
    ///
    /// A batch that would breach an ownership, payload, per-batch,
    /// per-plugin-per-query or per-query limit is rejected whole - never
    /// truncated to fit - so nothing is merged, no quota or terminal state is
    /// consumed and no repaint is scheduled (spec 11.7).
    fn accept(&mut self, batch: ResultBatch) -> Result<(), RejectReason> {
        let ResultBatch {
            generation,
            plugin,
            state,
            items,
        } = batch;

        if self.active != Some(generation) {
            return Err(RejectReason::StaleGeneration);
        }
        if self
            .batch_states
            .get(&plugin)
            .copied()
            .is_some_and(BatchState::is_terminal)
        {
            return Err(RejectReason::StreamTerminated);
        }
        if items.len() > self.limits.max_items_per_batch {
            return Err(RejectReason::QuotaExceeded);
        }
        self.validate_payloads(&plugin, &items)?;

        let new_identities = self.count_new_identities(&plugin, &items);
        let plugin_total = self
            .per_plugin
            .get(&plugin)
            .copied()
            .unwrap_or(0)
            .checked_add(new_identities)
            .ok_or(RejectReason::QuotaExceeded)?;
        let query_total = self
            .items
            .len()
            .checked_add(new_identities)
            .ok_or(RejectReason::QuotaExceeded)?;
        if plugin_total > self.limits.max_items_per_plugin_per_query
            || query_total > self.limits.max_items_per_query
        {
            return Err(RejectReason::QuotaExceeded);
        }

        if !items.is_empty() {
            self.merge(&plugin, items);
            if new_identities > 0 {
                self.per_plugin.insert(plugin.clone(), plugin_total);
            }
            self.pending_ui_update = true;
        }
        self.batch_states.insert(plugin, state);
        Ok(())
    }

    /// Advances the exclusive retirement floor to at least `generation` and
    /// drops active state when it falls below that floor.
    ///
    /// Recording the floor even with no active generation prevents a later
    /// `begin_generation` from resurrecting work already retired. Regressive
    /// retirement calls are idempotent.
    fn retire_before(&mut self, generation: Generation) {
        if generation <= self.retirement_floor {
            return;
        }
        self.retirement_floor = generation;

        let Some(active) = self.active else {
            return;
        };
        if active >= self.retirement_floor {
            return;
        }

        let had_items = !self.items.is_empty();
        self.active = None;
        self.reset();
        self.pending_ui_update |= had_items;
    }
}
