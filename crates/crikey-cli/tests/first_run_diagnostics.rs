//! Black-box tests for what a first launch tells its owner (spec 23, 28;
//! acceptance §31.29).
//!
//! Two things a fresh install gets wrong are pinned here.
//!
//! A standard plugin directory that does not exist means nothing is installed,
//! which is what every first launch looks like. It used to be scanned anyway and
//! reported as `modern plugin unavailable (...): cannot scan modern plugin root:
//! No such file or directory`, so a correct install greeted its owner with two
//! errors — while `crikey plugin doctor` called the same profile healthy. A root
//! that exists and cannot be read is a different thing entirely and must still
//! be named. That test drives the real binary against a real X server, because
//! the diagnostic is printed after the launcher's window exists: a headless
//! `crikey run` ends at the renderer, long before any provider loads, and would
//! pass without ever reaching the code it is about.
//!
//! A fatal startup failure from the desktop entry point used to leave no trace
//! at all. `crikey-launcher` is GUI-subsystem on Windows and its stderr is
//! discarded there, so the owner saw a process that vanished. The durable half
//! of the answer — the per-user `startup.log` — is platform-independent and is
//! tested here; the Windows dialog that names it cannot be run on this host.

// Linux only, and stated rather than silently skipped: the harness needs an
// `Xvfb` display and unix permission bits to make a root unreadable. The
// behaviour under test is platform-independent, so pinning it on one platform
// pins it everywhere; the Windows equivalent of an unreadable directory is not
// something this host can produce.
#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The binary under test, as built by cargo for this integration target.
const CRIKEY: &str = env!("CARGO_BIN_EXE_crikey");

/// The graphical entry point, which is the one that must not fail silently.
const CRIKEY_LAUNCHER: &str = env!("CARGO_BIN_EXE_crikey-launcher");

/// How long a launch is given to reach the provider stage. Generous because a
/// debug-built launcher on a loaded machine is slow, and bounded because the
/// launcher never exits on its own: the deadline is a failure, not a schedule.
const STARTUP_LIMIT: Duration = Duration::from_secs(60);

/// How long `Xvfb` is given to accept connections.
const SERVER_READY_LIMIT: Duration = Duration::from_secs(15);

/// Gap between polls. Polling, not sleeping-as-synchronisation: every loop below
/// ends on an observable — the display socket, or a line of output.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Hands out a distinct display number per server within this process.
static NEXT_DISPLAY_OFFSET: AtomicU32 = AtomicU32::new(0);

/// A private `Xvfb` instance, killed when the guard is dropped.
///
/// Dropping is what makes the test full-suite safe: a panicking test still
/// unwinds through this, so no orphaned server outlives the run.
struct XvfbServer {
    display: String,
    child: Child,
}

impl XvfbServer {
    /// Starts a server on an unused display number and waits until its socket
    /// exists.
    ///
    /// Panics — loudly and by name — if `Xvfb` is absent. This test cannot
    /// observe what it is about without a display, so a missing server is a
    /// failure, never a skip.
    fn start() -> Self {
        let base = 100 + (std::process::id() % 700);
        let offset = NEXT_DISPLAY_OFFSET.fetch_add(1, Ordering::Relaxed);
        let mut last_error = String::new();
        for attempt in 0..16 {
            let number = base + offset * 16 + attempt;
            let display = format!(":{number}");
            let socket = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
            if socket.exists() || Path::new(&format!("/tmp/.X{number}-lock")).exists() {
                continue;
            }
            let child = match Command::new("Xvfb")
                .arg(&display)
                .args(["-screen", "0", "640x480x24", "-nolisten", "tcp"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(error) => panic!(
                    "this test requires a real X server; spawning `Xvfb {display}` failed: \
                     {error}. A missing Xvfb is a test failure, never a skip."
                ),
            };
            let mut server = Self { display, child };
            match server.wait_until_ready(&socket) {
                Ok(()) => return server,
                Err(error) => last_error = error,
            }
            // `server` drops here, killing the failed attempt.
        }
        panic!("no Xvfb came up after 16 display numbers from :{base}; last failure: {last_error}");
    }

    fn wait_until_ready(&mut self, socket: &Path) -> Result<(), String> {
        let deadline = Instant::now() + SERVER_READY_LIMIT;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Err(format!("Xvfb {} exited early with {status}", self.display)),
                Ok(None) => {}
                Err(error) => return Err(format!("Xvfb {} could not be polled: {error}", self.display)),
            }
            if socket.exists() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "Xvfb {} never created {}",
                    self.display,
                    socket.display()
                ));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for XvfbServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A private profile removed when the test that made it ends.
struct Profile {
    path: PathBuf,
}

