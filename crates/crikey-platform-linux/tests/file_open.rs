//! Public-API contract for opening a file or folder on Linux (spec 18.2).
//!
//! Picking a file row has to hand that exact path to the desktop's handler.
//! "Exact" is the whole test: a path is a byte string on this platform
//! (ADR-0007), so a name that is not valid Unicode must arrive unchanged, and a
//! name containing shell metacharacters must arrive as a *name* rather than as
//! something a shell got to interpret.
//!
//! Nothing here spawns a real handler. `xdg-open` would resolve whatever the
//! build host has registered and open a browser, an editor or a file manager
//! window on a CI machine, and it would report nothing back. So the helper is
//! injected: a recording script stands where `xdg-open` goes, which exercises
//! the real spawn -- the same `Command`, the same argv marshalling -- while
//! making the argument vector observable. It reports over a fifo, which is also
//! what synchronises the test with it: a read of a fifo returns exactly when
//! the writer has finished, so nothing here sleeps or guesses how long a
//! process takes.

#![cfg(target_os = "linux")]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crikey_core::{CoreError, PlatformPath};
use crikey_platform::{Capability, CapabilityState, FileOpener};
use crikey_platform_linux::{LinuxBackend, XdgOpener};

/// A liveness guard, never a timing assertion: a correct open reports back in
/// milliseconds and every wait ends with it. The bound only turns a regression
/// that blocks forever into a failure instead of a hung run.
const RESPONSE_LIMIT: Duration = Duration::from_secs(60);

/// The whole point of the seam: the path is one argv entry, not a command.
///
/// Kills the bug where a path is pasted into a command string. Every character
/// in this name is one a shell would act on -- `;` ends a command, `$(...)`
/// substitutes one, `&&` chains one, a newline separates two -- and all of them
/// are legal in a Linux filename. If any construction between here and the
/// helper built a command line, this test would see several arguments, or the
/// helper would not run at all.
#[test]
fn a_path_full_of_shell_metacharacters_arrives_as_one_argument() {
    let recorder = Recorder::new();
    let hostile = recorder.sibling("report; rm -rf ~ && $(reboot) `id`\n'\"quoted\".txt");

    let argv = recorder.record(|opener| opener.open_path(&PlatformPath::from(hostile.clone())));

    assert_eq!(
        argv,
        vec![hostile.into_os_string().into_vec()],
        "the path must reach the helper as exactly one argument, unmodified"
    );
}

/// A path that is not valid Unicode still opens (spec 18.3, ADR-0007).
///
/// Kills the bug where the path is round-tripped through `String` somewhere on
/// the way to the helper: a lossy conversion replaces the offending bytes with
/// U+FFFD, which names a different file -- usually no file at all -- and the
/// user is told their file does not exist.
#[test]
fn a_path_that_is_not_valid_unicode_reaches_the_helper_unchanged() {
    let recorder = Recorder::new();
    // A lone 0xFF can begin no UTF-8 sequence, so this name has no lossless
    // `String` spelling at all.
    let raw = OsString::from_vec(b"caf\xffe menu.pdf".to_vec());
    assert!(raw.to_str().is_none(), "the fixture must not be valid UTF-8");
    let path = recorder.sibling_raw(&raw);

    let argv = recorder.record(|opener| opener.open_path(&PlatformPath::from(path.clone())));

    assert_eq!(
        argv,
        vec![path.into_os_string().into_vec()],
        "every byte of the path must survive the trip to the helper"
    );
}

/// Revealing opens the containing directory, not the file.
///
/// Kills the bug where "reveal" is wired to the same call as "open": running
/// `xdg-open` on a spreadsheet starts the spreadsheet editor, which is the one
/// outcome a user asking to see it in their file manager did not want.
#[test]
fn revealing_hands_the_helper_the_containing_directory() {
    let recorder = Recorder::new();
    let file = recorder.sibling("quarterly.ods");

    let argv = recorder.record(|opener| opener.reveal_path(&PlatformPath::from(file.clone())));

    assert_eq!(
        argv,
        vec![file
            .parent()
            .expect("the fixture is inside the scratch directory")
            .as_os_str()
            .as_bytes()
            .to_vec()],
        "revealing must open the directory that holds the file"
    );
}

