//! Catalog store (spec 10, 22, 25.1).
//!
//! Target: at least 500,000 indexed items with responsive search.

use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use crikey_core::{Action, Category, Generation, Item, ItemId, PluginId};

/// Why a catalog lifecycle operation or update was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogError {
    /// The publishing or retiring instance is not the plugin's active instance.
    StaleInstance,
    /// At least one item is not owned by the plugin publishing the update.
    OwnerMismatch,
    /// The incoming vector contains more raw items than one update may process.
    BatchItemLimitExceeded { actual: usize, limit: usize },
    /// The update would retain too many unique items for one plugin.
    PluginItemLimitExceeded { actual: usize, limit: usize },
    /// The update would retain too many unique items across all plugins.
    TotalItemLimitExceeded { actual: usize, limit: usize },
    /// One item, including its nested actions, exceeds the payload limit.
    ItemPayloadLimitExceeded { actual: usize, limit: usize },
    /// The complete incoming batch exceeds the payload limit.
    BatchPayloadLimitExceeded { actual: usize, limit: usize },
    /// Checked payload accounting could not represent the payload size.
    PayloadSizeOverflow,
    /// Checked retained-item accounting could not represent the item count.
    ItemCountOverflow,
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleInstance => formatter.write_str("catalog operation came from a stale plugin instance"),
            Self::OwnerMismatch => {
                formatter.write_str("catalog update contains an item owned by another plugin")
            }
            Self::BatchItemLimitExceeded { actual, limit } => {
                write!(
                    formatter,
                    "catalog update contains {actual} raw items; the limit is {limit}"
                )
            }
            Self::PluginItemLimitExceeded { actual, limit } => {
                write!(
                    formatter,
                    "catalog update would retain {actual} items for one plugin; the limit is {limit}"
                )
            }
            Self::TotalItemLimitExceeded { actual, limit } => {
                write!(
                    formatter,
                    "catalog update would retain {actual} total items; the limit is {limit}"
                )
            }
            Self::ItemPayloadLimitExceeded { actual, limit } => {
                write!(
                    formatter,
                    "catalog item payload is {actual} bytes; the limit is {limit}"
                )
            }
            Self::BatchPayloadLimitExceeded { actual, limit } => {
                write!(
                    formatter,
                    "catalog batch payload is {actual} bytes; the limit is {limit}"
                )
            }
            Self::PayloadSizeOverflow => formatter.write_str("catalog payload size cannot be represented"),
            Self::ItemCountOverflow => formatter.write_str("catalog item count cannot be represented"),
        }
    }
}

impl std::error::Error for CatalogError {}

/// Resource limits applied atomically before a catalog update allocates or
/// mutates retained state.
///
/// Batch item counts include duplicates. Plugin and total counts apply to
/// unique retained stable IDs. Payload bytes use a deterministic,
/// length-prefixed accounting of every [`Item`] and nested [`Action`] field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogLimits {
    pub max_batch_items: usize,
    pub max_plugin_items: usize,
    pub max_total_items: usize,
    pub max_item_bytes: usize,
    pub max_batch_bytes: usize,
}

impl Default for CatalogLimits {
    fn default() -> Self {
        Self {
            max_batch_items: 500_000,
            max_plugin_items: 500_000,
            max_total_items: 500_000,
            max_item_bytes: 1_048_576,
            max_batch_bytes: 1_073_741_824,
        }
    }
}

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
    /// Makes `instance` the only publisher authorized for `plugin`.
    ///
    /// Instance numbers are a monotonic high-water mark. Repeating the current
    /// number is idempotent (and reactivates a retired instance), while a lower
    /// number is rejected.
    fn activate_instance(&mut self, plugin: &PluginId, instance: u64) -> Result<(), CatalogError>;

    /// Revokes an active instance without discarding its high-water mark or
    /// retained catalog slice.
    fn retire_instance(&mut self, plugin: &PluginId, instance: u64) -> Result<(), CatalogError>;

    /// Removes one plugin's retained items without changing its authorization.
    fn invalidate(&mut self, plugin: &PluginId);

    /// Applies a plugin's catalog contribution. Updates from superseded or
    /// retired plugin instances are rejected (spec 14.8).
    fn apply(
        &mut self,
        plugin: &PluginId,
        instance: u64,
        update: CatalogUpdate,
        items: Vec<Item>,
    ) -> Result<(), CatalogError>;

    /// Returns an item from one plugin's catalog slice.
    fn get(&self, plugin: &PluginId, id: &ItemId) -> Option<&Item>;

    /// Returns the number of retained items owned by `plugin`.
    fn plugin_len(&self, plugin: &PluginId) -> usize;

    /// Returns `plugin`'s retained items in their stable catalog order.
    fn items(&self, plugin: &PluginId) -> &[Item];

    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Default)]
