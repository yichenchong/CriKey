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
//! * Selection is anchored by owner and `ItemId`. A republish within one
//!   generation follows that item, falling back to the previous index clamped
//!   into range. A return search retains its original choice across partial
//!   publications until completion or explicit navigation/query editing.
//!   An ordinary new generation starts at the top. Navigation never wraps.
//! * `PAGE_SIZE: usize` is the crate constant `PageDown`/`PageUp` move by.
//! * `ViewModel::rows` is an `Arc<[ResultRow]>` shared with the model, so a
//!   frame that changes only the query or the selection costs a refcount bump
//!   instead of a deep copy of the whole result list (spec 25.5).
//! * `ViewModel::action_panel` shares immutable bounded action state. Discovery
//!   includes host settings for default-only rows, owns its query/navigation,
//!   and preserves exact owner/item/action focus through admitted refreshes.
//!   Missing targets invalidate rather than redirect. Escape clears action
//!   search, then leaves a submenu, then closes the panel before root cancellation.
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
//! * Execution effects carry exact owner/item/action identity; all renderers
//!   still present ordinary frames through `LauncherWindow`.

use std::sync::Arc;

use crikey_core::{
    Action, ActionId, Category, ExecutionPolicy, Generation, GenerationTracker, ItemId, PageFrame, PluginId,
};
use crikey_ui::{
    LauncherViewModel, LauncherWindow, PageSurface, ResultRow, SettingControl, SettingRow, UiCommand,
    UiEffect, ViewModel, WebImeEvent, WebSurfaceFrame, WebSurfaceInput, PAGE_SIZE,
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
        plugin: PluginId("core".into()),
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

fn action_intent(item: &str, action: &str) -> UiCommand {
    UiCommand::ActivateAction {
        target: crikey_ui::ActionTarget {
            owner: PluginId("core".into()),
            item: ItemId(item.into()),
        },
        identity: crikey_ui::ActionIdentity::Plugin(ActionId(action.into())),
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
        UiCommand::FocusPreview,
        UiCommand::LeavePreview,
        UiCommand::ShowPreviewDetail,
        UiCommand::SetPreviewRatio(0.6),
        UiCommand::SelectNext,
        UiCommand::SelectPrevious,
        UiCommand::PageDown,
        UiCommand::PageUp,
        UiCommand::Complete,
        UiCommand::ShowActions,
        UiCommand::ExecuteDefault,
        action_intent("alpha", "copy-path"),
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
        expect_frame(&mut view_model).action_panel.is_none(),
        "a launcher opens with no action list"
    );

    assert_eq!(
        view_model.apply(UiCommand::ShowActions),
        None,
        "opening the action list is pure UI state and schedules no host work"
    );
    assert!(
        expect_frame(&mut view_model).action_panel.is_some(),
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
    assert_eq!(expect_frame(&mut view_model).selected, 2);

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
    assert_eq!(expect_frame(&mut view_model).selected, 0);

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
    assert_eq!(expect_frame(&mut view_model).selected, last);

    for expected in [last - PAGE_SIZE, last - PAGE_SIZE * 2, 0] {
        assert_eq!(view_model.apply(UiCommand::PageUp), None);
        assert_eq!(expect_frame(&mut view_model).selected, expected);
    }
    assert_eq!(view_model.apply(UiCommand::PageUp), None);
    assert_eq!(expect_frame(&mut view_model).selected, 0);
}

#[test]
fn root_follow_revision_tracks_intent_and_identity_not_cosmetic_publication() {
    let generations = GenerationTracker::new();
    let generation = generations.advance();
    let mut state = open_showing(generation, &["alpha", "beta", "gamma"]);
    let first = expect_frame(&mut state);
    let mut changed = first.rows.to_vec();
    changed[0].label = "Updated title".into();
    changed[0].description = "Updated description".into();
    changed[0].status = Some("Updated status".into());
    state.publish(generation, changed, false);
    let cosmetic = expect_frame(&mut state);
    assert!(!std::sync::Arc::ptr_eq(&first.rows, &cosmetic.rows));
    assert_eq!(cosmetic.root_selection_revision, first.root_selection_revision);
    assert_eq!(cosmetic.rows[0].label, "Updated title");
    assert_eq!(cosmetic.rows[0].status.as_deref(), Some("Updated status"));

    let mut changed = cosmetic.rows.to_vec();
    changed[0].plugin = PluginId("another-owner".into());
    state.publish(generation, changed, false);
    let owner = expect_frame(&mut state);
    assert_eq!(owner.selected, 0);
    assert_ne!(owner.root_selection_revision, cosmetic.root_selection_revision);

    let _ = state.apply(UiCommand::SelectPrevious);
    let boundary = expect_frame(&mut state);
    assert_eq!(boundary.selected, 0);
    assert_ne!(boundary.root_selection_revision, owner.root_selection_revision);
    let _ = state.apply(UiCommand::SelectNext);
    let _ = state.apply(UiCommand::SelectPrevious);
    state.publish(generation, boundary.rows.to_vec(), false);
    let coalesced = expect_frame(&mut state);
    assert_eq!(coalesced.selected, 0);
    assert_ne!(
        coalesced.root_selection_revision,
        boundary.root_selection_revision
    );

    let _ = state.apply(UiCommand::SetQuery("another query".into()));
    let next = generations.advance();
    state.begin_generation(next);
    state.publish(next, coalesced.rows.to_vec(), false);
    let queried = expect_frame(&mut state);
    assert_eq!(queried.selected, 0);
    assert_eq!(queried.root_selection_revision, coalesced.root_selection_revision);
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

/// The reported defect: results arrive in waves, and a later wave outranking
/// the first must not drag the highlight down the list.
///
/// Typing `mor` answers from the catalog first, putting a substring match on
/// "Memory Diagnostics Tool" in row zero. The file provider's prefix match on
/// "Mortgage Analysis" is ranked in afterwards and takes the top. Anchoring on
/// row zero's item would follow "Memory" to row one, moving a selection the
/// user never made; the launcher would then open the wrong thing on Enter.
#[test]
fn an_untouched_selection_stays_at_the_top_when_a_later_answer_outranks_it() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["memory", "water"]);
    assert_eq!(selected_id(&expect_frame(&mut view_model)), "memory");

    view_model.publish(generation, rows(&["mortgage", "memory", "water"]), false);

    let model = expect_frame(&mut view_model);
    assert_eq!(model.selected, 0);
    assert_eq!(
        selected_id(&model),
        "mortgage",
        "an unchosen selection belongs to whatever now ranks first"
    );
}

/// The other half of the same rule, and the reason it is a flag rather than a
/// removal: once the user has chosen a row, a later wave must not move it.
#[test]
fn a_chosen_selection_still_holds_its_row_when_a_later_answer_arrives_above_it() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["memory", "water"]);
    select_index(&mut view_model, 1);
    assert_eq!(selected_id(&expect_frame(&mut view_model)), "water");

    view_model.publish(generation, rows(&["mortgage", "memory", "water"]), false);

    let model = expect_frame(&mut view_model);
    assert_eq!(model.selected, 2);
    assert_eq!(
        selected_id(&model),
        "water",
        "a chosen row is followed, not abandoned"
    );
}

