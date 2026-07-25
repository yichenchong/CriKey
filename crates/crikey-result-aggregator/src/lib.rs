//! Result aggregation (spec 11.5, 11.6, 11.7, 12).

use crikey_core::{Generation, Item, PluginId};

/// Completion state reported by a modern plugin batch (spec 12.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchState {
    Partial,
    Final,
    Cancelled,
    Failed,
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
    /// Plugin exceeded its per-query result quota (spec 11.7).
    QuotaExceeded,
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
    pub max_ui_updates_per_frame: usize,
}

impl Default for ResultLimits {
    fn default() -> Self {
        Self {
            max_items_per_batch: 50,
            max_items_per_plugin_per_query: 250,
            max_items_per_query: 2_000,
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