/// A root reveals as itself, because it is not inside anything.
#[test]
fn revealing_a_root_hands_the_helper_the_root() {
    let recorder = Recorder::new();

    let argv = recorder.record(|opener| opener.reveal_path(&PlatformPath::from(PathBuf::from("/"))));

    assert_eq!(argv, vec![b"/".to_vec()], "a root contains itself");
}

/// A file whose name begins with `-` opens instead of printing usage.
///
/// Kills the bug where a relative path is handed over bare and the helper reads
/// it as an option. `./` names the same file and cannot collide with an
/// absolute path, because only a relative one can start with a dash.
#[test]
fn a_name_beginning_with_a_dash_is_passed_as_an_operand() {
    let recorder = Recorder::new();

    let argv = recorder.record(|opener| opener.open_path(&PlatformPath::from(PathBuf::from("-h"))));

    assert_eq!(
        argv,
        vec![b"./-h".to_vec()],
        "a dash-leading relative path must be spelled so it cannot be read as an option"
    );
}

/// An empty path names no file, so it is refused rather than handed over.
#[test]
fn an_empty_path_is_refused_by_name() {
    let opener = XdgOpener::with_helper("/nonexistent/xdg-open");

    for message in [
        invalid(opener.open_path(&PlatformPath::from(PathBuf::new()))),
        invalid(opener.reveal_path(&PlatformPath::from(PathBuf::new()))),
    ] {
        assert!(
            message.contains("empty path"),
            "the refusal should say what was wrong with it, got: {message}"
        );
    }
}

/// A missing helper is reported with both the path and the kernel's reason.
///
/// Kills the bug where a session with no xdg-utils fails silently: the row
/// appears to do nothing, and there is nothing in the message to act on.
#[test]
fn a_helper_that_is_not_installed_reports_itself_and_the_os_error() {
    let opener = XdgOpener::with_helper("/nonexistent/xdg-open");

    let message = invalid(opener.open_path(&PlatformPath::from(PathBuf::from("/etc/hostname"))));

    assert!(
        message.contains("/etc/hostname"),
        "the refusal names the path that could not be opened, got: {message}"
    );
    assert!(
        message.contains("/nonexistent/xdg-open"),
        "the refusal names the helper that was tried, got: {message}"
    );
    assert!(
        message.contains(&std::io::Error::from_raw_os_error(2).to_string()),
        "the refusal carries what the kernel said, got: {message}"
    );
}

/// The backend hands out the opener the app wires in, and claims it.
#[test]
fn the_backend_opens_through_the_trait_object_the_app_wires_in() {
    let recorder = Recorder::new();
    let backend = LinuxBackend::new().with_file_opener(Some(recorder.opener()));
    let file = recorder.sibling("notes.txt");

    assert_eq!(
        backend.capability(Capability::FileOpen),
        CapabilityState::Available,
        "a session with a helper claims file opening"
    );

    let reading = recorder.reading();
    backend
        .file_opener()
        .expect("a session with a helper hands out an opener")
        .open_path(&PlatformPath::from(file.clone()))
        .expect("the recording helper is spawnable");

    assert_eq!(argv(&collect(reading)), vec![file.into_os_string().into_vec()]);
}

/// A session with no helper hands out nothing and claims nothing.
///
/// Kills the bug where the backend offers an opener that can only ever fail: a
/// container with no xdg-utils has no way to open a file, and the launcher has
/// to be able to say so rather than present a row whose action never works.
#[test]
fn a_session_with_no_helper_claims_no_file_opening_at_all() {
    let backend = LinuxBackend::new().with_file_opener(None);

    assert!(
        backend.file_opener().is_none(),
        "a session with no helper must hand out no opener"
    );
    assert_eq!(
        backend.capability(Capability::FileOpen),
        CapabilityState::Unavailable,
        "and it must not claim the capability either"
    );
}