impl Profile {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("crikey-first-run-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");
        Self { path }
    }

    /// Where `crikey plugin install` would put plugins of `kind`, which is the
    /// root the launcher scans without being told to.
    fn installed_root(&self, kind: &str) -> PathBuf {
        self.path.join("data").join("plugins").join(kind)
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        // The test makes one directory unreadable; without restoring the mode
        // the tree cannot be walked and the profile would outlive the run.
        let unreadable = self.installed_root("native");
        if unreadable.exists() {
            let _ = fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700));
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A launcher started for its startup diagnostics, killed when dropped.
struct Launch {
    child: Child,
    stderr: Arc<Mutex<Vec<String>>>,
}

impl Launch {
    fn start(profile: &Profile, display: &str) -> Self {
        let mut command = Command::new(CRIKEY);
        command.arg("run");
        command.env("DISPLAY", display);
        command.env_remove("WAYLAND_DISPLAY");
        command.env("CRIKEY_CONFIG_DIR", profile.path.join("config"));
        command.env("CRIKEY_DATA_DIR", profile.path.join("data"));
        command.env("CRIKEY_CACHE_DIR", profile.path.join("cache"));
        command.env("CRIKEY_STATE_DIR", profile.path.join("state"));
        command.env("CRIKEY_LEGACY_CACHE_ROOT", profile.path.join("legacy-cache"));
        // Absent, so the only roots in play are the standard installed ones this
        // test controls.
        command.env_remove("CRIKEY_LEGACY_PACKAGE_ROOTS");
        command.env_remove("CRIKEY_MODERN_PLUGIN_ROOTS");
        command.env_remove("CRIKEY_NATIVE_PLUGIN_ROOTS");
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::piped());
        let mut child = command.spawn().expect("the crikey binary runs");

