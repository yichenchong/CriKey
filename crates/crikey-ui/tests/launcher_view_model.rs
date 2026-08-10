//! Behavioural contract for the launcher view model
//! (spec 6.1 - 6.5, 25.1, 25.5; ADR-0002 warm activation; roadmap M1
//! "Launcher window ... hidden-window warm activation").
//!
//! Written before the implementation. It pins the public API the M1 view model
//! must expose, all of it renderer-free: no window system, no GPU surface, no
//! wall clock, no threads, no sleeps.
//!
//! # Pinned API
//!
//! * `LauncherViewModel::new()` constructs the *warm* launcher: fully built,
//!   hidden, presenting nothing. Activation shows an existing object; it never
//!   builds one (ADR-0002).
//! * `activate()` / `dismiss()` / `is_visible()` toggle visibility, and both
//!   mutators are idempotent. `dismiss()` clears the query, the rows, the
//!   selection and the pending flag — but not the object: the same instance is
//!   reactivated for the next hotkey press.
//! * A hidden launcher is inert. Every `apply`, `begin_generation` and
//!   `publish` is ignored while hidden and `frame()` is always `None`; only
//!   `activate()` moves a hidden launcher.
//! * `apply(UiCommand) -> Option<UiEffect>` is the sole command entry point.
//!   `UiEffect::{Query(String), Execute { item: ItemId, action: ActionId },
//!   Dismissed}` must be `Debug + Clone + PartialEq`. A command that changes
//!   nothing returns `None`.
//! * `begin_generation(Generation)` makes a *strictly newer* generation active
//!   and marks results outstanding. "Newer" is measured against the highest
//!   generation this launcher has ever begun — a floor that survives
//!   dismissal — so a retired generation can never reactivate, in this session
//!   or in any later one (spec 6.5: no reordering across generations). Before
//!   the first call there is no active generation, so nothing can be published.
//! * `publish(Generation, Vec<ResultRow>, bool)` replaces the row set and the
//!   pending flag *only* when the generation is exactly the active one. Older,
//!   never-begun, pre-first-generation and pre-dismiss publishes are discarded
//!   whole (spec 6.2.7).
//! * `dismiss()` is a session boundary rather than a hide: it retires the
//!   accept target, so results still in flight when the launcher closed can
//!   never publish rows into the reopened launcher.
//! * `frame() -> Option<ViewModel>` yields the newest state once per accepted
//!   mutation and `None` when nothing changed. Several publishes between two
//!   `frame()` calls coalesce into one view model: the result list is never
//!   rerendered per plugin item (spec 25.5). Every accepted mutation makes a
//!   frame available; every rejected or no-op one does not.
//! * `ViewModel::generation` is the *active* generation — the one the query
//!   text belongs to. Rows may still be the previous generation's while
//!   `pending_plugins` is true; editing the query never blanks the list
//!   (spec 6.5: no flicker from late responses).
//! * Selection is anchored by `ItemId`. A republish within one generation keeps
//!   the same item selected wherever it moved to; if the anchor is gone the
//!   previous index is clamped into range. The first publish of a new
//!   generation resets the selection to the first row. Navigation clamps at
//!   both ends and never wraps.
//! * `PAGE_SIZE: usize` is the crate constant `PageDown`/`PageUp` move by.
//! * `ViewModel::rows` is an `Arc<[ResultRow]>` shared with the model, so a
//!   frame that changes only the query or the selection costs a refcount bump
//!   instead of a deep copy of the whole result list (spec 25.5).
//! * `ViewModel::actions_open` carries the action list of the selected row.
//!   `ShowActions` opens it when that row has alternate actions and does
//!   nothing when it has none; selection movement, a publish, an execution and
//!   dismissal close it. `Cancel` is a ladder: it closes the settings surface
//!   first, then an open action list, then clears a non-empty query, and only
//!   then dismisses (spec 6.3).
//! * An empty query carries no rows. A publish that lands while the query is
//!   empty is accepted with its rows dropped, and an edit back to the empty
//!   query drops the rows that were standing: an untyped launcher is the query
//!   field and nothing else.
//! * `set_settings(Vec<SettingRow>)` describes the settings surface and is the
//!   one mutator a hidden launcher accepts, because it carries the host's
//!   configuration rather than a launcher session. `open_settings(Option<&str>)`
//!   shows the surface, optionally aiming the keyboard at one key, and
//!   `close_settings()` hides it; `dismiss()` closes the surface but keeps the
//!   rows. `SetSetting` and `Quit` are the host's work and surface as
//!   `UiEffect::SetSetting` and `UiEffect::Quit`, while `OpenSettings` and
//!   `CloseSettings` are pure UI state and produce no effect.
//! * Existing `ResultRow`, `ViewModel`, `UiCommand` and `LauncherWindow` stay
//!   source-compatible: a frame is presentable through `LauncherWindow` as is.

use std::sync::Arc;

use crikey_core::{Action, ActionId, Category, ExecutionPolicy, Generation, GenerationTracker, ItemId};
use crikey_ui::{
    LauncherViewModel, LauncherWindow, ResultRow, SettingRow, UiCommand, UiEffect, ViewModel, PAGE_SIZE,
};

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

fn action(id: &str) -> Action {
    Action {
        action_id: ActionId(id.to_owned()),
        label: id.to_owned(),
        description: String::new(),
        applicable_categories: vec![Category::Application],
        icon_reference: None,
        execution_policy: ExecutionPolicy::HostMediated,
    }
}

/// A minimal row identified by `id`, carrying exactly one default action.
fn row(id: &str) -> ResultRow {
    ResultRow {
        item: ItemId(id.to_owned()),
        label: id.to_uppercase(),
        description: String::new(),
        icon_reference: None,
        icon: None,
        category: "application".to_owned(),
        plugin_name: "core".to_owned(),
        highlights: Vec::new(),
        argument_hint: None,
        status: None,
        default_action: Some(action("run")),
        alternate_actions: Vec::new(),
    }
}

fn rows(ids: &[&str]) -> Vec<ResultRow> {
    ids.iter().copied().map(row).collect()
}

/// `count` rows with ids `item-000`, `item-001`, ... in index order.
fn numbered_rows(count: usize) -> Vec<ResultRow> {
    (0..count).map(|index| row(&format!("item-{index:03}"))).collect()
}

fn row_ids(model: &ViewModel) -> Vec<&str> {
    model.rows.iter().map(|entry| entry.item.0.as_str()).collect()
}

#[track_caller]
fn selected_id(model: &ViewModel) -> &str {
    model
        .rows
        .get(model.selected)
        .map(|entry| entry.item.0.as_str())
        .expect("the selected index must address a row")
}

/// Every command variant, so "ignored while hidden" and "never breaks the
/// selection invariant" are exhaustive claims rather than sampled ones.
fn every_command() -> Vec<UiCommand> {
    vec![
        UiCommand::SetQuery("typed".to_owned()),
        UiCommand::SelectNext,
        UiCommand::SelectPrevious,
        UiCommand::PageDown,
        UiCommand::PageUp,
        UiCommand::Complete,
        UiCommand::ShowActions,
        UiCommand::ExecuteDefault,
        UiCommand::ExecuteAlternate(0),
        UiCommand::Cancel,
        UiCommand::Dismiss,
    ]
}

#[track_caller]
fn expect_frame(view_model: &mut LauncherViewModel) -> ViewModel {
    view_model
        .frame()
        .expect("an accepted state change must leave exactly one frame pending")
}

#[track_caller]
fn expect_idle(view_model: &mut LauncherViewModel) {
    let pending = view_model.frame();
    assert!(
        pending.is_none(),
        "nothing changed, so no frame may be produced; got {pending:?}"
    );
}

/// One fresh generation, minted through the real tracker because `Generation`
/// has no public constructor beyond `ZERO`.
fn fresh_generation() -> Generation {
    GenerationTracker::new().advance()
}

