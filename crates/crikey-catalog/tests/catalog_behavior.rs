//! Public-API contract for the owner-scoped in-memory catalog.
//!
//! This covers legacy catalog replacement/merge and stale-instance rejection
//! (spec 14.8), explicit per-plugin invalidation (spec 22.4), and the per-plugin
//! slices selected by ADR-0008. Persistence and archive layout are deliberately
//! outside this contract.

use std::collections::BTreeMap;

use crikey_catalog::{CatalogError, CatalogLimits, CatalogStore, CatalogUpdate, MemoryCatalog};
use crikey_core::{
    Action, ActionId, ArgumentPolicy, Category, ExecutionPolicy, HitPolicy, Item, ItemId, PluginId,
};

fn plugin(id: &str) -> PluginId {
    PluginId(id.to_owned())
}

fn item(owner: &PluginId, stable_id: &str, label: &str) -> Item {
    let category = Category::PluginDefined("fixture.application".to_owned());
    let metadata = BTreeMap::from([
        ("fixture-owner".to_owned(), owner.0.clone()),
        ("fixture-stable-id".to_owned(), stable_id.to_owned()),
    ]);

    Item {
        stable_id: ItemId(stable_id.to_owned()),
        plugin_id: owner.clone(),
        category: category.clone(),
        label: label.to_owned(),
        description: format!("Description for {label}"),
        target: format!("catalog://{}/{stable_id}", owner.0),
        search_terms: vec![label.to_lowercase(), stable_id.to_owned()],
        icon_reference: Some(format!("icon://{stable_id}")),
        argument_policy: ArgumentPolicy::Required,
        hit_policy: HitPolicy::Ignored,
        score_hint: 37,
        metadata,
        actions: vec![Action {
            action_id: ActionId(format!("open-{stable_id}")),
            label: format!("Open {label}"),
            description: format!("Open the {label} fixture"),
            applicable_categories: vec![category],
            icon_reference: Some(format!("action-icon://{stable_id}")),
            execution_policy: ExecutionPolicy::Plugin,
        }],
    }
}

fn stored<'a>(catalog: &'a MemoryCatalog, owner: &PluginId, stable_id: &str) -> Option<&'a Item> {
    catalog.get(owner, &ItemId(stable_id.to_owned()))
}

fn ordered_ids<'a>(catalog: &'a MemoryCatalog, owner: &PluginId) -> Vec<&'a str> {
    catalog
        .items(owner)
        .iter()
        .map(|item| item.stable_id.0.as_str())
        .collect()
}

fn publish(
    catalog: &mut MemoryCatalog,
    owner: &PluginId,
    instance: u64,
    update: CatalogUpdate,
    items: Vec<Item>,
) {
    catalog
        .apply(owner, instance, update, items)
        .expect("the active plugin instance may update its catalog");
}
fn activate(catalog: &mut MemoryCatalog, owner: &PluginId, instance: u64) {
    catalog
        .activate_instance(owner, instance)
        .expect("a fresh or current high-water instance may activate");
}

fn measured_item_payload(candidate: &Item) -> usize {
    let owner = candidate.plugin_id.clone();
    let mut catalog = MemoryCatalog::with_limits(CatalogLimits {
        max_item_bytes: 0,
        max_batch_bytes: usize::MAX,
        ..CatalogLimits::default()
    });
    activate(&mut catalog, &owner, 1);

    match catalog.apply(&owner, 1, CatalogUpdate::Replace, vec![candidate.clone()]) {
        Err(CatalogError::ItemPayloadLimitExceeded { actual, limit: 0 }) => actual,
        result => panic!("zero item-payload limit returned {result:?}"),
    }
}

fn assert_same_item(actual: &Item, expected: &Item) {
    assert_eq!(actual.stable_id, expected.stable_id);
    assert_eq!(actual.plugin_id, expected.plugin_id);
    assert_eq!(actual.category, expected.category);
    assert_eq!(actual.label, expected.label);
    assert_eq!(actual.description, expected.description);
    assert_eq!(actual.target, expected.target);
    assert_eq!(actual.search_terms, expected.search_terms);
    assert_eq!(actual.icon_reference, expected.icon_reference);
    assert_eq!(actual.argument_policy, expected.argument_policy);
    assert_eq!(actual.hit_policy, expected.hit_policy);
    assert_eq!(actual.score_hint, expected.score_hint);
    assert_eq!(actual.metadata, expected.metadata);
    assert_eq!(actual.actions.len(), expected.actions.len());

    for (actual, expected) in actual.actions.iter().zip(&expected.actions) {
        assert_eq!(actual.action_id, expected.action_id);
        assert_eq!(actual.label, expected.label);
        assert_eq!(actual.description, expected.description);
        assert_eq!(actual.applicable_categories, expected.applicable_categories);
        assert_eq!(actual.icon_reference, expected.icon_reference);
        assert_eq!(actual.execution_policy, expected.execution_policy);
    }
}

