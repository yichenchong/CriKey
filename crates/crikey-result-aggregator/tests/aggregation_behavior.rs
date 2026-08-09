//! Behavioural contract for the in-memory result aggregator
//! (spec 11.5 - 11.7, 12.1, 12.5, 12.6; roadmap M1 "generation gating,
//! owner-scoped dedup, limits").
//!
//! These tests pin the public API and observable behaviour of the M1
//! aggregator:
//!
//! * `MemoryResultAggregator::new(ResultLimits)` builds an aggregator with no
//!   active generation, so nothing can be merged before `begin_generation`.
//! * `begin_generation(Generation)` advances monotonically to exactly one
//!   active generation and discards every item, quota counter, stream state and
//!   pending snapshot belonging to the previous one. Repeating the active
//!   generation is idempotent, and retired generations cannot reactivate.
//! * `items() -> &[Item]` exposes the retained set in *first-acceptance order*.
//!   No ranking score reaches the aggregator in M1, so first-acceptance order is
//!   the deterministic tie-break that keeps the list stable (spec 11.5, 11.6);
//!   an enrichment update replaces its item in place and never moves it.
//! * `take_ui_update() -> Option<Vec<Item>>` yields a pending snapshot equal to
//!   `items()` at the moment it is taken. Accepts coalesce into the newest
//!   snapshot, and `begin_frame()` replenishes the configured per-frame budget.
//! * `accept` / `retire_before` come from the `ResultAggregator` trait. A batch
//!   that would breach any safety limit is rejected *whole*: no item merged, no
//!   quota consumed, no UI update scheduled (spec 11.7).
//! * `retire_before(g)` advances an exclusive retirement floor. If the active
//!   generation falls below it, the aggregator is left with no active
//!   generation and later batches for it are `StaleGeneration`.
//! * `BatchState` is retained per plugin. A terminal batch merges its items,
//!   then rejects all later traffic from that plugin until the next generation
//!   (spec 12.5).

use std::collections::BTreeMap;

use crikey_core::{
    ArgumentPolicy, Category, Generation, GenerationTracker, HitPolicy, Item, ItemId, PluginId,
};
use crikey_result_aggregator::{
    BatchState, MemoryResultAggregator, RejectReason, ResultAggregator, ResultBatch, ResultLimits,
};

// ---------------------------------------------------------------------------
// Fixtures. Deliberately tiny: limits are small enough to breach in one batch.
// ---------------------------------------------------------------------------

fn limits(per_batch: usize, per_plugin: usize, per_query: usize) -> ResultLimits {
    ResultLimits {
        max_items_per_batch: per_batch,
        max_items_per_plugin_per_query: per_plugin,
        max_items_per_query: per_query,
        max_icon_reference_bytes_per_batch: usize::MAX,
        max_metadata_bytes_per_batch: usize::MAX,
        max_ui_updates_per_frame: 1,
    }
}

fn plugin(id: &str) -> PluginId {
    PluginId(id.to_owned())
}

fn item(owner: &PluginId, stable_id: &str, label: &str) -> Item {
    Item {
        stable_id: ItemId(stable_id.to_owned()),
        plugin_id: owner.clone(),
        category: Category::Application,
        label: label.to_owned(),
        description: String::new(),
        target: format!("/usr/bin/{stable_id}"),
        search_terms: Vec::new(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    }
}

fn batch(generation: Generation, owner: &PluginId, state: BatchState, items: Vec<Item>) -> ResultBatch {
    ResultBatch {
        generation,
        plugin: owner.clone(),
        state,
        items,
    }
}

fn partial(generation: Generation, owner: &PluginId, items: Vec<Item>) -> ResultBatch {
    batch(generation, owner, BatchState::Partial, items)
}

/// Stable identifiers in retained order: the only observable ordering the
/// aggregator can offer while no score is available.
fn ids(items: &[Item]) -> Vec<&str> {
    items.iter().map(|it| it.stable_id.0.as_str()).collect()
}

/// An aggregator already on a fresh generation, with the opening UI update
/// drained so each test starts from a quiet state.
fn started(tracker: &GenerationTracker, limits: ResultLimits) -> (MemoryResultAggregator, Generation) {
    let generation = tracker.advance();
    let mut aggregator = MemoryResultAggregator::new(limits);
    aggregator.begin_generation(generation);
    let _ = aggregator.take_ui_update();
    aggregator.begin_frame();
    (aggregator, generation)
}
#[test]
fn manifest_result_limits_override_global_plugin_ceilings() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(8, 8, 8));
    let owner = plugin("dev.crikey.manifest-limited");
    aggregator.set_plugin_limits(owner.clone(), 3, 2);

    assert_eq!(
        aggregator.accept(partial(
            generation,
            &owner,
            vec![
                item(&owner, "one", "One"),
                item(&owner, "two", "Two"),
                item(&owner, "three", "Three"),
            ],
        )),
        Err(RejectReason::QuotaExceeded),
        "the manifest batch ceiling rejects a whole oversized publication"
    );
    aggregator
        .accept(partial(
            generation,
            &owner,
            vec![item(&owner, "one", "One"), item(&owner, "two", "Two")],
        ))
        .expect("the first manifest-sized batch fits");
    assert_eq!(
        aggregator.accept(partial(
            generation,
            &owner,
            vec![item(&owner, "three", "Three"), item(&owner, "four", "Four")],
        )),
        Err(RejectReason::QuotaExceeded),
        "the manifest query ceiling rejects the breaching publication"
    );
}

