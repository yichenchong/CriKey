//! Selection affinity through the whole search service (spec 11.3).
//!
//! The ranker has always had a per-query affinity signal. What is tested here
//! is that it reaches a user at the keystroke they are on: a choice made once
//! the query was finished has to inform the query while it is still being
//! typed, which is where a launcher can actually save the typing.

use crikey_app::{App, SearchService, StartupStage};
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

/// Isolates the *per-query* signal from the per-item one.
///
/// Both fixtures are selected the same number of times, so `selection_frequency`
/// and `selection_recency_secs` are identical and cannot explain the order.
/// Both labels are the same length and match `chr` the same way, so match
/// quality cannot either. The only difference is which query was on screen when
/// the selection happened - and one of those queries is a continuation of what
/// is being typed now.
fn select(service: &mut SearchService, query: &str, label: &str, times: usize) {
    for _ in 0..times {
        service.submit_query(query).expect("query accepted");
        let chosen = service
            .results()
            .iter()
            .find(|hit| hit.item.label == label)
            .map(|hit| hit.item.stable_id.clone())
            .unwrap_or_else(|| panic!("{label:?} is on screen for {query:?}"));
        assert!(service.record_selection(&chosen), "selection is recorded");
    }
}

#[test]
fn a_prior_selection_lifts_the_item_while_the_query_is_still_being_typed() {
    let mut service = service(&["Chrome Alpha", "Chrome Bravo"]);

    // With no history at all, the tie breaks towards Alpha.
    service.submit_query("chr").expect("query accepted");
    assert_eq!(
        labels(&service),
        vec!["Chrome Alpha".to_string(), "Chrome Bravo".to_string()],
        "the fixture must tie on match quality and break towards Alpha"
    );

    // Equal item-level history for both, earned under different queries. Only
    // Bravo's query is one the user is part-way through when they type `chr`.
    select(&mut service, "alpha", "Chrome Alpha", 4);
    select(&mut service, "chrome bravo", "Chrome Bravo", 4);

    service.submit_query("chr").expect("query accepted");

    assert_eq!(
        labels(&service).first().map(String::as_str),
        Some("Chrome Bravo"),
        "the item chosen under a continuation of `chr` should now lead, got {:?}",
        labels(&service)
    );
}

/// The signal follows the evidence, not the labels or the baseline order.
///
/// Roles swapped *and* the affinity placed on the item that starts behind:
/// `Chrome Bravo` is the longer label, so it scores lower on match quality and
/// trails `Chrome Zulu` with no history. It can only lead by having earned it.
/// Without this, the test above would also pass on a rule that happened to
/// favour whichever item the matcher already preferred.
#[test]
fn the_lift_follows_whichever_item_earned_it() {
    let mut service = service(&["Chrome Bravo", "Chrome Zulu"]);

    service.submit_query("chr").expect("query accepted");
    assert_eq!(
        labels(&service),
        vec!["Chrome Zulu".to_string(), "Chrome Bravo".to_string()],
        "the shorter label leads before any history exists"
    );

    select(&mut service, "zulu", "Chrome Zulu", 4);
    select(&mut service, "chrome bravo", "Chrome Bravo", 4);

    service.submit_query("chr").expect("query accepted");

    assert_eq!(
        labels(&service).first().map(String::as_str),
        Some("Chrome Bravo"),
        "the item that started behind earned the lift and must lead, got {:?}",
        labels(&service)
    );
}