#[test]
fn catalog_updates_require_an_activated_instance() {
    let mut catalog = MemoryCatalog::new();
    let apps = plugin("dev.crikey.apps");
    let candidate = item(&apps, "alpha", "Alpha");

    let rejected = catalog.apply(&apps, 41, CatalogUpdate::Replace, vec![candidate.clone()]);

    assert_eq!(rejected, Err(CatalogError::StaleInstance));
    assert!(catalog.is_empty());
    assert!(catalog.items(&apps).is_empty());
    assert_eq!(catalog.plugin_len(&apps), 0);

    activate(&mut catalog, &apps, 41);
    publish(
        &mut catalog,
        &apps,
        41,
        CatalogUpdate::Replace,
        vec![candidate.clone()],
    );

    assert_same_item(
        stored(&catalog, &apps, "alpha").expect("activated update is visible"),
        &candidate,
    );
}

#[test]
fn superseded_instance_is_rejected_without_mutating_the_active_catalog() {
    let mut catalog = MemoryCatalog::new();
    let apps = plugin("dev.crikey.apps");
    activate(&mut catalog, &apps, 7);
    activate(&mut catalog, &apps, 8);

    let active_item = item(&apps, "active", "Active instance value");
    publish(
        &mut catalog,
        &apps,
        8,
        CatalogUpdate::Replace,
        vec![active_item.clone()],
    );

    let rejected = catalog.apply(
        &apps,
        7,
        CatalogUpdate::Replace,
        vec![item(&apps, "stale", "Stale instance value")],
    );

    assert_eq!(rejected, Err(CatalogError::StaleInstance));
    assert_eq!(ordered_ids(&catalog, &apps), ["active"]);
    assert_same_item(
        stored(&catalog, &apps, "active").expect("active data survives stale traffic"),
        &active_item,
    );
    assert!(stored(&catalog, &apps, "stale").is_none());
    assert_eq!(catalog.len(), 1);
}

#[test]
fn owner_validation_rejects_the_entire_update_atomically() {
    let mut catalog = MemoryCatalog::new();
    let apps = plugin("dev.crikey.apps");
    let files = plugin("dev.crikey.files");
    activate(&mut catalog, &apps, 11);

    let baseline = item(&apps, "alpha", "Alpha");
    publish(
        &mut catalog,
        &apps,
        11,
        CatalogUpdate::Replace,
        vec![baseline.clone()],
    );

    let rejected = catalog.apply(
        &apps,
        11,
        CatalogUpdate::Merge,
        vec![
            item(&apps, "beta", "Beta"),
            item(&files, "foreign", "Foreign owner"),
        ],
    );

    assert_eq!(rejected, Err(CatalogError::OwnerMismatch));
    assert_eq!(ordered_ids(&catalog, &apps), ["alpha"]);
    assert_same_item(
        stored(&catalog, &apps, "alpha").expect("baseline item remains"),
        &baseline,
    );
    assert!(stored(&catalog, &apps, "beta").is_none());
    assert!(stored(&catalog, &apps, "foreign").is_none());
    assert!(stored(&catalog, &files, "foreign").is_none());
    assert_eq!(catalog.plugin_len(&apps), 1);
    assert_eq!(catalog.plugin_len(&files), 0);
    assert_eq!(catalog.len(), 1);
}

#[test]
fn replace_discards_the_previous_plugin_slice_and_uses_replacement_order() {
    let mut catalog = MemoryCatalog::new();
    let apps = plugin("dev.crikey.apps");
    activate(&mut catalog, &apps, 13);

    publish(
        &mut catalog,
        &apps,
        13,
        CatalogUpdate::Replace,
        vec![item(&apps, "alpha", "Old Alpha"), item(&apps, "beta", "Beta")],
    );

    let gamma = item(&apps, "gamma", "Gamma");
    let rebuilt_alpha = item(&apps, "alpha", "Rebuilt Alpha");
    publish(
        &mut catalog,
        &apps,
        13,
        CatalogUpdate::Replace,
        vec![gamma.clone(), rebuilt_alpha.clone()],
    );

    assert_eq!(ordered_ids(&catalog, &apps), ["gamma", "alpha"]);
    assert!(stored(&catalog, &apps, "beta").is_none());
    assert_same_item(
        stored(&catalog, &apps, "gamma").expect("new replacement item"),
        &gamma,
    );
    assert_same_item(
        stored(&catalog, &apps, "alpha").expect("rebuilt item"),
        &rebuilt_alpha,
    );
    assert_eq!(catalog.plugin_len(&apps), 2);
    assert_eq!(catalog.len(), 2);
}