// ---------------------------------------------------------------------------
// Accepting the active generation.
// ---------------------------------------------------------------------------

#[test]
fn active_generation_batch_is_retained_in_arrival_order() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(8, 8, 8));
    let apps = plugin("dev.crikey.apps");

    aggregator
        .accept(partial(
            generation,
            &apps,
            vec![
                item(&apps, "alpha", "Alpha"),
                item(&apps, "beta", "Beta"),
                item(&apps, "gamma", "Gamma"),
            ],
        ))
        .expect("a batch for the active generation is merged");

    assert_eq!(ids(aggregator.items()), ["alpha", "beta", "gamma"]);
}

#[test]
fn items_from_several_plugins_keep_first_acceptance_order() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(8, 8, 8));
    let apps = plugin("dev.crikey.apps");
    let files = plugin("dev.crikey.files");

    aggregator
        .accept(partial(generation, &apps, vec![item(&apps, "alpha", "Alpha")]))
        .expect("first plugin merges");
    aggregator
        .accept(partial(generation, &files, vec![item(&files, "notes", "Notes")]))
        .expect("second plugin merges");
    aggregator
        .accept(partial(generation, &apps, vec![item(&apps, "beta", "Beta")]))
        .expect("first plugin merges again");

    // Arrival order, not plugin-grouped order: reordering already-shown rows is
    // exactly the disruption spec 11.6 asks the aggregator to avoid.
    assert_eq!(ids(aggregator.items()), ["alpha", "notes", "beta"]);
}

// ---------------------------------------------------------------------------
// Generation gating (spec 8.1).
// ---------------------------------------------------------------------------

#[test]
fn stale_generation_batch_is_rejected_without_mutating_state() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, old) = started(&tracker, limits(8, 8, 8));
    let apps = plugin("dev.crikey.apps");

    aggregator
        .accept(partial(old, &apps, vec![item(&apps, "alpha", "Alpha")]))
        .expect("the old generation was active at the time");

    let current = tracker.advance();
    aggregator.begin_generation(current);
    aggregator
        .accept(partial(current, &apps, vec![item(&apps, "beta", "Beta")]))
        .expect("the new generation accepts its own results");
    let _ = aggregator.take_ui_update();

    let rejected = aggregator.accept(partial(old, &apps, vec![item(&apps, "gamma", "Gamma")]));

    assert_eq!(rejected, Err(RejectReason::StaleGeneration));
    assert_eq!(ids(aggregator.items()), ["beta"]);
    assert!(
        aggregator.take_ui_update().is_none(),
        "a rejected batch changes nothing, so there is nothing to repaint"
    );
}

#[test]
fn cancelled_batch_from_an_obsolete_generation_is_rejected_before_terminal_state() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, old) = started(&tracker, limits(8, 8, 8));
    let apps = plugin("dev.crikey.apps");
    let current = tracker.advance();
    aggregator.begin_generation(current);
    let _ = aggregator.take_ui_update();

    assert_eq!(
        aggregator.accept(batch(
            old,
            &apps,
            BatchState::Cancelled,
            vec![item(&apps, "late", "Late")],
        )),
        Err(RejectReason::StaleGeneration)
    );
    assert!(aggregator.items().is_empty());
    assert_eq!(aggregator.plugin_state(&apps), None);
    assert!(aggregator.take_ui_update().is_none());
}

#[test]
fn batch_for_a_generation_never_begun_is_rejected() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, _active) = started(&tracker, limits(8, 8, 8));
    let apps = plugin("dev.crikey.apps");
    let unannounced = tracker.advance();

    let rejected = aggregator.accept(partial(unannounced, &apps, vec![item(&apps, "alpha", "Alpha")]));

    assert_eq!(rejected, Err(RejectReason::StaleGeneration));
    assert!(aggregator.items().is_empty());

    // The very same batch merges once the aggregator is told the generation is live.
    aggregator.begin_generation(unannounced);
    aggregator
        .accept(partial(unannounced, &apps, vec![item(&apps, "alpha", "Alpha")]))
        .expect("the generation is active now");
    assert_eq!(ids(aggregator.items()), ["alpha"]);
}

