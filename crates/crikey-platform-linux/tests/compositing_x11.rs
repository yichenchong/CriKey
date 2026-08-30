//! Compositing detection against a *real* X server (spec 18.2).
//!
//! `Capability::Compositing` decides whether the launcher window carries an
//! alpha channel into its corners, so a wrong answer is visible: a claimed
//! compositor that is not there leaves solid black notches cut out of the
//! rounded corners on the user's screen. The answer comes from the owner of the
//! `_NET_WM_CM_S<screen>` selection, and nothing but a live X server can
//! produce a selection owner -- an in-process double could not fail an
//! `InternAtom`, could not hold an ownership, and could not lose one.
//!
//! A missing or unusable `Xvfb` is a **test failure**, never a skip: there is no
//! `#[ignore]` and no early return here. A skipped test is not evidence. The
//! `XvfbServer` guard follows `clipboard_x11.rs` and `window_x11.rs`, which
//! established the pattern, and is copied rather than shared because integration
//! tests are separate binaries.
//!
//! # Why the test plays the compositing manager
//!
//! A bare `Xvfb` has no compositing manager, so it is the negative case for
//! free -- and that is exactly the desktop this capability exists to detect.
//! The positive case needs no compositor either: `_NET_WM_CM_S0` is an ordinary
//! selection, and "a compositing manager" is, to every client that asks, a
//! client holding it. A fixture connection creating a window and taking the
//! ownership is therefore not a stand-in for the real thing; it is the real
//! protocol state a real compositor puts the server into.
//!
//! # This file contains exactly ONE `#[test]`, deliberately
//!
//! `LinuxBackend::capability` probes `$DISPLAY`, so exercising it means mutating
//! the process environment. `std::env::set_var` is process-global and the
//! harness runs tests on concurrent threads, so a second test in this binary
//! could observe -- or clobber -- this one's `$DISPLAY` mid-flight. **Do not add
//! a second `#[test]` here.** A new case joins the single test below, in order.
//!
//! The order is the evidence. Ownership is taken *and then released* against
//! one unchanged display, because a probe that cached its first answer, or that
//! read whether the atom exists rather than who owns it, passes every check that
//! only ever goes one way.
//!
//! Waits are bounded against an explicit deadline, never a fixed sleep used as
//! synchronisation, so a hang becomes a named failure.

#![cfg(target_os = "linux")]

use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crikey_platform::{Capability, CapabilityState};
use crikey_platform_linux::{compositor_is_running, DesktopEnvironment, LinuxBackend};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, ConnectionExt, CreateWindowAux, Time, Window, WindowClass};
use x11rb::rust_connection::RustConnection;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, NONE};

/// Ceiling on an `Xvfb` becoming connectable. Not a performance assertion: it
/// turns a server that never comes up into a named failure rather than a stall.
const SERVER_READY_LIMIT: Duration = Duration::from_secs(20);

/// Gap between connection attempts while waiting for that server. Polling, not
/// sleeping-as-synchronisation: the loop below ends on its observable -- a
/// connection the server accepted -- never on elapsed time.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A display number no server in this suite can be listening on, used to prove
/// that an unreachable display answers rather than panics. Far above the range
/// `Xvfb` hands out through `-displayfd`, and unreachable for a second reason:
/// nothing ever creates `/tmp/.X11-unix/X4242`.
const DEAD_DISPLAY: &str = ":4242";

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

/// A private `Xvfb`, killed when the guard is dropped.
///
/// Dropping is what makes the test full-suite safe: a panicking test still
/// unwinds through this, so no orphaned server outlives the run and no display
/// number leaks to the next test.
struct XvfbServer {
    display: String,
    socket: PathBuf,
    child: Child,
}