#[test]
fn merge_updates_existing_items_in_place_and_appends_new_items_in_batch_order() {
    let mut catalog = MemoryCatalog::new();
    let apps = plugin("dev.crikey.apps");
    activate(&mut catalog, &apps, 17);

    let beta = item(&apps, "beta", "Beta");
    publish(
        &mut catalog,
        &apps,
        17,
        CatalogUpdate::Replace,
        vec![item(&apps, "alpha", "Old Alpha"), beta.clone()],
    );

    let gamma = item(&apps, "gamma", "Gamma");
    let updated_alpha = item(&apps, "alpha", "Updated Alpha");
    let delta = item(&apps, "delta", "Delta");
    publish(
        &mut catalog,
        &apps,
        17,
        CatalogUpdate::Merge,
        vec![gamma.clone(), updated_alpha.clone(), delta.clone()],
    );

    assert_eq!(ordered_ids(&catalog, &apps), ["alpha", "beta", "gamma", "delta"]);
    assert_same_item(
        stored(&catalog, &apps, "alpha").expect("merged value replaces in place"),
        &updated_alpha,
    );
    assert_same_item(
        stored(&catalog, &apps, "beta").expect("untouched value remains"),
        &beta,
    );
    assert_same_item(
        stored(&catalog, &apps, "gamma").expect("first appended value"),
        &gamma,
    );
    assert_same_item(
        stored(&catalog, &apps, "delta").expect("second appended value"),
        &delta,
    );
}

#[test]
fn duplicate_ids_use_the_last_value_in_each_update() {
    let mut catalog = MemoryCatalog::new();
    let apps = plugin("dev.crikey.apps");
    activate(&mut catalog, &apps, 19);

    let replacement_winner = item(&apps, "duplicate", "Replace winner");
    let companion = item(&apps, "companion", "Companion");
    publish(
        &mut catalog,
        &apps,
        19,
        CatalogUpdate::Replace,
        vec![
            item(&apps, "duplicate", "Replace loser"),
            companion.clone(),
            replacement_winner.clone(),
        ],
    );

    assert_same_item(
        stored(&catalog, &apps, "duplicate").expect("deduplicated replacement"),
        &replacement_winner,
    );
    assert_eq!(catalog.plugin_len(&apps), 2);
    assert_eq!(ordered_ids(&catalog, &apps), ["duplicate", "companion"]);
    assert_same_item(
        stored(&catalog, &apps, "companion").expect("companion index remains exact"),
        &companion,
    );

    let appended_winner = item(&apps, "appended", "Merge append winner");
    let existing_winner = item(&apps, "duplicate", "Merge existing winner");
    publish(
        &mut catalog,
        &apps,
        19,
        CatalogUpdate::Merge,
        vec![
            item(&apps, "appended", "Merge append loser"),
            item(&apps, "duplicate", "Merge existing loser"),
            appended_winner.clone(),
            existing_winner.clone(),
        ],
    );

    assert_same_item(
        stored(&catalog, &apps, "appended").expect("one appended identity"),
        &appended_winner,
    );
    assert_same_item(
        stored(&catalog, &apps, "duplicate").expect("updated existing identity"),
        &existing_winner,
    );
    assert_eq!(
        ordered_ids(&catalog, &apps),
        ["duplicate", "companion", "appended"]
    );
    assert_same_item(
        stored(&catalog, &apps, "companion").expect("companion lookup index remains exact"),
        &companion,
    );
    assert_eq!(catalog.plugin_len(&apps), 3);
    assert_eq!(catalog.len(), 3);
}

