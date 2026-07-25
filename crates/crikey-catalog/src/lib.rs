//! Catalog store (spec 10, 22, 25.1).
//!
//! Target: at least 500,000 indexed items with responsive search.

use crikey_core::{Generation, Item, ItemId, PluginId};

/// Ownership of catalog contributions is per plugin so a rebuild or a crashed
/// worker only invalidates its own slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogUpdate {
    /// Legacy `set_catalog()` and modern full rebuild.
    Replace,
    /// Legacy `merge_catalog()`.
    Merge,
}

pub trait CatalogStore {
    /// Applies a plugin's catalog contribution. Updates from superseded plugin
    /// instances must be rejected (spec 14.8).
    fn apply(&mut self, plugin: &PluginId, update: CatalogUpdate, items: Vec<Item>);
    fn get(&self, id: &ItemId) -> Option<&Item>;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Persistent cache of the core catalog, loaded during stage 2 of startup.
pub trait CatalogCache {
    fn load(&self) -> Option<Vec<Item>>;
    fn store(&self, items: &[Item], generation: Generation);
    fn invalidate(&self, plugin: &PluginId);
}