#[test]
fn begin_generation_clears_items_and_quota_counters() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, first) = started(&tracker, limits(4, 2, 8));
    let apps = plugin("dev.crikey.apps");

    aggregator
        .accept(partial(
            first,
            &apps,
            vec![item(&apps, "alpha", "Alpha"), item(&apps, "beta", "Beta")],
        ))
        .expect("two items fit the per-plugin quota of two");
    assert_eq!(
        aggregator.accept(partial(first, &apps, vec![item(&apps, "gamma", "Gamma")])),
        Err(RejectReason::QuotaExceeded),
        "the per-plugin quota is exhausted for this generation"
    );

    let second = tracker.advance();
    aggregator.begin_generation(second);

    assert!(
        aggregator.items().is_empty(),
        "results of a retired generation must not survive the transition"
    );
    aggregator
        .accept(partial(
            second,
            &apps,
            vec![item(&apps, "delta", "Delta"), item(&apps, "epsilon", "Epsilon")],
        ))
        .expect("quota is per query, so the new generation starts the plugin at zero");
    assert_eq!(ids(aggregator.items()), ["delta", "epsilon"]);
    assert_eq!(
        aggregator.accept(partial(first, &apps, vec![item(&apps, "alpha", "Alpha")])),
        Err(RejectReason::StaleGeneration)
    );
}

#[test]
fn begin_generation_replaces_a_pending_update_with_the_new_empty_snapshot() {
    let tracker = GenerationTracker::new();
    let first = tracker.advance();
    let mut aggregator = MemoryResultAggregator::new(limits(8, 8, 8));
    let apps = plugin("dev.crikey.apps");

    aggregator.begin_generation(first);
    aggregator
        .accept(partial(
            first,
            &apps,
            vec![item(&apps, "alpha", "Alpha"), item(&apps, "beta", "Beta")],
        ))
        .expect("merged into the active generation");

    // The UI never drained that update; the generation changes underneath it.
    let second = tracker.advance();
    aggregator.begin_generation(second);

    let update = aggregator
        .take_ui_update()
        .expect("clearing the list is itself a visible state change");
    assert!(
        update.is_empty(),
        "a pending snapshot must never leak items of a retired generation"
    );
    assert_eq!(ids(&update), ids(aggregator.items()));
    assert!(aggregator.take_ui_update().is_none());
}

#[test]
fn retire_before_keeps_the_named_generation_and_drops_older_state() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, active) = started(&tracker, limits(8, 8, 8));
    let apps = plugin("dev.crikey.apps");

    aggregator
        .accept(partial(
            active,
            &apps,
            vec![item(&apps, "alpha", "Alpha"), item(&apps, "beta", "Beta")],
        ))
        .expect("merged into the active generation");

    // "Before" is exclusive: the active generation is not older than itself.
    aggregator.retire_before(active);
    assert_eq!(ids(aggregator.items()), ["alpha", "beta"]);

    let newer = tracker.advance();
    aggregator.retire_before(newer);

    assert!(
        aggregator.items().is_empty(),
        "state for generations older than the retirement point is dropped"
    );
    assert_eq!(
        aggregator.accept(partial(active, &apps, vec![item(&apps, "gamma", "Gamma")])),
        Err(RejectReason::StaleGeneration),
        "a retired generation can never merge again"
    );
}

#[test]
fn repeating_the_active_generation_is_idempotent() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(4, 1, 8));
    let apps = plugin("dev.crikey.apps");

    aggregator
        .accept(partial(generation, &apps, vec![item(&apps, "alpha", "Alpha")]))
        .expect("the one retained-identity slot is available");
    let _ = aggregator.take_ui_update();

    aggregator.begin_generation(generation);

    assert_eq!(ids(aggregator.items()), ["alpha"]);
    assert_eq!(aggregator.plugin_state(&apps), Some(BatchState::Partial));
    assert_eq!(
        aggregator.accept(partial(generation, &apps, vec![item(&apps, "beta", "Beta")])),
        Err(RejectReason::QuotaExceeded),
        "an idempotent begin must not erase retained items or quota counters"
    );
    aggregator.begin_frame();
    assert!(
        aggregator.take_ui_update().is_none(),
        "an idempotent begin does not schedule an empty replacement snapshot"
    );
}

#[test]
fn an_older_begin_cannot_replace_a_newer_active_generation() {
    let tracker = GenerationTracker::new();
    let older = tracker.advance();
    let newer = tracker.advance();
    let mut aggregator = MemoryResultAggregator::new(limits(8, 8, 8));
    let apps = plugin("dev.crikey.apps");

    aggregator.begin_generation(newer);
    let _ = aggregator.take_ui_update();
    aggregator.begin_frame();
    aggregator
        .accept(partial(newer, &apps, vec![item(&apps, "current", "Current")]))
        .expect("newer generation is active");

    aggregator.begin_generation(older);

    assert_eq!(ids(aggregator.items()), ["current"]);
    assert_eq!(
        aggregator.accept(partial(older, &apps, vec![item(&apps, "stale", "Stale")])),
        Err(RejectReason::StaleGeneration)
    );
    aggregator
        .accept(partial(
            newer,
            &apps,
            vec![item(&apps, "still-current", "Still current")],
        ))
        .expect("regressive begin left the newer generation active");
    assert_eq!(ids(aggregator.items()), ["current", "still-current"]);
}