#[test]
fn the_same_item_id_is_isolated_between_plugin_owners() {
    let mut catalog = MemoryCatalog::new();
    let apps = plugin("dev.crikey.apps");
    let files = plugin("dev.crikey.files");
    activate(&mut catalog, &apps, 23);
    activate(&mut catalog, &files, 29);

    let app_item = item(&apps, "shared", "Application Shared");
    let file_item = item(&files, "shared", "File Shared");
    publish(
        &mut catalog,
        &apps,
        23,
        CatalogUpdate::Replace,
        vec![app_item.clone()],
    );
    publish(
        &mut catalog,
        &files,
        29,
        CatalogUpdate::Replace,
        vec![file_item.clone()],
    );

    assert_same_item(
        stored(&catalog, &apps, "shared").expect("app-owned identity"),
        &app_item,
    );
    assert_same_item(
        stored(&catalog, &files, "shared").expect("file-owned identity"),
        &file_item,
    );
    assert_eq!(catalog.plugin_len(&apps), 1);
    assert_eq!(catalog.plugin_len(&files), 1);
    assert_eq!(catalog.len(), 2);
}

#[test]
fn invalidation_removes_only_the_target_plugin_slice() {
    let mut catalog = MemoryCatalog::new();
    let apps = plugin("dev.crikey.apps");
    let files = plugin("dev.crikey.files");
    activate(&mut catalog, &apps, 31);
    activate(&mut catalog, &files, 37);

    publish(
        &mut catalog,
        &apps,
        31,
        CatalogUpdate::Replace,
        vec![item(&apps, "alpha", "Alpha"), item(&apps, "beta", "Beta")],
    );
    let file_item = item(&files, "notes", "Notes");
    publish(
        &mut catalog,
        &files,
        37,
        CatalogUpdate::Replace,
        vec![file_item.clone()],
    );

    catalog.invalidate(&apps);

    assert!(catalog.items(&apps).is_empty());
    assert!(stored(&catalog, &apps, "alpha").is_none());
    assert!(stored(&catalog, &apps, "beta").is_none());
    assert_eq!(catalog.plugin_len(&apps), 0);
    assert_eq!(ordered_ids(&catalog, &files), ["notes"]);
    assert_same_item(
        stored(&catalog, &files, "notes").expect("other plugin survives"),
        &file_item,
    );
    assert_eq!(catalog.plugin_len(&files), 1);
    assert_eq!(catalog.len(), 1);
}

#[test]
fn global_and_per_plugin_lengths_track_unique_retained_items() {
    let mut catalog = MemoryCatalog::new();
    let apps = plugin("dev.crikey.apps");
    let files = plugin("dev.crikey.files");
    activate(&mut catalog, &apps, 41);
    activate(&mut catalog, &files, 43);

    assert_eq!(catalog.len(), 0);
    assert_eq!(catalog.plugin_len(&apps), 0);
    assert_eq!(catalog.plugin_len(&files), 0);

    publish(
        &mut catalog,
        &apps,
        41,
        CatalogUpdate::Replace,
        vec![item(&apps, "alpha", "Alpha"), item(&apps, "beta", "Beta")],
    );
    publish(
        &mut catalog,
        &files,
        43,
        CatalogUpdate::Replace,
        vec![item(&files, "notes", "Notes")],
    );
    assert_eq!(
        (
            catalog.len(),
            catalog.plugin_len(&apps),
            catalog.plugin_len(&files)
        ),
        (3, 2, 1)
    );

    publish(
        &mut catalog,
        &apps,
        41,
        CatalogUpdate::Merge,
        vec![
            item(&apps, "alpha", "Updated Alpha"),
            item(&apps, "gamma", "Gamma"),
        ],
    );
    assert_eq!(
        (
            catalog.len(),
            catalog.plugin_len(&apps),
            catalog.plugin_len(&files)
        ),
        (4, 3, 1)
    );

    publish(
        &mut catalog,
        &apps,
        41,
        CatalogUpdate::Replace,
        vec![item(&apps, "delta", "Delta")],
    );
    assert_eq!(
        (
            catalog.len(),
            catalog.plugin_len(&apps),
            catalog.plugin_len(&files)
        ),
        (2, 1, 1)
    );
}

#[test]
fn an_empty_merge_is_a_successful_no_op() {
    let mut catalog = MemoryCatalog::new();
    let apps = plugin("dev.crikey.apps");
    activate(&mut catalog, &apps, 47);

    let alpha = item(&apps, "alpha", "Alpha");
    let beta = item(&apps, "beta", "Beta");
    publish(
        &mut catalog,
        &apps,
        47,
        CatalogUpdate::Replace,
        vec![alpha.clone(), beta.clone()],
    );

    publish(&mut catalog, &apps, 47, CatalogUpdate::Merge, Vec::new());

    assert_eq!(ordered_ids(&catalog, &apps), ["alpha", "beta"]);
    assert_same_item(stored(&catalog, &apps, "alpha").expect("alpha remains"), &alpha);
    assert_same_item(stored(&catalog, &apps, "beta").expect("beta remains"), &beta);
    assert_eq!(catalog.plugin_len(&apps), 2);
    assert_eq!(catalog.len(), 2);
}