impl XvfbServer {
    /// Starts a private server and waits until it accepts connections.
    ///
    /// The number is chosen by `Xvfb` itself and reported through
    /// `-displayfd`, never picked here. Picking one and then spawning is a
    /// check-then-act race that two concurrently running test binaries really
    /// do lose: both see the same number free, the loser then finds the
    /// winner's socket where it expected its own, concludes its server is up,
    /// and talks to a display it does not own. `Xvfb` binds or moves on
    /// internally, so asking it is the only atomic way to get a number.
    ///
    /// The write to that descriptor happens once the server is listening, so
    /// reading it is also the readiness check: there is nothing left to poll.
    ///
    /// Panics -- loudly and by name -- if `Xvfb` is absent or never comes up.
    fn start() -> Self {
        let mut child = match Command::new("Xvfb")
            .args(["-displayfd", "1"])
            .args(["-screen", "0", "640x480x24", "-nolisten", "tcp"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => panic!(
                "this test requires a real X server; spawning `Xvfb` failed: {error}. \
                 A missing Xvfb is a test failure, never a skip."
            ),
        };

        let number = Self::reported_display(&mut child);
        let server = Self {
            display: format!(":{number}"),
            socket: PathBuf::from(format!("/tmp/.X11-unix/X{number}")),
            child,
        };
        server.await_connectable();
        server
    }

    /// Blocks until a client connection succeeds, bounded by
    /// [`SERVER_READY_LIMIT`].
    ///
    /// `-displayfd` reports the number as soon as the socket is bound, which is
    /// a moment before the server finishes accepting clients on it, and this
    /// test's first assertion is a *negative* one: a "no compositor here"
    /// derived from a connection the server was not yet answering would be a
    /// pass with no evidence behind it.
    fn await_connectable(&self) {
        drop(connect_within(&self.display));
    }

    /// The display this server owns, for a client that needs its own
    /// connection.
    ///
    /// Every client goes through the same bounded retry rather than connecting
    /// once, because one success does not make the next one certain: a server
    /// still finishing its startup accepts a connection and then closes it
    /// during the X11 handshake, and a fixture that took that for a dead
    /// display failed this test roughly one run in three. What the test needs
    /// is not "a connection worked once" but "connections work", and the only
    /// honest way to have the second is to retry the observable.
    fn client(&self) -> (RustConnection, usize) {
        connect_within(&self.display)
    }

    /// The display number the server reports, bounded by
    /// [`SERVER_READY_LIMIT`].
    ///
    /// Read on another thread because the read blocks: a server that starts and
    /// then never reports would otherwise wedge the test rather than fail it.
    fn reported_display(child: &mut Child) -> u32 {
        let descriptor = child
            .stdout
            .take()
            .expect("`-displayfd 1` was asked for, so stdout is a pipe");
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut line = String::new();
            let outcome = BufReader::new(descriptor).read_line(&mut line).map(|_| line);
            let _ = sender.send(outcome);
        });

        match receiver.recv_timeout(SERVER_READY_LIMIT) {
            Ok(Ok(line)) => line.trim().parse().unwrap_or_else(|error| {
                panic!("Xvfb reported {line:?} as its display, which is not a number: {error}")
            }),
            Ok(Err(error)) => panic!("Xvfb's display descriptor could not be read: {error}"),
            Err(_) => {
                // The child is killed here rather than left for the guard: this
                // path has no `XvfbServer` to drop yet.
                let _ = child.kill();
                panic!("Xvfb did not report a display within {SERVER_READY_LIMIT:?}")
            }
        }
    }

    /// The `:N` string a client connects with.
    fn display(&self) -> &str {
        &self.display
    }
}

impl Drop for XvfbServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Xvfb normally removes these itself; a killed one may not.
        let _ = fs::remove_file(&self.socket);
        if let Some(number) = self.display.strip_prefix(':') {
            let _ = fs::remove_file(format!("/tmp/.X{number}-lock"));
        }
    }
}

