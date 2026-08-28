//! Host-mediated contract for opening a file result (spec 18.2, 18.3).
//!
//! The gap this defends is the one that made file search unusable: the mapping
//! produced items carrying `crikey.file.open`, and the host execution path
//! accepted exactly one action id, which was the application launch. Selecting
//! a file was refused. So what is asserted here is the gate, end to end -- an
//! owner in the grant map gets its path opened, an owner outside it does not,
//! and a path that is not valid Unicode survives the trip.
//!
//! Nothing spawns a real handler. The Linux backend's opener helper is injected
//! with a recording script, exactly as `crikey-platform-linux`'s own coverage
//! does it, so the argv the desktop would have received is observable instead
//! of opening a browser window on a CI machine.

#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use crikey_app::{ActionSubmission, App, PluginActionRouter, SearchService, StartupStage};
use crikey_core::{ActionId, Item, ItemId, PluginId};
use crikey_platform::{
    encode_target, file_items, FileHit, FileKind, FILE_OPEN_ACTION_ID, FILE_REVEAL_ACTION_ID,
};
use crikey_platform_linux::{LinuxBackend, XdgOpener};

/// The owner the composition root publishes file results under.
const FILE_CATALOG_PLUGIN: &str = "builtin.crikey.files";

/// A liveness guard, never a timing assertion: a correct open reports back in
/// milliseconds. The bound only turns a regression that blocks forever into a
/// failure instead of a hung run.
const RESPONSE_LIMIT: Duration = Duration::from_secs(60);

/// A granted owner's file row opens its exact path.
///
/// Kills the original defect directly: before the host learned this action id,
/// this call came back "unsupported host-mediated action".
#[test]
fn selecting_a_file_result_opens_its_path() {
    let recorder = Recorder::new();
    let path = recorder.sibling("Quarterly Report; final $(x).ods");
    let owner = PluginId(FILE_CATALOG_PLUGIN.to_owned());
    let item = file_item(&owner, &path);
    let mut service = granted_service(&recorder, &owner);
    let selected = merged(&mut service, item);

    let reading = recorder.reading();
    let submission = service
        .execute(&selected, &ActionId(FILE_OPEN_ACTION_ID.to_owned()))
        .expect("a granted owner's file row opens");

    assert!(
        matches!(submission, ActionSubmission::Completed),
        "opening a file is host-mediated and completes in place"
    );
    assert_eq!(
        argv(&collect(reading)),
        vec![path.into_os_string().into_vec()],
        "the desktop must be handed the exact path, as one argument"
    );
}

/// A path with no `String` spelling still opens (ADR-0007).
///
/// Kills the bug where the target survives encoding but is then decoded
/// lossily somewhere in the execution path: the user is told a file they can
/// see in the list does not exist.
#[test]
fn a_file_whose_name_is_not_valid_unicode_still_opens() {
    let recorder = Recorder::new();
    // A lone 0xFF can begin no UTF-8 sequence, so this name has no lossless
    // `String` spelling at all.
    let raw = OsString::from_vec(b"pla\xffn.md".to_vec());
    assert!(raw.to_str().is_none(), "the fixture must not be valid UTF-8");
    let path = recorder.scratch.join(&raw);
    let owner = PluginId(FILE_CATALOG_PLUGIN.to_owned());
    let item = file_item(&owner, &path);
    let mut service = granted_service(&recorder, &owner);
    let selected = merged(&mut service, item);

    let reading = recorder.reading();
    service
        .execute(&selected, &ActionId(FILE_OPEN_ACTION_ID.to_owned()))
        .expect("a path that is not valid Unicode is still a path");

    assert_eq!(
        argv(&collect(reading)),
        vec![path.into_os_string().into_vec()],
        "every byte of the path must reach the desktop"
    );
}

/// The reveal action is separately dispatched and opens the parent.
#[test]
fn revealing_a_file_result_opens_its_containing_directory() {
    let recorder = Recorder::new();
    let path = recorder.sibling("receipt.pdf");
    let owner = PluginId(FILE_CATALOG_PLUGIN.to_owned());
    let item = file_item(&owner, &path);
    let mut service = granted_service(&recorder, &owner);
    let selected = merged(&mut service, item);

    let reading = recorder.reading();
    service
        .execute(&selected, &ActionId(FILE_REVEAL_ACTION_ID.to_owned()))
        .expect("a granted owner's file row reveals");

    assert_eq!(
        argv(&collect(reading)),
        vec![recorder.scratch.as_os_str().as_bytes().to_vec()],
        "revealing opens the directory, not the document"
    );
}