#[test]
fn the_retirement_floor_prevents_reactivation_even_without_active_state() {
    let tracker = GenerationTracker::new();
    let retired = tracker.advance();
    let floor = tracker.advance();
    let mut aggregator = MemoryResultAggregator::new(limits(8, 8, 8));
    let apps = plugin("dev.crikey.apps");

    aggregator.retire_before(floor);
    aggregator.retire_before(retired);
    aggregator.begin_generation(retired);

    assert_eq!(
        aggregator.accept(partial(retired, &apps, vec![item(&apps, "stale", "Stale")])),
        Err(RejectReason::StaleGeneration),
        "a regressive retirement call must not lower the recorded floor"
    );

    aggregator.begin_generation(floor);
    aggregator
        .accept(partial(floor, &apps, vec![item(&apps, "current", "Current")]))
        .expect("the exclusive floor itself remains eligible");
    assert_eq!(ids(aggregator.items()), ["current"]);
}

// ---------------------------------------------------------------------------
// Safety limits (spec 11.7). Every breach rejects the batch atomically.
// ---------------------------------------------------------------------------

#[test]
fn oversized_batch_is_rejected_atomically() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(2, 8, 8));
    let apps = plugin("dev.crikey.apps");

    let rejected = aggregator.accept(partial(
        generation,
        &apps,
        vec![
            item(&apps, "alpha", "Alpha"),
            item(&apps, "beta", "Beta"),
            item(&apps, "gamma", "Gamma"),
        ],
    ));

    assert_eq!(rejected, Err(RejectReason::QuotaExceeded));
    assert!(
        aggregator.items().is_empty(),
        "an over-large batch is discarded whole, never truncated to the limit"
    );
    assert!(aggregator.take_ui_update().is_none());

    aggregator
        .accept(partial(
            generation,
            &apps,
            vec![item(&apps, "delta", "Delta"), item(&apps, "epsilon", "Epsilon")],
        ))
        .expect("a batch exactly at the per-batch limit is accepted");
    assert_eq!(ids(aggregator.items()), ["delta", "epsilon"]);
}

#[test]
fn per_plugin_per_query_quota_rejects_the_breaching_batch_whole() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(4, 3, 16));
    let apps = plugin("dev.crikey.apps");

    aggregator
        .accept(partial(
            generation,
            &apps,
            vec![item(&apps, "alpha", "Alpha"), item(&apps, "beta", "Beta")],
        ))
        .expect("two of the three allowed items");

    let rejected = aggregator.accept(partial(
        generation,
        &apps,
        vec![item(&apps, "gamma", "Gamma"), item(&apps, "delta", "Delta")],
    ));

    assert_eq!(rejected, Err(RejectReason::QuotaExceeded));
    assert_eq!(
        ids(aggregator.items()),
        ["alpha", "beta"],
        "not one item of the breaching batch is merged"
    );

    aggregator
        .accept(partial(generation, &apps, vec![item(&apps, "gamma", "Gamma")]))
        .expect("the rejected batch consumed no quota, so one slot is still free");
    assert_eq!(ids(aggregator.items()), ["alpha", "beta", "gamma"]);
}

#[test]
fn per_plugin_quota_is_isolated_between_plugins() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(4, 2, 16));
    let apps = plugin("dev.crikey.apps");
    let files = plugin("dev.crikey.files");

    aggregator
        .accept(partial(
            generation,
            &apps,
            vec![item(&apps, "alpha", "Alpha"), item(&apps, "beta", "Beta")],
        ))
        .expect("the first plugin fills its quota");
    assert_eq!(
        aggregator.accept(partial(generation, &apps, vec![item(&apps, "gamma", "Gamma")])),
        Err(RejectReason::QuotaExceeded)
    );

    aggregator
        .accept(partial(
            generation,
            &files,
            vec![item(&files, "notes", "Notes"), item(&files, "todo", "Todo")],
        ))
        .expect("a greedy plugin must not starve a well-behaved one");

    assert_eq!(ids(aggregator.items()), ["alpha", "beta", "notes", "todo"]);
}