/// The choice belongs to the answer it was made in. A new query starts over at
/// the top even for a user who had navigated in the previous one.
#[test]
fn a_new_generation_forgets_that_the_user_had_navigated() {
    let tracker = GenerationTracker::new();
    let first = tracker.advance();
    let second = tracker.advance();

    let mut view_model = open_showing(first, &["memory", "water"]);
    select_index(&mut view_model, 1);
    let _ = expect_frame(&mut view_model);

    view_model.begin_generation(second);
    view_model.publish(second, rows(&["memory", "water"]), false);
    let _ = expect_frame(&mut view_model);
    view_model.publish(second, rows(&["mortgage", "memory", "water"]), false);

    let model = expect_frame(&mut view_model);
    assert_eq!(model.selected, 0);
    assert_eq!(selected_id(&model), "mortgage");
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
            owner: PluginId("core".into()),
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
fn execute_alternate_runs_the_named_alternate_action() {
    let generation = fresh_generation();
    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = view_model.apply(UiCommand::SetQuery(FIXTURE_QUERY.to_owned()));
    view_model.begin_generation(generation);

    let mut alpha = row("alpha");
    alpha.alternate_actions = vec![action("copy-path"), action("reveal")];
    view_model.publish(generation, vec![alpha], false);
    let _ = expect_frame(&mut view_model);

    for expected in ["copy-path", "reveal"] {
        assert_eq!(
            view_model.apply(action_intent("alpha", expected)),
            Some(UiEffect::Execute {
                owner: PluginId("core".into()),
                item: ItemId("alpha".to_owned()),
                action: ActionId(expected.to_owned()),
            })
        );
    }
}

#[test]
fn unknown_action_identity_is_rejected() {
    let generation = fresh_generation();
    let mut view_model = LauncherViewModel::new();
    view_model.activate();
    let _ = view_model.apply(UiCommand::SetQuery(FIXTURE_QUERY.to_owned()));
    view_model.begin_generation(generation);

    let mut alpha = row("alpha");
    alpha.alternate_actions = vec![action("copy-path")];
    view_model.publish(generation, vec![alpha], false);
    let _ = expect_frame(&mut view_model);

    for identity in ["missing", "unavailable", ""] {
        assert_eq!(
            view_model.apply(action_intent("alpha", identity)),
            None,
            "unknown action {identity:?} must be refused"
        );
    }

    // A replacement item cannot inherit a previous item's action intent.
    view_model.publish(generation, rows(&["beta"]), false);
    let _ = expect_frame(&mut view_model);
    assert_eq!(view_model.apply(action_intent("alpha", "copy-path")), None);
    expect_idle(&mut view_model);
}

#[test]
fn executing_leaves_the_launcher_open_and_its_visible_state_untouched() {
    let generation = fresh_generation();
    let mut view_model = open_showing(generation, &["alpha", "beta"]);
    let before = expect_frame(&mut view_model);
    assert_eq!(before.query, FIXTURE_QUERY);
    assert_eq!(before.selected, 0);

    assert_eq!(
        view_model.apply(UiCommand::ExecuteDefault),
        Some(UiEffect::Execute {
            owner: PluginId("core".into()),
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
    assert_eq!(view_model.apply(action_intent("alpha", "copy-path")), None);
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
        plugin: PluginId("builtin.applications".into()),
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
    assert_eq!(presented.plugin, PluginId("builtin.applications".into()));
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
fn default_only_rows_still_offer_real_host_settings() {
    let mut model = open_showing(fresh_generation(), &["alpha"]);
    let _ = expect_frame(&mut model);
    assert_eq!(model.apply(UiCommand::ShowActions), None);
    let frame = expect_frame(&mut model);
    let panel = frame.action_panel.as_ref().unwrap();
    assert!(panel
        .visible_entries()
        .any(|entry| entry.presentation.label == "Launcher Settings"));
    let target = panel.tree().target().clone();
    assert_eq!(
        model.apply(UiCommand::ActivateAction {
            target,
            identity: crikey_ui::ActionIdentity::Submenu("launcher-settings".into()),
        }),
        None
    );
    let frame = expect_frame(&mut model);
    assert_eq!(frame.action_panel.as_ref().unwrap().path().len(), 1);
    assert_eq!(model.apply(UiCommand::ActivateSelectedAction), None);
    assert!(model.is_settings_open());
    assert_eq!(expect_frame(&mut model).query, FIXTURE_QUERY);
}

#[test]
fn action_navigation_and_search_execute_the_non_first_action_without_retyping_root() {
    let mut model = open_showing_actions(fresh_generation(), &["alpha", "beta"]);
    model.apply(UiCommand::SelectNext);
    let frame = expect_frame(&mut model);
    assert_eq!(selected_id(&frame), "alpha");
    assert_eq!(
        frame.action_panel.as_ref().unwrap().selected(),
        Some(&crikey_ui::ActionIdentity::Plugin(ActionId("copy-path".into())))
    );
    model.apply(UiCommand::SetActionQuery("reveal".into()));
    let frame = expect_frame(&mut model);
    assert_eq!(frame.query, FIXTURE_QUERY);
    assert_eq!(frame.action_panel.as_ref().unwrap().visible_entries().len(), 1);
    assert_eq!(
        model.apply(UiCommand::ActivateSelectedAction),
        Some(UiEffect::Execute {
            owner: PluginId("core".into()),
            item: ItemId("alpha".into()),
            action: ActionId("reveal".into()),
        })
    );
    assert!(expect_frame(&mut model).action_panel.is_none());
}

#[test]
fn provider_refresh_preserves_action_query_and_exact_owner_focus() {
    let generation = fresh_generation();
    let mut model = open_showing_actions(generation, &["alpha", "beta"]);
    model.apply(UiCommand::SetActionQuery("reveal".into()));
    let original = expect_frame(&mut model);
    let mut sibling = rows_with_alternates(&["alpha"]).remove(0);
    sibling.plugin = PluginId("other".into());
    let mut replacement = vec![sibling];
    replacement.extend(rows_with_alternates(&["beta", "alpha"]));
    model.publish(generation, replacement, true);
    let frame = expect_frame(&mut model);
    assert_eq!(frame.selected, 2);
    assert_eq!(frame.action_panel.as_ref().unwrap().query(), "reveal");
    assert_eq!(
        frame.action_panel.as_ref().unwrap().selected(),
        original.action_panel.as_ref().unwrap().selected()
    );
    assert_eq!(
        model.apply(UiCommand::ActivateSelectedAction),
        Some(UiEffect::Execute {
            owner: PluginId("core".into()),
            item: ItemId("alpha".into()),
            action: ActionId("reveal".into()),
        })
    );
}

#[test]
fn vanished_owner_or_action_invalidates_and_never_revives_or_redirects() {
    for remove_action in [false, true] {
        let generation = fresh_generation();
        let mut model = open_showing_actions(generation, &["alpha", "beta"]);
        model.apply(UiCommand::SetActionQuery("reveal".into()));
        let _ = expect_frame(&mut model);
        let mut replacement = rows_with_alternates(&["alpha", "beta"]);
        if remove_action {
            replacement[0].alternate_actions.pop();
        } else {
            replacement[0].plugin = PluginId("other".into());
        }
        model.publish(generation, replacement, false);
        assert!(expect_frame(&mut model)
            .action_panel
            .as_ref()
            .unwrap()
            .invalidation()
            .is_some());
        assert_eq!(model.apply(UiCommand::ActivateSelectedAction), None);
        assert_eq!(model.apply(action_intent("alpha", "reveal")), None);
        model.publish(generation, rows_with_alternates(&["alpha", "beta"]), false);
        assert_eq!(model.apply(UiCommand::ActivateSelectedAction), None);
        assert!(expect_frame(&mut model)
            .action_panel
            .as_ref()
            .unwrap()
            .invalidation()
            .is_some());
    }
}

#[test]
fn moving_to_the_same_root_row_preserves_action_search_and_activation() {
    let mut model = open_showing_actions(fresh_generation(), &["alpha"]);
    model.apply(UiCommand::SetActionQuery("reveal".into()));
    let mut frame = expect_frame(&mut model);
    for command in [
        UiCommand::SelectNext,
        UiCommand::SelectPrevious,
        UiCommand::PageDown,
        UiCommand::PageUp,
    ] {
        assert_eq!(model.apply(command), None);
        if let Some(updated) = model.frame() {
            frame = updated;
        }
        assert_eq!(selected_id(&frame), "alpha");
        assert_eq!(frame.action_panel.as_ref().unwrap().query(), "reveal");
    }
    assert_eq!(
        model.apply(UiCommand::ActivateSelectedAction),
        Some(UiEffect::Execute {
            owner: PluginId("core".into()),
            item: ItemId("alpha".into()),
            action: ActionId("reveal".into()),
        })
    );
}

#[test]
fn nested_back_clears_search_then_returns_to_panel_without_touching_root() {
    let mut model = open_showing_actions(fresh_generation(), &["alpha"]);
    let frame = model.frame();
    assert!(frame.is_none());
    let target = crikey_ui::ActionTarget {
        owner: PluginId("core".into()),
        item: ItemId("alpha".into()),
    };
    model.apply(UiCommand::ActivateAction {
        target,
        identity: crikey_ui::ActionIdentity::Submenu("launcher-settings".into()),
    });
    assert_eq!(
        expect_frame(&mut model)
            .action_panel
            .as_ref()
            .unwrap()
            .path()
            .len(),
        1
    );
    model.apply(UiCommand::SetActionQuery("settings".into()));
    model.apply(UiCommand::Cancel);
    let frame = expect_frame(&mut model);
    assert_eq!(frame.query, FIXTURE_QUERY);
    assert_eq!(frame.action_panel.as_ref().unwrap().query(), "");
    model.apply(UiCommand::Cancel);
    assert!(expect_frame(&mut model)
        .action_panel
        .as_ref()
        .unwrap()
        .path()
        .is_empty());
    model.apply(UiCommand::Cancel);
    assert!(expect_frame(&mut model).action_panel.is_none());
}

#[test]
fn executing_closes_the_action_list_but_a_rejected_pick_leaves_it_open() {
    let generation = fresh_generation();
    let mut view_model = open_showing_actions(generation, &["alpha"]);

    assert_eq!(
        view_model.apply(action_intent("alpha", "missing")),
        None,
        "an out-of-range pick runs nothing, so it closes nothing"
    );
    expect_idle(&mut view_model);

    assert_eq!(
        view_model.apply(action_intent("alpha", "reveal")),
        Some(UiEffect::Execute {
            owner: PluginId("core".into()),
            item: ItemId("alpha".to_owned()),
            action: ActionId("reveal".to_owned()),
        })
    );
    assert!(
        expect_frame(&mut view_model).action_panel.is_none(),
        "running a pick closes the list it was picked from"
    );
    assert!(
        view_model.is_visible(),
        "dismissal after execution is still the host's decision"
    );

    // The default action closes the list the same way.
    assert_eq!(view_model.apply(UiCommand::ShowActions), None);
    assert!(expect_frame(&mut view_model).action_panel.is_some());
    assert_eq!(
        view_model.apply(UiCommand::ExecuteDefault),
        Some(UiEffect::Execute {
            owner: PluginId("core".into()),
            item: ItemId("alpha".to_owned()),
            action: ActionId("run".to_owned()),
        })
    );
    assert!(expect_frame(&mut view_model).action_panel.is_none());
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
    assert!(expect_frame(&mut view_model).action_panel.is_none());

    view_model.begin_generation(second);
    view_model.publish(second, rows_with_alternates(&["alpha"]), false);
    assert!(
        expect_frame(&mut view_model).action_panel.is_none(),
        "the reopened launcher must not resurrect the closed session's overlay"
    );
}

#[test]
fn cancel_takes_three_presses_to_close_a_launcher_with_an_open_action_list() {
    let generation = fresh_generation();
    let mut view_model = open_showing_actions(generation, &["alpha"]);

    // Rung one: the action list closes and nothing else moves.
    assert_eq!(
        view_model.apply(UiCommand::Cancel),
        None,
        "closing the action list schedules no host work"
    );
    let model = expect_frame(&mut view_model);
    assert!(model.action_panel.is_none());
    assert_eq!(
        model.query, FIXTURE_QUERY,
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
    assert!(model.action_panel.is_none());
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
    let generations = GenerationTracker::new();
    let generation = generations.advance();
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
    let replacement_generation = generations.advance();
    view_model.begin_generation(replacement_generation);
    view_model.publish(replacement_generation, rows(&["delta"]), false);
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
        let model = expect_frame(&mut view_model);
        assert_eq!(model.selected, 0);
        assert_eq!(selected_id(&model), "only");
        assert!(model.action_panel.is_none());
    }

    assert_eq!(
        view_model.apply(UiCommand::ExecuteDefault),
        Some(UiEffect::Execute {
            owner: PluginId("core".into()),
            item: ItemId("only".to_owned()),
            action: ActionId("run".to_owned()),
        }),
        "the one row is still the selected, executable one"
    );
}

#[test]
fn a_query_of_multi_byte_characters_survives_editing_and_completion() {
    let generations = GenerationTracker::new();
    let generation = generations.advance();
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
    let completion_generation = generations.advance();
    view_model.begin_generation(completion_generation);
    view_model.publish(completion_generation, vec![completion], false);
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
        control: SettingControl::Text,
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

/// The host reads `launcher.show-hints` at startup, before the launcher has
/// ever been shown, and again after every write. The value therefore has to be
/// accepted while hidden and has to be carried by every frame afterwards --
/// including the frames a later mutation produces, which is where a value
/// stored but not published would go missing.
#[test]
fn the_hint_line_choice_reaches_every_frame_the_launcher_publishes() {
    let mut view_model = LauncherViewModel::new();
    assert!(
        !view_model.set_show_hints(true),
        "the launcher already shows its hints, so this changed nothing"
    );

    assert!(view_model.set_show_hints(false), "hiding them is a change");
    // Hidden: nothing to redraw, and nothing was drawn.
    expect_idle(&mut view_model);

    view_model.activate();
    let opened = expect_frame(&mut view_model);
    assert!(
        !opened.show_hints,
        "the first frame after activation must already honour the setting"
    );

    view_model.set_settings(vec![setting("launcher.show-hints", "false")]);
    let later = expect_frame(&mut view_model);
    assert!(
        !later.show_hints,
        "an unrelated mutation must not republish the hints the user hid"
    );

    assert!(view_model.set_show_hints(true), "showing them again is a change");
    let restored = expect_frame(&mut view_model);
    assert!(restored.show_hints);
    assert!(
        !view_model.set_show_hints(true),
        "and setting the value it already has produces no frame"
    );
    expect_idle(&mut view_model);
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

/// Spec 32.2 and the web wire contract both say one live surface. The view
/// model owns a single slot rather than a collection, and opening another page
/// must replace the old owner before any renderer can observe both.
#[test]
fn opening_a_page_replaces_the_only_live_surface() {
    let mut model = LauncherViewModel::new();
    model.activate();
    let _ = model.frame();

    let surface = |plugin: &str, page_id: &str| PageSurface {
        plugin: PluginId(plugin.to_owned()),
        page_id: page_id.to_owned(),
        plugin_name: plugin.to_owned(),
        frame: Arc::new(PageFrame::default()),
        stale: true,
        answered: false,
        web_surface_id: None,
        web_frame: None,
        web_generation: 0,
        web_caret_area: None,
    };

    assert!(model.open_page(surface("modern.first", "first")));
    assert_eq!(
        model.page().expect("first page is open").plugin,
        PluginId("modern.first".to_owned())
    );
    assert!(model.open_page(surface("modern.second", "second")));
    let open = model.page().expect("replacement page is open");
    assert_eq!(open.plugin, PluginId("modern.second".to_owned()));
    assert_eq!(open.page_id, "second");
}

fn web_page_surface() -> PageSurface {
    PageSurface {
        plugin: PluginId("modern.web".to_owned()),
        page_id: "browser".to_owned(),
        plugin_name: "Web Plugin".to_owned(),
        frame: Arc::new(PageFrame {
            generation: 1,
            web: Some(crikey_core::PageWebSurface {
                url: "https://example.invalid/".to_owned(),
                ..crikey_core::PageWebSurface::default()
            }),
            ..PageFrame::default()
        }),
        stale: false,
        answered: true,
        web_surface_id: None,
        web_frame: None,
        web_generation: 0,
        web_caret_area: None,
    }
}

fn engine_frame(surface_id: u64, generation: u64, bytes: &[u8]) -> Arc<WebSurfaceFrame> {
    Arc::new(WebSurfaceFrame {
        surface_id,
        generation,
        pixel_width: 2,
        pixel_height: 1,
        rgba8: Arc::from(bytes),
    })
}

#[test]
fn web_frames_are_admitted_only_for_the_live_surface_and_a_new_generation() {
    let mut model = LauncherViewModel::new();
    model.activate();
    let _ = model.frame();
    assert!(model.open_page(web_page_surface()));
    assert!(!model.declare_web_surface(0), "zero is not a surface identity");
    assert!(model.declare_web_surface(7));
    assert!(
        !model.declare_web_surface(7),
        "reattaching the same engine is a no-op"
    );

    assert!(
        !model.update_web_frame(engine_frame(8, 1, &[1; 8])),
        "a frame from a replaced or unrelated engine must be dropped"
    );
    assert!(
        !model.update_web_frame(engine_frame(7, 1, &[1; 7])),
        "an incomplete RGBA raster must be dropped before upload"
    );
    assert!(model.update_web_frame(engine_frame(7, 2, &[1; 8])));
    assert!(
        !model.update_web_frame(engine_frame(7, 2, &[2; 8])),
        "a duplicate generation cannot replace pixels already accepted"
    );
    assert!(
        !model.update_web_frame(engine_frame(7, 1, &[3; 8])),
        "an older generation cannot walk the page backwards"
    );

    let page = model.page().expect("the web page remains open");
    assert_eq!(page.web_generation, 2);
    assert_eq!(
        page.web_frame
            .as_ref()
            .expect("the accepted pixels stand")
            .rgba8
            .as_ref(),
        &[1; 8]
    );
}

#[test]
fn replacing_or_changing_a_web_surface_retires_its_pixels_and_caret() {
    let mut model = LauncherViewModel::new();
    model.activate();
    let _ = model.frame();
    assert!(model.open_page(web_page_surface()));
    assert!(model.declare_web_surface(7));
    assert!(model.update_web_frame(engine_frame(7, 2, &[1; 8])));
    assert!(model.update_web_caret_area(7, 4.0, 5.0, 2.0, 12.0));
    assert!(
        !model.update_web_caret_area(8, 1.0, 2.0, 3.0, 4.0),
        "caret reports are scoped to the live surface"
    );
    assert!(
        !model.update_web_caret_area(7, f32::NAN, 2.0, 3.0, 4.0),
        "non-finite geometry never reaches the window system"
    );

    assert!(model.clear_web_frame(7));
    let cleared = model.page().expect("clearing pixels does not close the page");
    assert!(cleared.web_frame.is_none());
    assert!(cleared.web_caret_area.is_none());
    assert_eq!(
        cleared.web_generation, 2,
        "eviction retains the generation floor and the fact that a first frame arrived"
    );
    assert!(!model.update_web_frame(engine_frame(7, 2, &[2; 8])));
    assert!(model.update_web_frame(engine_frame(7, 3, &[3; 8])));

    assert!(model.declare_web_surface(9));
    let replacement = model.page().expect("the replacement is attached");
    assert_eq!(replacement.web_surface_id, Some(9));
    assert_eq!(replacement.web_generation, 0);
    assert!(replacement.web_frame.is_none());
    assert!(replacement.web_caret_area.is_none());
    assert!(!model.update_web_frame(engine_frame(7, 4, &[4; 8])));

    assert!(model.update_page(
        Arc::new(PageFrame {
            generation: 2,
            title: "Display list now".to_owned(),
            ..PageFrame::default()
        }),
        false,
    ));
    let display_list = model.page().expect("the page changed rendering model");
    assert!(display_list.web_surface_id.is_none());
    assert_eq!(display_list.web_generation, 0);
    assert!(display_list.web_frame.is_none());
    assert!(display_list.web_caret_area.is_none());
    assert!(!model.update_web_frame(engine_frame(9, 1, &[5; 8])));
}

#[test]
fn web_input_and_ime_commands_remain_transport_independent_host_effects() {
    let mut model = LauncherViewModel::new();
    model.activate();
    let _ = model.frame();
    let input = WebSurfaceInput::Focus {
        surface_id: 7,
        focused: true,
    };
    assert_eq!(
        model.apply(UiCommand::WebInput(input.clone())),
        Some(UiEffect::WebInput(input))
    );
    let ime = WebImeEvent::Commit {
        surface_id: 7,
        text: "é".to_owned(),
    };
    assert_eq!(
        model.apply(UiCommand::WebIme(ime.clone())),
        Some(UiEffect::WebIme(ime))
    );
}

#[test]
fn a_web_caret_rejects_overflowing_endpoints_without_losing_the_valid_anchor() {
    let mut model = LauncherViewModel::new();
    model.activate();
    let _ = model.frame();
    assert!(model.open_page(web_page_surface()));
    assert!(model.declare_web_surface(7));
    assert!(model.update_web_caret_area(7, 4.0, 5.0, 2.0, 12.0));
    assert!(!model.update_web_caret_area(7, f32::MAX, 5.0, f32::MAX, 12.0));
    assert!(!model.update_web_caret_area(7, 4.0, f32::MAX, 2.0, f32::MAX));
    assert!(!model.update_web_caret_area(7, 4.0, 5.0, -1.0, 12.0));
    assert_eq!(
        model.page().expect("web page").web_caret_area,
        Some((4.0, 5.0, 2.0, 12.0))
    );
}

#[test]
fn settings_retire_root_publications_and_return_requests_one_fresh_search() {
    for close in [UiCommand::Cancel, UiCommand::CloseSettings] {
        let tracker = GenerationTracker::new();
        let old = tracker.advance();
        let mut model = open_showing(old, &["alpha"]);
        let root = model.view().expect("visible root");
        assert_eq!(root.1, crikey_ui::ViewKind::RootList);
        let _ = expect_frame(&mut model);

        model.apply(UiCommand::OpenSettings);
        let settings = model.view().expect("visible settings");
        assert_ne!(settings.0, root.0);
        assert_eq!(settings.1, crikey_ui::ViewKind::Settings);
        let _ = expect_frame(&mut model);
        model.publish(old, rows(&["late"]), true);
        expect_idle(&mut model);
        assert_eq!(model.take_query_refresh(), None);

        assert_eq!(model.apply(close), None);
        let returned = expect_frame(&mut model);
        assert_eq!(row_ids(&returned), vec!["alpha"]);
        assert!(!returned.pending_plugins);
        model.publish(old, rows(&["still-late"]), true);
        model.begin_generation(old);
        expect_idle(&mut model);
        assert_eq!(model.take_query_refresh().as_deref(), Some(FIXTURE_QUERY));
        assert_eq!(model.take_query_refresh(), None);

        let fresh = tracker.advance();
        let returned_view = model.view();
        model.begin_generation(fresh);
        model.publish(fresh, rows(&["fresh"]), false);
        assert_eq!(row_ids(&expect_frame(&mut model)), vec!["fresh"]);
        assert_eq!(
            model.view(),
            returned_view,
            "queries do not replace logical views"
        );
        model.publish(old, rows(&["late-again"]), true);
        expect_idle(&mut model);
    }
}

/// Return to a deliberately selected asynchronous result, before any fresh
/// publication. Owner identity is distinct from the unchanged display name.
fn returning_to_selected_provider() -> (LauncherViewModel, Generation, GenerationTracker) {
    let tracker = GenerationTracker::new();
    let old = tracker.advance();
    let mut model = open_showing(old, &["builtin", "shared"]);
    let mut provider = row("shared");
    provider.plugin = PluginId("provider".into());
    model.publish(old, vec![row("builtin"), provider], false);
    model.apply(UiCommand::SelectNext);
    assert_eq!(expect_frame(&mut model).selected, 1);
    model.apply(UiCommand::OpenSettings);
    model.apply(UiCommand::CloseSettings);
    assert_eq!(model.take_query_refresh().as_deref(), Some(FIXTURE_QUERY));
    let fresh = tracker.advance();
    model.begin_generation(fresh);
    let _ = expect_frame(&mut model);
    (model, fresh, tracker)
}

#[test]
fn return_selection_survives_phased_publications_and_matches_the_exact_owner() {
    let (mut model, fresh, _) = returning_to_selected_provider();
    model.publish(fresh, rows(&["builtin"]), true);
    assert_eq!(selected_id(&expect_frame(&mut model)), "builtin");

    // An empty wave must not erase the return intent either.
    model.publish(fresh, Vec::new(), true);
    assert!(expect_frame(&mut model).rows.is_empty());
    model.publish(fresh, rows(&["shared", "builtin"]), true);
    assert_eq!(expect_frame(&mut model).selected, 0);

    let mut provider = row("shared");
    provider.plugin = PluginId("provider".into());
    model.publish(fresh, vec![row("shared"), row("builtin"), provider], false);
    let restored = expect_frame(&mut model);
    assert_eq!(restored.selected, 2);
    assert_eq!(
        restored.rows[restored.selected].plugin,
        PluginId("provider".into())
    );
    assert!(!restored.pending_plugins);
}

#[test]
fn navigation_return_retains_an_untouched_owner_target_across_fresh_generation() {
    let tracker = GenerationTracker::new();
    let old = tracker.advance();
    let mut model = open_showing(old, &["shared"]);
    let mut provider = row("shared");
    provider.plugin = PluginId("provider".into());
    model.publish(old, vec![provider.clone()], false);
    let _ = expect_frame(&mut model);
    model.apply(UiCommand::OpenSettings);
    model.apply(UiCommand::CloseSettings);
    assert_eq!(model.take_query_refresh().as_deref(), Some(FIXTURE_QUERY));
    let fresh = tracker.advance();
    model.begin_generation(fresh);
    model.publish(fresh, rows(&["shared"]), true);
    assert_eq!(expect_frame(&mut model).selected, 0);
    model.publish(fresh, vec![row("shared"), provider], false);
    let restored = expect_frame(&mut model);
    assert_eq!(restored.selected, 1);
    assert_eq!(
        restored.rows[restored.selected].plugin,
        PluginId("provider".into())
    );
}

#[test]
fn explicit_navigation_cancels_return_selection_even_when_clamped_or_empty() {
    for command in [
        UiCommand::SelectNext,
        UiCommand::SelectPrevious,
        UiCommand::PageDown,
        UiCommand::PageUp,
    ] {
        for empty in [false, true] {
            let (mut model, fresh, _) = returning_to_selected_provider();
            model.publish(fresh, if empty { Vec::new() } else { rows(&["builtin"]) }, true);
            let _ = expect_frame(&mut model);
            model.apply(command.clone());
            let mut provider = row("shared");
            provider.plugin = PluginId("provider".into());
            model.publish(fresh, vec![row("builtin"), provider], false);
            assert_eq!(selected_id(&expect_frame(&mut model)), "builtin");
        }
    }
    // A real move chooses a new anchor, not just a fallback index.
    let (mut model, fresh, _) = returning_to_selected_provider();
    model.publish(fresh, rows(&["first", "second"]), true);
    let _ = expect_frame(&mut model);
    model.apply(UiCommand::SelectPrevious);
    let mut provider = row("shared");
    provider.plugin = PluginId("provider".into());
    model.publish(fresh, vec![row("second"), provider, row("first")], false);
    assert_eq!(selected_id(&expect_frame(&mut model)), "first");
}

#[test]
fn completed_return_search_retires_an_absent_restoration_target() {
    let (mut model, fresh, _) = returning_to_selected_provider();
    model.publish(fresh, rows(&["builtin"]), false);
    let _ = expect_frame(&mut model);
    let mut provider = row("shared");
    provider.plugin = PluginId("provider".into());
    model.publish(fresh, vec![row("builtin"), provider], false);
    assert_eq!(selected_id(&expect_frame(&mut model)), "builtin");
}

#[test]
fn query_completion_supersedes_the_pending_return_selection() {
    let (mut model, fresh, tracker) = returning_to_selected_provider();
    model.publish(fresh, rows(&["builtin"]), true);
    let _ = expect_frame(&mut model);
    assert_eq!(
        model.apply(UiCommand::Complete),
        Some(UiEffect::Query("BUILTIN".into()))
    );
    assert_eq!(expect_frame(&mut model).query, "BUILTIN");
    let mut provider = row("shared");
    provider.plugin = PluginId("provider".into());
    model.publish(fresh, vec![row("builtin"), provider.clone()], false);
    expect_idle(&mut model);
    let completed = tracker.advance();
    model.begin_generation(completed);
    model.publish(completed, vec![row("builtin"), provider], false);
    assert_eq!(selected_id(&expect_frame(&mut model)), "builtin");
}

#[test]
fn leaving_custom_pages_retires_old_rows_and_preserves_close_effects() {
    for close in [UiCommand::Cancel, UiCommand::ClosePage] {
        let tracker = GenerationTracker::new();
        let old = tracker.advance();
        let mut model = open_showing(old, &["alpha"]);
        let _ = expect_frame(&mut model);
        assert!(model.open_page(web_page_surface()));
        assert_eq!(model.view().unwrap().1, crikey_ui::ViewKind::CustomPage);
        let _ = expect_frame(&mut model);
        model.publish(old, rows(&["late"]), true);
        expect_idle(&mut model);
        assert_eq!(model.apply(close), Some(UiEffect::ClosePage));
        assert_eq!(row_ids(&expect_frame(&mut model)), vec!["alpha"]);
        model.publish(old, rows(&["after-close"]), true);
        expect_idle(&mut model);
        assert_eq!(model.take_query_refresh().as_deref(), Some(FIXTURE_QUERY));
        assert_eq!(model.take_query_refresh(), None);
        let fresh = tracker.advance();
        model.begin_generation(fresh);
        model.publish(fresh, rows(&["fresh"]), false);
        assert_eq!(row_ids(&expect_frame(&mut model)), vec!["fresh"]);
    }
}

#[test]
fn dismissal_discards_an_unconsumed_return_search() {
    let mut model = open_showing(fresh_generation(), &["alpha"]);
    model.open_settings(None);
    model.close_settings();
    model.dismiss();
    assert_eq!(model.view(), None);
    assert_eq!(model.take_query_refresh(), None);
    model.activate();
    assert_eq!(model.take_query_refresh(), None);
}

#[test]
fn a_return_search_reconciles_the_existing_user_selection() {
    let tracker = GenerationTracker::new();
    let old = tracker.advance();
    let mut model = open_showing(old, &["alpha", "beta"]);
    select_index(&mut model, 1);
    model.open_settings(None);
    model.close_settings();
    assert_eq!(model.take_query_refresh().as_deref(), Some(FIXTURE_QUERY));
    let fresh = tracker.advance();
    model.begin_generation(fresh);
    model.publish(fresh, rows(&["beta", "alpha"]), false);
    let returned = expect_frame(&mut model);
    assert_eq!(returned.rows[returned.selected].item, ItemId("beta".to_owned()));
}

#[test]
fn preview_answers_retire_on_selection_query_surface_and_hide() {
    use crikey_ui::PreviewStatus;
    let mut model = LauncherViewModel::new();
    model.activate();
    model.apply(UiCommand::SetQuery("preview".into()));
    model.begin_generation(Generation::from_raw(1));
    model.publish(Generation::from_raw(1), rows(&["A", "B"]), false);
    let a = model.preview_selection().unwrap();
    assert!(model.publish_preview(&a, PreviewStatus::Loading));
    model.apply(UiCommand::SelectNext);
    let b = model.preview_selection().unwrap();
    assert_ne!(a.revision, b.revision);
    assert!(!model.publish_preview(&a, PreviewStatus::Error("late A".into())));
    assert!(model.publish_preview(&b, PreviewStatus::Empty));
    model.apply(UiCommand::SelectPrevious);
    let a_again = model.preview_selection().unwrap();
    assert_ne!(a.revision, a_again.revision);
    assert!(!model.publish_preview(&a, PreviewStatus::Empty));
    model.apply(UiCommand::SetQuery("new query".into()));
    assert!(!model.publish_preview(&a_again, PreviewStatus::Empty));
    model.begin_generation(Generation::from_raw(2));
    model.publish(Generation::from_raw(2), rows(&["A"]), false);
    let query_two = model.preview_selection().unwrap();
    model.open_settings(None);
    assert!(!model.publish_preview(&query_two, PreviewStatus::Empty));
    model.close_settings();
    model.begin_generation(Generation::from_raw(3));
    model.publish(Generation::from_raw(3), rows(&["A"]), false);
    let visible = model.preview_selection().unwrap();
    model.dismiss();
    assert!(!model.publish_preview(&visible, PreviewStatus::Empty));
    model.activate();
    assert!(!model.publish_preview(&visible, PreviewStatus::Empty));
}

#[test]
fn preview_focus_and_detail_back_preserve_query_selection_and_content() {
    use crikey_ui::{prepare_preview, PreviewStatus};
    let mut model = LauncherViewModel::new();
    model.activate();
    model.apply(UiCommand::SetQuery("preview".into()));
    model.begin_generation(Generation::from_raw(1));
    model.publish(Generation::from_raw(1), rows(&["A", "B"]), false);
    model.apply(UiCommand::SelectNext);
    let selection = model.preview_selection().unwrap();
    let content = Arc::new(
        prepare_preview(Arc::new(crikey_core::PreviewContent {
            text: Some(crikey_core::PreviewText::Plain("B detail".into())),
            ..Default::default()
        }))
        .unwrap(),
    );
    assert!(model.publish_preview(&selection, PreviewStatus::Content(Arc::clone(&content))));
    model.apply(UiCommand::ShowPreviewDetail);
    let detail = expect_frame(&mut model);
    assert!(detail.preview.focused && detail.preview.detail);
    assert_eq!(selected_id(&detail), "B");
    model.apply(UiCommand::Cancel);
    let back = expect_frame(&mut model);
    assert!(!back.preview.focused && !back.preview.detail);
    assert_eq!(back.query, "preview");
    assert_eq!(selected_id(&back), "B");
    let Some(PreviewStatus::Content(retained)) = &back.preview.status else {
        panic!("Back lost detail");
    };
    assert!(Arc::ptr_eq(&content, retained));
    model.apply(UiCommand::FocusPreview);
    model.apply(UiCommand::LeavePreview);
    assert_eq!(model.preview_selection(), Some(selection));
}

#[test]
fn preview_link_intent_cannot_follow_a_changed_selection() {
    use crikey_ui::{PreviewLinkTarget, PreviewStatus};
    let mut model = LauncherViewModel::new();
    model.activate();
    model.apply(UiCommand::SetQuery("preview".into()));
    model.begin_generation(Generation::from_raw(1));
    model.publish(Generation::from_raw(1), rows(&["A", "B"]), false);
    let selection = model.preview_selection().unwrap();
    model.publish_preview(&selection, PreviewStatus::Empty);
    let target = PreviewLinkTarget {
        plugin: selection.plugin.clone(),
        selection,
        incarnation: 3,
        request: 5,
    };
    model.set_preview_link_target(target.clone());
    model.apply(UiCommand::SelectNext);
    assert_eq!(
        model.apply(UiCommand::OpenPreviewLink {
            target,
            url: "https://example.org/".into()
        }),
        None
    );
}

#[test]
fn preview_selection_and_republication_keep_exact_owner_for_colliding_item_ids() {
    use crikey_ui::PreviewStatus;
    let mut first = row("shared");
    first.plugin = PluginId("native.first".into());
    first.plugin_name = "Same display name".into();
    let mut second = row("shared");
    second.plugin = PluginId("modern.second".into());
    second.plugin_name = "Same display name".into();
    let mut model = LauncherViewModel::new();
    model.activate();
    model.apply(UiCommand::SetQuery("shared".into()));
    model.begin_generation(Generation::from_raw(1));
    model.publish(
        Generation::from_raw(1),
        vec![first.clone(), second.clone()],
        false,
    );
    let first_request = model.preview_selection().unwrap();
    assert_eq!(first_request.plugin, first.plugin);
    model.apply(UiCommand::SelectNext);
    let second_request = model.preview_selection().unwrap();
    assert_eq!(second_request.plugin, second.plugin);
    assert!(!model.publish_preview(&first_request, PreviewStatus::Empty));
    model.publish(Generation::from_raw(1), vec![second.clone(), first], false);
    assert_eq!(model.preview_selection(), Some(second_request.clone()));
    let frame = model.frame().unwrap();
    assert_eq!(frame.selected, 0);
    assert_eq!(frame.rows[frame.selected].plugin, second.plugin);
    model.publish(Generation::from_raw(1), vec![row("shared")], false);
    assert!(!model.publish_preview(&second_request, PreviewStatus::Empty));
}

#[test]
fn stale_pointer_actions_and_retired_query_targets_are_not_reinterpreted() {
    let generation = fresh_generation();
    let mut model = open_showing(generation, &["alpha", "beta"]);
    let _ = expect_frame(&mut model);
    model.apply(UiCommand::SelectNext);
    let _ = expect_frame(&mut model);
    assert_eq!(model.apply(action_intent("alpha", "run")), None);
    model.apply(UiCommand::SetQuery("changed".into()));
    assert_eq!(model.apply(action_intent("beta", "run")), None);
    assert_eq!(model.apply(UiCommand::ExecuteDefault), None);
}

#[test]
fn action_frames_share_immutable_state_and_refresh_does_not_mutate_old_frames() {
    let generation = fresh_generation();
    let mut model = open_showing_actions(generation, &["alpha"]);
    assert!(model.set_selected_status("status".into()));
    let old = expect_frame(&mut model);
    assert!(model.set_selected_status("next status".into()));
    let same = expect_frame(&mut model);
    assert!(Arc::ptr_eq(
        old.action_panel.as_ref().unwrap(),
        same.action_panel.as_ref().unwrap()
    ));
    model.apply(UiCommand::SetActionQuery("reveal".into()));
    let changed = expect_frame(&mut model);
    assert_eq!(old.action_panel.as_ref().unwrap().query(), "");
    assert_eq!(changed.action_panel.as_ref().unwrap().query(), "reveal");
    model.publish(generation, vec![], false);
    let vanished = expect_frame(&mut model);
    assert!(vanished.action_panel.as_ref().unwrap().invalidation().is_some());
    assert!(changed.action_panel.as_ref().unwrap().invalidation().is_none());
}

#[test]
fn action_bounds_refuse_visibly_and_never_offer_a_truncated_executable_panel() {
    let generation = fresh_generation();
    let mut model = open_showing(generation, &["alpha"]);
    let mut oversized = row("alpha");
    oversized.alternate_actions = vec![action("oversized")];
    oversized.alternate_actions[0].label = "x".repeat(crikey_ui::action_panel::MAX_ACTION_TEXT_BYTES + 1);
    model.publish(generation, vec![oversized], false);
    let _ = expect_frame(&mut model);
    assert_eq!(model.apply(UiCommand::ShowActions), None);
    let frame = expect_frame(&mut model);
    assert!(frame.action_panel.is_none());
    assert!(frame.rows[0].status.as_ref().unwrap().contains("text limit"));
    assert_eq!(model.apply(UiCommand::ExecuteDefault), None);
}

#[test]
fn oversized_action_search_preserves_the_last_admitted_query_and_selection() {
    let mut model = open_showing_actions(fresh_generation(), &["alpha"]);
    model.apply(UiCommand::SetActionQuery("reveal".into()));
    let before = expect_frame(&mut model);
    model.apply(UiCommand::SetActionQuery(
        "x".repeat(crikey_ui::action_panel::MAX_ACTION_QUERY_BYTES + 1),
    ));
    let after = expect_frame(&mut model);
    assert_eq!(after.action_panel.as_ref().unwrap().query(), "reveal");
    assert_eq!(
        after.action_panel.as_ref().unwrap().selected(),
        before.action_panel.as_ref().unwrap().selected()
    );
    assert!(after.rows[0]
        .status
        .as_ref()
        .unwrap()
        .contains("Action search exceeds"));
}

#[test]
fn refresh_lost_action_focus_stays_inert_until_explicit_navigation() {
    let generation = fresh_generation();
    let mut model = open_showing_actions(generation, &["alpha"]);
    model.apply(UiCommand::SetActionQuery("copy".into()));
    assert_eq!(
        expect_frame(&mut model).action_panel.as_ref().unwrap().selected(),
        Some(&crikey_ui::ActionIdentity::Plugin(ActionId("copy-path".into())))
    );
    let mut replacement = rows_with_alternates(&["alpha"]);
    replacement[0].alternate_actions[0].label = "Duplicate".into();
    model.publish(generation, replacement.clone(), false);
    assert_eq!(
        expect_frame(&mut model).action_panel.as_ref().unwrap().selected(),
        None
    );
    replacement[0].default_action.as_mut().unwrap().label = "Copy alternate".into();
    model.publish(generation, replacement.clone(), false);
    model.publish(generation, replacement, false);
    let frame = expect_frame(&mut model);
    let panel = frame.action_panel.as_ref().unwrap();
    assert_eq!(panel.visible_entries().len(), 1);
    assert_eq!(panel.selected(), None);
    assert_eq!(model.apply(UiCommand::ActivateSelectedAction), None);
    model.apply(UiCommand::SelectNext);
    assert_eq!(
        model.apply(UiCommand::ActivateSelectedAction),
        Some(UiEffect::Execute {
            owner: PluginId("core".into()),
            item: ItemId("alpha".into()),
            action: ActionId("run".into()),
        })
    );
}

#[test]
fn action_status_admission_bounds_utf8_once_for_row_and_hover() {
    let mut model = open_showing(fresh_generation(), &["alpha"]);
    let _ = expect_frame(&mut model);
    assert!(model.set_selected_status(format!("Action failed: {}", "é".repeat(8192))));
    let frame = expect_frame(&mut model);
    let status = frame.rows[0].status.as_ref().unwrap();
    assert!(status.starts_with("Action failed: "));
    assert!(status.ends_with('…'));
    assert!(status.len() <= crikey_ui::action_panel::MAX_ACTION_TEXT_BYTES);
}

#[derive(Debug)]
struct NavigationTestClock(std::sync::atomic::AtomicU64);
impl crikey_ui::NavigationClock for NavigationTestClock {
    fn now(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.0.load(std::sync::atomic::Ordering::Relaxed))
    }
}

#[test]
fn navigation_back_restores_owner_query_selection_and_logical_scroll() {
    let mut model = LauncherViewModel::new();
    model.activate();
    model.apply(UiCommand::SetQuery("shared".into()));
    model.begin_generation(Generation::from_raw(1));
    let first = row("shared");
    let mut second = row("shared");
    second.plugin = PluginId("other.owner".into());
    model.publish(
        Generation::from_raw(1),
        vec![first.clone(), second.clone()],
        false,
    );
    model.apply(UiCommand::SelectNext);
    model.apply(UiCommand::SetLogicalScroll(crikey_ui::LogicalScroll {
        row: 1,
        offset: 7.0,
    }));
    model.open_page(web_page_surface());
    assert!(model.open_settings(None));
    model.close_settings();
    assert!(model.page().is_none(), "covered pixels are never revived");
    let route = model
        .take_page_reopen()
        .expect("Back requests a new page session");
    assert!(matches!(route, crikey_ui::NavigationRoute::CustomPage { .. }));
    model.open_page(web_page_surface());
    model.close_page();
    assert_eq!(model.take_query_refresh().as_deref(), Some("shared"));
    model.begin_generation(Generation::from_raw(2));
    model.publish(Generation::from_raw(1), vec![row("stale")], false);
    model.publish(Generation::from_raw(2), vec![second.clone(), first], false);
    let restored = model.frame().unwrap();
    assert_eq!(restored.rows[restored.selected].plugin, second.plugin);
    assert_eq!(restored.query, "shared");
    assert_eq!(
        restored.navigation.scroll,
        crikey_ui::LogicalScroll { row: 1, offset: 7.0 }
    );
}

#[test]
fn delayed_resume_uses_fake_clock_exact_boundary_and_fresh_admission() {
    for age in [99, 100] {
        let clock = Arc::new(NavigationTestClock(std::sync::atomic::AtomicU64::new(0)));
        let mut model = LauncherViewModel::new();
        model.set_navigation_clock(clock.clone());
        model.set_resume_delay(std::time::Duration::from_millis(100));
        model.activate();
        model.apply(UiCommand::SetQuery("retained".into()));
        model.begin_generation(Generation::from_raw(1));
        model.publish(Generation::from_raw(1), rows(&["A", "B"]), false);
        model.apply(UiCommand::SelectNext);
        let old_view = model.view();
        model.dismiss();
        clock.0.store(age, std::sync::atomic::Ordering::Relaxed);
        model.activate();
        assert_ne!(model.view(), old_view);
        model.publish(Generation::from_raw(1), rows(&["late"]), false);
        if age == 99 {
            assert_eq!(model.take_query_refresh().as_deref(), Some("retained"));
            model.begin_generation(Generation::from_raw(2));
            model.publish(Generation::from_raw(2), rows(&["A", "B"]), false);
            let frame = model.frame().unwrap();
            assert_eq!(selected_id(&frame), "B");
        } else {
            assert_eq!(model.take_query_refresh(), None);
            let frame = model.frame().unwrap();
            assert!(frame.query.is_empty() && frame.rows.is_empty());
        }
    }
}

#[test]
fn covered_page_retains_only_route_and_owner_loss_is_recoverable() {
    let mut model = LauncherViewModel::new();
    model.activate();
    let page = web_page_surface();
    let pixels = Arc::downgrade(&page.frame);
    model.open_page(page);
    model.open_settings(None);
    assert!(
        pixels.upgrade().is_none(),
        "navigation must not retain arbitrary custom content"
    );
    model.close_settings();
    let route = model.take_page_reopen().unwrap();
    model.page_reopen_failed(route, "owner was removed".into());
    assert_eq!(model.take_query_refresh(), None);
    let unavailable = model.frame().unwrap();
    assert!(unavailable.navigation.can_go_back);
    assert!(unavailable
        .navigation
        .unavailable
        .unwrap()
        .contains("owner was removed"));
    model.apply(UiCommand::Back);
    assert_eq!(model.view().unwrap().1, crikey_ui::ViewKind::RootList);
}

#[test]
fn navigation_refuses_depth_and_retained_content_excess_visibly() {
    let mut model = LauncherViewModel::new();
    model.activate();
    for _ in 1..crikey_ui::navigation::MAX_NAVIGATION_ENTRIES {
        assert!(model.open_page(web_page_surface()));
    }
    assert!(!model.open_page(web_page_surface()));
    assert!(model.frame().unwrap().navigation.unavailable.is_some());

    let mut large = LauncherViewModel::new();
    large.activate();
    large.apply(UiCommand::SetQuery("large".into()));
    large.begin_generation(Generation::from_raw(1));
    let mut content = row("large");
    content.description = "x".repeat(crikey_ui::navigation::MAX_NAVIGATION_BYTES);
    large.publish(Generation::from_raw(1), vec![content], false);
    assert!(!large.open_page(web_page_surface()));
    assert!(large.frame().unwrap().navigation.unavailable.is_some());
}

#[test]
fn recall_is_an_empty_root_intent_not_result_or_action_navigation() {
    let mut model = LauncherViewModel::new();
    model.activate();
    assert_eq!(model.apply(UiCommand::RecallQuery), Some(UiEffect::RecallQuery));
    model.apply(UiCommand::SetQuery("query".into()));
    assert_eq!(model.apply(UiCommand::RecallQuery), None);
    model.open_settings(None);
    assert_eq!(model.apply(UiCommand::RecallQuery), None);
}

#[test]
fn retained_navigation_counts_overallocated_icons_once_per_shared_allocation() {
    let mut pixels = Vec::with_capacity(crikey_ui::navigation::MAX_NAVIGATION_BYTES);
    pixels.extend_from_slice(&[0, 0, 0, 255]);
    let icon = Arc::new(crikey_platform::IconImage::new("overallocated", 1, 1, pixels).unwrap());
    assert!(icon.retained_bytes() > crikey_ui::navigation::MAX_NAVIGATION_BYTES);
    let mut model = LauncherViewModel::new();
    model.activate();
    model.apply(UiCommand::SetQuery("tiny image".into()));
    model.begin_generation(Generation::from_raw(1));
    let mut result = row("tiny");
    result.icon = Some(icon);
    model.publish(Generation::from_raw(1), vec![result], false);
    assert!(
        !model.open_page(web_page_surface()),
        "pixel length alone cannot bound retained allocations"
    );

    let mut pixels = Vec::with_capacity(3 * 1024 * 1024);
    pixels.extend_from_slice(&[0, 0, 0, 255]);
    let shared = Arc::new(crikey_platform::IconImage::new("shared", 1, 1, pixels).unwrap());
    let mut model = LauncherViewModel::new();
    model.activate();
    model.apply(UiCommand::SetQuery("shared".into()));
    model.begin_generation(Generation::from_raw(1));
    let mut first = row("first");
    first.icon = Some(shared.clone());
    let mut second = row("second");
    second.icon = Some(shared);
    model.publish(Generation::from_raw(1), vec![first, second], false);
    for _ in 1..crikey_ui::navigation::MAX_NAVIGATION_ENTRIES {
        assert!(
            model.open_page(web_page_surface()),
            "shared row/icon allocations are counted only once"
        );
    }
}

#[test]
fn recall_picker_selects_a_bounded_submission_without_executing_it() {
    let mut model = LauncherViewModel::new();
    model.activate();
    model.set_query_recall(vec!["newest".into(), "older".into()]);
    model.apply(UiCommand::SelectNext);
    assert_eq!(
        model.apply(UiCommand::ExecuteDefault),
        Some(UiEffect::Query("older".into()))
    );
    let frame = model.frame().unwrap();
    assert_eq!(frame.query, "older");
    assert!(frame.navigation.recall.is_empty());
    assert!(
        model.submitted_root_query().is_none(),
        "choosing recall is not a fresh submission"
    );
}

#[test]
fn reusable_restoration_keeps_logical_scope_and_excludes_argument_drafts() {
    let mut model = LauncherViewModel::new();
    model.activate();
    model.restore_navigation_state(crikey_ui::NavigationRestoration {
        query: "root".into(),
        scope: Some("documents".into()),
        ..Default::default()
    });
    model.open_settings(None);
    model.close_settings();
    assert_eq!(model.navigation_restoration().scope.as_deref(), Some("documents"));
    model.begin_generation(Generation::from_raw(1));
    let mut argument = row("argument");
    argument.argument_hint = Some("argument required".into());
    model.publish(Generation::from_raw(1), vec![argument], false);
    assert!(model.navigation_restoration().query.is_empty());
    assert!(model.submitted_root_query().is_none());
}

#[test]
fn custom_route_resume_never_reuses_a_web_identity_or_old_pixels() {
    let clock = Arc::new(NavigationTestClock(std::sync::atomic::AtomicU64::new(0)));
    let mut model = LauncherViewModel::new();
    model.set_navigation_clock(clock.clone());
    model.set_resume_delay(std::time::Duration::from_millis(100));
    model.activate();
    model.open_page(web_page_surface());
    let before = model.view();
    model.declare_web_surface(41);
    assert!(model.update_web_frame(engine_frame(41, 3, &[1; 8])));
    model.dismiss();
    clock.0.store(99, std::sync::atomic::Ordering::Relaxed);
    model.activate();
    assert_ne!(model.view(), before);
    assert!(model.page().is_none());
    assert!(matches!(
        model.take_page_reopen(),
        Some(crikey_ui::NavigationRoute::CustomPage { .. })
    ));
    model.open_page(web_page_surface());
    assert!(model.page().unwrap().web_frame.is_none());
    model.declare_web_surface(42);
    assert!(!model.update_web_frame(engine_frame(41, 4, &[2; 8])));
    assert!(model.update_web_frame(engine_frame(42, 1, &[3; 8])));
    model.apply(UiCommand::Back);
    assert_eq!(model.view().unwrap().1, crikey_ui::ViewKind::RootList);
}

#[test]
fn settings_suspends_an_unavailable_route_until_back_not_the_refresh_drain() {
    let mut model = LauncherViewModel::new();
    model.activate();
    model.open_page(web_page_surface());
    model.open_settings(None);
    model.close_settings();
    let route = model.take_page_reopen().unwrap();
    model.page_reopen_failed(route.clone(), "owner unavailable".into());
    model.open_settings(None);
    assert!(
        model.take_page_reopen().is_none(),
        "covered routes cannot open over Settings"
    );
    let settings = model.frame().unwrap();
    assert!(settings.settings_open);
    assert!(settings.navigation.unavailable.is_none());
    model.close_settings();
    assert_eq!(model.take_page_reopen(), Some(route));
}

#[test]
fn restored_scroll_survives_provider_waves_until_explicit_navigation_or_edit() {
    let mut model = LauncherViewModel::new();
    model.activate();
    model.apply(UiCommand::SetQuery("rows".into()));
    model.begin_generation(Generation::from_raw(1));
    model.publish(Generation::from_raw(1), numbered_rows(40), false);
    model.apply(UiCommand::SetLogicalScroll(crikey_ui::LogicalScroll {
        row: 20,
        offset: 3.0,
    }));
    model.open_settings(None);
    model.close_settings();
    model.take_query_refresh().unwrap();
    model.begin_generation(Generation::from_raw(2));
    model.publish(Generation::from_raw(2), numbered_rows(40), true);
    assert!(model.frame().unwrap().navigation.preserve_scroll);
    model.publish(Generation::from_raw(2), numbered_rows(41), false);
    let refreshed = model.frame().unwrap();
    assert!(refreshed.navigation.preserve_scroll);
    assert_eq!(refreshed.navigation.scroll.row, 20);
    assert_eq!(
        refreshed.selected, 0,
        "selected row intentionally remains outside the restored viewport"
    );
    model.apply(UiCommand::SelectNext);
    assert!(!model.frame().unwrap().navigation.preserve_scroll);
}