/// What the fixtures type before they publish rows.
///
/// Publishing into an empty query is not a state the launcher can be in: an
/// untyped launcher is the query field and nothing else, so `publish` drops
/// the rows. Every fixture that wants a result list types first, exactly as a
/// user must.
const FIXTURE_QUERY: &str = "fix";

/// A visible launcher with `ids` published for `generation`. Activation, the
/// typed query, the generation change and the publish all coalesce, so the
/// caller drains exactly one frame.
fn open_showing(generation: Generation, ids: &[&str]) -> LauncherViewModel {
    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = view_model.apply(UiCommand::SetQuery(FIXTURE_QUERY.to_owned()));
    view_model.begin_generation(generation);
    view_model.publish(generation, rows(ids), false);
    view_model
}

/// `row(id)` plus two alternate actions, so the action list has something to
/// show for it.
fn row_with_alternates(id: &str) -> ResultRow {
    let mut entry = row(id);
    entry.alternate_actions = vec![action("copy-path"), action("reveal")];
    entry
}

fn rows_with_alternates(ids: &[&str]) -> Vec<ResultRow> {
    ids.iter().copied().map(row_with_alternates).collect()
}

/// A visible launcher showing `ids` — every row carrying alternates — with the
/// action list already open over the selected row and every frame drained.
#[track_caller]
fn open_showing_actions(generation: Generation, ids: &[&str]) -> LauncherViewModel {
    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = view_model.apply(UiCommand::SetQuery(FIXTURE_QUERY.to_owned()));
    view_model.begin_generation(generation);
    view_model.publish(generation, rows_with_alternates(ids), false);
    assert!(
        !expect_frame(&mut view_model).actions_open,
        "a launcher opens with no action list"
    );

    assert_eq!(
        view_model.apply(UiCommand::ShowActions),
        None,
        "opening the action list is pure UI state and schedules no host work"
    );
    assert!(
        expect_frame(&mut view_model).actions_open,
        "ShowActions must open the action list of a row that has alternates"
    );
    view_model
}

#[track_caller]
fn select_index(view_model: &mut LauncherViewModel, index: usize) {
    for _ in 0..index {
        assert_eq!(
            view_model.apply(UiCommand::SelectNext),
            None,
            "navigation is pure UI state and never produces an effect"
        );
    }
}

// ---------------------------------------------------------------------------
// Warm activation (spec 6.1, 25.1; ADR-0002).
// ---------------------------------------------------------------------------

#[test]
fn a_new_launcher_is_already_constructed_but_hidden_and_presents_nothing() {
    let mut view_model = LauncherViewModel::new();

    assert!(!view_model.is_visible());
    expect_idle(&mut view_model);
    expect_idle(&mut view_model);
}

#[test]
fn activation_presents_one_immediate_empty_frame() {
    let mut view_model = LauncherViewModel::new();

    view_model.activate();
    assert!(view_model.is_visible());

    let model = expect_frame(&mut view_model);
    assert_eq!(model.query, "");
    assert!(model.rows.is_empty());
    assert_eq!(model.selected, 0);
    assert!(!model.pending_plugins);
    assert_eq!(model.generation, Generation::ZERO);

    expect_idle(&mut view_model);
}

#[test]
fn activating_an_open_launcher_changes_nothing() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta"]);
    select_index(&mut view_model, 1);
    let _ = expect_frame(&mut view_model);

    view_model.activate();

    assert!(view_model.is_visible());
    expect_idle(&mut view_model);

    // Force a frame: query, rows and selection all survived the redundant
    // activation untouched.
    assert_eq!(view_model.apply(UiCommand::SelectPrevious), None);
    let model = expect_frame(&mut view_model);
    assert_eq!(row_ids(&model), vec!["alpha", "beta"]);
    assert_eq!(model.selected, 0);
    assert_eq!(model.generation, generation);
}

#[test]
fn dismiss_clears_the_query_the_rows_and_the_selection() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta", "gamma"]);
    assert_eq!(
        view_model.apply(UiCommand::SetQuery("gam".to_owned())),
        Some(UiEffect::Query("gam".to_owned()))
    );
    select_index(&mut view_model, 2);
    let before = expect_frame(&mut view_model);
    assert_eq!(before.query, "gam");
    assert_eq!(before.selected, 2);

    view_model.dismiss();

    assert!(!view_model.is_visible());
    expect_idle(&mut view_model);

    view_model.activate();
    let model = expect_frame(&mut view_model);
    assert_eq!(model.query, "");
    assert!(model.rows.is_empty());
    assert_eq!(model.selected, 0);
    assert!(!model.pending_plugins);
}

#[test]
fn dismissing_a_hidden_launcher_changes_nothing() {
    let mut view_model = LauncherViewModel::new();

    view_model.dismiss();
    assert!(!view_model.is_visible());
    expect_idle(&mut view_model);

    view_model.activate();
    let _ = expect_frame(&mut view_model);

    view_model.dismiss();
    view_model.dismiss();
    assert!(!view_model.is_visible());
    expect_idle(&mut view_model);
}

#[test]
fn the_same_launcher_survives_repeated_activation_cycles() {
    let tracker = GenerationTracker::new();
    let mut view_model = LauncherViewModel::new();

    for cycle in 0..3 {
        let generation = tracker.advance();
        let id = format!("cycle-{cycle}");

        view_model.activate();
        assert!(view_model.is_visible());

        let _ = view_model.apply(UiCommand::SetQuery(FIXTURE_QUERY.to_owned()));
        view_model.begin_generation(generation);
        view_model.publish(generation, rows(&[id.as_str()]), false);

        let model = expect_frame(&mut view_model);
        assert_eq!(row_ids(&model), vec![id.as_str()]);
        assert_eq!(model.generation, generation);
        assert_eq!(model.selected, 0);

        view_model.dismiss();
        assert!(!view_model.is_visible());
    }
}

#[test]
fn a_hidden_launcher_ignores_every_command_generation_and_publish() {
    let tracker = GenerationTracker::new();
    let begun = tracker.advance();
    let later = tracker.advance();

    let mut view_model = open_showing(begun, &["alpha", "beta"]);
    let _ = expect_frame(&mut view_model);
    view_model.dismiss();

    for command in every_command() {
        assert_eq!(
            view_model.apply(command.clone()),
            None,
            "{command:?} must be ignored while hidden"
        );
        assert!(
            !view_model.is_visible(),
            "{command:?} must not reopen the launcher"
        );
        expect_idle(&mut view_model);
    }

    view_model.begin_generation(later);
    view_model.publish(later, rows(&["ghost"]), true);
    view_model.publish(begun, rows(&["ghost"]), true);
    expect_idle(&mut view_model);

    // Reactivation shows the cleared launcher: nothing offered while hidden was
    // retained.
    view_model.activate();
    let model = expect_frame(&mut view_model);
    assert!(model.rows.is_empty());
    assert_eq!(model.query, "");
    assert!(!model.pending_plugins);
}

// ---------------------------------------------------------------------------
// Query editing (spec 6.2.1, 6.5, 25.1).
// ---------------------------------------------------------------------------

#[test]
fn setting_the_query_emits_a_query_effect_and_renders_in_the_next_frame() {
    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = expect_frame(&mut view_model);

    assert_eq!(
        view_model.apply(UiCommand::SetQuery("fi".to_owned())),
        Some(UiEffect::Query("fi".to_owned()))
    );

    let model = expect_frame(&mut view_model);
    assert_eq!(model.query, "fi");
    assert!(
        model.rows.is_empty(),
        "query text renders without waiting for any result"
    );
    expect_idle(&mut view_model);
}