#[test]
fn total_per_query_limit_rejects_the_breaching_batch_atomically() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(4, 3, 4));
    let apps = plugin("dev.crikey.apps");
    let files = plugin("dev.crikey.files");

    aggregator
        .accept(partial(
            generation,
            &apps,
            vec![
                item(&apps, "alpha", "Alpha"),
                item(&apps, "beta", "Beta"),
                item(&apps, "gamma", "Gamma"),
            ],
        ))
        .expect("three of the four items retained per query");

    let rejected = aggregator.accept(partial(
        generation,
        &files,
        vec![item(&files, "notes", "Notes"), item(&files, "todo", "Todo")],
    ));

    assert_eq!(
        rejected,
        Err(RejectReason::QuotaExceeded),
        "within its own plugin quota, but it would breach the per-query total"
    );
    assert_eq!(ids(aggregator.items()), ["alpha", "beta", "gamma"]);

    aggregator
        .accept(partial(generation, &files, vec![item(&files, "notes", "Notes")]))
        .expect("a batch that fits the remaining per-query room is accepted");
    assert_eq!(ids(aggregator.items()), ["alpha", "beta", "gamma", "notes"]);
}

#[test]
fn mismatched_item_owner_rejects_the_whole_batch_without_consuming_quota() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(4, 3, 8));
    let apps = plugin("dev.crikey.apps");
    let files = plugin("dev.crikey.files");

    aggregator
        .accept(partial(generation, &apps, vec![item(&apps, "alpha", "Alpha")]))
        .expect("baseline item");
    let _ = aggregator.take_ui_update();
    aggregator.begin_frame();

    let rejected = aggregator.accept(partial(
        generation,
        &apps,
        vec![item(&apps, "beta", "Beta"), item(&files, "foreign", "Foreign")],
    ));

    assert_eq!(rejected, Err(RejectReason::OwnerMismatch));
    assert_eq!(ids(aggregator.items()), ["alpha"]);
    assert_eq!(aggregator.plugin_state(&apps), Some(BatchState::Partial));
    assert_eq!(aggregator.plugin_state(&files), None);
    assert!(
        aggregator.take_ui_update().is_none(),
        "owner validation happens before any item or repaint state is mutated"
    );

    aggregator
        .accept(partial(
            generation,
            &apps,
            vec![item(&apps, "beta", "Beta"), item(&apps, "gamma", "Gamma")],
        ))
        .expect("the rejected identities consumed no retained-identity quota");
    assert_eq!(ids(aggregator.items()), ["alpha", "beta", "gamma"]);
}

#[test]
fn icon_reference_byte_limit_is_preflighted_atomically() {
    let tracker = GenerationTracker::new();
    let mut configured = limits(4, 3, 8);
    configured.max_icon_reference_bytes_per_batch = 8;
    let (mut aggregator, generation) = started(&tracker, configured);
    let apps = plugin("dev.crikey.apps");

    aggregator
        .accept(partial(generation, &apps, vec![item(&apps, "alpha", "Alpha")]))
        .expect("baseline item");
    let _ = aggregator.take_ui_update();
    aggregator.begin_frame();

    let mut beta = item(&apps, "beta", "Beta");
    beta.icon_reference = Some("1234".into());
    let mut gamma = item(&apps, "gamma", "Gamma");
    gamma.icon_reference = Some("56789".into());
    let rejected = aggregator.accept(batch(generation, &apps, BatchState::Final, vec![beta, gamma]));

    assert_eq!(rejected, Err(RejectReason::PayloadTooLarge));
    assert_eq!(ids(aggregator.items()), ["alpha"]);
    assert_eq!(
        aggregator.plugin_state(&apps),
        Some(BatchState::Partial),
        "a rejected terminal batch must not end the stream"
    );
    assert!(aggregator.take_ui_update().is_none());

    let mut beta = item(&apps, "beta", "Beta");
    beta.icon_reference = Some("1234".into());
    let mut gamma = item(&apps, "gamma", "Gamma");
    gamma.icon_reference = Some("5678".into());
    aggregator
        .accept(partial(generation, &apps, vec![beta, gamma]))
        .expect("a batch exactly at the byte limit fits and rejected work used no quota");
    assert_eq!(ids(aggregator.items()), ["alpha", "beta", "gamma"]);
}

#[test]
fn metadata_byte_limit_counts_keys_and_values_and_is_atomic() {
    let tracker = GenerationTracker::new();
    let mut configured = limits(4, 3, 8);
    configured.max_metadata_bytes_per_batch = 8;
    let (mut aggregator, generation) = started(&tracker, configured);
    let apps = plugin("dev.crikey.apps");

    aggregator
        .accept(partial(generation, &apps, vec![item(&apps, "alpha", "Alpha")]))
        .expect("baseline item");
    let _ = aggregator.take_ui_update();
    aggregator.begin_frame();

    let mut beta = item(&apps, "beta", "Beta");
    beta.metadata.insert("ab".into(), "cd".into());
    let mut gamma = item(&apps, "gamma", "Gamma");
    gamma.metadata.insert("ef".into(), "ghi".into());
    let rejected = aggregator.accept(batch(generation, &apps, BatchState::Final, vec![beta, gamma]));

    assert_eq!(rejected, Err(RejectReason::PayloadTooLarge));
    assert_eq!(ids(aggregator.items()), ["alpha"]);
    assert_eq!(aggregator.plugin_state(&apps), Some(BatchState::Partial));
    assert!(aggregator.take_ui_update().is_none());

    let mut beta = item(&apps, "beta", "Beta");
    beta.metadata.insert("ab".into(), "cd".into());
    let mut gamma = item(&apps, "gamma", "Gamma");
    gamma.metadata.insert("e".into(), "fgh".into());
    aggregator
        .accept(partial(generation, &apps, vec![beta, gamma]))
        .expect("combined metadata exactly at the byte limit is accepted");
    assert_eq!(ids(aggregator.items()), ["alpha", "beta", "gamma"]);
}

