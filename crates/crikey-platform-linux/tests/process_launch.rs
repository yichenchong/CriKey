//! Public-API contract for launching a discovered application (spec 18.1).
//!
//! This is the launch half of the M1 "global hotkey + app discovery"
//! deliverable on Linux (roadmap M1): the row a user picks has to start a
//! program, with the argument vector discovery produced reaching that program
//! unchanged.
//!
//! Every case runs a real executable out of a unique temp directory that is
//! removed when the test ends, so runs are order independent and leave nothing
//! behind. The executable reports the argv it was given over a fifo, which is
//! also what synchronises the test with it: a read of a fifo returns exactly
//! when the writer has finished, so nothing here sleeps or guesses how long a
//! process takes.

#![cfg(target_os = "linux")]

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crikey_core::{CoreError, PlatformPath};
use crikey_platform::{Capability, CapabilityState, ProcessLauncher};
use crikey_platform_linux::{CommandLauncher, LinuxBackend};

/// A liveness guard, never a timing assertion: a correct launch reports back
/// in milliseconds and every wait ends with it. The bound only turns a
/// regression that blocks forever into a failure instead of a hung run.
const RESPONSE_LIMIT: Duration = Duration::from_secs(60);

#[test]
fn a_launched_program_receives_its_arguments_with_their_boundaries_intact() {
    let recorder = Recorder::new();
    let launcher = CommandLauncher::new();
    let arguments = [
        "--flag".to_owned(),
        "two words".to_owned(),
        String::new(),
        "quote\"and\\backslash".to_owned(),
        "--tab\there".to_owned(),
        "%f".to_owned(),
        "ünïcøde".to_owned(),
    ];

    let observed = recorder.record(|target| launcher.launch(target, &arguments));

    // One argument in, one argument out: an argument containing spaces must
    // not arrive as two, and an empty argument must not vanish.
    assert_eq!(observed, arguments);
}

#[test]
fn a_program_launched_without_arguments_receives_none() {
    let recorder = Recorder::new();
    let launcher = CommandLauncher::new();

    let observed = recorder.record(|target| launcher.launch(target, &[]));

    assert_eq!(observed, Vec::<String>::new());
}

#[test]
fn a_launch_uses_an_existing_working_directory() {
    let recorder = Recorder::new();
    let working_directory = recorder.sibling("working-directory");
    fs::create_dir(&working_directory).expect("working directory fixture is creatable");
    let fifo = recorder.sibling("working-directory-result");
    make_fifo(&fifo);
    let script = recorder.sibling("record-working-directory");
    fs::write(&script, format!("#!/bin/sh\npwd > '{}'\n", fifo.display()))
        .expect("working-directory probe is writable");
    set_mode(&script, 0o755);

    let reading =
        thread::spawn(move || fs::read_to_string(fifo).expect("working-directory fifo is readable"));
    CommandLauncher::new()
        .launch_in(
            &PlatformPath::from(script),
            &[],
            Some(&PlatformPath::from(working_directory.clone())),
        )
        .expect("launching the working-directory probe succeeds");
    let observed = reading.join().expect("working-directory reader joins");

    assert_eq!(observed.trim_end(), working_directory.display().to_string());
}

#[test]
fn a_missing_working_directory_is_ignored() {
    let recorder = Recorder::new();
    let missing = recorder.sibling("missing-working-directory");
    let observed =
        CommandLauncher::new().launch_in(&recorder.target(), &[], Some(&PlatformPath::from(missing)));

    assert!(
        observed.is_ok(),
        "a stale desktop Path= must not block launch: {observed:?}"
    );
    assert_eq!(recorder.observed(), Vec::<String>::new());
}

#[test]
fn launching_returns_before_the_program_it_started_has_finished() {
    let recorder = Recorder::new();
    let target = recorder.target();
    let (sender, receiver) = mpsc::channel();

    // Nothing is draining the recording fifo yet, so the script cannot get
    // past its first redirection. A launcher that waited for its child would
    // never come back from this call -- so it is made on a thread nothing
    // joins, and the bounded wait below turns that into a failure rather than
    // a hung run.
    thread::spawn(move || {
        // A failed send only means this test already gave up.
        let _ = sender.send(CommandLauncher::new().launch(&target, &[]));
    });

    receiver
        .recv_timeout(RESPONSE_LIMIT)
        .expect("launch must return instead of waiting for the program to exit")
        .expect("launching an executable script succeeds");

    // The child really was started, not merely reported: draining the fifo
    // now releases it and yields what it recorded.
    assert_eq!(recorder.observed(), Vec::<String>::new());
}

#[test]
fn launching_a_target_that_does_not_exist_reports_the_target_and_the_os_error() {
    let recorder = Recorder::new();
    let launcher = CommandLauncher::new();
    let missing = recorder.sibling("no-such-program");

    let message = invalid(launcher.launch(&PlatformPath::from(missing.clone()), &[]));

    assert!(
        message.contains(&missing.display().to_string()),
        "the failure must name the target that could not be launched: {message}"
    );
    assert!(
        message.contains(&os_error(NO_SUCH_FILE)),
        "the failure must keep the operating system's own detail: {message}"
    );
}