#[test]
fn each_distinct_edit_is_a_new_query_state() {
    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = expect_frame(&mut view_model);

    for text in ["f", "fi", "fir", "fi"] {
        assert_eq!(
            view_model.apply(UiCommand::SetQuery(text.to_owned())),
            Some(UiEffect::Query(text.to_owned()))
        );
        assert_eq!(expect_frame(&mut view_model).query, text);
    }
}

#[test]
fn retyping_the_identical_query_is_not_a_new_query_state() {
    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = expect_frame(&mut view_model);

    assert_eq!(
        view_model.apply(UiCommand::SetQuery("fi".to_owned())),
        Some(UiEffect::Query("fi".to_owned()))
    );
    let _ = expect_frame(&mut view_model);

    assert_eq!(view_model.apply(UiCommand::SetQuery("fi".to_owned())), None);
    expect_idle(&mut view_model);
}

#[test]
fn editing_the_query_keeps_the_previous_rows_and_selection_until_the_next_publish() {
    let tracker = GenerationTracker::new();
    let first = tracker.advance();
    let second = tracker.advance();

    let mut view_model = open_showing(first, &["alpha", "beta", "gamma"]);
    select_index(&mut view_model, 1);
    let _ = expect_frame(&mut view_model);

    assert_eq!(
        view_model.apply(UiCommand::SetQuery("be".to_owned())),
        Some(UiEffect::Query("be".to_owned()))
    );
    view_model.begin_generation(second);

    let model = expect_frame(&mut view_model);
    assert_eq!(model.query, "be");
    assert_eq!(
        model.generation, second,
        "the frame carries the live query's generation"
    );
    assert_eq!(
        row_ids(&model),
        vec!["alpha", "beta", "gamma"],
        "rapid typing must not flicker the list empty"
    );
    assert_eq!(selected_id(&model), "beta");
    assert!(
        model.pending_plugins,
        "results for the new generation are still outstanding"
    );
}

// ---------------------------------------------------------------------------
// Selection: clamped, never wrapping (spec 6.3, 25.5).
// ---------------------------------------------------------------------------

#[test]
fn selection_moves_one_row_at_a_time_and_clamps_at_the_last_row() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta", "gamma"]);
    assert_eq!(expect_frame(&mut view_model).selected, 0);

    for expected in [1, 2] {
        assert_eq!(view_model.apply(UiCommand::SelectNext), None);
        assert_eq!(expect_frame(&mut view_model).selected, expected);
    }

    assert_eq!(view_model.apply(UiCommand::SelectNext), None);
    expect_idle(&mut view_model);

    assert_eq!(view_model.apply(UiCommand::SelectPrevious), None);
    assert_eq!(
        expect_frame(&mut view_model).selected,
        1,
        "the last row must not wrap around to the first"
    );
}

#[test]
fn selection_clamps_at_the_first_row() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta", "gamma"]);
    let _ = expect_frame(&mut view_model);

    assert_eq!(view_model.apply(UiCommand::SelectPrevious), None);
    expect_idle(&mut view_model);

    select_index(&mut view_model, 1);
    let _ = expect_frame(&mut view_model);
    assert_eq!(view_model.apply(UiCommand::SelectPrevious), None);
    assert_eq!(expect_frame(&mut view_model).selected, 0);
}

#[test]
fn page_navigation_moves_a_whole_page_and_clamps_without_wrapping() {
    const {
        assert!(
            PAGE_SIZE >= 2,
            "a page must span more than one row for page navigation to mean anything"
        );
    }

    let generation = fresh_generation();
    let count = PAGE_SIZE * 2 + 3;
    let last = count - 1;

    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = view_model.apply(UiCommand::SetQuery(FIXTURE_QUERY.to_owned()));
    view_model.begin_generation(generation);
    view_model.publish(generation, numbered_rows(count), false);
    assert_eq!(expect_frame(&mut view_model).selected, 0);

    for expected in [PAGE_SIZE, PAGE_SIZE * 2, last] {
        assert_eq!(view_model.apply(UiCommand::PageDown), None);
        assert_eq!(expect_frame(&mut view_model).selected, expected);
    }
    assert_eq!(view_model.apply(UiCommand::PageDown), None);
    expect_idle(&mut view_model);

    for expected in [last - PAGE_SIZE, last - PAGE_SIZE * 2, 0] {
        assert_eq!(view_model.apply(UiCommand::PageUp), None);
        assert_eq!(expect_frame(&mut view_model).selected, expected);
    }
    assert_eq!(view_model.apply(UiCommand::PageUp), None);
    expect_idle(&mut view_model);
}

#[test]
fn selection_follows_the_same_item_when_a_republish_reorders_the_rows() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta", "gamma", "delta"]);
    select_index(&mut view_model, 2);
    assert_eq!(selected_id(&expect_frame(&mut view_model)), "gamma");

    view_model.publish(generation, rows(&["delta", "gamma", "alpha", "beta"]), false);

    let model = expect_frame(&mut view_model);
    assert_eq!(row_ids(&model), vec!["delta", "gamma", "alpha", "beta"]);
    assert_eq!(model.selected, 1);
    assert_eq!(selected_id(&model), "gamma");
}

#[test]
fn selection_keeps_its_index_when_the_anchor_vanishes_but_the_index_still_fits() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta", "gamma", "delta"]);
    select_index(&mut view_model, 2);
    let _ = expect_frame(&mut view_model);

    view_model.publish(generation, rows(&["alpha", "beta", "epsilon"]), false);

    let model = expect_frame(&mut view_model);
    assert_eq!(model.selected, 2);
    assert_eq!(selected_id(&model), "epsilon");
}

#[test]
fn selection_clamps_into_range_when_the_republished_list_shrinks_past_it() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta", "gamma", "delta"]);
    select_index(&mut view_model, 3);
    let _ = expect_frame(&mut view_model);

    view_model.publish(generation, rows(&["alpha", "beta"]), false);

    let model = expect_frame(&mut view_model);
    assert_eq!(model.selected, 1);
    assert_eq!(selected_id(&model), "beta");
}

#[test]
fn a_new_generation_resets_the_selection_to_the_first_row() {
    let tracker = GenerationTracker::new();
    let first = tracker.advance();
    let second = tracker.advance();

    let mut view_model = open_showing(first, &["alpha", "beta", "gamma"]);
    select_index(&mut view_model, 2);
    assert_eq!(selected_id(&expect_frame(&mut view_model)), "gamma");

    view_model.begin_generation(second);
    // The previously selected item is still present, but a new query state
    // starts at the top: the anchor lives inside one generation only.
    view_model.publish(second, rows(&["delta", "gamma", "alpha"]), false);

    let model = expect_frame(&mut view_model);
    assert_eq!(model.selected, 0);
    assert_eq!(selected_id(&model), "delta");
}

// ---------------------------------------------------------------------------
// Generation gating (spec 6.2.7, 6.5).
// ---------------------------------------------------------------------------

#[test]
fn the_zero_generation_sentinel_cannot_become_a_live_query() {
    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = expect_frame(&mut view_model);

    view_model.begin_generation(Generation::ZERO);
    expect_idle(&mut view_model);

    view_model.publish(Generation::ZERO, rows(&["ghost"]), false);
    expect_idle(&mut view_model);
}

#[test]
fn publishing_before_the_first_generation_is_ignored() {
    let generation = fresh_generation();
    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = expect_frame(&mut view_model);

    view_model.publish(Generation::ZERO, rows(&["ghost"]), true);
    view_model.publish(generation, rows(&["ghost"]), true);
    expect_idle(&mut view_model);

    let _ = view_model.apply(UiCommand::SetQuery(FIXTURE_QUERY.to_owned()));
    view_model.begin_generation(generation);
    view_model.publish(generation, rows(&["alpha"]), false);
    assert_eq!(row_ids(&expect_frame(&mut view_model)), vec!["alpha"]);
}