/// Connects to `display`, retrying until [`SERVER_READY_LIMIT`] runs out.
///
/// Bounded on an observable rather than a sleep, like everything else in this
/// suite: what is being waited for is a connection this server actually
/// answers, and the wait ends the moment there is one.
///
/// The retry exists because a freshly started Xvfb will accept a connection
/// and then close it partway through the X11 handshake, which x11rb reports as
/// a reset or a short read. That is a server still starting, not a server that
/// is not there, and a fixture that could not tell the two apart failed about
/// one run in three on a loaded machine.
fn connect_within(display: &str) -> (RustConnection, usize) {
    let deadline = Instant::now() + SERVER_READY_LIMIT;
    loop {
        match RustConnection::connect(Some(display)) {
            Ok(connected) => return connected,
            Err(error) if Instant::now() >= deadline => panic!(
                "Xvfb reported {display} but never answered a connection within \
                 {SERVER_READY_LIMIT:?}: {error}"
            ),
            Err(_) => thread::sleep(POLL_INTERVAL),
        }
    }
}

// ---------------------------------------------------------------------------
// The compositing manager
// ---------------------------------------------------------------------------

/// An independent X client that announces itself the way a compositing manager
/// does: by owning `_NET_WM_CM_S<screen>` on the screen it composites.
///
/// Held for the length of the test, because the ownership is: the server drops
/// a selection when the owning connection closes, which is the property that
/// makes this answer un-stale and would make a fixture that connected, claimed
/// and disconnected prove nothing at all.
struct CompositingManager {
    connection: RustConnection,
    window: Window,
    selection: Atom,
}

impl CompositingManager {
    /// Connects to `server` and creates the window an ownership hangs off.
    ///
    /// The selection name carries the screen number of *this* connection, which
    /// is the same one the probe derives from its own: a manager that announced
    /// itself on screen 0 while the probe asked about screen 1 would be a
    /// fixture that tested nothing.
    fn connect(server: &XvfbServer) -> Self {
        let (connection, screen) = server.client();
        let root = connection.setup().roots[screen].root;
        let window = connection.generate_id().expect("generating a fixture window id");
        connection
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                COPY_FROM_PARENT,
                &CreateWindowAux::new(),
            )
            .expect("creating the fixture window")
            .check()
            .expect("creating the fixture window");

        let name = format!("_NET_WM_CM_S{screen}");
        let selection = connection
            .intern_atom(false, name.as_bytes())
            .unwrap_or_else(|error| panic!("interning {name}: {error}"))
            .reply()
            .unwrap_or_else(|error| panic!("interning {name}: {error}"))
            .atom;

        Self {
            connection,
            window,
            selection,
        }
    }

    /// Takes the ownership, and does not return until the server confirms it.
    ///
    /// The confirming round trip is not politeness: the probe asks over a
    /// *separate* connection, and X orders requests per connection only. Without
    /// it, a probe that answered correctly could still be racing the claim.
    fn claim(&self) {
        self.set_owner(self.window);
    }

    /// Releases the ownership the way a compositing manager shutting down does,
    /// and waits for the server to agree that nobody holds the selection.
    fn release(&self) {
        self.set_owner(NONE);
    }

    fn set_owner(&self, owner: Window) {
        self.connection
            .set_selection_owner(owner, self.selection, Time::CURRENT_TIME)
            .expect("setting the compositing selection owner")
            .check()
            .expect("setting the compositing selection owner");

        let settled = self
            .connection
            .get_selection_owner(self.selection)
            .expect("reading back the compositing selection owner")
            .reply()
            .expect("reading back the compositing selection owner")
            .owner;
        assert_eq!(
            settled, owner,
            "the fixture's own connection must see the ownership it just set before the probe is asked"
        );
    }
}

// ---------------------------------------------------------------------------
// The environment
// ---------------------------------------------------------------------------

/// Sets `$DISPLAY` and puts back whatever was there when dropped.
///
/// A guard rather than a pair of calls because the restoration has to survive a
/// panicking assertion: the rest of the run -- and anything the developer does
/// in this shell afterwards -- must not inherit a `$DISPLAY` naming a server
/// this test already killed.
struct DisplayVar {
    previous: Option<String>,
}

impl DisplayVar {
    fn set(value: &str) -> Self {
        let previous = env::var("DISPLAY").ok();
        env::set_var("DISPLAY", value);
        Self { previous }
    }