struct PluginCatalog {
    items: Vec<Item>,
    positions: HashMap<ItemId, usize>,
}

impl PluginCatalog {
    fn from_items(items: Vec<Item>, unique_items: usize) -> Self {
        let mut catalog = Self {
            items: Vec::with_capacity(unique_items),
            positions: HashMap::with_capacity(unique_items),
        };
        let added = catalog.merge(items);
        debug_assert_eq!(added, unique_items);
        catalog
    }

    fn merge(&mut self, items: Vec<Item>) -> usize {
        let mut added = 0usize;

        for item in items {
            if let Some(&position) = self.positions.get(&item.stable_id) {
                self.items[position] = item;
            } else {
                let position = self.items.len();
                let displaced = self.positions.insert(item.stable_id.clone(), position);
                debug_assert!(displaced.is_none());
                self.items.push(item);
                added += 1;
            }
        }

        added
    }

    fn get(&self, id: &ItemId) -> Option<&Item> {
        self.positions
            .get(id)
            .and_then(|&position| self.items.get(position))
    }
}

const LENGTH_PREFIX_BYTES: usize = 8;
const ENUM_TAG_BYTES: usize = 1;
const SCORE_HINT_BYTES: usize = 4;

fn add_payload_bytes(total: &mut usize, bytes: usize) -> Result<(), CatalogError> {
    *total = total
        .checked_add(bytes)
        .ok_or(CatalogError::PayloadSizeOverflow)?;
    Ok(())
}

fn add_string_payload(total: &mut usize, value: &str) -> Result<(), CatalogError> {
    add_payload_bytes(total, LENGTH_PREFIX_BYTES)?;
    add_payload_bytes(total, value.len())
}

fn add_category_payload(total: &mut usize, category: &Category) -> Result<(), CatalogError> {
    add_payload_bytes(total, ENUM_TAG_BYTES)?;
    if let Category::PluginDefined(name) = category {
        add_string_payload(total, name)?;
    }
    Ok(())
}

fn add_optional_string_payload(total: &mut usize, value: Option<&str>) -> Result<(), CatalogError> {
    add_payload_bytes(total, ENUM_TAG_BYTES)?;
    if let Some(value) = value {
        add_string_payload(total, value)?;
    }
    Ok(())
}

fn add_action_payload(total: &mut usize, action: &Action) -> Result<(), CatalogError> {
    add_string_payload(total, &action.action_id.0)?;
    add_string_payload(total, &action.label)?;
    add_string_payload(total, &action.description)?;
    add_payload_bytes(total, LENGTH_PREFIX_BYTES)?;
    for category in &action.applicable_categories {
        add_category_payload(total, category)?;
    }
    add_optional_string_payload(total, action.icon_reference.as_deref())?;
    add_payload_bytes(total, ENUM_TAG_BYTES)
}

