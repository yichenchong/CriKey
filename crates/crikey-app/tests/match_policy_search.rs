//! `SearchService` must apply one match policy consistently across keystrokes.
//!
//! The warm path revisits the previous keystroke's candidates and re-filters
//! them. If that filter is stricter than the matcher, a result appears on one
//! keystroke and disappears on the next — the production failure mode that a
//! catalog-level test alone would not catch, because the wiring lives here.

use crikey_app::{App, SearchService, StartupStage};
use crikey_core::{ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId};
use crikey_query::{MatchMethod, MatchPolicy};

/// Startup stages a service must acknowledge before it accepts queries.
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

/// Types `text` one character at a time and reports the labels present after
/// each keystroke.
fn labels_while_typing(service: &mut SearchService, text: &str) -> Vec<Vec<String>> {
    let mut per_keystroke = Vec::new();
    for end in text.char_indices().map(|(index, ch)| index + ch.len_utf8()) {
        service
            .submit_query(&text[..end])
            .expect("service should accept the query");
        per_keystroke.push(
            service
                .results()
                .iter()
                .map(|hit| hit.item.label.clone())
                .collect(),
        );
    }
    per_keystroke
}

/// Types `text` one character at a time and reports every method reported.
fn methods_while_typing(service: &mut SearchService, text: &str) -> Vec<MatchMethod> {
    let mut methods = Vec::new();
    for end in text.char_indices().map(|(index, ch)| index + ch.len_utf8()) {
        service
            .submit_query(&text[..end])
            .expect("service should accept the query");
        methods.extend(service.results().iter().map(|hit| hit.method));
    }
    methods
}

/// With the subsequence policy enabled, a subsequence-only result must survive
/// every keystroke rather than vanishing once the warm path takes over.
#[test]
fn subsequence_results_survive_successive_keystrokes() {
    let mut service = service(&["Memory Diagnostic Tool", "Notepad", "Task Manager"]);
    service.set_match_policy(MatchPolicy::Subsequence);

    let per_keystroke = labels_while_typing(&mut service, "manic");
    for (index, labels) in per_keystroke.iter().enumerate() {
        assert!(
            labels.iter().any(|label| label == "Memory Diagnostic Tool"),
            "lost the subsequence result after {} character(s): {labels:?}",
            index + 1
        );
    }
}

/// The default service never credits a subsequence, on any keystroke.
///
/// Early prefixes of the query legitimately match by stronger readings — `m`
/// really is a prefix of `Memory Diagnostic Tool` — so the contract is about the
/// *method*, plus the label being gone once the full query is typed.
#[test]
fn default_service_never_credits_a_subsequence() {
    let mut service = service(&["Memory Diagnostic Tool", "Notepad", "Task Manager"]);

    let methods = methods_while_typing(&mut service, "manic");
    assert!(
        !methods.contains(&MatchMethod::Fuzzy),
        "the default service credited a subsequence: {methods:?}"
    );
    // `manic` decomposes over nothing in this catalog, so the answer is empty.
    assert!(
        service.results().is_empty(),
        "expected no results for the fully typed query, got {:?}",
        service
            .results()
            .iter()
            .map(|hit| hit.item.label.clone())
            .collect::<Vec<_>>()
    );
}

/// Abbreviations survive every keystroke under the default policy: each prefix
/// of `vscode` still decomposes over `Visual Studio Code`.
#[test]
fn word_prefix_results_survive_successive_keystrokes() {
    let mut service = service(&["Visual Studio Code", "Memory Diagnostic Tool", "Notepad"]);

    for (index, labels) in labels_while_typing(&mut service, "vscode").iter().enumerate() {
        assert!(
            labels.iter().any(|label| label == "Visual Studio Code"),
            "lost the word-prefix result after {} character(s): {labels:?}",
            index + 1
        );
    }
}

/// Switching policy mid-session must not serve results filtered under the old
/// one: the cache was narrowed strictly and cannot answer a looser query.
#[test]
fn changing_policy_discards_the_narrowed_cache() {
    let mut service = service(&["Memory Diagnostic Tool", "Notepad"]);

    // Warm the cache strictly; `manic` matches nothing.
    for text in ["m", "ma", "man"] {
        service.submit_query(text).expect("query should be accepted");
    }
    assert!(!service
        .results()
        .iter()
        .any(|hit| hit.item.label == "Memory Diagnostic Tool"));

    // Opting in must recover the candidate the strict pass discarded.
    service.set_match_policy(MatchPolicy::Subsequence);
    service.submit_query("manic").expect("query should be accepted");
    assert!(
        service
            .results()
            .iter()
            .any(|hit| hit.item.label == "Memory Diagnostic Tool"),
        "policy change did not invalidate the strictly narrowed cache"
    );
}