/// An owner the router does not know is refused, and nothing is opened.
///
/// Kills the bug where a new action id is wired into the dispatch without being
/// wired into the gate: an unattributable item must be refused rather than
/// resolved to some other owner's grants, exactly as an application launch is.
#[test]
fn a_file_item_from_an_unknown_owner_is_refused() {
    let recorder = Recorder::new();
    let path = recorder.sibling("stolen.txt");
    let stranger = PluginId("third.party.impostor".to_owned());
    let item = file_item(&stranger, &path);
    // The router knows the real file catalog and nothing else, which is the
    // situation a plugin-carrying build is always in.
    let mut service = granted_service(&recorder, &PluginId(FILE_CATALOG_PLUGIN.to_owned()));
    // The stranger's row still reaches the answer: an unknown owner is refused
    // at the grant map, not by being unable to publish a result.
    let selected = merged(&mut service, item);

    for action in [FILE_OPEN_ACTION_ID, FILE_REVEAL_ACTION_ID] {
        let refusal = service
            .execute(&selected, &ActionId(action.to_owned()))
            .expect_err("an owner outside the grant map must not have the host open anything")
            .to_string();

        assert!(
            refusal.contains("third.party.impostor"),
            "the refusal names the owner it refused, got: {refusal}"
        );
        assert!(
            refusal.contains("document open"),
            "the refusal names the operation it refused, got: {refusal}"
        );
    }

    // And no helper ran. With a fifo this could only be left as a remark -- an
    // unread fifo is indistinguishable from an unwritten one without blocking.
    // A recording that was never created is a positive fact, so the refusal is
    // now proved rather than described.
    assert!(
        !recorder.ran(),
        "a refused action must not have launched the helper"
    );
}

/// An action id the host does not implement is refused by name.
#[test]
fn an_unimplemented_host_action_is_refused_by_name() {
    let recorder = Recorder::new();
    let path = recorder.sibling("notes.txt");
    let owner = PluginId(FILE_CATALOG_PLUGIN.to_owned());
    let mut item = file_item(&owner, &path);
    item.actions[0].action_id = ActionId("crikey.file.shred".to_owned());
    let mut service = granted_service(&recorder, &owner);
    let selected = merged(&mut service, item);

    let refusal = service
        .execute(&selected, &ActionId("crikey.file.shred".to_owned()))
        .expect_err("the host implements a closed set of actions")
        .to_string();

    assert!(
        refusal.contains("crikey.file.shred"),
        "the refusal names the action it does not implement, got: {refusal}"
    );
}

/// The item the mapping produces for `path`, so the test opens what a real row
/// carries rather than an item written by hand.
fn file_item(owner: &PluginId, path: &Path) -> Item {
    let hit = FileHit {
        name: path
            .file_name()
            .expect("the fixture has a name")
            .to_string_lossy()
            .into_owned(),
        path: crikey_core::PlatformPath::from(path.to_path_buf()),
        kind: FileKind::File,
        modified_unix_seconds: None,
    };
    let mut items = file_items(owner, std::slice::from_ref(&hit));
    assert_eq!(items.len(), 1, "one hit maps to one item");
    let item = items.remove(0);
    assert_eq!(
        item.target,
        encode_target(&hit.path),
        "the item carries the lossless target the opener decodes"
    );
    item
}

/// A search service whose backend opens through the recorder and whose router
/// grants exactly `owner`.
fn granted_service(recorder: &Recorder, owner: &PluginId) -> SearchService {
    let app = App::with_backend(LinuxBackend::new().with_file_opener(Some(recorder.opener())));
    let mut router = PluginActionRouter::default();
    router
        .register_host_catalog(owner.clone())
        .expect("the host catalog registers once");

    let mut service = SearchService::new(app);
    service.set_plugin_action_router(Arc::new(router));
    for stage in PRE_QUERY_STAGES {
        service
            .complete_stage(stage)
            .expect("startup milestones are acknowledged in order");
    }
    service
}

/// The milestones that must be acknowledged before a query is accepted.
const PRE_QUERY_STAGES: [StartupStage; 3] = [
    StartupStage::WindowAndHotkey,
    StartupStage::PersistedCatalog,
    StartupStage::AcceptQueries,
];

/// Puts `item` into the ranked answer the way the launcher does, and hands back
/// the id the execution path resolves against.
///
/// This is the whole route a selected file takes. The launcher never hands an
/// item to the service: the file provider answers a query asynchronously, its
/// batch is merged into the answer for that generation, and execution then
/// happens by id against that answer. Handing a hand-built item straight to a
/// dispatcher would assert the gate while asserting nothing about whether a
/// file row can reach it -- which is precisely the defect this file exists for.
fn merged(service: &mut SearchService, item: Item) -> ItemId {
    let selected = item.stable_id.clone();
    let owner = item.plugin_id.clone();
    let query = matching_query(&item.label);
    let generation = service.submit_query(&query).expect("the query is accepted");
    assert!(
        service.merge_query_items(generation, &owner, vec![item]),
        "the provider's batch answers the current generation"
    );
    assert!(
        service.results().iter().any(|hit| hit.item.stable_id == selected),
        "the merged file must be in the answer the execution path resolves against"
    );
    selected
}

