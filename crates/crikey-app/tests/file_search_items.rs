//! End-to-end contract for file search reaching the item model.
//!
//! The seam this defends is the one that was missing when the platform
//! backends were first written: three implementations of `FileSearchService`
//! existed, compiled, and were tested, and nothing in the host ever called
//! one. A backend nobody invokes is indistinguishable from an unimplemented
//! capability, so the test that matters is not "does the Linux walker find a
//! file" — that lives in `crikey-platform-linux` — but "does a file on disk
//! arrive as an `Item` a user could select and open".
//!
//! Runs on Linux, where the backend walks a real directory. The macOS and
//! Windows backends satisfy the same trait and produce items through the same
//! `file_items` mapping, but their own end-to-end coverage is CI's business.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crikey_app::App;
use crikey_core::{Category, ExecutionPolicy, PluginId};
use crikey_platform::{FileSearchQuery, FILE_OPEN_ACTION_ID, MAX_FILE_HITS};
use crikey_platform_linux::{FilesystemSearch, LinuxBackend};

/// A scratch tree removed when the test that made it ends.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "crikey-file-search-items-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the clock is after the epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("the scratch directory is creatable");
        Self { path }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn service(root: &Path) -> App {
    App::with_backend(
        LinuxBackend::new().with_file_search(FilesystemSearch::walking(vec![root.to_path_buf()])),
    )
}

fn query(text: &str) -> FileSearchQuery {
    FileSearchQuery {
        normalized: text.to_owned(),
        limit: MAX_FILE_HITS,
        deadline: Duration::from_secs(5),
    }
}

#[test]
fn a_file_on_disk_becomes_a_selectable_item_with_an_open_action() {
    let scratch = Scratch::new("file");
    fs::write(scratch.path.join("quarterly-report.txt"), b"x").expect("the fixture file is writable");

    let owner = PluginId("builtin.crikey.files".to_owned());
    let app = service(&scratch.path);
    let (items, _) = app
        .search_file_items(&owner, &query("quarterly"))
        .expect("this build has a file search")
        .expect("the search runs");

    let item = items
        .iter()
        .find(|item| item.label == "quarterly-report.txt")
        .unwrap_or_else(|| panic!("the fixture file must be found; got {items:?}"));

    assert_eq!(item.category, Category::File);
    assert_eq!(item.plugin_id, owner, "the item belongs to the builtin owner");
    assert!(
        item.description.contains(
            scratch
                .path
                .file_name()
                .expect("the scratch directory is named")
                .to_str()
                .expect("the scratch name is UTF-8")
        ),
        "the containing directory is shown so two same-named files are distinguishable; got {:?}",
        item.description
    );

    // Selecting a result has to do something. An item with no action is a row
    // the user can highlight and not act on, which is the shape of this defect
    // if the mapping ever loses its action.
    let action = item
        .actions
        .first()
        .unwrap_or_else(|| panic!("a file item must carry an open action; got {:?}", item.actions));
    assert_eq!(action.action_id.0, FILE_OPEN_ACTION_ID);
    assert_eq!(
        action.execution_policy,
        ExecutionPolicy::HostMediated,
        "opening a path is the host's to perform, never the plugin's"
    );
}

#[test]
fn a_directory_is_categorised_apart_from_a_file() {
    // Ranking weights a directory above a file, so the distinction has to
    // survive the mapping rather than being flattened to Category::File.
    let scratch = Scratch::new("dir");
    fs::create_dir(scratch.path.join("invoices")).expect("the fixture directory is creatable");
    fs::write(scratch.path.join("invoices.txt"), b"x").expect("the fixture file is writable");

    let owner = PluginId("builtin.crikey.files".to_owned());
    let app = service(&scratch.path);
    let (items, _) = app
        .search_file_items(&owner, &query("invoices"))
        .expect("this build has a file search")
        .expect("the search runs");

    let directory = items
        .iter()
        .find(|item| item.label == "invoices")
        .unwrap_or_else(|| panic!("the directory must be found; got {items:?}"));
    let file = items
        .iter()
        .find(|item| item.label == "invoices.txt")
        .unwrap_or_else(|| panic!("the file must be found; got {items:?}"));

    assert_eq!(directory.category, Category::Directory);
    assert_eq!(file.category, Category::File);
    assert_ne!(
        directory.stable_id, file.stable_id,
        "two entries sharing a prefix are still two distinct items"
    );
}

#[test]
fn a_search_that_matches_nothing_is_an_empty_answer_and_not_a_missing_capability() {
    // The two are different states and the caller must be able to tell them
    // apart: `None` means this build cannot search files at all, `Some` with
    // no items means it searched and there was nothing there.
    let scratch = Scratch::new("empty");
    fs::write(scratch.path.join("ledger.txt"), b"x").expect("the fixture file is writable");

    let owner = PluginId("builtin.crikey.files".to_owned());
    let app = service(&scratch.path);
    let (items, _) = app
        .search_file_items(&owner, &query("nothing-here-matches-this"))
        .expect("a build with a file search reports Some even when it finds nothing")
        .expect("the search runs");

    assert!(items.is_empty(), "no fixture matches, so no item is produced");
}