        // Drained on a thread: the launcher never exits by itself, so nothing
        // here may wait for end-of-file, and a full stderr pipe would block the
        // launcher before it printed what the test is waiting for.
        let handle = child.stderr.take().expect("stderr was piped");
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let collected = Arc::clone(&stderr);
        std::thread::spawn(move || {
            for line in BufReader::new(handle).lines().map_while(Result::ok) {
                collected
                    .lock()
                    .expect("the stderr log is not poisoned")
                    .push(line);
            }
        });
        Self { child, stderr }
    }

    fn lines(&self) -> Vec<String> {
        self.stderr
            .lock()
            .expect("the stderr log is not poisoned")
            .clone()
    }

    /// Waits until some stderr line contains `marker`, and returns everything
    /// printed up to that point.
    fn wait_for(&mut self, marker: &str) -> Vec<String> {
        let deadline = Instant::now() + STARTUP_LIMIT;
        loop {
            let lines = self.lines();
            if lines.iter().any(|line| line.contains(marker)) {
                return lines;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "the launcher exited with {status} before printing `{marker}`; stderr:\n{}",
                    lines.join("\n")
                );
            }
            assert!(
                Instant::now() < deadline,
                "the launcher never printed `{marker}` within {STARTUP_LIMIT:?}; stderr:\n{}",
                lines.join("\n")
            );
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for Launch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A plugin root that was never created is silence; one that exists and cannot
/// be read is still named.
///
/// Both halves are asserted from a single launch, and the ordering is what makes
/// the silent half real rather than vacuous: the launcher loads the modern
/// provider and prints its unavailable entries strictly before it does the same
/// for the native one. So the native permission diagnostic arriving proves the
/// modern stage has already been and gone, and the absence of a modern
/// diagnostic at that moment is a decision the launcher made, not a message that
/// had yet to be printed.
#[test]
fn a_missing_plugin_root_is_silent_and_an_unreadable_one_is_still_reported() {
    let server = XvfbServer::start();
    let profile = Profile::new("roots");

    // Modern: never created, the state of every fresh install.
    let modern = profile.installed_root("modern");
    assert!(
        !modern.exists(),
        "the fixture is wrong: {} must not exist",
        modern.display()
    );
    // Native: created and stripped of every permission, so `read_dir` fails for
    // a reason that is genuinely the operator's problem.
    let native = profile.installed_root("native");
    fs::create_dir_all(&native).expect("the native root is creatable");
    fs::set_permissions(&native, fs::Permissions::from_mode(0o000))
        .expect("the native root's mode is settable");

    let mut launch = Launch::start(&profile, &server.display);
    let lines = launch.wait_for("native plugin unavailable");
    let printed = lines.join("\n");

    assert!(
        lines
            .iter()
            .any(|line| line.contains("native plugin unavailable")
                && line.contains(&native.display().to_string())),
        "the unreadable root must be named in the diagnostic; stderr:\n{printed}"
    );
    assert!(
        !lines
            .iter()
            .any(|line| line.contains("modern plugin unavailable")),
        "a plugin root that was never created means no plugins are installed, which is not a \
         failure and must not be reported; stderr:\n{printed}"
    );
    assert!(
        !modern.exists(),
        "scanning must not create the directory it scans; {} appeared",
        modern.display()
    );
}

/// Runs one binary to completion on `profile` with a startup failure it cannot
/// get past, whichever entry point takes it.
///
/// The failure is a catalog cache root that cannot be created, not a missing
/// display. Removing `DISPLAY` is fatal on Linux and meaningless on Windows
/// and macOS, where the launcher would open its window, stay resident, and
/// leave `output()` below waiting for a process that is working correctly.
/// The cache root is resolved before any window exists and fails on every host
/// for one reason: the path is an ordinary file, so no directory can be made
/// there.
fn fail_to_start(binary: &str, args: &[&str], profile: &Profile) -> (Option<i32>, String) {
    let mut command = Command::new(binary);
    command.args(args);
    command.env_remove("DISPLAY");
    command.env_remove("WAYLAND_DISPLAY");
    command.env_remove("WAYLAND_SOCKET");
    command.env("CRIKEY_CONFIG_DIR", profile.path.join("config"));
    command.env("CRIKEY_DATA_DIR", profile.path.join("data"));
    command.env("CRIKEY_CACHE_DIR", profile.path.join("cache"));
    command.env("CRIKEY_STATE_DIR", profile.path.join("state"));
    command.env("CRIKEY_LEGACY_CACHE_ROOT", profile.path.join("legacy-cache"));
    let blocked = profile.path.join("catalog-root-is-a-file");
    fs::write(&blocked, b"not a directory").expect("the blocking file is writable");
    command.env("CRIKEY_CATALOG_CACHE_ROOT", &blocked);
    command.env_remove("CRIKEY_LEGACY_PACKAGE_ROOTS");
    command.env_remove("CRIKEY_MODERN_PLUGIN_ROOTS");
    command.env_remove("CRIKEY_NATIVE_PLUGIN_ROOTS");
    let output = command.output().expect("the binary runs");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A desktop launch that cannot start records why in the per-user state
/// directory; the console launch keeps writing to stderr and leaves no file.
///
/// The evidence has to survive the process, because on Windows the stderr both
/// entry points write to is discarded for the graphical one: without the file
/// there is nothing anywhere on the machine to explain why a double-clicked
/// shortcut did nothing. The console half of the assertion is what keeps the
/// fix from turning `crikey run` into a command that litters: a terminal
/// already shows the reason, so nothing is written for it.
///
/// Two consecutive desktop failures are run because appending is the point. A
/// launcher that fails every time is diagnosed from the sequence, and a log
/// truncated on each launch would hold only the last attempt.
#[test]
fn a_fatal_desktop_launch_records_its_reason_where_a_console_launch_need_not() {
    let desktop = Profile::new("startup-log-desktop");
    let log = desktop.path.join("state").join("startup.log");

    let (first_code, first_stderr) = fail_to_start(CRIKEY_LAUNCHER, &[], &desktop);
    assert_eq!(
        first_code,
        Some(70),
        "a launch that could not start must report EX_SOFTWARE; stderr:\n{first_stderr}"
    );
    let after_one = fs::read_to_string(&log).unwrap_or_else(|error| {
        panic!(
            "the desktop entry point must record why it could not start in {}: {error}; \
             stderr:\n{first_stderr}",
            log.display()
        )
    });
    assert!(
        after_one.contains("launcher failed"),
        "the log must carry the diagnostic, not merely exist; got:\n{after_one}"
    );
    assert_eq!(
        after_one.lines().count(),
        1,
        "one failed launch is one entry; got:\n{after_one}"
    );

    let (second_code, second_stderr) = fail_to_start(CRIKEY_LAUNCHER, &[], &desktop);
    assert_eq!(second_code, Some(70), "stderr:\n{second_stderr}");
    let after_two = fs::read_to_string(&log).expect("the log is still readable");
    assert_eq!(
        after_two.lines().count(),
        2,
        "the log is appended to, so a repeated failure keeps its history; got:\n{after_two}"
    );

    let console = Profile::new("startup-log-console");
    let (console_code, console_stderr) = fail_to_start(CRIKEY, &["run"], &console);
    assert_eq!(
        console_code,
        Some(70),
        "the console path's exit code is unchanged; stderr:\n{console_stderr}"
    );
    assert!(
        console_stderr.contains("crikey: launcher failed:"),
        "the console path still says why on stderr; got:\n{console_stderr}"
    );
    let console_log = console.path.join("state").join("startup.log");
    assert!(
        !console_log.exists(),
        "a terminal already shows the reason, so `crikey run` must leave no log behind; {} exists",
        console_log.display()
    );
}