fn item_payload_bytes(item: &Item) -> Result<usize, CatalogError> {
    let mut total = 0usize;
    add_string_payload(&mut total, &item.stable_id.0)?;
    add_string_payload(&mut total, &item.plugin_id.0)?;
    add_category_payload(&mut total, &item.category)?;
    add_string_payload(&mut total, &item.label)?;
    add_string_payload(&mut total, &item.description)?;
    add_string_payload(&mut total, &item.target)?;

    add_payload_bytes(&mut total, LENGTH_PREFIX_BYTES)?;
    for term in &item.search_terms {
        add_string_payload(&mut total, term)?;
    }
    add_optional_string_payload(&mut total, item.icon_reference.as_deref())?;
    add_payload_bytes(&mut total, ENUM_TAG_BYTES)?;
    add_payload_bytes(&mut total, ENUM_TAG_BYTES)?;
    add_payload_bytes(&mut total, SCORE_HINT_BYTES)?;

    add_payload_bytes(&mut total, LENGTH_PREFIX_BYTES)?;
    for (key, value) in &item.metadata {
        add_string_payload(&mut total, key)?;
        add_string_payload(&mut total, value)?;
    }

    add_payload_bytes(&mut total, LENGTH_PREFIX_BYTES)?;
    for action in &item.actions {
        add_action_payload(&mut total, action)?;
    }
    Ok(total)
}

#[derive(Debug, Clone, Copy)]
struct PluginInstance {
    high_water: u64,
    active: bool,
}

#[derive(Debug, Clone, Copy)]
struct UpdatePlan {
    unique_batch_items: usize,
    merge_additions: usize,
    projected_total_items: usize,
}

/// Owner-scoped in-memory catalog with stable per-plugin item ordering.
#[derive(Debug, Default)]
pub struct MemoryCatalog {
    instances: HashMap<PluginId, PluginInstance>,
    plugins: HashMap<PluginId, PluginCatalog>,
    item_count: usize,
    limits: CatalogLimits,
}

impl MemoryCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limits(limits: CatalogLimits) -> Self {
        Self {
            instances: HashMap::new(),
            plugins: HashMap::new(),
            item_count: 0,
            limits,
        }
    }

    fn plan_update(
        &self,
        plugin: &PluginId,
        update: CatalogUpdate,
        items: &[Item],
    ) -> Result<UpdatePlan, CatalogError> {
        if items.len() > self.limits.max_batch_items {
            return Err(CatalogError::BatchItemLimitExceeded {
                actual: items.len(),
                limit: self.limits.max_batch_items,
            });
        }

        if items.iter().any(|item| &item.plugin_id != plugin) {
            return Err(CatalogError::OwnerMismatch);
        }

        let mut batch_bytes = 0usize;
        for item in items {
            let item_bytes = item_payload_bytes(item)?;
            if item_bytes > self.limits.max_item_bytes {
                return Err(CatalogError::ItemPayloadLimitExceeded {
                    actual: item_bytes,
                    limit: self.limits.max_item_bytes,
                });
            }
            batch_bytes = batch_bytes
                .checked_add(item_bytes)
                .ok_or(CatalogError::PayloadSizeOverflow)?;
        }
        if batch_bytes > self.limits.max_batch_bytes {
            return Err(CatalogError::BatchPayloadLimitExceeded {
                actual: batch_bytes,
                limit: self.limits.max_batch_bytes,
            });
        }

        let mut unique_ids = HashSet::new();
        for item in items {
            unique_ids.insert(&item.stable_id);
        }

        let existing = self.plugins.get(plugin);
        let previous_plugin_items = existing.map_or(0, |catalog| catalog.items.len());
        let (projected_plugin_items, merge_additions) = match update {
            CatalogUpdate::Replace => (unique_ids.len(), 0),
            CatalogUpdate::Merge => {
                let mut additions = 0usize;
                for id in &unique_ids {
                    if existing.is_none_or(|catalog| !catalog.positions.contains_key(*id)) {
                        additions = additions.checked_add(1).ok_or(CatalogError::ItemCountOverflow)?;
                    }
                }
                let projected = previous_plugin_items
                    .checked_add(additions)
                    .ok_or(CatalogError::ItemCountOverflow)?;
                (projected, additions)
            }
        };

        if projected_plugin_items > self.limits.max_plugin_items {
            return Err(CatalogError::PluginItemLimitExceeded {
                actual: projected_plugin_items,
                limit: self.limits.max_plugin_items,
            });
        }

        let other_plugin_items = self
            .item_count
            .checked_sub(previous_plugin_items)
            .ok_or(CatalogError::ItemCountOverflow)?;
        let projected_total_items = other_plugin_items
            .checked_add(projected_plugin_items)
            .ok_or(CatalogError::ItemCountOverflow)?;
        if projected_total_items > self.limits.max_total_items {
            return Err(CatalogError::TotalItemLimitExceeded {
                actual: projected_total_items,
                limit: self.limits.max_total_items,
            });
        }

        Ok(UpdatePlan {
            unique_batch_items: unique_ids.len(),
            merge_additions,
            projected_total_items,
        })
    }
}