#[test]
fn an_empty_replace_clears_only_the_target_plugin_slice() {
    let mut catalog = MemoryCatalog::new();
    let apps = plugin("dev.crikey.apps");
    let files = plugin("dev.crikey.files");
    activate(&mut catalog, &apps, 53);
    activate(&mut catalog, &files, 59);

    publish(
        &mut catalog,
        &apps,
        53,
        CatalogUpdate::Replace,
        vec![item(&apps, "alpha", "Alpha"), item(&apps, "beta", "Beta")],
    );
    let notes = item(&files, "notes", "Notes");
    publish(
        &mut catalog,
        &files,
        59,
        CatalogUpdate::Replace,
        vec![notes.clone()],
    );

    publish(&mut catalog, &apps, 53, CatalogUpdate::Replace, Vec::new());

    assert!(catalog.items(&apps).is_empty());
    assert_eq!(catalog.plugin_len(&apps), 0);
    assert_eq!(ordered_ids(&catalog, &files), ["notes"]);
    assert_same_item(
        stored(&catalog, &files, "notes").expect("unrelated slice remains"),
        &notes,
    );
    assert_eq!(catalog.plugin_len(&files), 1);
    assert_eq!(catalog.len(), 1);
}

#[test]
fn activation_is_monotonic_and_equal_activation_is_idempotent() {
    let mut catalog = MemoryCatalog::new();
    let apps = plugin("dev.crikey.apps");

    assert_eq!(catalog.activate_instance(&apps, 7), Ok(()));
    let alpha = item(&apps, "alpha", "Alpha");
    publish(
        &mut catalog,
        &apps,
        7,
        CatalogUpdate::Replace,
        vec![alpha.clone()],
    );

    assert_eq!(catalog.activate_instance(&apps, 7), Ok(()));
    assert_eq!(catalog.activate_instance(&apps, 8), Ok(()));
    assert_eq!(
        catalog.activate_instance(&apps, 7),
        Err(CatalogError::StaleInstance)
    );
    assert_eq!(
        catalog.apply(
            &apps,
            7,
            CatalogUpdate::Merge,
            vec![item(&apps, "stale", "Stale")],
        ),
        Err(CatalogError::StaleInstance)
    );

    let current = item(&apps, "current", "Current");
    publish(
        &mut catalog,
        &apps,
        8,
        CatalogUpdate::Merge,
        vec![current.clone()],
    );
    assert_eq!(ordered_ids(&catalog, &apps), ["alpha", "current"]);
    assert_same_item(
        stored(&catalog, &apps, "alpha").expect("equal activation preserves retained data"),
        &alpha,
    );
    assert_same_item(
        stored(&catalog, &apps, "current").expect("failed regression leaves instance 8 active"),
        &current,
    );
}

#[test]
fn retirement_revokes_late_updates_preserves_high_water_and_allows_reactivation() {
    let mut catalog = MemoryCatalog::new();
    let apps = plugin("dev.crikey.apps");
    activate(&mut catalog, &apps, 11);

    let baseline = item(&apps, "baseline", "Baseline");
    publish(
        &mut catalog,
        &apps,
        11,
        CatalogUpdate::Replace,
        vec![baseline.clone()],
    );
    assert_eq!(catalog.retire_instance(&apps, 11), Ok(()));

    for update in [CatalogUpdate::Replace, CatalogUpdate::Merge] {
        let rejected = catalog.apply(&apps, 11, update, vec![item(&apps, "late", "Late update")]);
        assert_eq!(rejected, Err(CatalogError::StaleInstance));
        assert_eq!(ordered_ids(&catalog, &apps), ["baseline"]);
        assert_same_item(
            stored(&catalog, &apps, "baseline").expect("retirement retains the plugin slice"),
            &baseline,
        );
    }

    assert_eq!(
        catalog.activate_instance(&apps, 10),
        Err(CatalogError::StaleInstance)
    );
    assert_eq!(catalog.activate_instance(&apps, 11), Ok(()));
    let reactivated = item(&apps, "reactivated", "Reactivated");
    publish(
        &mut catalog,
        &apps,
        11,
        CatalogUpdate::Merge,
        vec![reactivated.clone()],
    );

    assert_eq!(catalog.activate_instance(&apps, 12), Ok(()));
    assert_eq!(
        catalog.retire_instance(&apps, 11),
        Err(CatalogError::StaleInstance)
    );
    let newest = item(&apps, "newest", "Newest");
    publish(
        &mut catalog,
        &apps,
        12,
        CatalogUpdate::Merge,
        vec![newest.clone()],
    );
    assert_eq!(
        ordered_ids(&catalog, &apps),
        ["baseline", "reactivated", "newest"]
    );
    assert_same_item(
        stored(&catalog, &apps, "reactivated").expect("equal high-water reactivates"),
        &reactivated,
    );
    assert_same_item(
        stored(&catalog, &apps, "newest").expect("stale retirement cannot revoke instance 12"),
        &newest,
    );
}