#[test]
fn zero_result_quotas_reject_nonempty_batches_without_mutation() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(0, 0, 0));
    let apps = plugin("dev.crikey.apps");

    assert_eq!(
        aggregator.accept(partial(
            generation,
            &apps,
            vec![item(&apps, "blocked", "Blocked")],
        )),
        Err(RejectReason::QuotaExceeded)
    );
    assert!(aggregator.items().is_empty());
    assert_eq!(aggregator.plugin_state(&apps), None);
    assert!(aggregator.take_ui_update().is_none());

    // A zero quota still permits an empty completion marker; it consumes no
    // retained-item budget and makes the stream state observable.
    aggregator
        .accept(batch(generation, &apps, BatchState::Final, Vec::new()))
        .expect("an empty completion has no result to quota");
    assert_eq!(aggregator.plugin_state(&apps), Some(BatchState::Final));
}

// ---------------------------------------------------------------------------
// Identity: dedup by `(PluginId, ItemId)` (spec 10.2, 12.6).
// ---------------------------------------------------------------------------

#[test]
fn duplicate_item_id_replaces_the_previous_value_in_place() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(8, 8, 8));
    let apps = plugin("dev.crikey.apps");

    aggregator
        .accept(partial(
            generation,
            &apps,
            vec![
                item(&apps, "alpha", "Alpha"),
                item(&apps, "beta", "Beta"),
                item(&apps, "gamma", "Gamma"),
            ],
        ))
        .expect("initial fast batch");

    let mut enriched = item(&apps, "alpha", "Alpha Editor 2.0");
    enriched.description = "enriched by a slower pass".to_owned();
    enriched.score_hint = 42;
    aggregator
        .accept(partial(generation, &apps, vec![enriched]))
        .expect("enrichment of an existing stable id");

    assert_eq!(
        ids(aggregator.items()),
        ["alpha", "beta", "gamma"],
        "an enrichment update must not move the row it updates (spec 11.5, 11.6)"
    );
    let updated = &aggregator.items()[0];
    assert_eq!(updated.label, "Alpha Editor 2.0");
    assert_eq!(updated.description, "enriched by a slower pass");
    assert_eq!(updated.score_hint, 42);
}

#[test]
fn replacing_an_existing_item_consumes_no_additional_quota() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(4, 2, 16));
    let apps = plugin("dev.crikey.apps");

    aggregator
        .accept(partial(
            generation,
            &apps,
            vec![item(&apps, "alpha", "Alpha"), item(&apps, "beta", "Beta")],
        ))
        .expect("the plugin quota of two is now full");

    let mut enriched = item(&apps, "alpha", "Alpha Editor 2.0");
    enriched.score_hint = 7;
    aggregator
        .accept(partial(generation, &apps, vec![enriched]))
        .expect("quota counts retained items, and a replacement retains no extra item");

    assert_eq!(ids(aggregator.items()), ["alpha", "beta"]);
    assert_eq!(aggregator.items()[0].score_hint, 7);
    assert_eq!(
        aggregator.accept(partial(generation, &apps, vec![item(&apps, "gamma", "Gamma")])),
        Err(RejectReason::QuotaExceeded),
        "a genuinely new item is still refused once the quota is full"
    );
}

#[test]
fn identical_labels_with_distinct_stable_ids_are_all_retained() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(8, 8, 8));
    let files = plugin("dev.crikey.files");
    let notes = plugin("dev.crikey.notes");

    aggregator
        .accept(partial(
            generation,
            &files,
            vec![
                item(&files, "files::/home/a/Notes", "Notes"),
                item(&files, "files::/home/b/Notes", "Notes"),
            ],
        ))
        .expect("two distinct files may share a display label");
    aggregator
        .accept(partial(
            generation,
            &notes,
            vec![item(&notes, "notes::inbox", "Notes")],
        ))
        .expect("a different plugin may also use that label");

    assert_eq!(
        ids(aggregator.items()),
        ["files::/home/a/Notes", "files::/home/b/Notes", "notes::inbox"],
        "identity is the stable id, never the label (spec 10.2)"
    );
    assert!(aggregator.items().iter().all(|it| it.label == "Notes"));
}