#[test]
fn a_publish_for_an_older_generation_is_discarded_whole() {
    let tracker = GenerationTracker::new();
    let older = tracker.advance();
    let active = tracker.advance();

    let mut view_model = open_showing(active, &["alpha", "beta", "gamma"]);
    select_index(&mut view_model, 1);
    let _ = expect_frame(&mut view_model);

    view_model.publish(older, rows(&["stale-one", "stale-two"]), true);
    expect_idle(&mut view_model);

    // Force a frame through an unrelated change: rows, selection, generation
    // and pending flag must all still be the active generation's.
    assert_eq!(view_model.apply(UiCommand::SelectNext), None);
    let model = expect_frame(&mut view_model);
    assert_eq!(row_ids(&model), vec!["alpha", "beta", "gamma"]);
    assert_eq!(model.selected, 2);
    assert_eq!(model.generation, active);
    assert!(!model.pending_plugins);
}

#[test]
fn a_publish_for_a_generation_that_was_never_begun_is_discarded_whole() {
    let tracker = GenerationTracker::new();
    let active = tracker.advance();
    let never_begun = tracker.advance();

    let mut view_model = open_showing(active, &["alpha", "beta"]);
    let _ = expect_frame(&mut view_model);

    view_model.publish(never_begun, rows(&["unexpected"]), true);
    expect_idle(&mut view_model);

    assert_eq!(view_model.apply(UiCommand::SelectNext), None);
    let model = expect_frame(&mut view_model);
    assert_eq!(row_ids(&model), vec!["alpha", "beta"]);
    assert_eq!(model.generation, active);
}

#[test]
fn only_a_strictly_newer_generation_becomes_active() {
    let tracker = GenerationTracker::new();
    let older = tracker.advance();
    let active = tracker.advance();
    let newer = tracker.advance();

    let mut view_model = open_showing(active, &["alpha"]);
    let _ = expect_frame(&mut view_model);

    view_model.begin_generation(active);
    expect_idle(&mut view_model);
    view_model.begin_generation(older);
    expect_idle(&mut view_model);

    // A retired generation cannot reactivate itself through `begin_generation`.
    view_model.publish(older, rows(&["stale"]), true);
    expect_idle(&mut view_model);

    view_model.begin_generation(newer);
    let model = expect_frame(&mut view_model);
    assert_eq!(model.generation, newer);
    assert!(model.pending_plugins);
    assert_eq!(row_ids(&model), vec!["alpha"]);

    view_model.publish(newer, rows(&["fresh"]), false);
    let model = expect_frame(&mut view_model);
    assert_eq!(row_ids(&model), vec!["fresh"]);
    assert!(!model.pending_plugins);
}

// ---------------------------------------------------------------------------
// Frame coalescing (spec 25.5).
// ---------------------------------------------------------------------------

#[test]
fn several_publishes_coalesce_into_one_frame_carrying_the_newest_state() {
    let generation = fresh_generation();
    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = view_model.apply(UiCommand::SetQuery(FIXTURE_QUERY.to_owned()));
    view_model.begin_generation(generation);

    view_model.publish(generation, rows(&["alpha"]), true);
    view_model.publish(generation, rows(&["alpha", "beta"]), true);
    view_model.publish(generation, rows(&["alpha", "beta", "gamma"]), false);

    let model = expect_frame(&mut view_model);
    assert_eq!(row_ids(&model), vec!["alpha", "beta", "gamma"]);
    assert!(!model.pending_plugins);
    assert_eq!(model.generation, generation);
    expect_idle(&mut view_model);
}

#[test]
fn a_publish_that_only_flips_the_pending_flag_still_reaches_the_next_frame() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta"]);
    view_model.publish(generation, rows(&["alpha", "beta"]), true);
    assert!(expect_frame(&mut view_model).pending_plugins);

    view_model.publish(generation, rows(&["alpha", "beta"]), false);

    let model = expect_frame(&mut view_model);
    assert!(!model.pending_plugins);
    assert_eq!(row_ids(&model), vec!["alpha", "beta"]);
    expect_idle(&mut view_model);
}

#[test]
fn a_drained_frame_is_never_reproduced() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha"]);
    let _ = expect_frame(&mut view_model);

    expect_idle(&mut view_model);
    expect_idle(&mut view_model);
    expect_idle(&mut view_model);
}

// ---------------------------------------------------------------------------
// Execution (spec 6.2.10, 6.4).
// ---------------------------------------------------------------------------

#[test]
fn execute_default_runs_the_selected_rows_default_action() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta"]);
    select_index(&mut view_model, 1);
    let _ = expect_frame(&mut view_model);

    assert_eq!(
        view_model.apply(UiCommand::ExecuteDefault),
        Some(UiEffect::Execute {
            item: ItemId("beta".to_owned()),
            action: ActionId("run".to_owned()),
        })
    );
}

#[test]
fn execute_default_is_rejected_when_the_selected_row_has_no_default_action() {
    let generation = fresh_generation();
    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = view_model.apply(UiCommand::SetQuery(FIXTURE_QUERY.to_owned()));
    view_model.begin_generation(generation);

    let mut without_default = row("alpha");
    without_default.default_action = None;
    view_model.publish(generation, vec![without_default], false);
    let _ = expect_frame(&mut view_model);

    assert_eq!(view_model.apply(UiCommand::ExecuteDefault), None);
    expect_idle(&mut view_model);
}

#[test]
fn execute_alternate_runs_the_indexed_alternate_action() {
    let generation = fresh_generation();
    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = view_model.apply(UiCommand::SetQuery(FIXTURE_QUERY.to_owned()));
    view_model.begin_generation(generation);

    let mut alpha = row("alpha");
    alpha.alternate_actions = vec![action("copy-path"), action("reveal")];
    view_model.publish(generation, vec![alpha], false);
    let _ = expect_frame(&mut view_model);

    for (index, expected) in [(0usize, "copy-path"), (1, "reveal")] {
        assert_eq!(
            view_model.apply(UiCommand::ExecuteAlternate(index)),
            Some(UiEffect::Execute {
                item: ItemId("alpha".to_owned()),
                action: ActionId(expected.to_owned()),
            })
        );
    }
}

#[test]
fn execute_alternate_out_of_range_is_rejected() {
    let generation = fresh_generation();
    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = view_model.apply(UiCommand::SetQuery(FIXTURE_QUERY.to_owned()));
    view_model.begin_generation(generation);

    let mut alpha = row("alpha");
    alpha.alternate_actions = vec![action("copy-path")];
    view_model.publish(generation, vec![alpha], false);
    let _ = expect_frame(&mut view_model);

    for index in [1usize, 2, usize::MAX] {
        assert_eq!(
            view_model.apply(UiCommand::ExecuteAlternate(index)),
            None,
            "alternate index {index} is out of range"
        );
    }

    // A row with no alternate actions at all rejects index zero.
    view_model.publish(generation, rows(&["beta"]), false);
    let _ = expect_frame(&mut view_model);
    assert_eq!(view_model.apply(UiCommand::ExecuteAlternate(0)), None);
    expect_idle(&mut view_model);
}

#[test]
fn executing_leaves_the_launcher_open_and_its_visible_state_untouched() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta"]);
    assert_eq!(
        view_model.apply(UiCommand::SetQuery("al".to_owned())),
        Some(UiEffect::Query("al".to_owned()))
    );
    let before = expect_frame(&mut view_model);
    assert_eq!(before.query, "al");
    assert_eq!(before.selected, 0);

    assert_eq!(
        view_model.apply(UiCommand::ExecuteDefault),
        Some(UiEffect::Execute {
            item: ItemId("alpha".to_owned()),
            action: ActionId("run".to_owned()),
        })
    );

    assert!(
        view_model.is_visible(),
        "dismissal after execution is the host's decision, not the view model's"
    );
    expect_idle(&mut view_model);
}

