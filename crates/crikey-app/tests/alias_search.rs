//! Aliases through the whole search service, across keystrokes (spec 21.2).
//!
//! `AliasTable` is unit tested in `crikey-query`. What can only be tested here
//! is the wiring: an alias rewrites the query *before* the incremental
//! candidate cache narrows against it, so a rewritten query must not be
//! narrowed against the set a differently-rewritten query produced.

use crikey_app::{AliasTable, App, SearchService, StartupStage};
use crikey_core::{ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId};

const PRE_QUERY_STAGES: [(StartupStage, StartupStage); 3] = [
    (StartupStage::WindowAndHotkey, StartupStage::PersistedCatalog),
    (StartupStage::PersistedCatalog, StartupStage::AcceptQueries),
    (StartupStage::AcceptQueries, StartupStage::RequiredWorkers),
];

fn plugin() -> PluginId {
    PluginId("dev.crikey.apps".to_string())
}

fn item(label: &str) -> Item {
    let owner = plugin();
    let category = Category::Application;
    Item {
        stable_id: ItemId::derived(&owner, &category, label),
        plugin_id: owner,
        category,
        label: label.to_string(),
        description: String::new(),
        target: format!("/usr/bin/{label}"),
        search_terms: Vec::new(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: Default::default(),
        actions: Vec::new(),
    }
}

fn service(labels: &[&str]) -> SearchService {
    let mut service = SearchService::new(App::new());
    for (stage, next) in PRE_QUERY_STAGES {
        assert_eq!(service.complete_stage(stage), Ok(Some(next)));
    }
    service
        .replace_catalog(&plugin(), 1, labels.iter().map(|label| item(label)).collect())
        .expect("catalog should accept items");
    service
}

fn labels(service: &SearchService) -> Vec<String> {
    service
        .results()
        .iter()
        .map(|hit| hit.item.label.clone())
        .collect()
}

fn aliases(pairs: &[(&str, &str)]) -> AliasTable {
    AliasTable::new(pairs.iter().map(|(alias, target)| (*alias, *target)))
}

/// The whole point, end to end.
#[test]
fn a_configured_alias_finds_the_item_it_names() {
    let mut service = service(&["Settings", "Notepad"]);
    service.set_aliases(aliases(&[("ss", "Settings")]));

    service.submit_query("ss").expect("query accepted");

    assert_eq!(labels(&service), vec!["Settings".to_string()]);
}

/// Without the alias the same query finds nothing, which is what makes the
/// previous test meaningful rather than a coincidence of the fixture.
#[test]
fn the_same_query_finds_nothing_without_the_alias() {
    let mut service = service(&["Settings", "Notepad"]);

    service.submit_query("ss").expect("query accepted");

    assert!(labels(&service).is_empty(), "got {:?}", labels(&service));
}

/// The incremental cache narrows the previous keystroke's candidates. Typing
/// into and back out of an alias must not strand the result set against a
/// query that is no longer being asked.
#[test]
fn results_are_correct_across_the_keystrokes_that_form_an_alias() {
    let mut service = service(&["Settings", "Sound Recorder", "Notepad"]);
    service.set_aliases(aliases(&[("ss", "Settings")]));

    let mut seen = Vec::new();
    for prefix in ["s", "ss", "sso"] {
        service.submit_query(prefix).expect("query accepted");
        seen.push((prefix, labels(&service)));
    }

    let (_, after_s) = &seen[0];
    assert!(
        after_s.contains(&"Sound Recorder".to_string()),
        "`s` is an ordinary prefix query, got {after_s:?}"
    );

    let (_, after_ss) = &seen[1];
    assert_eq!(
        after_ss,
        &vec!["Settings".to_string()],
        "`ss` is the alias and resolves to exactly its target"
    );

    let (_, after_sso) = &seen[2];
    assert!(
        after_sso.is_empty(),
        "`sso` is not the alias and matches nothing, got {after_sso:?}"
    );
}

/// A candidate set narrowed under the previous table is not a valid superset
/// for the next one, so installing a table must discard it.
#[test]
fn changing_the_alias_table_rescores_the_next_query() {
    let mut service = service(&["Settings", "Notepad"]);
    service.set_aliases(aliases(&[("ed", "Settings")]));
    service.submit_query("ed").expect("query accepted");
    assert_eq!(labels(&service), vec!["Settings".to_string()]);

    service.set_aliases(aliases(&[("ed", "Notepad")]));
    service.submit_query("ed").expect("query accepted");

    assert_eq!(
        labels(&service),
        vec!["Notepad".to_string()],
        "the new table decides, not the set the old one narrowed"
    );
}

/// Retracting every alias restores unaliased behaviour rather than leaving the
/// last rewrite latched.
#[test]
fn clearing_the_alias_table_restores_the_literal_query() {
    let mut service = service(&["Settings", "Notepad"]);
    service.set_aliases(aliases(&[("ss", "Settings")]));
    service.submit_query("ss").expect("query accepted");
    assert_eq!(labels(&service), vec!["Settings".to_string()]);

    service.set_aliases(AliasTable::default());
    service.submit_query("ss").expect("query accepted");

    assert!(labels(&service).is_empty(), "got {:?}", labels(&service));
}

/// An alias composes with the rest of the query, so it is usable mid-sentence
/// and not only as a whole-query shorthand.
#[test]
fn an_alias_narrows_alongside_an_ordinary_token() {
    let mut service = service(&["Sound Recorder", "Sound Settings", "Notepad"]);
    service.set_aliases(aliases(&[("snd", "Sound")]));

    service.submit_query("snd rec").expect("query accepted");

    assert_eq!(labels(&service), vec!["Sound Recorder".to_string()]);
}