#[test]
fn equal_item_ids_from_different_plugins_remain_distinct() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(4, 1, 2));
    let apps = plugin("dev.crikey.apps");
    let files = plugin("dev.crikey.files");

    aggregator
        .accept(partial(
            generation,
            &apps,
            vec![item(&apps, "shared", "Application result")],
        ))
        .expect("first composite identity");
    aggregator
        .accept(partial(
            generation,
            &files,
            vec![item(&files, "shared", "File result")],
        ))
        .expect("the same ItemId under a different owner is a distinct identity");

    assert_eq!(ids(aggregator.items()), ["shared", "shared"]);
    assert_eq!(aggregator.items()[0].plugin_id, apps);
    assert_eq!(aggregator.items()[1].plugin_id, files);

    aggregator
        .accept(partial(
            generation,
            &apps,
            vec![item(&apps, "shared", "Enriched application result")],
        ))
        .expect("same-owner enrichment consumes no additional retained-identity quota");

    assert_eq!(aggregator.items().len(), 2);
    assert_eq!(aggregator.items()[0].label, "Enriched application result");
    assert_eq!(aggregator.items()[1].label, "File result");
}

// ---------------------------------------------------------------------------
// Streaming and UI update coalescing (spec 12.1, 12.5, 11.7).
// ---------------------------------------------------------------------------

#[test]
fn partial_batches_accumulate_and_the_final_batch_completes_them() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(8, 8, 8));
    let apps = plugin("dev.crikey.apps");

    aggregator
        .accept(partial(
            generation,
            &apps,
            vec![item(&apps, "alpha", "Alpha"), item(&apps, "beta", "Beta")],
        ))
        .expect("initial fast batch");
    let first_paint = aggregator
        .take_ui_update()
        .expect("a partial batch is shown immediately");
    assert_eq!(ids(&first_paint), ["alpha", "beta"]);

    assert_eq!(aggregator.plugin_state(&apps), Some(BatchState::Partial));
    aggregator.begin_frame();
    aggregator
        .accept(batch(
            generation,
            &apps,
            BatchState::Final,
            vec![item(&apps, "gamma", "Gamma")],
        ))
        .expect("the closing batch of the stream");
    assert_eq!(aggregator.plugin_state(&apps), Some(BatchState::Final));
    let second_paint = aggregator
        .take_ui_update()
        .expect("the completed set is repainted in the next frame");
    assert_eq!(
        ids(&second_paint),
        ["alpha", "beta", "gamma"],
        "a final batch completes the stream, it does not replace what came before"
    );
    assert_eq!(ids(aggregator.items()), ["alpha", "beta", "gamma"]);
}

#[test]
fn failed_batch_does_not_discard_already_accepted_items() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(8, 8, 8));
    let apps = plugin("dev.crikey.apps");

    aggregator
        .accept(partial(
            generation,
            &apps,
            vec![item(&apps, "alpha", "Alpha"), item(&apps, "beta", "Beta")],
        ))
        .expect("useful partial results arrived first");

    aggregator
        .accept(batch(generation, &apps, BatchState::Failed, Vec::new()))
        .expect("a completion report is not a rejection");
    assert_eq!(aggregator.plugin_state(&apps), Some(BatchState::Failed));

    assert_eq!(
        ids(aggregator.items()),
        ["alpha", "beta"],
        "a late failure must not throw away results the user can already use"
    );
}

#[test]
fn every_terminal_state_is_exposed_and_rejects_post_terminal_traffic() {
    let tracker = GenerationTracker::new();
    let apps = plugin("dev.crikey.apps");

    for terminal in [BatchState::Final, BatchState::Cancelled, BatchState::Failed] {
        let (mut aggregator, generation) = started(&tracker, limits(8, 8, 8));
        aggregator
            .accept(batch(
                generation,
                &apps,
                terminal,
                vec![item(&apps, "terminal-item", "Terminal item")],
            ))
            .expect("a terminal batch may carry its last useful result");

        assert_eq!(aggregator.plugin_state(&apps), Some(terminal));
        assert_eq!(ids(aggregator.items()), ["terminal-item"]);
        let _ = aggregator.take_ui_update();
        aggregator.begin_frame();

        assert_eq!(
            aggregator.accept(partial(
                generation,
                &apps,
                vec![item(&apps, "too-late", "Too late")],
            )),
            Err(RejectReason::StreamTerminated)
        );
        assert_eq!(ids(aggregator.items()), ["terminal-item"]);
        assert_eq!(aggregator.plugin_state(&apps), Some(terminal));
        assert!(
            aggregator.take_ui_update().is_none(),
            "post-terminal rejection cannot schedule a repaint"
        );

        let next = tracker.advance();
        aggregator.begin_generation(next);
        assert_eq!(aggregator.plugin_state(&apps), None);
        aggregator
            .accept(partial(next, &apps, vec![item(&apps, "fresh", "Fresh")]))
            .expect("terminal state is scoped to one generation");
        assert_eq!(ids(aggregator.items()), ["fresh"]);
    }
}

