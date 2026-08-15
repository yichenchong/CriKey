//! A plugin decides whether its catalog may outlive the process (spec 22.1).
//!
//! The launcher loads persisted slices and answers queries from them *before*
//! any plugin has run, which is the whole value of the cache and also its
//! hazard: a plugin whose items are only true while it is running would have
//! last week's answers offered as live ones. These tests pin both halves - the
//! slice is never written, and one written by an earlier declaration is
//! withdrawn from disk and from the running catalog.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use crikey_app::{App, CatalogBuild, SearchService, StartupStage};
use crikey_catalog::{CacheError, CachedSlice, CatalogCache};
use crikey_core::{ArgumentPolicy, Category, Generation, HitPolicy, Item, ItemId, PluginId};

const PRE_QUERY_STAGES: [(StartupStage, StartupStage); 3] = [
    (StartupStage::WindowAndHotkey, StartupStage::PersistedCatalog),
    (StartupStage::PersistedCatalog, StartupStage::AcceptQueries),
    (StartupStage::AcceptQueries, StartupStage::RequiredWorkers),
];

fn plugin() -> PluginId {
    PluginId("modern.sessions".to_owned())
}

fn item(owner: &PluginId, label: &str) -> Item {
    let category = Category::Application;
    Item {
        stable_id: ItemId::derived(owner, &category, label),
        plugin_id: owner.clone(),
        category,
        label: label.to_owned(),
        description: String::new(),
        target: format!("/usr/bin/{label}"),
        search_terms: Vec::new(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    }
}

/// Records what reached the on-disk cache, and can be primed with a slice a
/// previous run left behind.
#[derive(Debug, Default)]
struct RecordingCache {
    stored: Mutex<Vec<CachedSlice>>,
    invalidated: Mutex<Vec<PluginId>>,
    persisted: Mutex<Vec<CachedSlice>>,
}

impl RecordingCache {
    fn with_persisted(slice: CachedSlice) -> Self {
        Self {
            persisted: Mutex::new(vec![slice]),
            ..Self::default()
        }
    }

    fn stored_owners(&self) -> Vec<PluginId> {
        self.stored
            .lock()
            .expect("cache lock")
            .iter()
            .map(|slice| slice.plugin.clone())
            .collect()
    }

    fn invalidated_owners(&self) -> Vec<PluginId> {
        self.invalidated.lock().expect("cache lock").clone()
    }

    /// What a *next launch* would find on disk, which is the state that
    /// matters. The write log alone cannot answer it: loading a slice
    /// republishes it, so an owner can legitimately be written and then
    /// withdrawn within one run.
    fn holds(&self, plugin: &PluginId) -> bool {
        self.persisted
            .lock()
            .expect("cache lock")
            .iter()
            .any(|slice| &slice.plugin == plugin)
    }
}

impl CatalogCache for RecordingCache {
    fn load_slice(&self, plugin: &PluginId) -> Result<Option<CachedSlice>, CacheError> {
        Ok(self
            .persisted
            .lock()
            .expect("cache lock")
            .iter()
            .find(|slice| &slice.plugin == plugin)
            .cloned())
    }

    fn store_slice(&self, slice: &CachedSlice) -> Result<(), CacheError> {
        self.stored.lock().expect("cache lock").push(slice.clone());
        let mut persisted = self.persisted.lock().expect("cache lock");
        persisted.retain(|held| held.plugin != slice.plugin);
        persisted.push(slice.clone());
        Ok(())
    }

    fn invalidate(&self, plugin: &PluginId) -> Result<(), CacheError> {
        self.invalidated.lock().expect("cache lock").push(plugin.clone());
        self.persisted
            .lock()
            .expect("cache lock")
            .retain(|slice| &slice.plugin != plugin);
        Ok(())
    }

    fn plugins(&self) -> Result<Vec<PluginId>, CacheError> {
        Ok(self
            .persisted
            .lock()
            .expect("cache lock")
            .iter()
            .map(|slice| slice.plugin.clone())
            .collect())
    }
}

fn accepting(cache: &Arc<RecordingCache>) -> SearchService {
    let mut service = SearchService::new(App::new());
    for (stage, next) in PRE_QUERY_STAGES {
        assert_eq!(service.complete_stage(stage), Ok(Some(next)));
    }
    service.set_catalog_cache(Arc::clone(cache) as Arc<dyn CatalogCache + Send + Sync>);
    service
}

fn build(owner: &PluginId, labels: &[&str], persist: bool) -> CatalogBuild {
    CatalogBuild {
        plugin: owner.clone(),
        instance: 1,
        generation: Generation::ZERO,
        items: labels.iter().map(|label| item(owner, label)).collect(),
        persist,
    }
}

fn labels(service: &SearchService) -> Vec<String> {
    service
        .results()
        .iter()
        .map(|hit| hit.item.label.clone())
        .collect()
}

/// The default is unchanged: a plugin that says nothing is still cached, which
/// is what makes the launcher answer at startup.
#[test]
fn a_plugin_that_permits_persistence_is_written_to_the_cache() {
    let cache = Arc::new(RecordingCache::default());
    let mut service = accepting(&cache);

    build(&plugin(), &["Session A"], true)
        .publish(&mut service)
        .expect("publication accepted");

    assert_eq!(cache.stored_owners(), vec![plugin()]);
}

/// The declaration's whole point.
#[test]
fn a_plugin_that_refuses_persistence_is_never_written() {
    let cache = Arc::new(RecordingCache::default());
    let mut service = accepting(&cache);

    build(&plugin(), &["Session A"], false)
        .publish(&mut service)
        .expect("publication accepted");

    assert!(
        cache.stored_owners().is_empty(),
        "nothing may reach disk, got {:?}",
        cache.stored_owners()
    );
}

/// Refusing persistence must not refuse the items: they are live and correct
/// for as long as the process runs.
#[test]
fn refusing_persistence_still_serves_the_items_in_memory() {
    let cache = Arc::new(RecordingCache::default());
    let mut service = accepting(&cache);
    build(&plugin(), &["Session A"], false)
        .publish(&mut service)
        .expect("publication accepted");

    service.submit_query("session").expect("query accepted");

    assert_eq!(labels(&service), vec!["Session A".to_string()]);
}

/// A plugin that used to permit persistence has a slice on disk. Skipping the
/// write would leave it there forever, to be served at every future startup.
#[test]
fn a_slice_from_an_earlier_declaration_is_withdrawn_from_disk() {
    let stale = CachedSlice {
        plugin: plugin(),
        instance: 1,
        generation: Generation::ZERO,
        items: vec![item(&plugin(), "Yesterday's Session")],
    };
    let cache = Arc::new(RecordingCache::with_persisted(stale));
    let mut service = accepting(&cache);
    service
        .load_persisted_catalog(cache.as_ref())
        .expect("cache enumerates");

    service.set_catalog_persistence(&plugin(), false);

    assert_eq!(
        cache.invalidated_owners(),
        vec![plugin()],
        "the stale slice is deleted, not merely left unwritten"
    );
    assert!(!cache.holds(&plugin()), "so a next launch finds nothing to serve");
}

/// The stale slice is already searchable by the time the manifest is read, so
/// deleting the file is not enough - it has to leave the running catalog too.
#[test]
fn a_cache_loaded_slice_stops_answering_once_the_plugin_refuses_persistence() {
    let stale = CachedSlice {
        plugin: plugin(),
        instance: 1,
        generation: Generation::ZERO,
        items: vec![item(&plugin(), "Yesterday's Session")],
    };
    let cache = Arc::new(RecordingCache::with_persisted(stale));
    let mut service = accepting(&cache);
    service
        .load_persisted_catalog(cache.as_ref())
        .expect("cache enumerates");
    service.submit_query("yesterday").expect("query accepted");
    assert_eq!(
        labels(&service),
        vec!["Yesterday's Session".to_string()],
        "the fixture must actually be answering before the declaration arrives"
    );

    service.set_catalog_persistence(&plugin(), false);
    service.submit_query("yesterday").expect("query accepted");

    assert!(
        labels(&service).is_empty(),
        "a slice from a previous process must stop answering, got {:?}",
        labels(&service)
    );
}

/// Only cache-sourced items are withdrawn. Items the plugin published during
/// this run are current by definition, and dropping them would delete a
/// working catalog to protect against staleness that cannot exist.
#[test]
fn items_published_this_run_survive_the_refusal() {
    let stale = CachedSlice {
        plugin: plugin(),
        instance: 1,
        generation: Generation::ZERO,
        items: vec![item(&plugin(), "Yesterday's Session")],
    };
    let cache = Arc::new(RecordingCache::with_persisted(stale));
    let mut service = accepting(&cache);
    service
        .load_persisted_catalog(cache.as_ref())
        .expect("cache enumerates");

    build(&plugin(), &["Live Session"], false)
        .publish(&mut service)
        .expect("publication accepted");
    service.submit_query("session").expect("query accepted");

    assert_eq!(
        labels(&service),
        vec!["Live Session".to_string()],
        "the live publication replaced the cached slice and must be kept"
    );
    assert!(
        !cache.holds(&plugin()),
        "and a next launch finds nothing to serve"
    );
}

/// A plugin may go back to permitting persistence without a restart.
#[test]
fn permitting_persistence_again_resumes_writing() {
    let cache = Arc::new(RecordingCache::default());
    let mut service = accepting(&cache);
    build(&plugin(), &["Session A"], false)
        .publish(&mut service)
        .expect("publication accepted");
    assert!(cache.stored_owners().is_empty());

    build(&plugin(), &["Session B"], true)
        .publish(&mut service)
        .expect("publication accepted");

    assert_eq!(cache.stored_owners(), vec![plugin()]);
}
