//! Per-query selection affinity, and the bound that keeps it from being lost.
//!
//! Two properties are load bearing and neither was true before:
//!
//! * affinity applies to the query the user is *part-way through*, not only to
//!   the finished one they eventually typed;
//! * the store is bounded, because the file it is persisted to is refused above
//!   a size limit and a refused file loads as empty - so unbounded growth ended
//!   in losing the whole history, not in a graceful degradation.

use std::collections::BTreeMap;

use crikey_core::{ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId};
use crikey_query::{DefaultNormalizer, NormalizedQuery, Normalizer};
use crikey_ranking::{RankingSignals, SelectionHistory};

fn item(label: &str) -> Item {
    let plugin = PluginId("apps".to_owned());
    Item {
        stable_id: ItemId(label.to_ascii_lowercase()),
        plugin_id: plugin,
        category: Category::Application,
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

fn query(text: &str) -> NormalizedQuery {
    DefaultNormalizer::default().normalize(text)
}

/// The `query_history` signal an item would receive for `text`.
fn signal(history: &SelectionHistory, item: &Item, text: &str) -> f32 {
    let asked = query(text);
    let mut signals = RankingSignals::default();
    history.augment(item, &history.affinities_for(&asked), 0, None, &mut signals);
    signals.query_history
}

/// The motivating case: the affinity is wanted while the query is being typed,
/// not only once it has been finished.
#[test]
fn a_selection_counts_for_the_prefixes_of_the_query_that_earned_it() {
    let chrome = item("Google Chrome");
    let mut history = SelectionHistory::default();
    history.record(&chrome, &query("chrome"), 0);

    assert!(
        signal(&history, &chrome, "chr") > 0.0,
        "typing towards `chrome` must already benefit"
    );
    assert!(signal(&history, &chrome, "c") > 0.0);
    assert!(signal(&history, &chrome, "chrome") > 0.0);
}

/// A prefix is not a suffix and not a substring: only continuations of what is
/// on screen are evidence for what is on screen.
#[test]
fn a_selection_does_not_count_for_unrelated_queries() {
    let chrome = item("Google Chrome");
    let mut history = SelectionHistory::default();
    history.record(&chrome, &query("chrome"), 0);

    assert_eq!(signal(&history, &chrome, "rome"), 0.0, "not a suffix match");
    assert_eq!(signal(&history, &chrome, "chz"), 0.0, "not a divergent query");
    assert_eq!(
        signal(&history, &chrome, "chromex"),
        0.0,
        "the user has typed past everything ever recorded"
    );
}

/// Evidence accumulates across the different queries that reached the item,
/// because each of those selections happened with the shorter query on screen.
#[test]
fn counts_accumulate_across_the_queries_that_share_a_prefix() {
    let chrome = item("Google Chrome");
    let mut history = SelectionHistory::default();
    history.record(&chrome, &query("chrome"), 0);
    history.record(&chrome, &query("chr"), 0);

    let combined = signal(&history, &chrome, "chr");
    let exact_only = signal(&history, &chrome, "chrome");
    assert!(
        combined > exact_only,
        "`chr` sees both selections ({combined}) and `chrome` only its own ({exact_only})"
    );
}

/// Affinity is per item: one item's history must not lift another's.
#[test]
fn affinity_is_attributed_to_the_item_that_earned_it() {
    let chrome = item("Google Chrome");
    let chromium = item("Chromium");
    let mut history = SelectionHistory::default();
    history.record(&chrome, &query("chrome"), 0);

    assert!(signal(&history, &chrome, "chr") > 0.0);
    assert_eq!(signal(&history, &chromium, "chr"), 0.0);
}

/// More selections must never score lower, or the ranking would argue with the
/// evidence that produced it.
#[test]
fn repeated_selections_never_lower_the_signal() {
    let chrome = item("Google Chrome");
    let mut history = SelectionHistory::default();
    let mut previous = 0.0f32;
    for _ in 0..8 {
        history.record(&chrome, &query("chrome"), 0);
        let now = signal(&history, &chrome, "chr");
        assert!(now >= previous, "{now} < {previous}");
        previous = now;
    }
}

/// An empty query has no evidence to apply; it must not sum the whole history.
#[test]
fn an_empty_query_has_no_affinity() {
    let chrome = item("Google Chrome");
    let mut history = SelectionHistory::default();
    history.record(&chrome, &query("chrome"), 0);

    assert!(history.affinities_for(&query("")).is_empty());
}

/// The bound exists, and it is the most-used records that survive it.
#[test]
fn the_affinity_store_is_bounded_and_keeps_the_most_used() {
    let favourite = item("Favourite");
    let mut history = SelectionHistory::default();

    // Something used constantly, recorded under one query.
    for _ in 0..64 {
        history.record(&favourite, &query("fav"), 0);
    }
    // Then a flood of one-off queries, far past any sane cap.
    for index in 0..20_000 {
        history.record(&item(&format!("Once {index}")), &query(&format!("q{index}")), 0);
    }

    let snapshot = history.snapshot();
    assert!(
        snapshot.query_affinities.len() <= 8_192,
        "affinities are bounded, got {}",
        snapshot.query_affinities.len()
    );
    assert!(
        snapshot.selections.len() <= 4_096,
        "item records are bounded, got {}",
        snapshot.selections.len()
    );
    assert!(
        signal(&history, &favourite, "fav") > 0.0,
        "the record used 64 times must outlive 20,000 used once"
    );
}

/// A file written before the caps existed, or edited past them, must not
/// reintroduce unbounded growth on load.
#[test]
fn restoring_an_oversized_snapshot_re_applies_the_bound() {
    let mut history = SelectionHistory::default();
    for index in 0..20_000 {
        history.record(&item(&format!("Once {index}")), &query(&format!("q{index}")), 0);
    }
    let mut snapshot = history.snapshot();
    // Re-inflate the snapshot the way a hand-edited file could.
    for index in 20_000..40_000 {
        let extra = item(&format!("Extra {index}"));
        snapshot
            .query_affinities
            .push(crikey_ranking::QueryAffinityRecord {
                plugin: extra.plugin_id.clone(),
                item: extra.stable_id.clone(),
                query: format!("x{index}"),
                count: 1,
            });
    }

    let restored = SelectionHistory::from_snapshot(snapshot).snapshot();

    assert!(
        restored.query_affinities.len() <= 8_192,
        "got {}",
        restored.query_affinities.len()
    );
}

/// Persistence must stay lossless for everything that survives the bound: a
/// restored history has to score identically, or every launch would quietly
/// rewrite the user's ranking.
#[test]
fn a_snapshot_round_trip_preserves_prefix_affinity() {
    let chrome = item("Google Chrome");
    let mut history = SelectionHistory::default();
    history.record(&chrome, &query("chrome"), 0);
    history.record(&chrome, &query("chr"), 0);

    let restored = SelectionHistory::from_snapshot(history.snapshot());

    for text in ["c", "ch", "chr", "chro", "chrome"] {
        assert_eq!(
            signal(&restored, &chrome, text),
            signal(&history, &chrome, text),
            "restored history disagrees at {text:?}"
        );
    }
}

/// A record cap bounds how many keys are held, not how large they are. A query
/// is whatever the user typed - including something pasted - so without a size
/// bound the store could hold hundreds of megabytes, which no amount of
/// trimming at save time can undo because the memory is already taken.
#[test]
fn an_enormous_query_is_not_kept_as_an_affinity_key() {
    let firefox = item("Firefox");
    let mut history = SelectionHistory::default();
    let huge = "x".repeat(64 * 1024);

    history.record(&firefox, &query(&huge), 0);

    let snapshot = history.snapshot();
    assert!(
        snapshot.query_affinities.is_empty(),
        "a key that long is never retyped and must not be stored"
    );
    assert_eq!(
        snapshot.selections.len(),
        1,
        "but the selection itself is still worth remembering"
    );
}

/// The bound must not reject the ordinary case; a real query is tiny.
#[test]
fn an_ordinary_query_is_kept() {
    let firefox = item("Firefox");
    let mut history = SelectionHistory::default();

    history.record(&firefox, &query("firefox"), 0);

    assert_eq!(history.snapshot().query_affinities.len(), 1);
}

/// The item key is not safe to assume small either. A plugin chooses its own
/// ids, and this crate does not sit behind the catalog check that bounds an
/// item's payload - so an enormous id would otherwise be cloned into the
/// entries map and into every affinity that item ever earned.
#[test]
fn an_enormous_item_id_is_not_recorded_at_all() {
    let mut enormous = item("Firefox");
    enormous.stable_id = ItemId("x".repeat(64 * 1024));
    let mut history = SelectionHistory::default();

    history.record(&enormous, &query("fire"), 0);

    let snapshot = history.snapshot();
    assert!(
        snapshot.selections.is_empty(),
        "there is no half of the record that would not hold the id"
    );
    assert!(snapshot.query_affinities.is_empty());
}

/// An ordinary id is unaffected; real ids are short.
#[test]
fn an_ordinary_item_id_is_recorded() {
    let firefox = item("Firefox");
    let mut history = SelectionHistory::default();

    history.record(&firefox, &query("fire"), 0);

    assert_eq!(history.snapshot().selections.len(), 1);
}

/// The size bounds must hold for the store, not for one way into it.
///
/// A history file well under the reader's byte limit can still carry a handful
/// of enormous ids or queries - it only takes one. Restoring those would clone
/// them into every lookup, write them straight back out, and let them survive
/// eviction on the strength of whatever count the file claims.
#[test]
fn restoring_a_snapshot_rejects_oversized_keys() {
    let ordinary = item("Firefox");
    let huge_id = ItemId("x".repeat(64 * 1024));
    let huge_query = "q".repeat(64 * 1024);

    let snapshot = crikey_ranking::SelectionHistorySnapshot {
        selections: vec![
            crikey_ranking::SelectionRecord {
                plugin: ordinary.plugin_id.clone(),
                item: ordinary.stable_id.clone(),
                frequency: 3,
                last_selected_secs: Some(10),
            },
            crikey_ranking::SelectionRecord {
                plugin: ordinary.plugin_id.clone(),
                item: huge_id.clone(),
                frequency: 9_999,
                last_selected_secs: Some(10),
            },
        ],
        query_affinities: vec![
            crikey_ranking::QueryAffinityRecord {
                plugin: ordinary.plugin_id.clone(),
                item: ordinary.stable_id.clone(),
                query: "fire".to_owned(),
                count: 3,
            },
            // Oversized by its query, with an otherwise ordinary item.
            crikey_ranking::QueryAffinityRecord {
                plugin: ordinary.plugin_id.clone(),
                item: ordinary.stable_id.clone(),
                query: huge_query,
                count: 9_999,
            },
            // Oversized by its item id.
            crikey_ranking::QueryAffinityRecord {
                plugin: ordinary.plugin_id.clone(),
                item: huge_id,
                query: "fire".to_owned(),
                count: 9_999,
            },
        ],
    };

    let restored = SelectionHistory::from_snapshot(snapshot).snapshot();

    assert_eq!(
        restored.selections.len(),
        1,
        "only the ordinary item survives, got {:?}",
        restored
            .selections
            .iter()
            .map(|r| r.item.0.len())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        restored.query_affinities.len(),
        1,
        "only the ordinary affinity survives, got {:?}",
        restored
            .query_affinities
            .iter()
            .map(|r| (r.item.0.len(), r.query.len()))
            .collect::<Vec<_>>()
    );
    assert_eq!(restored.query_affinities[0].query, "fire");
    // A high count must not buy an oversized record a place; the bound is not
    // a ranking decision.
    assert_eq!(restored.selections[0].frequency, 3);
}