#[test]
fn several_accepts_coalesce_into_one_newest_ui_snapshot() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(8, 8, 16));
    let apps = plugin("dev.crikey.apps");
    let files = plugin("dev.crikey.files");

    aggregator
        .accept(partial(generation, &apps, vec![item(&apps, "alpha", "Alpha")]))
        .expect("first accept");
    aggregator
        .accept(partial(generation, &files, vec![item(&files, "notes", "Notes")]))
        .expect("second accept");
    aggregator
        .accept(batch(
            generation,
            &apps,
            BatchState::Final,
            vec![item(&apps, "beta", "Beta")],
        ))
        .expect("third accept");

    let update = aggregator
        .take_ui_update()
        .expect("three accepts still owe the UI one repaint");
    assert_eq!(
        ids(&update),
        ["alpha", "notes", "beta"],
        "the pending update is the newest whole snapshot, not a per-batch delta"
    );
    assert_eq!(ids(&update), ids(aggregator.items()));
    assert!(
        aggregator.take_ui_update().is_none(),
        "at most one update is pending, and taking it clears the flag"
    );
}

#[test]
fn a_pending_update_is_not_reissued_when_nothing_changed() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(2, 8, 8));
    let apps = plugin("dev.crikey.apps");

    aggregator
        .accept(partial(generation, &apps, vec![item(&apps, "alpha", "Alpha")]))
        .expect("first accept");
    let first_paint = aggregator.take_ui_update().expect("one repaint owed");
    assert_eq!(ids(&first_paint), ["alpha"]);
    assert!(aggregator.take_ui_update().is_none());

    // Only rejected traffic follows: the retained set is untouched, so the UI
    // has nothing to redraw.
    assert_eq!(
        aggregator.accept(partial(
            generation,
            &apps,
            vec![
                item(&apps, "beta", "Beta"),
                item(&apps, "gamma", "Gamma"),
                item(&apps, "delta", "Delta"),
            ],
        )),
        Err(RejectReason::QuotaExceeded)
    );
    let retired = Generation::ZERO;
    assert_eq!(
        aggregator.accept(partial(retired, &apps, vec![item(&apps, "epsilon", "Epsilon")])),
        Err(RejectReason::StaleGeneration)
    );

    assert_eq!(ids(aggregator.items()), ["alpha"]);
    assert!(aggregator.take_ui_update().is_none());
}

#[test]
fn ui_update_budget_withholds_and_coalesces_until_the_next_frame() {
    let tracker = GenerationTracker::new();
    let (mut aggregator, generation) = started(&tracker, limits(8, 8, 8));
    let apps = plugin("dev.crikey.apps");

    aggregator
        .accept(partial(generation, &apps, vec![item(&apps, "alpha", "Alpha")]))
        .expect("first update");
    assert_eq!(
        ids(&aggregator.take_ui_update().expect("one update is allowed")),
        ["alpha"]
    );

    aggregator
        .accept(partial(generation, &apps, vec![item(&apps, "beta", "Beta")]))
        .expect("second result still merges");
    assert!(
        aggregator.take_ui_update().is_none(),
        "the frame's one-update budget is exhausted"
    );
    aggregator
        .accept(partial(generation, &apps, vec![item(&apps, "gamma", "Gamma")]))
        .expect("more results coalesce while painting is withheld");

    aggregator.begin_frame();
    let update = aggregator
        .take_ui_update()
        .expect("a new frame replenishes the update budget");
    assert_eq!(ids(&update), ["alpha", "beta", "gamma"]);
    assert!(aggregator.take_ui_update().is_none());
}

#[test]
fn ui_update_budget_honors_values_greater_than_one() {
    let tracker = GenerationTracker::new();
    let mut configured = limits(8, 8, 8);
    configured.max_ui_updates_per_frame = 2;
    let (mut aggregator, generation) = started(&tracker, configured);
    let apps = plugin("dev.crikey.apps");

    aggregator
        .accept(partial(generation, &apps, vec![item(&apps, "alpha", "Alpha")]))
        .expect("first result");
    assert!(aggregator.take_ui_update().is_some());
    aggregator
        .accept(partial(generation, &apps, vec![item(&apps, "beta", "Beta")]))
        .expect("second result");
    assert!(aggregator.take_ui_update().is_some());
    aggregator
        .accept(partial(generation, &apps, vec![item(&apps, "gamma", "Gamma")]))
        .expect("third result");
    assert!(
        aggregator.take_ui_update().is_none(),
        "exactly two updates are available in this frame"
    );

    aggregator.begin_frame();
    assert_eq!(
        ids(&aggregator
            .take_ui_update()
            .expect("the pending third result is available next frame")),
        ["alpha", "beta", "gamma"]
    );
}