#[test]
fn launching_a_file_that_is_not_executable_reports_the_permission_error() {
    let recorder = Recorder::new();
    let launcher = CommandLauncher::new();
    let unrunnable = recorder.sibling("not-executable");
    fs::write(&unrunnable, "#!/bin/sh\n").expect("fixture file is writable");
    set_mode(&unrunnable, 0o644);

    let message = invalid(launcher.launch(&PlatformPath::from(unrunnable.clone()), &[]));

    assert!(
        message.contains(&unrunnable.display().to_string()),
        "the failure must name the target that could not be launched: {message}"
    );
    assert!(
        message.contains(&os_error(PERMISSION_DENIED)),
        "the failure must keep the operating system's own detail: {message}"
    );
}

#[test]
fn the_backend_launches_through_the_trait_object_the_app_wires_in() {
    let recorder = Recorder::new();
    let backend = LinuxBackend::new();
    let launcher: &dyn ProcessLauncher = backend.process_launcher();
    let arguments = ["--from".to_owned(), "the backend".to_owned()];

    let observed = recorder.record(|target| launcher.launch(target, &arguments));

    assert_eq!(observed, arguments);
    assert_eq!(
        backend.capability(Capability::ProcessLaunch),
        CapabilityState::Available
    );
}

#[test]
fn opening_a_uri_is_refused_rather_than_guessed_at() {
    let backend = LinuxBackend::new();
    let uri = "https://example.invalid/page";

    let message = invalid(backend.process_launcher().open_uri(uri));

    assert!(
        message.contains(uri),
        "the refusal must name what was asked for: {message}"
    );
    // No portal client and no handler lookup means no URI opening, and a
    // capability is claimed only once something stands behind it (spec 18.2).
    assert_eq!(
        backend.capability(Capability::UriOpen),
        CapabilityState::Unavailable
    );
}

/// `ENOENT` and `EACCES`, the two failures a launcher actually hits: a target
/// that was uninstalled since discovery, and one the user may not run.
const NO_SUCH_FILE: i32 = 2;
const PERMISSION_DENIED: i32 = 13;

/// An executable that reports the argument vector it was handed.
///
/// The program is a real file with the executable bit set, launched by path
/// like any discovered application, so what is exercised is the same spawn a
/// user's pick goes through. It writes its argv to a fifo, NUL separated and
/// preceded by a count, because NUL is the one byte an argument cannot contain
/// and the count catches a trailing empty argument that a separator based
/// reading would otherwise lose.
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
            "crikey-process-launch-{}-{}",
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
        fs::write(
            &program,
            format!(
                "#!/bin/sh\n\
                 exec > '{fifo}'\n\
                 printf '%s\\0' \"$#\"\n\
                 for argument in \"$@\"; do printf '%s\\0' \"$argument\"; done\n",
                fifo = fifo.display()
            ),
        )
        .expect("recording program is writable");
        set_mode(&program, 0o755);

        Self {
            scratch,
            program,
            fifo,
        }
    }

    /// The recording program, as the launcher takes it.
    fn target(&self) -> PlatformPath {
        PlatformPath::from(self.program.clone())
    }

    /// A path beside the recording program that nothing created.
    fn sibling(&self, name: &str) -> PathBuf {
        self.scratch.join(name)
    }

    /// Runs `launch` against the recording program and returns the argv it saw.
    ///
    /// The reader is started first, before `launch` is called on this thread:
    /// a launcher that waited for its child would otherwise deadlock against a
    /// fifo nobody is reading, and a hung run reports nothing. That the
    /// launcher does not wait is pinned separately, where the wait can fail
    /// instead of hang.
    fn record(&self, launch: impl FnOnce(&PlatformPath) -> crikey_core::Result<()>) -> Vec<String> {
        let reading = self.reading();
        launch(&self.target()).expect("launching an executable script succeeds");

        argv(&collect(reading))
    }

    /// The argv of a program that is already running.
    fn observed(&self) -> Vec<String> {
        argv(&collect(self.reading()))
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
        .expect("the launched program must run and report its argv")
}

/// Decodes `count\0argument\0...` back into the vector the program received.
fn argv(recorded: &[u8]) -> Vec<String> {
    let mut records: Vec<String> = recorded
        .split(|byte| *byte == 0)
        .map(|record| String::from_utf8_lossy(record).into_owned())
        .collect();

    // Every record is terminated rather than separated, so splitting leaves
    // one empty tail behind.
    assert_eq!(
        records.pop().as_deref(),
        Some(""),
        "the recording is NUL terminated"
    );
    let count: usize = records
        .remove(0)
        .parse()
        .expect("the program reports how many arguments it received");
    assert_eq!(
        records.len(),
        count,
        "the program's own count must match the arguments it wrote"
    );

    records
}

fn invalid(result: crikey_core::Result<()>) -> String {
    match result.expect_err("this launch cannot succeed") {
        CoreError::Invalid(message) => message,
        other => panic!("a launch failure is reported as Invalid, got {other:?}"),
    }
}

/// What the operating system says about `code`, spelled the way `std` spells it
/// in the error a failed spawn returns.
fn os_error(code: i32) -> String {
    io::Error::from_raw_os_error(code).to_string()
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("fixture mode is settable");
}

fn make_fifo(path: &Path) {
    let status = Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("mkfifo is available on a Linux host");

    assert!(status.success(), "mkfifo could not create {}", path.display());
}