    /// Repoints the same guard, keeping the *original* value to restore.
    fn point_at(&self, value: &str) {
        env::set_var("DISPLAY", value);
    }
}

impl Drop for DisplayVar {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => env::set_var("DISPLAY", value),
            None => env::remove_var("DISPLAY"),
        }
    }
}

/// What the backend reports for compositing in `environment`, built fresh so
/// that no answer can come from a cell an earlier step filled.
fn reported(environment: DesktopEnvironment) -> CapabilityState {
    LinuxBackend::with_desktop_environment(environment).capability(Capability::Compositing)
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// Compositing is reported from the live owner of `_NET_WM_CM_S<screen>`.
///
/// The steps run in this order because each kills a distinct bug that the
/// others leave alive:
///
/// 1. a bare `Xvfb` -- a real display with no compositing manager, which is the
///    desktop this capability exists to detect -- answers "no compositor",
///    directly and through `capability`. Kills the probe that reports
///    `Available` for any display it can reach, which is the failure that puts
///    black notches on a non-composited desktop;
/// 2. a client takes the ownership and the same display now answers
///    `Available`. Kills the probe wired to a constant `false`, and pins that a
///    real window id in the owner field is read as a compositor;
/// 3. `Headless` still answers `Unavailable` while that ownership is held.
///    Kills the arm that probes whatever `$DISPLAY` happens to name regardless
///    of the session the backend was built for -- a check that a dead display
///    could not discriminate, because an unreachable one answers `Unavailable`
///    too;
/// 4. the owner is released and the answer goes back to `Unavailable`. Kills
///    two bugs at once: a cached answer, which a user who quits their
///    compositor would keep being given, and a probe that reads whether the
///    atom exists rather than who owns it -- `InternAtom` leaves the name
///    behind forever, so after step 2 that bug answers `Available` here;
/// 5. an unreachable display answers `Unavailable` rather than panicking or
///    hanging, which is what a capability query owes its caller.
#[test]
fn compositing_is_read_from_the_live_selection_owner() {
    let server = XvfbServer::start();

    // 1. A real display, no compositing manager.
    assert!(
        !compositor_is_running(Some(server.display())),
        "a bare Xvfb has no compositing manager, so nothing owns _NET_WM_CM_S0 on it"
    );
    let display = DisplayVar::set(server.display());
    assert_eq!(
        reported(DesktopEnvironment::X11),
        CapabilityState::Unavailable,
        "an X11 session whose display has no compositing manager must not be told transparency works"
    );

    // 2. A compositing manager announces itself on that same display.
    let manager = CompositingManager::connect(&server);
    manager.claim();
    assert!(
        compositor_is_running(Some(server.display())),
        "a client owning _NET_WM_CM_S0 is what a compositing manager is, to every client that asks"
    );
    assert_eq!(
        reported(DesktopEnvironment::X11),
        CapabilityState::Available,
        "with a compositing manager on the display, transparency really does composite"
    );

    // 3. The session gate still wins over a live, composited display.
    assert_eq!(
        reported(DesktopEnvironment::Headless),
        CapabilityState::Unavailable,
        "a session with no display server composites nothing, whatever $DISPLAY happens to name"
    );

    // 4. The manager quits.
    manager.release();
    assert!(
        !compositor_is_running(Some(server.display())),
        "the selection is unowned again, so the answer must go back with it rather than be remembered"
    );
    assert_eq!(
        reported(DesktopEnvironment::X11),
        CapabilityState::Unavailable,
        "a user who stops their compositor must stop being promised transparency"
    );

    // 5. A display nothing answers on.
    assert!(
        !compositor_is_running(Some(DEAD_DISPLAY)),
        "an unreachable display is not evidence of a compositor, and asking about one must not panic"
    );
    display.point_at(DEAD_DISPLAY);
    assert_eq!(
        reported(DesktopEnvironment::X11),
        CapabilityState::Unavailable,
        "a capability query that cannot reach the display answers the safe shape instead of failing"
    );
}
