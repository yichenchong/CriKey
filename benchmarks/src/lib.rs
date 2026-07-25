//! Synthetic workloads for CriKey performance work (spec 25, 27.3).

use std::collections::BTreeMap;

use crikey_core::{ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId};

/// Stress-test target from the specification: 500,000 indexed catalog items.
pub const STRESS_CATALOG_SIZE: usize = 500_000;

/// Builds a deterministic synthetic catalog of `count` items.
pub fn synthetic_catalog(count: usize) -> Vec<Item> {
    let plugin = PluginId("crikey.benchmarks".into());
    (0..count)
        .map(|index| {
            let target = format!("/synthetic/app-{index:06}");
            let label = format!("Synthetic Application {index}");
            Item {
                stable_id: ItemId::derived(&plugin, &Category::Application, &target),
                plugin_id: plugin.clone(),
                category: Category::Application,
                label: label.clone(),
                description: format!("Benchmark fixture #{index}"),
                target,
                search_terms: vec![label],
                icon_reference: None,
                argument_policy: ArgumentPolicy::Forbidden,
                hit_policy: HitPolicy::Recorded,
                score_hint: 0,
                metadata: BTreeMap::new(),
                actions: Vec::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_items_have_unique_stable_ids() {
        let items = synthetic_catalog(1_000);
        let unique: std::collections::HashSet<_> = items.iter().map(|i| i.stable_id.clone()).collect();
        assert_eq!(unique.len(), items.len());
    }
}