// ---------------------------------------------------------------------------
// Cancellation and dismissal (spec 6.3).
// ---------------------------------------------------------------------------

#[test]
fn cancel_clears_a_non_empty_query_without_dismissing() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta"]);
    assert_eq!(
        view_model.apply(UiCommand::SetQuery("al".to_owned())),
        Some(UiEffect::Query("al".to_owned()))
    );
    let _ = expect_frame(&mut view_model);

    assert_eq!(
        view_model.apply(UiCommand::Cancel),
        Some(UiEffect::Query(String::new()))
    );

    assert!(view_model.is_visible());
    let model = expect_frame(&mut view_model);
    assert_eq!(model.query, "");
    assert!(
        model.rows.is_empty(),
        "an empty query shows nothing but the text field, so clearing it drops the rows"
    );
}

#[test]
fn cancel_on_an_empty_query_dismisses_the_launcher() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta"]);
    // The first press only empties the query; the launcher is bare after it.
    assert_eq!(
        view_model.apply(UiCommand::Cancel),
        Some(UiEffect::Query(String::new()))
    );
    let _ = expect_frame(&mut view_model);

    assert_eq!(view_model.apply(UiCommand::Cancel), Some(UiEffect::Dismissed));

    assert!(!view_model.is_visible());
    expect_idle(&mut view_model);

    view_model.activate();
    let model = expect_frame(&mut view_model);
    assert!(model.rows.is_empty());
    assert_eq!(model.query, "");
}

#[test]
fn cancel_takes_two_presses_to_close_a_launcher_that_has_a_query() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha"]);
    assert_eq!(
        view_model.apply(UiCommand::SetQuery("a".to_owned())),
        Some(UiEffect::Query("a".to_owned()))
    );
    let _ = expect_frame(&mut view_model);

    assert_eq!(
        view_model.apply(UiCommand::Cancel),
        Some(UiEffect::Query(String::new()))
    );
    assert!(view_model.is_visible());
    let _ = expect_frame(&mut view_model);

    assert_eq!(view_model.apply(UiCommand::Cancel), Some(UiEffect::Dismissed));
    assert!(!view_model.is_visible());
}

#[test]
fn the_dismiss_command_reports_the_dismissed_effect_exactly_once() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha"]);
    assert_eq!(
        view_model.apply(UiCommand::SetQuery("a".to_owned())),
        Some(UiEffect::Query("a".to_owned()))
    );
    let _ = expect_frame(&mut view_model);

    assert_eq!(
        view_model.apply(UiCommand::Dismiss),
        Some(UiEffect::Dismissed),
        "Dismiss closes whatever the query holds, unlike Cancel"
    );
    assert!(!view_model.is_visible());

    assert_eq!(view_model.apply(UiCommand::Dismiss), None);
    expect_idle(&mut view_model);
}

// ---------------------------------------------------------------------------
// Empty result list safety.
// ---------------------------------------------------------------------------

#[test]
fn navigation_is_safe_on_an_empty_result_list() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &[]);
    let model = expect_frame(&mut view_model);
    assert!(model.rows.is_empty());
    assert_eq!(model.selected, 0);

    for command in [
        UiCommand::SelectNext,
        UiCommand::SelectPrevious,
        UiCommand::PageDown,
        UiCommand::PageUp,
    ] {
        assert_eq!(view_model.apply(command.clone()), None, "{command:?}");
        expect_idle(&mut view_model);
    }
}

#[test]
fn execution_is_rejected_on_an_empty_result_list() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &[]);
    let _ = expect_frame(&mut view_model);

    assert_eq!(view_model.apply(UiCommand::ExecuteDefault), None);
    assert_eq!(view_model.apply(UiCommand::ExecuteAlternate(0)), None);
    expect_idle(&mut view_model);
}

#[test]
fn emptying_a_populated_list_resets_the_selection() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta", "gamma"]);
    select_index(&mut view_model, 2);
    let _ = expect_frame(&mut view_model);

    view_model.publish(generation, Vec::new(), false);

    let model = expect_frame(&mut view_model);
    assert!(model.rows.is_empty());
    assert_eq!(model.selected, 0);
    assert_eq!(view_model.apply(UiCommand::ExecuteDefault), None);
}

#[test]
fn no_command_can_leave_the_selection_out_of_range() {
    let tracker = GenerationTracker::new();

    for command in every_command() {
        let generation = tracker.advance();
        let mut view_model = open_showing(generation, &["alpha", "beta", "gamma", "delta", "epsilon"]);
        select_index(&mut view_model, 3);
        assert_eq!(expect_frame(&mut view_model).selected, 3);

        let _effect = view_model.apply(command.clone());

        if let Some(model) = view_model.frame() {
            assert!(
                model.selected < model.rows.len().max(1),
                "{command:?} left selection {} outside {} rows",
                model.selected,
                model.rows.len()
            );
        }
        expect_idle(&mut view_model);
    }
}

// ---------------------------------------------------------------------------
// Display fields and the renderer contract (spec 6.4, 25.5).
// ---------------------------------------------------------------------------

#[test]
fn published_rows_reach_the_frame_with_every_display_field_intact() {
    let generation = fresh_generation();
    let decorated = ResultRow {
        item: ItemId("app:/usr/bin/firefox".to_owned()),
        label: "Firefox".to_owned(),
        description: "Web browser".to_owned(),
        icon_reference: Some("icon:firefox".to_owned()),
        icon: None,
        category: "application".to_owned(),
        plugin_name: "Applications".to_owned(),
        highlights: vec![(0usize, 2usize), (3usize, 5usize)],
        argument_hint: Some("url".to_owned()),
        status: Some("indexing".to_owned()),
        default_action: Some(action("launch")),
        alternate_actions: vec![action("open-private"), action("reveal")],
    };

    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = view_model.apply(UiCommand::SetQuery(FIXTURE_QUERY.to_owned()));
    view_model.begin_generation(generation);
    view_model.publish(generation, vec![decorated], false);

    let model = expect_frame(&mut view_model);
    let presented = model
        .rows
        .first()
        .expect("the published row must reach the frame");
    assert_eq!(presented.item, ItemId("app:/usr/bin/firefox".to_owned()));
    assert_eq!(presented.label, "Firefox");
    assert_eq!(presented.description, "Web browser");
    assert_eq!(presented.icon_reference.as_deref(), Some("icon:firefox"));
    assert_eq!(presented.category, "application");
    assert_eq!(presented.plugin_name, "Applications");
    assert_eq!(presented.highlights, vec![(0usize, 2usize), (3usize, 5usize)]);
    assert_eq!(presented.argument_hint.as_deref(), Some("url"));
    assert_eq!(presented.status.as_deref(), Some("indexing"));
    assert_eq!(
        presented
            .default_action
            .as_ref()
            .map(|entry| entry.action_id.0.as_str()),
        Some("launch")
    );
    assert_eq!(
        presented
            .alternate_actions
            .iter()
            .map(|entry| entry.action_id.0.as_str())
            .collect::<Vec<_>>(),
        vec!["open-private", "reveal"]
    );
}

/// Renderer stand-in: records exactly the calls a real window would turn into
/// draw work. It touches no display server, so the presentation contract stays
/// testable on a headless machine.
#[derive(Debug, Default)]
struct RecordingWindow {
    shown: bool,
    presented: Vec<(u64, String, Vec<String>, usize)>,
}

impl LauncherWindow for RecordingWindow {
    fn show(&mut self) {
        self.shown = true;
    }

    fn hide(&mut self) {
        self.shown = false;
    }

    fn is_visible(&self) -> bool {
        self.shown
    }

    fn present(&mut self, model: &ViewModel) {
        self.presented.push((
            model.generation.get(),
            model.query.clone(),
            model.rows.iter().map(|entry| entry.item.0.clone()).collect(),
            model.selected,
        ));
    }
}

