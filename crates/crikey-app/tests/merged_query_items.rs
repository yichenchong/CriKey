//! Ranking asynchronously supplied items into the current answer.
//!
//! The behaviour under test is [`SearchService::merge_query_items`]: items a
//! provider produces after the catalog pass has already published must be
//! *ranked against* the catalog hits, not appended after them. The reported
//! bug is exactly that appending: typing `mor` put `Memory Diagnostics Tool`
//! (a substring reading of `me·mor·y`) above `Mortgage Analysis` (a prefix
//! reading), and no amount of use could change it.
//!
//! ```ignore
//! pub fn merge_query_items(
//!     &mut self,
//!     generation: Generation,
//!     plugin: &PluginId,
//!     items: Vec<Item>,
//! ) -> bool;
//! ```

use std::collections::BTreeMap;

use crikey_app::{App, ResultLimits, SearchService, StartupStage};
use crikey_core::{ArgumentPolicy, Category, Generation, HitPolicy, Item, ItemId, PluginId};
use crikey_query::MatchMethod;

/// The catalog owner, and the separate owner every merged batch belongs to.
const APPS: &str = "dev.crikey.applications";
const FILES: &str = "dev.crikey.files";
/// A second asynchronous owner, so per-plugin replacement is observable.
const NOTES: &str = "dev.crikey.notes";

const PRE_QUERY_STAGES: [StartupStage; 3] = [
    StartupStage::WindowAndHotkey,
    StartupStage::PersistedCatalog,
    StartupStage::AcceptQueries,
];