/// A recording program standing where `xdg-open` goes.
///
/// It writes the argv it was handed to a fifo, NUL separated and preceded by a
/// count, because NUL is the one byte an argument cannot contain and the count
/// catches a trailing empty argument that a separator based reading would
/// otherwise lose.
#[derive(Debug)]
struct Recorder {
    scratch: PathBuf,
    program: PathBuf,
    fifo: PathBuf,
}

impl Recorder {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);

        let scratch = std::env::temp_dir().join(format!(
            "crikey-file-open-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // A crashed earlier run must never leak fixtures into this one.
        let _ = fs::remove_dir_all(&scratch);
        fs::create_dir_all(&scratch).expect("scratch directory is creatable");

        let fifo = scratch.join("argv");
        make_fifo(&fifo);

        let program = scratch.join("record-argv");
        // The scratch path is built from a pid and a counter, so it holds
        // nothing the single quotes here would have to escape.
        write_program(
            &program,
            &format!(
                "#!/bin/sh\n\
                 exec > '{fifo}'\n\
                 printf '%s\\0' \"$#\"\n\
                 for argument in \"$@\"; do printf '%s\\0' \"$argument\"; done\n",
                fifo = fifo.display()
            ),
        );

        Self {
            scratch,
            program,
            fifo,
        }
    }

    /// An opener that runs the recording program instead of `xdg-open`.
    fn opener(&self) -> XdgOpener {
        XdgOpener::with_helper(self.program.clone())
    }

    /// A path beside the recording program that nothing created.
    fn sibling(&self, name: &str) -> PathBuf {
        self.scratch.join(name)
    }

    /// The same, for a name with no `str` spelling.
    fn sibling_raw(&self, name: &OsStr) -> PathBuf {
        self.scratch.join(name)
    }

    /// Runs `open` against the recording program and returns the argv it saw.
    ///
    /// The reader is started first, before `open` is called on this thread: an
    /// opener that waited for its child would otherwise deadlock against a fifo
    /// nobody is reading, and a hung run reports nothing.
    fn record(&self, open: impl Fn(&XdgOpener) -> crikey_core::Result<()>) -> Vec<Vec<u8>> {
        let reading = self.reading();
        open(&self.opener()).expect("the recording helper is spawnable");

        argv(&collect(reading))
    }

    /// Drains the fifo on a worker thread.
    ///
    /// Reading a fifo blocks until the writer opens it and returns when the
    /// writer closes it, which is exactly the synchronisation this needs: no
    /// polling, no sleeping, and no assumption about how long a process takes
    /// to start.
    fn reading(&self) -> mpsc::Receiver<Vec<u8>> {
        let fifo = self.fifo.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let recorded = fs::read(&fifo).expect("the recording fifo is readable");
            // A failed send only means this test already gave up.
            let _ = sender.send(recorded);
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
/// Bytes, not `String`: the point of half these tests is that a path with no
/// `str` spelling survives, and decoding the recording lossily would hide
/// exactly the corruption they exist to catch.
fn argv(recorded: &[u8]) -> Vec<Vec<u8>> {
    let mut records: Vec<Vec<u8>> = recorded.split(|byte| *byte == 0).map(<[u8]>::to_vec).collect();

    // Every record is terminated rather than separated, so splitting leaves
    // one empty tail behind.
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

fn invalid(result: crikey_core::Result<()>) -> String {
    match result.expect_err("this open cannot succeed") {
        CoreError::Invalid(message) => message,
        other => panic!("an open failure is reported as Invalid, got {other:?}"),
    }
}

/// Writes an executable script, out of reach of the exec-time text-busy race.
///
/// Staged under another name and renamed into place: this binary is
/// multi-threaded, so a sibling test that forks between this file's `open` and
/// `close` hands the child a writable descriptor, and the kernel then refuses
/// to exec the path with ETXTBSY. The rename gives the opener a name no
/// descriptor in this process has ever pointed at.
fn write_program(path: &Path, body: &str) {
    let staged = path.with_extension("staging");
    fs::write(&staged, body).expect("fixture program is writable");
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).expect("fixture mode is settable");
    fs::rename(&staged, path).expect("fixture program is movable into place");
}

fn make_fifo(path: &Path) {
    let status = Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("mkfifo is available on a Linux host");

    assert!(status.success(), "mkfifo could not create {}", path.display());
}