#[test]
fn frames_present_through_the_launcher_window_contract_once_per_batch() {
    let generation = fresh_generation();
    let mut window = RecordingWindow::default();
    let mut view_model = LauncherViewModel::new();

    view_model.activate();
    window.show();
    let _ = view_model.apply(UiCommand::SetQuery(FIXTURE_QUERY.to_owned()));
    view_model.begin_generation(generation);
    view_model.publish(generation, rows(&["alpha"]), true);
    view_model.publish(generation, rows(&["alpha", "beta"]), true);
    view_model.publish(generation, rows(&["alpha", "beta", "gamma"]), false);

    while let Some(model) = view_model.frame() {
        window.present(&model);
    }

    assert!(window.is_visible());
    assert_eq!(
        window.presented,
        vec![(
            generation.get(),
            FIXTURE_QUERY.to_owned(),
            vec!["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()],
            0
        )],
        "the list is presented once per batch, never once per plugin item"
    );

    view_model.dismiss();
    window.hide();
    while let Some(model) = view_model.frame() {
        window.present(&model);
    }
    assert!(!window.is_visible());
    assert_eq!(window.presented.len(), 1);
}

// ---------------------------------------------------------------------------
// Session boundaries: dismissal retires the accept target (spec 6.2.7, 6.5).
// ---------------------------------------------------------------------------

#[test]
fn a_publish_for_the_pre_dismiss_generation_never_reaches_the_reopened_launcher() {
    let tracker = GenerationTracker::new();
    let closed_over = tracker.advance();
    let reopened = tracker.advance();

    let mut view_model = open_showing(closed_over, &["alpha", "beta"]);
    let _ = expect_frame(&mut view_model);

    // The launcher closes while plugins are still answering `closed_over`.
    view_model.dismiss();
    view_model.activate();
    let model = expect_frame(&mut view_model);
    assert!(model.rows.is_empty());
    assert_eq!(
        model.generation,
        Generation::ZERO,
        "a reopened launcher has no accept target until a generation is begun"
    );

    // Those in-flight results arrive after the reopen. They belong to a session
    // that is over, so they must not populate the new one.
    view_model.publish(closed_over, rows(&["ghost-one", "ghost-two"]), true);
    expect_idle(&mut view_model);

    // Nor can that generation be begun again to make its results acceptable:
    // the generation floor outlived the dismissal.
    view_model.begin_generation(closed_over);
    expect_idle(&mut view_model);
    view_model.publish(closed_over, rows(&["ghost-three"]), true);
    expect_idle(&mut view_model);

    // Only a strictly newer generation fills the new session's list, and the
    // retired generation stays rejected right beside it.
    let _ = view_model.apply(UiCommand::SetQuery(FIXTURE_QUERY.to_owned()));
    view_model.begin_generation(reopened);
    view_model.publish(closed_over, rows(&["ghost-four"]), true);
    view_model.publish(reopened, rows(&["fresh"]), false);

    let model = expect_frame(&mut view_model);
    assert_eq!(row_ids(&model), vec!["fresh"]);
    assert_eq!(model.generation, reopened);
    assert!(!model.pending_plugins);
    expect_idle(&mut view_model);
}

#[test]
fn dismissing_while_results_are_outstanding_leaves_the_next_session_idle() {
    let tracker = GenerationTracker::new();
    let outstanding = tracker.advance();
    let next = tracker.advance();

    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = view_model.apply(UiCommand::SetQuery(FIXTURE_QUERY.to_owned()));
    view_model.begin_generation(outstanding);
    assert!(expect_frame(&mut view_model).pending_plugins);

    view_model.dismiss();
    view_model.activate();

    let model = expect_frame(&mut view_model);
    assert!(
        !model.pending_plugins,
        "the new session waits on nobody: the old generation's plugins are not its own"
    );
    assert_eq!(model.generation, Generation::ZERO);

    // The answer the previous session was waiting for is dropped whole.
    view_model.publish(outstanding, rows(&["late"]), false);
    expect_idle(&mut view_model);

    view_model.begin_generation(next);
    let model = expect_frame(&mut view_model);
    assert_eq!(model.generation, next);
    assert!(model.pending_plugins);
    assert!(
        model.rows.is_empty(),
        "the late answer must not have leaked into the new session's list"
    );
}

// ---------------------------------------------------------------------------
// The action list (spec 6.3).
// ---------------------------------------------------------------------------

#[test]
fn show_actions_opens_the_list_only_for_a_row_that_has_alternates() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta"]);
    assert!(
        !expect_frame(&mut view_model).actions_open,
        "a launcher opens with no action list"
    );

    // `alpha` carries a default action only: there is no list to put on screen,
    // so the command changes nothing at all.
    assert_eq!(view_model.apply(UiCommand::ShowActions), None);
    expect_idle(&mut view_model);

    view_model.publish(generation, rows_with_alternates(&["alpha", "beta"]), false);
    assert!(!expect_frame(&mut view_model).actions_open);

    assert_eq!(
        view_model.apply(UiCommand::ShowActions),
        None,
        "opening the list schedules no host work"
    );
    let model = expect_frame(&mut view_model);
    assert!(model.actions_open);
    assert_eq!(selected_id(&model), "alpha");
    assert_eq!(
        model
            .rows
            .get(model.selected)
            .expect("the selected index must address a row")
            .alternate_actions
            .iter()
            .map(|entry| entry.action_id.0.as_str())
            .collect::<Vec<_>>(),
        vec!["copy-path", "reveal"],
        "the frame carries the actions the open list renders"
    );

    // Reopening an open list is not a change.
    assert_eq!(view_model.apply(UiCommand::ShowActions), None);
    expect_idle(&mut view_model);
}

#[test]
fn moving_the_selection_closes_the_action_list() {
    let generation = fresh_generation();
    let mut view_model = open_showing_actions(generation, &["alpha", "beta", "gamma"]);

    assert_eq!(view_model.apply(UiCommand::SelectNext), None);
    let model = expect_frame(&mut view_model);
    assert!(
        !model.actions_open,
        "the list belongs to the row it was opened over, not to the launcher"
    );
    assert_eq!(selected_id(&model), "beta");
}

#[test]
fn a_navigation_command_that_moves_nothing_leaves_the_action_list_open() {
    let generation = fresh_generation();
    let mut view_model = open_showing_actions(generation, &["alpha"]);

    // One row: every navigation command clamps onto the selection it already
    // has, so none of them is a selection change.
    for command in [
        UiCommand::SelectNext,
        UiCommand::SelectPrevious,
        UiCommand::PageDown,
        UiCommand::PageUp,
    ] {
        assert_eq!(view_model.apply(command.clone()), None, "{command:?}");
        expect_idle(&mut view_model);
    }

    // A query edit is not a rung of the ladder either; the publish answering it
    // is what closes the list. Forcing a frame this way proves the list is
    // still open rather than merely unreported.
    assert_eq!(
        view_model.apply(UiCommand::SetQuery("al".to_owned())),
        Some(UiEffect::Query("al".to_owned()))
    );
    let model = expect_frame(&mut view_model);
    assert!(model.actions_open);
    assert_eq!(model.query, "al");
}

#[test]
fn a_publish_closes_the_action_list() {
    let generation = fresh_generation();
    let mut view_model = open_showing_actions(generation, &["alpha", "beta"]);

    // Even a republish that keeps the same row selected: the actions on screen
    // belong to a row snapshot that has just been replaced.
    view_model.publish(generation, rows_with_alternates(&["alpha", "beta"]), false);

    let model = expect_frame(&mut view_model);
    assert!(!model.actions_open);
    assert_eq!(selected_id(&model), "alpha");
}

