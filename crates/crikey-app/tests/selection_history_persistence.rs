//! Durable ranking history and the foreground-context signal (spec 11.3).
//!
//! Two halves of one user-visible promise — "the launcher remembers what you
//! pick" — and one guard against the way that promise is usually broken.
//!
//! # Persistence
//!
//! [`SelectionHistoryStore`] is the only thing standing between a selection and
//! the next launch, so what it must be is *lossless* and *incapable of failing
//! a startup*. Both are tested against real files: every "reloaded" assertion
//! constructs a fresh store over the same path and compares against a snapshot
//! taken from a fresh [`SearchService`], never against the in-memory value that
//! was just saved — which would pass even if `save` wrote nothing at all.
//!
//! # Context
//!
//! [`SearchService::refresh_foreground_category`] cannot be pinned here: its
//! answer depends on whether the machine running the test has a focused window,
//! which a headless builder does not and a developer's desktop does. What is
//! pinned is the part that decides anything,
//! [`SearchService::set_foreground_from_window`], which is a pure function of
//! a window and the catalog. The case that matters most is the negative one: a
//! backend that cannot answer must leave the context term off rather than have
//! the host invent a category for it.
//!
//! Nothing here sleeps, reads a clock, opens a socket or touches a display.
//! The only I/O is a per-test directory under [`std::env::temp_dir`], removed
//! when the test ends.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crikey_app::{App, SearchService, SelectionHistoryStore, StartupStage};
use crikey_core::{ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId};
use crikey_platform::{WindowHandle, WindowInfo};

const PLUGIN: &str = "dev.crikey.history";