#[test]
fn replace_and_merge_validation_failures_are_symmetric_and_atomic() {
    let mut catalog = MemoryCatalog::new();
    let apps = plugin("dev.crikey.apps");
    let files = plugin("dev.crikey.files");
    activate(&mut catalog, &apps, 21);

    let alpha = item(&apps, "alpha", "Alpha");
    let beta = item(&apps, "beta", "Beta");
    publish(
        &mut catalog,
        &apps,
        21,
        CatalogUpdate::Replace,
        vec![alpha.clone(), beta.clone()],
    );

    assert_eq!(
        catalog.apply(
            &apps,
            21,
            CatalogUpdate::Replace,
            vec![
                item(&apps, "replacement", "Replacement"),
                item(&files, "foreign", "Foreign"),
            ],
        ),
        Err(CatalogError::OwnerMismatch)
    );
    activate(&mut catalog, &apps, 22);
    assert_eq!(
        catalog.apply(
            &apps,
            21,
            CatalogUpdate::Merge,
            vec![item(&apps, "stale", "Stale")],
        ),
        Err(CatalogError::StaleInstance)
    );

    assert_eq!(ordered_ids(&catalog, &apps), ["alpha", "beta"]);
    assert_same_item(stored(&catalog, &apps, "alpha").expect("alpha remains"), &alpha);
    assert_same_item(stored(&catalog, &apps, "beta").expect("beta remains"), &beta);
    assert!(stored(&catalog, &apps, "replacement").is_none());
    assert!(stored(&catalog, &files, "foreign").is_none());
    assert_eq!(catalog.len(), 2);
}

#[test]
fn invalidation_is_idempotent_and_does_not_change_authorization() {
    let mut catalog = MemoryCatalog::new();
    let apps = plugin("dev.crikey.apps");
    let absent = plugin("dev.crikey.absent");
    activate(&mut catalog, &apps, 31);

    catalog.invalidate(&apps);
    catalog.invalidate(&absent);
    assert!(catalog.is_empty());

    publish(
        &mut catalog,
        &apps,
        31,
        CatalogUpdate::Replace,
        vec![item(&apps, "alpha", "Alpha")],
    );
    catalog.invalidate(&apps);
    catalog.invalidate(&apps);
    catalog.invalidate(&absent);
    assert!(catalog.is_empty());

    let after_invalidation = item(&apps, "beta", "Beta");
    publish(
        &mut catalog,
        &apps,
        31,
        CatalogUpdate::Merge,
        vec![after_invalidation.clone()],
    );
    assert_same_item(
        stored(&catalog, &apps, "beta").expect("invalidation does not retire the instance"),
        &after_invalidation,
    );
}

#[test]
fn default_limits_support_the_foundation_catalog_target() {
    let limits = CatalogLimits::default();

    assert!(limits.max_batch_items >= 500_000);
    assert!(limits.max_plugin_items >= 500_000);
    assert!(limits.max_total_items >= 500_000);
    assert!(limits.max_item_bytes > 0);
    assert!(limits.max_batch_bytes >= limits.max_item_bytes);
}

#[test]
fn raw_batch_limit_counts_duplicates_before_deduplication() {
    let mut catalog = MemoryCatalog::with_limits(CatalogLimits {
        max_batch_items: 2,
        ..CatalogLimits::default()
    });
    let apps = plugin("dev.crikey.apps");
    activate(&mut catalog, &apps, 41);
    let baseline = item(&apps, "baseline", "Baseline");
    publish(
        &mut catalog,
        &apps,
        41,
        CatalogUpdate::Replace,
        vec![baseline.clone()],
    );

    let duplicate = item(&apps, "duplicate", "Duplicate");
    let rejected = catalog.apply(
        &apps,
        41,
        CatalogUpdate::Merge,
        vec![duplicate.clone(), duplicate.clone(), duplicate],
    );

    assert_eq!(
        rejected,
        Err(CatalogError::BatchItemLimitExceeded { actual: 3, limit: 2 })
    );
    assert_eq!(ordered_ids(&catalog, &apps), ["baseline"]);
    assert_same_item(
        stored(&catalog, &apps, "baseline").expect("raw-count rejection is atomic"),
        &baseline,
    );
}