impl CatalogStore for MemoryCatalog {
    fn activate_instance(&mut self, plugin: &PluginId, instance: u64) -> Result<(), CatalogError> {
        if let Some(state) = self.instances.get_mut(plugin) {
            if instance < state.high_water {
                return Err(CatalogError::StaleInstance);
            }
            state.high_water = instance;
            state.active = true;
        } else {
            let displaced = self.instances.insert(
                plugin.clone(),
                PluginInstance {
                    high_water: instance,
                    active: true,
                },
            );
            debug_assert!(displaced.is_none());
        }
        Ok(())
    }

    fn retire_instance(&mut self, plugin: &PluginId, instance: u64) -> Result<(), CatalogError> {
        match self.instances.get_mut(plugin) {
            Some(state) if state.active && state.high_water == instance => {
                state.active = false;
                Ok(())
            }
            _ => Err(CatalogError::StaleInstance),
        }
    }

    fn invalidate(&mut self, plugin: &PluginId) {
        if let Some(catalog) = self.plugins.remove(plugin) {
            self.item_count -= catalog.items.len();
        }
    }

    fn apply(
        &mut self,
        plugin: &PluginId,
        instance: u64,
        update: CatalogUpdate,
        items: Vec<Item>,
    ) -> Result<(), CatalogError> {
        let is_active = self
            .instances
            .get(plugin)
            .is_some_and(|state| state.active && state.high_water == instance);
        if !is_active {
            return Err(CatalogError::StaleInstance);
        }

        let plan = self.plan_update(plugin, update, &items)?;

        match update {
            CatalogUpdate::Replace => {
                if plan.unique_batch_items == 0 {
                    self.plugins.remove(plugin);
                } else {
                    let replacement = PluginCatalog::from_items(items, plan.unique_batch_items);
                    self.plugins.insert(plugin.clone(), replacement);
                }
            }
            CatalogUpdate::Merge => {
                if plan.unique_batch_items == 0 {
                    return Ok(());
                }

                if let Some(catalog) = self.plugins.get_mut(plugin) {
                    catalog.items.reserve(plan.merge_additions);
                    catalog.positions.reserve(plan.merge_additions);
                    let added = catalog.merge(items);
                    debug_assert_eq!(added, plan.merge_additions);
                } else {
                    let catalog = PluginCatalog::from_items(items, plan.unique_batch_items);
                    let displaced = self.plugins.insert(plugin.clone(), catalog);
                    debug_assert!(displaced.is_none());
                }
            }
        }

        self.item_count = plan.projected_total_items;
        Ok(())
    }

    fn get(&self, plugin: &PluginId, id: &ItemId) -> Option<&Item> {
        self.plugins.get(plugin).and_then(|catalog| catalog.get(id))
    }

    fn plugin_len(&self, plugin: &PluginId) -> usize {
        self.plugins.get(plugin).map_or(0, |catalog| catalog.items.len())
    }

    fn items(&self, plugin: &PluginId) -> &[Item] {
        match self.plugins.get(plugin) {
            Some(catalog) => catalog.items.as_slice(),
            None => &[],
        }
    }

    fn len(&self) -> usize {
        self.item_count
    }
}

/// Persistent cache of the core catalog, loaded during stage 2 of startup.
pub trait CatalogCache {
    fn load(&self) -> Option<Vec<Item>>;
    fn store(&self, items: &[Item], generation: Generation);
    fn invalidate(&self, plugin: &PluginId);
}