/// A private directory removed when the test that made it ends.
#[derive(Debug)]
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);

        let path = std::env::temp_dir().join(format!(
            "crikey-selection-history-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // A crashed earlier run must never leak a history into this one.
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");

        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn item(id: &str, label: &str, category: Category, search_terms: &[&str]) -> Item {
    Item {
        stable_id: ItemId(id.to_owned()),
        plugin_id: PluginId(PLUGIN.to_owned()),
        category,
        label: label.to_owned(),
        description: String::new(),
        target: format!("app://{id}"),
        search_terms: search_terms.iter().map(|term| (*term).to_owned()).collect(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    }
}

/// A query-ready service holding `items`, the way `crikey run` has one by the
/// time the first keystroke arrives.
fn service_with(items: Vec<Item>) -> SearchService {
    let mut service = SearchService::new(App::new());
    for stage in [
        StartupStage::WindowAndHotkey,
        StartupStage::PersistedCatalog,
        StartupStage::AcceptQueries,
    ] {
        service
            .complete_stage(stage)
            .expect("startup milestone is in order");
    }
    service
        .replace_catalog(&PluginId(PLUGIN.to_owned()), 1, items)
        .expect("the fixture catalog is accepted");
    service
}

fn fixture_items() -> Vec<Item> {
    vec![
        item("firefox", "Firefox", Category::Application, &["web", "browser"]),
        item("notes", "Notes", Category::File, &[]),
        item("shell", "Terminal", Category::Command, &["konsole"]),
    ]
}

/// Records one confirmed selection of `label` under the query `raw`, at
/// `now_secs`, the way the launcher does after a successful execution.
fn select(service: &mut SearchService, raw: &str, label: &str, now_secs: u64) {
    service.set_history_time(now_secs);
    service.submit_query(raw).expect("the fixture query is accepted");
    let chosen = service
        .results()
        .iter()
        .find(|hit| hit.item.label == label)
        .map(|hit| hit.item.stable_id.clone())
        .unwrap_or_else(|| panic!("{raw} must return {label}"));
    assert!(
        service.record_selection(&chosen),
        "a visible item must be recordable"
    );
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// The whole point of the store. A history saved by one process and loaded by
/// the next must be indistinguishable from the one that was saved, field for
/// field — frequency, last-selected timestamp and per-query affinity alike.
/// Kills a store that serializes the item keys and drops a count, which would
/// still "remember the item" and quietly rerank it.
#[test]
fn a_saved_selection_history_reloads_with_every_field_intact() {
    let scratch = Scratch::new("round-trip");
    let store = SelectionHistoryStore::new(scratch.join("selection-history.json"));

    let mut writer = service_with(fixture_items());
    select(&mut writer, "fire", "Firefox", 1_000);
    select(&mut writer, "fire", "Firefox", 1_400);
    select(&mut writer, "web", "Firefox", 1_700);
    select(&mut writer, "term", "Terminal", 900);
    let saved = writer.selection_history_snapshot();
    store.save(&saved).expect("the scratch history is writable");

    let reloaded = SelectionHistoryStore::new(store.path()).load();

    assert_eq!(
        reloaded, saved,
        "a reloaded history must equal the one that was written"
    );
    assert_eq!(reloaded.selections.len(), 2, "two distinct items were selected");
    assert_eq!(
        reloaded.query_affinities.len(),
        3,
        "three distinct (item, query) pairs were confirmed"
    );
    let firefox = reloaded
        .selections
        .iter()
        .find(|record| record.item.0 == "firefox")
        .expect("the selected item is present");
    assert_eq!(firefox.frequency, 3);
    assert_eq!(firefox.last_selected_secs, Some(1_700));
    assert_eq!(firefox.plugin.0, PLUGIN);
}

/// The reload must reach the *ranker*, not merely round-trip through a field:
/// a service that stores a loaded history somewhere nothing scores from would
/// pass the round trip above and still learn nothing across launches. Two
/// items that match the query identically make the history the only thing that
/// can separate them, so the assertion cannot pass by accident.
#[test]
fn a_reloaded_history_reranks_the_next_launch_the_way_the_last_one_ended() {
    let scratch = Scratch::new("reranks");
    let store = SelectionHistoryStore::new(scratch.join("selection-history.json"));
    let contenders = || {
        vec![
            item("note-a", "Note Alpha", Category::File, &[]),
            item("note-b", "Note Beta", Category::File, &[]),
        ]
    };

    let mut cold = service_with(contenders());
    cold.submit_query("note").expect("the fixture query is accepted");
    let unlearned = cold.results()[0].item.label.clone();

    let mut writer = service_with(contenders());
    for at in [5_000, 5_100, 5_200] {
        select(&mut writer, "note", &unlearned_rival(&unlearned), at);
    }
    store
        .save(&writer.selection_history_snapshot())
        .expect("the scratch history is writable");

    let mut reader = service_with(contenders());
    reader.restore_selection_history(store.load());
    reader.set_history_time(5_300);
    reader
        .submit_query("note")
        .expect("the fixture query is accepted");

    assert_eq!(
        reader.results()[0].item.label,
        unlearned_rival(&unlearned),
        "the item selected in the previous launch must lead this one; \
         without a history {unlearned} leads"
    );
}

/// The contender the untrained ranker does *not* put first, so the test above
/// asserts a change rather than restating the tie-break.
fn unlearned_rival(leader: &str) -> String {
    match leader {
        "Note Alpha" => "Note Beta".to_owned(),
        other => {
            assert_eq!(other, "Note Beta", "the fixture has exactly two contenders");
            "Note Alpha".to_owned()
        }
    }
}

/// Text that is not a record must be a fresh history, never a startup failure.
/// The store is read before the launcher has a window, so a parse that could
/// refuse would turn one damaged state file into an unusable launcher — the
/// exact failure mode the startup journal exists to avoid and this file copies.
#[test]
fn a_corrupt_history_file_loads_as_an_empty_history_rather_than_failing_startup() {
    let scratch = Scratch::new("corrupt");

    // Each of these is a distinct way for the file to be wrong, and a parser
    // that is lenient about any one of them is inventing selections.
    for (label, bytes) in [
        (
            "truncated",
            r#"{"crikey_selection_history":1,"selections":[{"plu"#,
        ),
        ("not-json", "this was never a history"),
        ("empty", ""),
        ("no-magic", r#"{"selections":[],"query_affinities":[]}"#),
        (
            "future-version",
            r#"{"crikey_selection_history":2,"selections":[],"query_affinities":[]}"#,
        ),
        (
            "unknown-key",
            r#"{"crikey_selection_history":1,"selections":[],"query_affinities":[],"extra":1}"#,
        ),
        (
            "missing-field",
            r#"{"crikey_selection_history":1,"selections":[{"plugin":"p","item":"i"}],"query_affinities":[]}"#,
        ),
    ] {
        let path = scratch.join(&format!("{label}.json"));
        fs::write(&path, bytes).expect("the scratch history is writable");

        let loaded = SelectionHistoryStore::new(&path).load();

        assert!(
            loaded.selections.is_empty() && loaded.query_affinities.is_empty(),
            "a {label} history must load as empty, got {loaded:?}"
        );
    }
}

/// A file past the ceiling must be refused without being read into memory.
/// Reading it in full to discover it was too big is the bug: the allocation is
/// decided by whatever is on disk, before the process has a window, and an
/// allocator abort is not a failure any fallback can catch.
#[test]
fn an_oversized_history_file_loads_as_an_empty_history() {
    let scratch = Scratch::new("oversized");
    let path = scratch.join("selection-history.json");

    // A genuinely well-formed prefix, so the refusal can only be the size
    // ceiling and not a parse error reached first.
    let mut oversized =
        String::from(r#"{"crikey_selection_history":1,"selections":[],"query_affinities":[]}"#);
    let padding = usize::try_from(SelectionHistoryStore::MAX_BYTES).expect("the ceiling fits in memory");
    oversized.push_str(&" ".repeat(padding));
    fs::write(&path, &oversized).expect("the scratch history is writable");

    let loaded = SelectionHistoryStore::new(&path).load();

    assert!(
        loaded.selections.is_empty() && loaded.query_affinities.is_empty(),
        "a history past the ceiling must load as empty"
    );
}

/// An absent file is a first launch, and a path that is not a regular file is
/// treated the same way without being opened: a state path swapped for a
/// directory or a named pipe must not be able to block or fail a boot.
#[test]
fn an_absent_or_non_regular_history_path_loads_as_an_empty_history() {
    let scratch = Scratch::new("non-regular");

    let absent = SelectionHistoryStore::new(scratch.join("never-written.json")).load();
    assert!(absent.selections.is_empty() && absent.query_affinities.is_empty());

    let directory = scratch.join("a-directory");
    fs::create_dir(&directory).expect("the scratch directory is creatable");
    let opened = SelectionHistoryStore::new(&directory).load();
    assert!(opened.selections.is_empty() && opened.query_affinities.is_empty());
}

/// A save must not be able to leave a readable history replaced by a partial
/// one, and it must not leave its staging file behind. Publishing through a
/// rename is the mechanism; the observable is that the directory holds exactly
/// the record and nothing else.
#[test]
fn saving_publishes_atomically_and_leaves_no_staging_file_behind() {
    let scratch = Scratch::new("atomic");
    let path = scratch.join("selection-history.json");
    let store = SelectionHistoryStore::new(&path);

    let mut writer = service_with(fixture_items());
    select(&mut writer, "fire", "Firefox", 1_000);
    store
        .save(&writer.selection_history_snapshot())
        .expect("the scratch history is writable");
    select(&mut writer, "term", "Terminal", 1_100);
    store
        .save(&writer.selection_history_snapshot())
        .expect("the scratch history is rewritable");

    let leftovers = fs::read_dir(scratch.path())
        .expect("the scratch directory is readable")
        .map(|entry| entry.expect("the scratch entry is readable").file_name())
        .collect::<Vec<_>>();
    assert_eq!(
        leftovers.len(),
        1,
        "only the published record may remain, found {leftovers:?}"
    );
    assert_eq!(
        SelectionHistoryStore::new(&path).load(),
        writer.selection_history_snapshot(),
        "the second save must have replaced the first record wholesale"
    );
}

/// The store creates the state directory it was pointed at. `crikey run`
/// derives the path from `$XDG_STATE_HOME` and nothing guarantees that
/// `crikey/` under it exists yet on a first launch.
#[test]
fn saving_creates_a_missing_state_directory() {
    let scratch = Scratch::new("mkdir");
    let path = scratch
        .join("nested")
        .join("deeper")
        .join("selection-history.json");
    let store = SelectionHistoryStore::new(&path);

    let mut writer = service_with(fixture_items());
    select(&mut writer, "fire", "Firefox", 1_000);
    store
        .save(&writer.selection_history_snapshot())
        .expect("a missing state directory is created rather than reported");

    assert!(Path::new(&path).is_file(), "the record must have been published");
}

/// Item ids are plugin-supplied and queries are user data, so both may hold
/// quotes, backslashes and control characters. An escaping bug here is silent
/// in the launch that causes it — the record still writes — and only the
/// *next* launch discovers the history has become unparseable and empty.
#[test]
fn a_history_over_hostile_text_survives_the_round_trip() {
    let scratch = Scratch::new("escaping");
    let store = SelectionHistoryStore::new(scratch.join("selection-history.json"));

    let awkward = item(
        "quote\"back\\slash\nnewline\u{1}control",
        "Say \"hello\"",
        Category::Application,
        &["hello"],
    );
    let mut writer = service_with(vec![awkward]);
    select(&mut writer, "hello", "Say \"hello\"", 42);
    let saved = writer.selection_history_snapshot();
    store.save(&saved).expect("the scratch history is writable");

    assert_eq!(
        SelectionHistoryStore::new(store.path()).load(),
        saved,
        "quotes, backslashes and control characters must survive the encoding"
    );
}

// ---------------------------------------------------------------------------
// Foreground context
// ---------------------------------------------------------------------------

/// The honesty case, and the reason this signal took so long to wire: a
/// backend that cannot report a focused window — Wayland, an X display with no
/// EWMH manager, Windows and macOS in this build — must leave the context term
/// off. Defaulting to [`Category::Application`] because "windows belong to
/// applications" would promote every application row on every query, on every
/// desktop the launcher understands nothing about.
#[test]
fn a_backend_that_cannot_report_a_foreground_window_yields_no_category() {
    let mut service = service_with(fixture_items());
    service.set_foreground_category(Some(Category::Application));

    service.set_foreground_from_window(None);

    assert_eq!(
        service.foreground_category(),
        None,
        "no window must clear the context signal rather than default it"
    );
}

/// A window the desktop *did* report, but whose owning program it will not
/// name, is the same amount of evidence as no window at all. The title is
/// deliberately something a looser implementation would match on.
#[test]
fn a_foreground_window_with_no_named_owner_yields_no_category() {
    let mut service = service_with(fixture_items());

    service.set_foreground_from_window(Some(&WindowInfo {
        handle: WindowHandle(7),
        title: "Firefox".to_owned(),
        application: None,
    }));

    assert_eq!(
        service.foreground_category(),
        None,
        "a title is not evidence of which catalog item owns the window"
    );
}

/// A named owner that matches nothing in the catalog is also no evidence. The
/// launcher knows the categories of things it has catalogued and nothing else.
#[test]
fn a_foreground_program_absent_from_the_catalog_yields_no_category() {
    let mut service = service_with(fixture_items());

    service.set_foreground_from_window(Some(&WindowInfo {
        handle: WindowHandle(7),
        title: "Untitled".to_owned(),
        application: Some("some-program-nobody-catalogued".to_owned()),
    }));

    assert_eq!(service.foreground_category(), None);
}

/// The positive case, which is what makes all three negatives meaningful: a
/// window whose owner names a catalogued item contributes that item's category,
/// matched case-insensitively because `WM_CLASS` capitalisation is the
/// program's choice and a desktop entry's is the packager's.
#[test]
fn a_foreground_program_in_the_catalog_supplies_that_items_category() {
    let mut service = service_with(fixture_items());

    for (owner, expected) in [
        ("Firefox", Category::Application),
        ("firefox", Category::Application),
        ("FIREFOX", Category::Application),
        // Matched through a declared search term rather than the label.
        ("konsole", Category::Command),
    ] {
        service.set_foreground_from_window(Some(&WindowInfo {
            handle: WindowHandle(7),
            title: String::new(),
            application: Some(owner.to_owned()),
        }));

        assert_eq!(
            service.foreground_category(),
            Some(&expected),
            "{owner} names a catalogued item and must supply its category"
        );
    }
}

/// A substring rule would be the easy implementation and the wrong one: it
/// would let a program called `Note` claim the `Notes` item, and on a real
/// desktop it makes the context signal fire for programs the user never
/// catalogued.
#[test]
fn a_foreground_program_that_merely_resembles_a_catalog_entry_yields_no_category() {
    let mut service = service_with(fixture_items());

    for owner in ["Note", "Notesss", "Fire", "Terminal Emulator"] {
        service.set_foreground_from_window(Some(&WindowInfo {
            handle: WindowHandle(7),
            title: String::new(),
            application: Some(owner.to_owned()),
        }));

        assert_eq!(
            service.foreground_category(),
            None,
            "{owner} is not a catalog entry and must not match one"
        );
    }
}

// ---------------------------------------------------------------------------
// Size budget
// ---------------------------------------------------------------------------
//
// A record count is not a byte count. A query is whatever the user typed, and
// an item id is whatever a plugin chose, so a few thousand records of pasted
// text can still clear `MAX_BYTES`. The file that results does not load as a
// truncated history - it loads as *no* history, discarding everything the user
// ever taught the launcher. These pin both defences: the affinity key is
// bounded when it is recorded, and whatever reaches the file is trimmed to fit.

/// A hostile query must not be able to make the file unreadable.
#[test]
fn an_enormous_query_does_not_cost_the_user_their_history() {
    let scratch = Scratch::new("huge-query");
    let store = SelectionHistoryStore::new(scratch.join("selection-history.json"));

    let mut history = crikey_ranking::SelectionHistory::default();
    let ordinary = item("firefox", "Firefox", Category::Application, &[]);
    let normalizer = crikey_query::DefaultNormalizer::default();
    use crikey_query::Normalizer as _;

    // Something worth keeping, then a pasted novel.
    history.record(&ordinary, &normalizer.normalize("fire"), 10);
    let huge = "x".repeat(4 * 1024 * 1024);
    history.record(&ordinary, &normalizer.normalize(&huge), 20);

    store.save(&history.snapshot()).expect("history is writable");
    let reloaded = store.load();

    assert!(
        reloaded
            .query_affinities
            .iter()
            .any(|record| record.query == "fire"),
        "the ordinary affinity must survive, got {:?}",
        reloaded
            .query_affinities
            .iter()
            .map(|record| record.query.len())
            .collect::<Vec<_>>()
    );
    assert!(
        !reloaded
            .query_affinities
            .iter()
            .any(|record| record.query.len() > 4096),
        "the pasted query must never have been recorded as a key"
    );
    assert_eq!(
        reloaded.selections.len(),
        1,
        "and the item-level record is kept either way"
    );
}

/// Even if a snapshot arrives oversized, the file written from it must load.
#[test]
fn an_oversized_snapshot_is_trimmed_rather_than_written_unreadable() {
    let scratch = Scratch::new("oversized");
    let store = SelectionHistoryStore::new(scratch.join("selection-history.json"));

    // Built directly, bypassing the recording caps, the way a future caller or
    // a differently-bounded build could.
    let filler = "q".repeat(4_096);
    let mut snapshot = crikey_app::SelectionHistorySnapshot::default();
    snapshot.selections.push(crikey_app::SelectionRecord {
        plugin: PluginId(PLUGIN.to_owned()),
        item: ItemId("firefox".to_owned()),
        frequency: 500,
        last_selected_secs: Some(10),
    });
    for index in 0..4_096 {
        snapshot.query_affinities.push(crikey_app::QueryAffinityRecord {
            plugin: PluginId(PLUGIN.to_owned()),
            item: ItemId(format!("item-{index}")),
            query: format!("{filler}{index}"),
            count: 1,
        });
    }
    // The unbounded form of this snapshot is far past the limit.
    assert!(
        snapshot.query_affinities.len() as u64 * 4_096 > SelectionHistoryStore::MAX_BYTES,
        "the fixture must actually exceed the budget"
    );

    store.save(&snapshot).expect("history is writable");

    let written = fs::metadata(store.path()).expect("the file exists").len();
    assert!(
        written <= SelectionHistoryStore::MAX_BYTES,
        "wrote {written} bytes against a {} byte budget",
        SelectionHistoryStore::MAX_BYTES
    );
    let reloaded = store.load();
    assert_eq!(
        reloaded.selections.len(),
        1,
        "the most-used record survives, so the history is not lost"
    );
    assert!(
        !reloaded.query_affinities.is_empty(),
        "and the file still carries affinities rather than loading empty"
    );
}