#[test]
fn executing_closes_the_action_list_but_a_rejected_pick_leaves_it_open() {
    let generation = fresh_generation();
    let mut view_model = open_showing_actions(generation, &["alpha"]);

    assert_eq!(
        view_model.apply(UiCommand::ExecuteAlternate(2)),
        None,
        "an out-of-range pick runs nothing, so it closes nothing"
    );
    expect_idle(&mut view_model);

    assert_eq!(
        view_model.apply(UiCommand::ExecuteAlternate(1)),
        Some(UiEffect::Execute {
            item: ItemId("alpha".to_owned()),
            action: ActionId("reveal".to_owned()),
        })
    );
    assert!(
        !expect_frame(&mut view_model).actions_open,
        "running a pick closes the list it was picked from"
    );
    assert!(
        view_model.is_visible(),
        "dismissal after execution is still the host's decision"
    );

    // The default action closes the list the same way.
    assert_eq!(view_model.apply(UiCommand::ShowActions), None);
    assert!(expect_frame(&mut view_model).actions_open);
    assert_eq!(
        view_model.apply(UiCommand::ExecuteDefault),
        Some(UiEffect::Execute {
            item: ItemId("alpha".to_owned()),
            action: ActionId("run".to_owned()),
        })
    );
    assert!(!expect_frame(&mut view_model).actions_open);
    expect_idle(&mut view_model);
}

#[test]
fn dismissal_closes_the_action_list_and_the_next_session_opens_without_it() {
    let tracker = GenerationTracker::new();
    let first = tracker.advance();
    let second = tracker.advance();
    let mut view_model = open_showing_actions(first, &["alpha"]);

    assert_eq!(
        view_model.apply(UiCommand::Dismiss),
        Some(UiEffect::Dismissed),
        "Dismiss skips the Cancel ladder, open action list or not"
    );
    assert!(!view_model.is_visible());

    view_model.activate();
    assert!(!expect_frame(&mut view_model).actions_open);

    view_model.begin_generation(second);
    view_model.publish(second, rows_with_alternates(&["alpha"]), false);
    assert!(
        !expect_frame(&mut view_model).actions_open,
        "the reopened launcher must not resurrect the closed session's overlay"
    );
}

#[test]
fn cancel_takes_three_presses_to_close_a_launcher_with_an_open_action_list() {
    let generation = fresh_generation();
    let mut view_model = open_showing_actions(generation, &["alpha"]);
    assert_eq!(
        view_model.apply(UiCommand::SetQuery("al".to_owned())),
        Some(UiEffect::Query("al".to_owned()))
    );
    let before = expect_frame(&mut view_model);
    assert!(before.actions_open);
    assert_eq!(before.query, "al");

    // Rung one: the action list closes and nothing else moves.
    assert_eq!(
        view_model.apply(UiCommand::Cancel),
        None,
        "closing the action list schedules no host work"
    );
    let model = expect_frame(&mut view_model);
    assert!(!model.actions_open);
    assert_eq!(
        model.query, "al",
        "the first Cancel must not also clear the query"
    );
    assert_eq!(row_ids(&model), vec!["alpha"]);
    assert!(view_model.is_visible());

    // Rung two: the query clears.
    assert_eq!(
        view_model.apply(UiCommand::Cancel),
        Some(UiEffect::Query(String::new()))
    );
    let model = expect_frame(&mut view_model);
    assert_eq!(model.query, "");
    assert!(
        model.rows.is_empty(),
        "the cleared query leaves the launcher bare, so nothing is left to list"
    );
    assert!(view_model.is_visible());

    // Rung three: the launcher closes.
    assert_eq!(view_model.apply(UiCommand::Cancel), Some(UiEffect::Dismissed));
    assert!(!view_model.is_visible());
}

#[test]
fn cancel_closes_an_open_action_list_before_it_clears_the_query() {
    let generation = fresh_generation();
    let mut view_model = open_showing_actions(generation, &["alpha"]);

    assert_eq!(view_model.apply(UiCommand::Cancel), None);
    assert!(
        view_model.is_visible(),
        "the first Cancel spends itself on the action list, whatever else is showing"
    );
    let model = expect_frame(&mut view_model);
    assert!(!model.actions_open);
    assert_eq!(model.query, FIXTURE_QUERY);

    assert_eq!(
        view_model.apply(UiCommand::Cancel),
        Some(UiEffect::Query(String::new()))
    );
    assert!(view_model.is_visible());
    let _ = expect_frame(&mut view_model);

    assert_eq!(view_model.apply(UiCommand::Cancel), Some(UiEffect::Dismissed));
    assert!(!view_model.is_visible());
}

// ---------------------------------------------------------------------------
// Frame sharing (spec 25.5).
// ---------------------------------------------------------------------------

#[test]
fn frames_share_one_row_allocation_instead_of_deep_copying_it() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta", "gamma"]);
    let first = expect_frame(&mut view_model);

    // A keystroke: new query text, the same rows still standing.
    assert_eq!(
        view_model.apply(UiCommand::SetQuery("al".to_owned())),
        Some(UiEffect::Query("al".to_owned()))
    );
    let second = expect_frame(&mut view_model);
    assert_eq!(second.query, "al");
    assert!(
        Arc::ptr_eq(&first.rows, &second.rows),
        "a keystroke frame must share the published rows, not deep-copy them"
    );

    // Navigation is the same deal.
    assert_eq!(view_model.apply(UiCommand::SelectNext), None);
    let third = expect_frame(&mut view_model);
    assert_eq!(third.selected, 1);
    assert!(Arc::ptr_eq(&first.rows, &third.rows));

    assert_eq!(
        Arc::strong_count(&first.rows),
        4,
        "the model and the three frames handed out hold one allocation between them"
    );

    // Only a publish swaps the allocation, and a frame already handed out keeps
    // describing exactly what it was given.
    view_model.publish(generation, rows(&["delta"]), false);
    let fourth = expect_frame(&mut view_model);
    assert!(!Arc::ptr_eq(&first.rows, &fourth.rows));
    assert_eq!(row_ids(&first), vec!["alpha", "beta", "gamma"]);
    assert_eq!(row_ids(&fourth), vec!["delta"]);

    drop(second);
    drop(third);
    assert_eq!(
        Arc::strong_count(&first.rows),
        1,
        "the retired row set is released as soon as the last frame holding it goes"
    );
}

#[test]
fn an_action_failure_becomes_visible_on_the_selected_row() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta"]);
    let _ = expect_frame(&mut view_model);
    select_index(&mut view_model, 1);
    let _ = expect_frame(&mut view_model);

    assert!(
        view_model.set_selected_status("launch failed: access denied".to_owned()),
        "a selected row is there to carry the message"
    );
    let frame = expect_frame(&mut view_model);

    assert_eq!(frame.selected, 1);
    assert_eq!(frame.rows[0].status, None);
    assert_eq!(
        frame.rows[1].status.as_deref(),
        Some("launch failed: access denied")
    );
}

#[test]
fn an_action_failure_reports_itself_undelivered_when_no_row_is_selected() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha"]);
    let _ = expect_frame(&mut view_model);

    // The action outlives the results it was started from: a republish empties
    // the list before the failure comes back.
    view_model.publish(generation, Vec::new(), false);
    let frame = expect_frame(&mut view_model);
    assert!(frame.rows.is_empty());

    assert!(
        !view_model.set_selected_status("launch failed: access denied".to_owned()),
        "there is no row to carry the message, and the caller must be told"
    );
    expect_idle(&mut view_model);
}

// ---------------------------------------------------------------------------
// Boundary inputs: a one-row list and non-ASCII text.
// ---------------------------------------------------------------------------

#[test]
fn navigation_stays_put_on_a_single_result() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["only"]);
    assert_eq!(expect_frame(&mut view_model).selected, 0);

    for command in [
        UiCommand::SelectNext,
        UiCommand::PageDown,
        UiCommand::SelectPrevious,
        UiCommand::PageUp,
    ] {
        assert_eq!(view_model.apply(command), None);
        expect_idle(&mut view_model);
    }

    assert_eq!(
        view_model.apply(UiCommand::ExecuteDefault),
        Some(UiEffect::Execute {
            item: ItemId("only".to_owned()),
            action: ActionId("run".to_owned()),
        }),
        "the one row is still the selected, executable one"
    );
}