#[test]
fn plugin_unique_limit_accepts_the_boundary_and_rejects_both_update_modes_atomically() {
    let mut catalog = MemoryCatalog::with_limits(CatalogLimits {
        max_plugin_items: 2,
        ..CatalogLimits::default()
    });
    let apps = plugin("dev.crikey.apps");
    activate(&mut catalog, &apps, 43);
    let alpha = item(&apps, "alpha", "Alpha");
    let beta = item(&apps, "beta", "Beta");
    publish(
        &mut catalog,
        &apps,
        43,
        CatalogUpdate::Replace,
        vec![alpha.clone(), beta.clone()],
    );

    for update in [CatalogUpdate::Replace, CatalogUpdate::Merge] {
        let items = match update {
            CatalogUpdate::Replace => vec![
                item(&apps, "replacement-a", "Replacement A"),
                item(&apps, "replacement-b", "Replacement B"),
                item(&apps, "replacement-c", "Replacement C"),
            ],
            CatalogUpdate::Merge => vec![
                item(&apps, "alpha", "Changed Alpha"),
                item(&apps, "gamma", "Gamma"),
            ],
        };
        assert_eq!(
            catalog.apply(&apps, 43, update, items),
            Err(CatalogError::PluginItemLimitExceeded { actual: 3, limit: 2 })
        );
        assert_eq!(ordered_ids(&catalog, &apps), ["alpha", "beta"]);
        assert_same_item(stored(&catalog, &apps, "alpha").expect("alpha remains"), &alpha);
        assert_same_item(stored(&catalog, &apps, "beta").expect("beta remains"), &beta);
    }
}

#[test]
fn total_unique_limit_accounts_for_replacement_and_other_plugins() {
    let mut catalog = MemoryCatalog::with_limits(CatalogLimits {
        max_plugin_items: 3,
        max_total_items: 3,
        ..CatalogLimits::default()
    });
    let apps = plugin("dev.crikey.apps");
    let files = plugin("dev.crikey.files");
    activate(&mut catalog, &apps, 47);
    activate(&mut catalog, &files, 49);

    publish(
        &mut catalog,
        &apps,
        47,
        CatalogUpdate::Replace,
        vec![item(&apps, "alpha", "Alpha"), item(&apps, "beta", "Beta")],
    );
    let notes = item(&files, "notes", "Notes");
    publish(
        &mut catalog,
        &files,
        49,
        CatalogUpdate::Replace,
        vec![notes.clone()],
    );
    assert_eq!(catalog.len(), 3);

    assert_eq!(
        catalog.apply(
            &files,
            49,
            CatalogUpdate::Merge,
            vec![item(&files, "draft", "Draft")],
        ),
        Err(CatalogError::TotalItemLimitExceeded { actual: 4, limit: 3 })
    );
    assert_eq!(ordered_ids(&catalog, &files), ["notes"]);
    assert_same_item(
        stored(&catalog, &files, "notes").expect("total-limit rejection is atomic"),
        &notes,
    );

    publish(
        &mut catalog,
        &apps,
        47,
        CatalogUpdate::Replace,
        vec![item(&apps, "alpha", "Alpha")],
    );
    publish(
        &mut catalog,
        &files,
        49,
        CatalogUpdate::Merge,
        vec![item(&files, "draft", "Draft")],
    );
    assert_eq!(
        (
            catalog.len(),
            catalog.plugin_len(&apps),
            catalog.plugin_len(&files)
        ),
        (3, 1, 2)
    );
}

#[test]
fn payload_accounting_covers_nested_actions_and_empty_members() {
    let apps = plugin("dev.crikey.apps");
    let baseline = item(&apps, "alpha", "Alpha");
    let baseline_bytes = measured_item_payload(&baseline);

    let mut nested_action_text = baseline.clone();
    let suffix = " nested action payload";
    nested_action_text.actions[0].description.push_str(suffix);
    assert_eq!(
        measured_item_payload(&nested_action_text),
        baseline_bytes + suffix.len()
    );

    let mut empty_search_term = baseline.clone();
    empty_search_term.search_terms.push(String::new());
    assert!(measured_item_payload(&empty_search_term) > baseline_bytes);

    let mut empty_action_category = baseline.clone();
    empty_action_category.actions[0]
        .applicable_categories
        .push(Category::Application);
    assert!(measured_item_payload(&empty_action_category) > baseline_bytes);
}