/// A query the item's label matches: its leading run of ASCII alphanumerics.
///
/// Derived from the label rather than written per test because one fixture's
/// name is not valid Unicode, and its label -- the lossy spelling -- is only
/// reliably typeable up to the byte that has no character.
fn matching_query(label: &str) -> String {
    let query: String = label.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    assert!(
        query.chars().count() >= 3,
        "the fixture's name must give the matcher something to prefix, got: {label:?}"
    );
    query
}

/// A recording program standing where `xdg-open` goes.
///
/// It writes the argv it was handed to a file, NUL separated and preceded by a
/// count, because NUL is the one byte an argument cannot contain and the count
/// catches a trailing empty argument that a separator based reading would
/// otherwise lose.
#[derive(Debug)]
struct Recorder {
    scratch: PathBuf,
    program: PathBuf,
    recording: PathBuf,
}

impl Recorder {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);

        let scratch = std::env::temp_dir().join(format!(
            "crikey-file-open-action-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&scratch);
        fs::create_dir_all(&scratch).expect("scratch directory is creatable");

        // A plain file, written atomically, rather than a fifo. A fifo makes
        // the helper and the test rendezvous, which reads well until one side
        // never arrives: `exec > fifo` blocks in `open` until a reader appears,
        // and a helper blocked there holds the file descriptors it inherited
        // from the test harness, so `cargo test` waits on it forever rather
        // than reporting anything. A refusal test that never reads, or a run
        // interrupted between the two, is enough to reach that state. Writing
        // a regular file cannot block, and the rename is what keeps a reader
        // from seeing half an argv.
        let recording = scratch.join("argv");

        let program = scratch.join("record-argv");
        // The scratch path is built from a pid and a counter, so it holds
        // nothing the single quotes here would have to escape.
        write_program(
            &program,
            &format!(
                "#!/bin/sh\n\
                 exec > '{recording}.part'\n\
                 printf '%s\\0' \"$#\"\n\
                 for argument in \"$@\"; do printf '%s\\0' \"$argument\"; done\n\
                 mv '{recording}.part' '{recording}'\n",
                recording = recording.display()
            ),
        );

        Self {
            scratch,
            program,
            recording,
        }
    }

    fn opener(&self) -> XdgOpener {
        XdgOpener::with_helper(self.program.clone())
    }

    fn sibling(&self, name: &str) -> PathBuf {
        self.scratch.join(name)
    }

    /// Whether the helper ran at all.
    fn ran(&self) -> bool {
        self.recording.exists()
    }

    /// Waits for the helper's argv on a worker thread.
    ///
    /// Bounded, and ending on the observable rather than on a clock: the
    /// rename publishes the whole recording at once, so seeing the path is
    /// seeing a complete argv. A helper that never runs leaves this to expire,
    /// which `collect` reports by name instead of hanging the run.
    fn reading(&self) -> mpsc::Receiver<Vec<u8>> {
        let recording = self.recording.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let deadline = Instant::now() + RESPONSE_LIMIT;
            while Instant::now() < deadline {
                if let Ok(recorded) = fs::read(&recording) {
                    let _ = sender.send(recorded);
                    return;
                }
                thread::sleep(Duration::from_millis(2));
            }
        });

        receiver
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.scratch);
    }
}

fn collect(reading: mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
    reading
        .recv_timeout(RESPONSE_LIMIT)
        .expect("the helper must run and report its argv")
}

/// Decodes `count\0argument\0...` back into the vector the helper received.
///
/// Bytes, not `String`: one of these tests exists because a path has no `str`
/// spelling, and decoding lossily would hide the corruption it looks for.
fn argv(recorded: &[u8]) -> Vec<Vec<u8>> {
    let mut records: Vec<Vec<u8>> = recorded.split(|byte| *byte == 0).map(<[u8]>::to_vec).collect();

    assert_eq!(
        records.pop().as_deref(),
        Some(&b""[..]),
        "the recording is NUL terminated"
    );
    let count: usize = String::from_utf8(records.remove(0))
        .expect("the count is ASCII")
        .parse()
        .expect("the helper reports how many arguments it received");
    assert_eq!(
        records.len(),
        count,
        "the helper's own count must match the arguments it wrote"
    );

    records
}

/// Writes an executable script, out of reach of the exec-time text-busy race.
///
/// Staged under another name and renamed into place: this binary is
/// multi-threaded, so a sibling test that forks between this file's `open` and
/// `close` hands the child a writable descriptor, and the kernel then refuses
/// to exec the path with ETXTBSY.
fn write_program(path: &Path, body: &str) {
    let staged = path.with_extension("staging");
    fs::write(&staged, body).expect("fixture program is writable");
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).expect("fixture mode is settable");
    fs::rename(&staged, path).expect("fixture program is movable into place");
}