#[test]
fn a_query_of_multi_byte_characters_survives_editing_and_completion() {
    let generation = fresh_generation();
    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = view_model.apply(UiCommand::SetQuery(FIXTURE_QUERY.to_owned()));
    view_model.begin_generation(generation);

    // Two-, three- and four-byte characters, plus one that is several code
    // points: nothing here may be sliced on a byte index.
    let typed = "café 日本語 👩‍💻";
    assert_eq!(
        view_model.apply(UiCommand::SetQuery(typed.to_owned())),
        Some(UiEffect::Query(typed.to_owned()))
    );
    assert_eq!(expect_frame(&mut view_model).query, typed);

    // Dropping the last character is an ordinary edit of a shorter string, not
    // a one-byte truncation of the old one.
    let mut shortened = typed.to_owned();
    shortened.pop();
    assert_eq!(
        view_model.apply(UiCommand::SetQuery(shortened.clone())),
        Some(UiEffect::Query(shortened.clone()))
    );
    assert_eq!(expect_frame(&mut view_model).query, shortened);

    let mut completion = row("naïve");
    completion.label = "naïve étude".to_owned();
    view_model.publish(generation, vec![completion], false);
    assert_eq!(expect_frame(&mut view_model).query, shortened);

    assert_eq!(
        view_model.apply(UiCommand::Complete),
        Some(UiEffect::Query("naïve étude".to_owned()))
    );
    assert_eq!(expect_frame(&mut view_model).query, "naïve étude");

    assert_eq!(
        view_model.apply(UiCommand::Cancel),
        Some(UiEffect::Query(String::new())),
        "the first cancel clears the query rather than dismissing"
    );
    assert_eq!(expect_frame(&mut view_model).query, "");
    assert!(view_model.is_visible());
}

// ---------------------------------------------------------------------------
// The untyped launcher (owner report: an empty query listed suggestions).
// ---------------------------------------------------------------------------

#[test]
fn a_publish_for_an_empty_query_carries_no_rows() {
    let generation = fresh_generation();
    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    view_model.begin_generation(generation);

    // Whatever the host ranked for an untyped launcher, the model refuses to
    // carry it: the window is the query field and nothing else until the user
    // types.
    view_model.publish(generation, rows(&["alpha", "beta"]), false);

    let model = expect_frame(&mut view_model);
    assert_eq!(model.query, "");
    assert!(model.rows.is_empty());
    assert_eq!(model.selected, 0);
}

#[test]
fn emptying_the_query_drops_the_rows_it_was_showing() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta"]);
    assert_eq!(row_ids(&expect_frame(&mut view_model)), vec!["alpha", "beta"]);

    assert_eq!(
        view_model.apply(UiCommand::SetQuery(String::new())),
        Some(UiEffect::Query(String::new()))
    );

    let model = expect_frame(&mut view_model);
    assert!(model.rows.is_empty());
    assert_eq!(
        view_model.apply(UiCommand::ExecuteDefault),
        None,
        "a row the user can no longer see must not still be the one Enter runs"
    );
}

// ---------------------------------------------------------------------------
// Settings surface (owner report: nowhere to configure anything, nowhere to
// quit).
// ---------------------------------------------------------------------------

fn setting(key: &str, value: &str) -> SettingRow {
    SettingRow {
        key: key.to_owned(),
        label: key.to_owned(),
        value: value.to_owned(),
        source: "default".to_owned(),
    }
}

#[test]
fn the_settings_the_host_published_reach_the_frame() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha"]);
    let model = expect_frame(&mut view_model);
    assert!(!model.settings_open);
    assert!(model.settings.is_empty());

    view_model.set_settings(vec![setting("launcher.activation-hotkey", "Ctrl+Alt+Space")]);

    let model = expect_frame(&mut view_model);
    assert_eq!(model.settings.len(), 1);
    assert_eq!(model.settings[0].key, "launcher.activation-hotkey");
    assert_eq!(model.settings[0].value, "Ctrl+Alt+Space");
    assert!(
        !model.settings_open,
        "publishing the settings must not open the surface over the user's search"
    );

    // Republishing the same rows is not a change and produces no frame.
    view_model.set_settings(vec![setting("launcher.activation-hotkey", "Ctrl+Alt+Space")]);
    expect_idle(&mut view_model);
}

#[test]
fn settings_published_before_the_first_activation_are_already_there_when_it_opens() {
    let mut view_model = LauncherViewModel::new();
    // The host reads its configuration long before the first hotkey press.
    view_model.set_settings(vec![setting("launcher.activation-hotkey", "Ctrl+Alt+Space")]);
    expect_idle(&mut view_model);

    view_model.activate();

    let model = expect_frame(&mut view_model);
    assert_eq!(model.settings.len(), 1);
}

#[test]
fn the_host_can_open_the_settings_surface_on_the_row_it_needs_answered() {
    let mut view_model = LauncherViewModel::new();
    view_model.set_settings(vec![setting("launcher.activation-hotkey", "Ctrl+Alt+Space")]);
    view_model.activate();
    let _ = expect_frame(&mut view_model);

    view_model.open_settings(Some("launcher.activation-hotkey"));

    let model = expect_frame(&mut view_model);
    assert!(model.settings_open);
    assert_eq!(
        model.settings_focus.as_deref(),
        Some("launcher.activation-hotkey")
    );
}

#[test]
fn escape_closes_the_settings_surface_instead_of_dismissing_the_launcher() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha"]);
    let _ = expect_frame(&mut view_model);

    assert_eq!(
        view_model.apply(UiCommand::OpenSettings),
        None,
        "opening the settings surface is pure UI state and schedules no host work"
    );
    assert!(expect_frame(&mut view_model).settings_open);

    // Escape reaches the model as Cancel, and the surface is the topmost rung
    // of the ladder.
    assert_eq!(view_model.apply(UiCommand::Cancel), None);

    let model = expect_frame(&mut view_model);
    assert!(!model.settings_open);
    assert!(
        view_model.is_visible(),
        "closing the panel must not close the launcher"
    );
    assert_eq!(
        model.query, FIXTURE_QUERY,
        "the query the surface was opened over survives it"
    );
    assert_eq!(row_ids(&model), vec!["alpha"]);
}

#[test]
fn an_edited_setting_and_the_quit_control_are_the_hosts_work() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha"]);
    let _ = expect_frame(&mut view_model);

    assert_eq!(
        view_model.apply(UiCommand::SetSetting {
            key: "launcher.activation-hotkey".to_owned(),
            value: "Ctrl+Shift+Space".to_owned(),
        }),
        Some(UiEffect::SetSetting {
            key: "launcher.activation-hotkey".to_owned(),
            value: "Ctrl+Shift+Space".to_owned(),
        }),
        "the UI neither stores nor validates a setting: the host does both"
    );

    assert_eq!(view_model.apply(UiCommand::Quit), Some(UiEffect::Quit));
}

#[test]
fn dismissal_closes_the_settings_surface_but_keeps_the_settings() {
    let mut view_model = LauncherViewModel::new();
    view_model.set_settings(vec![setting("launcher.activation-hotkey", "Ctrl+Alt+Space")]);
    view_model.activate();
    view_model.open_settings(None);
    assert!(expect_frame(&mut view_model).settings_open);

    view_model.dismiss();
    view_model.activate();

    let model = expect_frame(&mut view_model);
    assert!(!model.settings_open);
    assert_eq!(
        model.settings.len(),
        1,
        "the rows describe the host's configuration, not this session"
    );
}