fn item(plugin: &str, id: &str, label: &str, category: Category) -> Item {
    Item {
        stable_id: ItemId(id.to_owned()),
        plugin_id: PluginId(plugin.to_owned()),
        category,
        label: label.to_owned(),
        description: String::new(),
        target: format!("target://{id}"),
        search_terms: Vec::new(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    }
}

fn app(id: &str, label: &str) -> Item {
    item(APPS, id, label, Category::Application)
}

fn file(id: &str, label: &str) -> Item {
    item(FILES, id, label, Category::File)
}

/// A query-ready service whose catalog holds only the substring-matching
/// application, so anything else in the answer arrived by merging.
///
/// `Memory Diagnostics Tool` answers `mor` through `me·mor·y`, which is the
/// weakest reading the strict matcher will credit on a label.
fn service() -> SearchService {
    let mut service = SearchService::new(App::new());
    for stage in PRE_QUERY_STAGES {
        service
            .complete_stage(stage)
            .expect("startup milestones are acknowledged in order");
    }
    service
        .replace_catalog(
            &PluginId(APPS.to_owned()),
            1,
            vec![
                app("memory", "Memory Diagnostics Tool"),
                app("water", "Water Clock"),
            ],
        )
        .expect("the fixture catalog is accepted");
    service
}

fn ids(service: &SearchService) -> Vec<String> {
    service
        .results()
        .iter()
        .map(|hit| hit.item.stable_id.0.clone())
        .collect()
}

fn files_plugin() -> PluginId {
    PluginId(FILES.to_owned())
}

// ---------------------------------------------------------------------------
// The reported bug
// ---------------------------------------------------------------------------

/// The whole point of the feature. A merged file whose name the query prefixes
/// must outrank an application the query merely occurs inside.
///
/// Fails on a merge that appends instead of re-sorting, and on one that scores
/// merged items with anything other than the catalog pass's own matcher: the
/// order here is decided by the match-method band, prefix over substring, and
/// survives the `File` category weight being well under `Application`'s.
#[test]
fn a_prefix_matching_merged_item_outranks_a_substring_matching_application() {
    let mut service = service();
    let generation = service.submit_query("mor").expect("query accepted");
    assert_eq!(
        ids(&service),
        vec!["memory".to_string()],
        "only the substring-matching application answers from the catalog"
    );

    assert!(service.merge_query_items(
        generation,
        &files_plugin(),
        vec![file("mortgage", "Mortgage Analysis")]
    ));

    assert_eq!(
        ids(&service),
        vec!["mortgage".to_string(), "memory".to_string()],
        "the prefix-matching file must lead the substring-matching application"
    );
    let leader = &service.results()[0];
    assert_eq!(
        leader.method,
        MatchMethod::Prefix,
        "the merged item must be placed by the shared matcher's own method"
    );
    assert!(
        !leader.highlights.is_empty(),
        "a merged hit carries the matcher's highlights, like any other hit"
    );
    assert!(service.results()[0].score > service.results()[1].score);
}

// ---------------------------------------------------------------------------
// Generation discipline
// ---------------------------------------------------------------------------

/// A batch that arrives after the user has typed another character answers a
/// question nobody is asking any more.
#[test]
fn a_merged_batch_from_a_stale_generation_is_refused_and_changes_nothing() {
    let mut service = service();
    let stale = service.submit_query("mor").expect("query accepted");
    service.submit_query("mor").expect("query accepted");

    assert!(
        !service.merge_query_items(
            stale,
            &files_plugin(),
            vec![file("mortgage", "Mortgage Analysis")]
        ),
        "a stale generation must be refused"
    );
    assert_eq!(
        ids(&service),
        vec!["memory".to_string()],
        "a refused batch must leave no trace in the answer"
    );
}

/// A generation that has never been allocated is not the current one either.
#[test]
fn a_merged_batch_for_an_unreached_generation_is_refused() {
    let mut service = service();
    service.submit_query("mor").expect("query accepted");

    assert!(!service.merge_query_items(
        Generation::from_raw(500),
        &files_plugin(),
        vec![file("mortgage", "Mortgage Analysis")]
    ));
    assert_eq!(ids(&service), vec!["memory".to_string()]);
}

// ---------------------------------------------------------------------------
// Replacement and lifetime
// ---------------------------------------------------------------------------

/// A provider that refines its answer replaces its own batch. Duplicating it
/// would show the same file twice and, worse, make a second batch that dropped
/// a file unable to withdraw it.
///
/// The second owner's batch is the control: replacement is scoped to the
/// plugin that re-sent, not to every merged item in the answer.
#[test]
fn a_second_merged_batch_for_one_plugin_replaces_the_first() {
    let mut service = service();
    let generation = service.submit_query("mor").expect("query accepted");

    assert!(service.merge_query_items(
        generation,
        &files_plugin(),
        vec![
            file("mortgage", "Mortgage Analysis"),
            file("morse", "Morse Table")
        ]
    ));
    assert!(service.merge_query_items(
        generation,
        &PluginId(NOTES.to_owned()),
        vec![item(NOTES, "morning", "Morning Notes", Category::File)]
    ));

    assert!(service.merge_query_items(
        generation,
        &files_plugin(),
        vec![file("mortgage", "Mortgage Analysis")]
    ));

    let mut observed = ids(&service);
    observed.sort();
    assert_eq!(
        observed,
        vec![
            "memory".to_string(),
            "morning".to_string(),
            "mortgage".to_string()
        ],
        "the re-sent batch replaces itself and leaves the other owner alone"
    );
}

/// Merged items live for their generation only. Nothing clears them
/// explicitly, so this is the test that has to prove the next query does.
#[test]
fn merged_items_are_gone_from_the_next_querys_answer() {
    let mut service = service();
    let generation = service.submit_query("mor").expect("query accepted");
    assert!(service.merge_query_items(
        generation,
        &files_plugin(),
        vec![file("mortgage", "Mortgage Analysis")]
    ));

    service.submit_query("mor").expect("query accepted");

    assert_eq!(
        ids(&service),
        vec!["memory".to_string()],
        "a merged item must not answer a later query"
    );
}

/// Merged items are not catalog items. Leaking one into the catalog would let
/// it answer a query the provider was never asked about, and would offer it to
/// the persisted catalog cache.
#[test]
fn a_merged_item_never_becomes_catalog_state() {
    let mut service = service();
    let generation = service.submit_query("mor").expect("query accepted");
    assert!(service.merge_query_items(
        generation,
        &files_plugin(),
        vec![file("mortgage", "Mortgage Analysis")]
    ));

    // A query only the merged label answers: the catalog must not know it.
    service.submit_query("mortgage").expect("query accepted");
    assert!(
        ids(&service).is_empty(),
        "the merged label must not be searchable, got {:?}",
        ids(&service)
    );
}

// ---------------------------------------------------------------------------
// Matching and identity
// ---------------------------------------------------------------------------

/// A provider may over-produce; the answer is still the current query's.
/// Dropping the non-matcher beats scoring it to zero, which would leave an
/// irrelevant row on screen at the bottom of the list.
#[test]
fn a_merged_item_that_does_not_match_the_query_is_dropped() {
    let mut service = service();
    let generation = service.submit_query("mor").expect("query accepted");

    assert!(service.merge_query_items(
        generation,
        &files_plugin(),
        vec![
            file("mortgage", "Mortgage Analysis"),
            file("budget", "Budget Spreadsheet"),
        ]
    ));

    assert_eq!(
        ids(&service),
        vec!["mortgage".to_string(), "memory".to_string()],
        "only the item that answers the query is merged"
    );
}

/// Ids address rows for selection and for history, so the answer must never
/// hold two of one id. The incumbent wins: it is already on screen, and a
/// catalog item is the durable definition of that id.
#[test]
fn a_merged_item_whose_id_collides_with_a_catalog_item_is_dropped() {
    let mut service = service();
    let generation = service.submit_query("mor").expect("query accepted");

    assert!(service.merge_query_items(
        generation,
        &files_plugin(),
        // Same stable id as the catalog application, with a label that would
        // otherwise win on a prefix match.
        vec![file("memory", "Mortgage Analysis")]
    ));

    assert_eq!(ids(&service), vec!["memory".to_string()]);
    let survivor = &service.results()[0];
    assert_eq!(
        survivor.item.label, "Memory Diagnostics Tool",
        "the catalog item keeps the id it owns"
    );
    assert_eq!(survivor.item.category, Category::Application);
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

/// Before merging existed, `record_selection` could not find a file row at all
/// and silently recorded nothing, so opening the same file every day never
/// improved its rank. A merged row must be recordable like any other.
#[test]
fn selecting_a_merged_item_records_history_for_it() {
    let mut service = service();
    service.set_history_time(1_000);
    let generation = service.submit_query("mor").expect("query accepted");
    assert!(service.merge_query_items(
        generation,
        &files_plugin(),
        vec![file("mortgage", "Mortgage Analysis")]
    ));

    assert!(
        service.record_selection(&ItemId("mortgage".to_owned())),
        "a merged row is a visible row"
    );

    let snapshot = service.selection_history_snapshot();
    let record = snapshot
        .selections
        .iter()
        .find(|record| record.item == ItemId("mortgage".to_owned()))
        .expect("the merged selection is recorded");
    assert_eq!(record.plugin, files_plugin());
    assert_eq!(record.frequency, 1);
    assert_eq!(record.last_selected_secs, Some(1_000));
    assert!(
        snapshot
            .query_affinities
            .iter()
            .any(|affinity| affinity.item == ItemId("mortgage".to_owned()) && affinity.query == "mor"),
        "the selection is attributed to the query that was on screen"
    );
}

/// Recorded history has to reach the merged item's score, or recording it
/// would be bookkeeping with no effect on rank.
///
/// Both labels answer `memo` with a label prefix, so the readings are the same
/// band and the only thing separating them is the `File` category weight: the
/// file trails on merit and can lead only by having been selected.
#[test]
fn recorded_history_lifts_a_merged_item_on_a_later_query() {
    let mut service = service();
    service.set_history_time(1_000);

    let baseline = service.submit_query("memo").expect("query accepted");
    assert!(service.merge_query_items(
        baseline,
        &files_plugin(),
        vec![file("notes", "Memory Notes Archive")]
    ));
    assert_eq!(
        ids(&service),
        vec!["memory".to_string(), "notes".to_string()],
        "the application must lead before any history exists"
    );
    let cold = service.results()[1].score;

    assert!(service.record_selection(&ItemId("notes".to_owned())));

    service.set_history_time(1_010);
    let warm = service.submit_query("memo").expect("query accepted");
    assert!(service.merge_query_items(warm, &files_plugin(), vec![file("notes", "Memory Notes Archive")]));

    assert_eq!(
        ids(&service),
        vec!["notes".to_string(), "memory".to_string()],
        "the selected file now leads"
    );
    assert!(
        service.results()[0].score > cold,
        "the recorded selection must raise the merged item's own score"
    );
}

/// A merged batch may not push the answer past the configured ceiling.
///
/// The catalog pass applies `max_items_per_query` inside `select_best`, so
/// appending to its answer re-opens the bound the launcher was configured
/// with -- `launcher.max-results` lowers exactly this number. Truncating after
/// the sort is what keeps it a discard of the *worst* rows: a file that
/// outranks an application displaces it rather than being dropped for having
/// arrived second.
#[test]
fn a_merged_batch_cannot_push_the_answer_past_the_result_limit() {
    let limits = ResultLimits {
        max_items_per_query: 3,
        ..ResultLimits::default()
    };
    let mut service = SearchService::new(App::with_limits(limits));
    for stage in PRE_QUERY_STAGES {
        service
            .complete_stage(stage)
            .expect("startup milestones are acknowledged in order");
    }
    service
        .replace_catalog(
            &PluginId(APPS.to_owned()),
            1,
            vec![app("memory", "Memory Diagnostics Tool")],
        )
        .expect("the fixture catalog is accepted");

    let generation = service.submit_query("mor").expect("query accepted");
    // Six prefix matches against a ceiling of three.
    let batch: Vec<Item> = (0..6)
        .map(|index| {
            file(
                &format!("mortgage-{index}"),
                &format!("Mortgage Analysis {index}"),
            )
        })
        .collect();
    assert!(service.merge_query_items(generation, &files_plugin(), batch));

    assert_eq!(
        service.results().len(),
        3,
        "the merged answer is still bounded by max_items_per_query, got {:?}",
        ids(&service)
    );
    assert!(
        service
            .results()
            .iter()
            .all(|hit| hit.item.stable_id.0.starts_with("mortgage-")),
        "the prefix matches displace the substring match rather than the reverse, got {:?}",
        ids(&service)
    );
}
