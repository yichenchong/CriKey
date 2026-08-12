//! Icons reaching the renderer's row model (spec 6.4, 10.1, 22.1).
//!
//! [`SearchService::result_rows`] is the seam between an item's `icon_reference`
//! -- a string only the platform that wrote it can interpret -- and the pixels a
//! result row draws. This is where that translation is defended: the row must
//! carry decoded pixels when the platform resolves the reference, must carry
//! none when it does not, and must resolve any one reference once per session
//! however many times it is presented.
//!
//! Every reference here is an absolute path to a file this test wrote, which is
//! the one shape of reference every backend resolves: the Linux icon source
//! accepts an absolute path before it consults a theme, the Windows one accepts
//! it once the resource index is stripped, and the macOS one takes nothing else.
//! A themed name would make the outcome depend on which icon themes the host
//! happens to have installed, which is a statement about the runner rather than
//! about the composition.

use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};

use crikey_app::{App, SearchService, StartupStage};
use crikey_core::{ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId};

const PLUGIN: &str = "dev.crikey.icon-rows";

/// A unique scratch directory that deletes itself when the test ends.
#[derive(Debug)]
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);

        let path = std::env::temp_dir().join(format!(
            "crikey-icon-rows-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");
        Self { path }
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path.join(name);
        fs::write(&path, bytes).expect("fixture is writable");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// An SVG of one solid colour, so the decoded pixels are predictable and the
/// decoded extent is the requested one rather than the file's.
fn svg() -> Vec<u8> {
    br##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="#112233"/></svg>"##
        .to_vec()
}

fn item(id: &str, label: &str, icon_reference: Option<String>) -> Item {
    Item {
        stable_id: ItemId(id.to_owned()),
        plugin_id: PluginId(PLUGIN.to_owned()),
        category: Category::Application,
        label: label.to_owned(),
        description: String::new(),
        target: format!("app://{id}"),
        search_terms: Vec::new(),
        icon_reference,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    }
}

/// A service holding `items` and ready to answer queries.
fn service(items: Vec<Item>) -> SearchService {
    let mut service = SearchService::new(App::new());
    for stage in [
        StartupStage::WindowAndHotkey,
        StartupStage::PersistedCatalog,
        StartupStage::AcceptQueries,
    ] {
        service.complete_stage(stage).expect("staged startup is in order");
    }
    service
        .replace_catalog(&PluginId(PLUGIN.to_owned()), 1, items)
        .expect("the catalog slice is accepted");
    service
}

#[test]
fn a_row_carries_the_pixels_its_icon_reference_resolved_to() {
    let scratch = Scratch::new();
    let path = scratch.write("app.svg", &svg());
    let reference = path.to_string_lossy().into_owned();
    let mut service = service(vec![item("one", "firefox", Some(reference.clone()))]);
    service.submit_query("firefox").expect("queries are accepted");

    let rows = service.result_rows();

    let row = rows.first().expect("the item answers its own label");
    let icon = row.icon.as_ref().expect("the reference resolved to pixels");
    // Requested at the platform's default edge, not the file's 16 pixels: a
    // vector source is rendered at the size the row will draw it.
    assert_eq!((icon.width(), icon.height()), (48, 48));
    assert_eq!(
        &icon.rgba()[..4],
        &[0x11, 0x22, 0x33, 0xff],
        "the decoded pixels are the fixture's colour"
    );
    // The reference survives beside the pixels: it is what identifies the icon to
    // anything that has to resolve it again.
    assert_eq!(row.icon_reference, Some(reference));
}

#[test]
fn a_row_whose_reference_resolves_to_nothing_still_reports_the_reference() {
    let scratch = Scratch::new();
    let missing = scratch.path.join("never-written.png");
    let mut service = service(vec![item(
        "one",
        "firefox",
        Some(missing.to_string_lossy().into_owned()),
    )]);
    service.submit_query("firefox").expect("queries are accepted");

    let rows = service.result_rows();

    let row = rows.first().expect("the item answers its own label");
    // A row is presented either way: an icon nobody can resolve is a display
    // detail, not a reason to drop a result.
    assert!(row.icon.is_none());
    assert_eq!(row.icon_reference, Some(missing.to_string_lossy().into_owned()));
}

#[test]
fn an_item_naming_no_icon_carries_no_pixels() {
    let mut service = service(vec![item("one", "firefox", None)]);
    service.submit_query("firefox").expect("queries are accepted");

    let rows = service.result_rows();

    let row = rows.first().expect("the item answers its own label");
    assert!(row.icon.is_none());
    assert!(row.icon_reference.is_none());
}

#[test]
fn a_reference_is_resolved_once_however_many_rows_and_publications_present_it() {
    let scratch = Scratch::new();
    let icon = scratch.write("app.svg", &svg());
    let reference = icon.to_string_lossy().into_owned();
    let mut service = service(vec![
        item("one", "firefox", Some(reference.clone())),
        item("two", "firefox-nightly", Some(reference)),
    ]);
    service.submit_query("firefox").expect("queries are accepted");

    let first = service.result_rows();
    let second = service.result_rows();

    // Two rows and two publications share one allocation: the decoded pixels are
    // memoised per reference, so a keystroke that leaves the results standing
    // does not re-render an SVG per row per frame.
    let shared = |rows: &[crikey_ui::ResultRow], index: usize| {
        Arc::clone(rows[index].icon.as_ref().expect("the reference resolved"))
    };
    assert_eq!(first.len(), 2, "both items answer the query");
    assert!(Arc::ptr_eq(&shared(&first, 0), &shared(&first, 1)));
    assert!(Arc::ptr_eq(&shared(&first, 0), &shared(&second, 0)));
}

/// "Once per session" has to mean the loader is not consulted again, not merely
/// that the pixels are shared.
///
/// `result_rows` runs on the UI thread for every row of every keystroke. A
/// reference that is re-resolved each time costs a theme-chain search and an
/// image decode per row per keystroke -- about 3 ms each against a real theme,
/// and a miss costs the most because it searches everything before giving up.
/// That is what put the launcher's spinner up for seconds on a machine with a
/// full Start Menu.
///
/// Deleting the file is how a test sees the difference: pixels already resolved
/// stay resolved, while a loader consulted again would find nothing there.
#[test]
fn a_resolved_reference_is_not_looked_up_again_on_the_next_keystroke() {
    let scratch = Scratch::new();
    let icon = scratch.write("app.svg", &svg());
    let reference = icon.to_string_lossy().into_owned();
    let mut service = service(vec![item("one", "firefox", Some(reference))]);
    service.submit_query("firefox").expect("queries are accepted");

    let first = service.result_rows();
    assert!(
        first[0].icon.is_some(),
        "the fixture resolves on the first publication"
    );

    fs::remove_file(&icon).expect("the fixture is removable");

    let second = service.result_rows();
    assert!(
        second[0].icon.is_some(),
        "a reference resolved once this session must not be resolved again: the \
         platform loader was consulted a second time and paid for a search that \
         the session had already answered"
    );
}

/// The exception, and the reason re-resolving was there in the first place: a
/// replaced catalog slice is an install, an upgrade or a removal, and any of
/// those can change what a reference resolves to.
#[test]
fn replacing_a_catalog_slice_lets_every_reference_resolve_again() {
    let scratch = Scratch::new();
    let icon = scratch.write("app.svg", &svg());
    let reference = icon.to_string_lossy().into_owned();
    let mut service = service(vec![item("one", "firefox", Some(reference.clone()))]);
    service.submit_query("firefox").expect("queries are accepted");
    assert!(service.result_rows()[0].icon.is_some());

    fs::remove_file(&icon).expect("the fixture is removable");
    service
        .replace_catalog(
            &PluginId(PLUGIN.to_owned()),
            2,
            vec![item("one", "firefox", Some(reference))],
        )
        .expect("the catalog slice is accepted");
    service.submit_query("firefox").expect("queries are accepted");

    assert!(
        service.result_rows()[0].icon.is_none(),
        "a replaced slice drops the memo, so the reference resolves against what \
         is on disk now rather than what was there before the install"
    );
}

#[test]
fn a_reference_naming_an_oversize_file_is_reported_as_no_icon_rather_than_read() {
    let scratch = Scratch::new();
    // Past the spec 11.7 payload limit. The refusal happens inside the platform
    // loader; what is defended here is that it degrades to a row without an icon
    // instead of failing the presentation.
    let oversize = scratch.write(
        "huge.png",
        &vec![0_u8; (crikey_platform::MAX_ICON_PAYLOAD_BYTES + 1) as usize],
    );
    let mut service = service(vec![item(
        "one",
        "firefox",
        Some(oversize.to_string_lossy().into_owned()),
    )]);
    service.submit_query("firefox").expect("queries are accepted");

    let rows = service.result_rows();

    assert!(rows.first().expect("the item answers").icon.is_none());
}