#[test]
fn item_and_batch_payload_limits_accept_exact_boundaries() {
    let apps = plugin("dev.crikey.apps");
    let candidate = item(&apps, "alpha", "Alpha");
    let item_bytes = measured_item_payload(&candidate);

    let mut exact_item = MemoryCatalog::with_limits(CatalogLimits {
        max_item_bytes: item_bytes,
        max_batch_bytes: item_bytes,
        ..CatalogLimits::default()
    });
    activate(&mut exact_item, &apps, 53);
    publish(
        &mut exact_item,
        &apps,
        53,
        CatalogUpdate::Replace,
        vec![candidate.clone()],
    );
    assert_eq!(exact_item.plugin_len(&apps), 1);

    let mut below_item = MemoryCatalog::with_limits(CatalogLimits {
        max_item_bytes: item_bytes - 1,
        max_batch_bytes: usize::MAX,
        ..CatalogLimits::default()
    });
    activate(&mut below_item, &apps, 53);
    assert_eq!(
        below_item.apply(&apps, 53, CatalogUpdate::Replace, vec![candidate.clone()],),
        Err(CatalogError::ItemPayloadLimitExceeded {
            actual: item_bytes,
            limit: item_bytes - 1,
        })
    );
    assert!(below_item.is_empty());

    let batch_bytes = item_bytes
        .checked_mul(2)
        .expect("the small fixture payload fits usize");
    let mut exact_batch = MemoryCatalog::with_limits(CatalogLimits {
        max_batch_items: 2,
        max_plugin_items: 1,
        max_total_items: 1,
        max_item_bytes: item_bytes,
        max_batch_bytes: batch_bytes,
    });
    activate(&mut exact_batch, &apps, 53);
    publish(
        &mut exact_batch,
        &apps,
        53,
        CatalogUpdate::Replace,
        vec![candidate.clone(), candidate.clone()],
    );
    assert_eq!(ordered_ids(&exact_batch, &apps), ["alpha"]);

    let mut below_batch = MemoryCatalog::with_limits(CatalogLimits {
        max_batch_items: 2,
        max_plugin_items: 1,
        max_total_items: 1,
        max_item_bytes: item_bytes,
        max_batch_bytes: batch_bytes - 1,
    });
    activate(&mut below_batch, &apps, 53);
    assert_eq!(
        below_batch.apply(
            &apps,
            53,
            CatalogUpdate::Replace,
            vec![candidate.clone(), candidate],
        ),
        Err(CatalogError::BatchPayloadLimitExceeded {
            actual: batch_bytes,
            limit: batch_bytes - 1,
        })
    );
    assert!(below_batch.is_empty());
}

#[test]
fn payload_rejections_are_atomic_for_replace_and_merge() {
    let apps = plugin("dev.crikey.apps");
    let baseline = item(&apps, "alpha", "Alpha");
    let baseline_bytes = measured_item_payload(&baseline);
    let mut oversized = baseline.clone();
    oversized.actions[0]
        .description
        .push_str(" exceeds the configured item payload");
    let oversized_bytes = measured_item_payload(&oversized);
    let mut catalog = MemoryCatalog::with_limits(CatalogLimits {
        max_item_bytes: baseline_bytes,
        max_batch_bytes: usize::MAX,
        ..CatalogLimits::default()
    });
    activate(&mut catalog, &apps, 59);
    publish(
        &mut catalog,
        &apps,
        59,
        CatalogUpdate::Replace,
        vec![baseline.clone()],
    );

    for update in [CatalogUpdate::Replace, CatalogUpdate::Merge] {
        assert_eq!(
            catalog.apply(&apps, 59, update, vec![oversized.clone()]),
            Err(CatalogError::ItemPayloadLimitExceeded {
                actual: oversized_bytes,
                limit: baseline_bytes,
            })
        );
        assert_eq!(ordered_ids(&catalog, &apps), ["alpha"]);
        assert_same_item(
            stored(&catalog, &apps, "alpha").expect("payload rejection preserves baseline"),
            &baseline,
        );
    }
}
