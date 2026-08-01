//! `LinuxBackend::window_service()` against a *real* X server (spec 18.2, 18.6).
//!
//! `window_x11.rs` proves `X11WindowService` itself. This file proves the one
//! remaining link: that the accessor the host calls actually hands back that
//! working service under X11, and hands back nothing under a session that
//! cannot carry it. Without this, `WindowEnumeration`/`WindowActivation =>
//! Available` is a claim no test stands behind -- the accessor could cache a
//! failed connection, or return a service that enumerates nothing, and every
//! existing test would still pass.
//!
//! # This file contains exactly ONE `#[test]`, deliberately
//!
//! The accessor reads `$DISPLAY` (via `X11WindowService::connect(None)`), so
//! exercising it means mutating the process environment. `std::env::set_var`
//! is process-global and the test harness runs tests on concurrent threads, so
//! a second test in this binary could observe -- or clobber -- this one's
//! `$DISPLAY` mid-flight. **Do not add a second `#[test]` here.** A new
//! accessor case either joins the single test below in order, or gets its own
//! test binary.
//!
//! # Three ordering hazards, all of which would make this test vacuous
//!
//! * a bare `Xvfb` has no window manager, so `_NET_SUPPORTED` is absent and
//!   `X11WindowService::connect` refuses by design. The EWMH handshake is
//!   therefore written *before* the backend is ever constructed; otherwise the
//!   accessor would answer `None` and a `.is_none()`-shaped test would "pass"
//!   against a dead accessor;
//! * `window_service()` caches in a `OnceLock`, so a call made while `$DISPLAY`
//!   still pointed elsewhere would poison the cell with `None` forever. The
//!   backend is built only after `$DISPLAY` names the prepared server;
//! * the `None` answers for Wayland and Headless are only evidence if they are
//!   not a cached connection failure, so those backends are built fresh -- and
//!   they are checked first while `$DISPLAY` still names the *live* server.
//!   That is the ordering the bug turns on: an accessor that connected before
//!   consulting the session would connect successfully there and answer
//!   `Some`. A dead display alone would not discriminate, because a refused
//!   local connection also produces `None`.
//!
//! `$DISPLAY` is restored to whatever it was, through a guard, so a panic
//! cannot leak it into the rest of the run. The `Xvfb` guard is the one from
//! `window_x11.rs`, copied rather than shared: integration tests are separate
//! binaries and these fixtures are the contract of *this* file.
//!
//! Waits are bounded polling against an explicit deadline, never a fixed sleep
//! used as synchronisation, so a hang becomes a named failure.

#![cfg(target_os = "linux")]

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crikey_platform::{Capability, CapabilityState, WindowHandle, WindowInfo, WindowService};
use crikey_platform_linux::{DesktopEnvironment, LinuxBackend};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, CreateWindowAux, PropMode, Window, WindowClass};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT};

/// Ceiling on an `Xvfb` becoming connectable. Not a performance assertion: it
/// turns a server that never comes up into a named failure rather than a stall.
const SERVER_READY_LIMIT: Duration = Duration::from_secs(20);

/// Gap between polls. Polling, not sleeping-as-synchronisation: the loop ends
/// on its observable -- the display socket -- never on elapsed time.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Ceiling on a non-X11 session answering `None`. It is a *liveness* bound, not
/// a benchmark: the answer must not depend on a display, so it cannot include a
/// connection attempt to the dead display `$DISPLAY` names. A TCP-less local
/// connect to an absent socket fails immediately, so anything near this bound
/// means the session check is not short-circuiting.
const SESSION_ANSWER_LIMIT: Duration = Duration::from_secs(5);

/// A display number nothing listens on, used to prove the non-X11 arms never
/// reach a server.
const DEAD_DISPLAY: &str = ":998";

/// The title the published window carries. Distinctive so that a service
/// enumerating some *other* window, or fabricating a title, cannot match it.
const WINDOW_TITLE: &str = "CriKey backend accessor window \u{2713} 4f1c9a";

/// Hands out a distinct display number per server within this process.
static NEXT_DISPLAY_OFFSET: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

/// A private `Xvfb` instance, killed when the guard is dropped.
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
    /// Starts a server on an unused display number and waits until it accepts
    /// connections.
    ///
    /// Panics -- loudly and by name -- if `Xvfb` is absent or never comes up.
    fn start() -> Self {
        // Derived from the pid so that two concurrently running test binaries
        // never pick the same number, and offset well clear of `:0`.
        let base = 100 + (std::process::id() % 700);
        let offset = NEXT_DISPLAY_OFFSET.fetch_add(1, Ordering::Relaxed);

        let mut last_error = String::new();
        for attempt in 0..16 {
            let number = base + offset * 16 + attempt;
            let display = format!(":{number}");
            let socket = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
            if socket.exists() || PathBuf::from(format!("/tmp/.X{number}-lock")).exists() {
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
                    "this test requires a real X server; spawning `Xvfb {display}` failed: {error}. \
                     A missing Xvfb is a test failure, never a skip."
                ),
            };

            let mut server = Self {
                display,
                socket,
                child,
            };
            match server.wait_until_ready() {
                Ok(()) => return server,
                Err(error) => last_error = error,
            }
            // `server` drops here, killing the failed attempt.
        }

        panic!("no Xvfb came up after 16 display numbers from :{base}; last failure: {last_error}");
    }

    /// Polls until the display socket exists and the server is still alive.
    fn wait_until_ready(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + SERVER_READY_LIMIT;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Err(format!("Xvfb {} exited early with {status}", self.display)),
                Ok(None) => {}
                Err(error) => return Err(format!("Xvfb {} could not be polled: {error}", self.display)),
            }
            if self.socket.exists() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "Xvfb {} did not accept connections within {SERVER_READY_LIMIT:?}",
                    self.display
                ));
            }
            thread::sleep(POLL_INTERVAL);
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

// ---------------------------------------------------------------------------
// The stand-in window manager
// ---------------------------------------------------------------------------

/// A second X client that plays the parts of an EWMH window manager this test
/// needs: it advertises support, owns the window and publishes the client list.
///
/// It must outlive every assertion, because a window belongs to the client that
/// created it: dropping this closes the connection and the server destroys the
/// window with it.
struct Ewmh {
    connection: RustConnection,
    root: Window,
    net_supported: u32,
    net_client_list: u32,
    net_wm_name: u32,
    net_active_window: u32,
    net_supporting_wm_check: u32,
    utf8_string: u32,
}

impl Ewmh {
    /// Connects to `server` and interns the atoms the fixture writes.
    fn connect(server: &XvfbServer) -> Self {
        let (connection, screen) = RustConnection::connect(Some(server.display()))
            .unwrap_or_else(|error| panic!("fixture could not reach {}: {error}", server.display()));
        let root = connection.setup().roots[screen].root;

        let atom = |name: &str| -> u32 {
            connection
                .intern_atom(false, name.as_bytes())
                .unwrap_or_else(|error| panic!("interning {name}: {error}"))
                .reply()
                .unwrap_or_else(|error| panic!("interning {name}: {error}"))
                .atom
        };

        Self {
            net_supported: atom("_NET_SUPPORTED"),
            net_client_list: atom("_NET_CLIENT_LIST"),
            net_wm_name: atom("_NET_WM_NAME"),
            net_active_window: atom("_NET_ACTIVE_WINDOW"),
            net_supporting_wm_check: atom("_NET_SUPPORTING_WM_CHECK"),
            utf8_string: atom("UTF8_STRING"),
            connection,
            root,
        }
    }

    /// Presents the whole EWMH handshake a live manager presents: the hint list
    /// naming the two properties this fixture implements, and the two-sided
    /// `_NET_SUPPORTING_WM_CHECK` that proves the manager is still running.
    fn advertise_ewmh(&self) {
        self.set_words(
            self.root,
            self.net_supported,
            AtomEnum::ATOM,
            &[self.net_client_list, self.net_active_window],
        );
        let check = self.create_window();
        self.set_words(
            self.root,
            self.net_supporting_wm_check,
            AtomEnum::WINDOW,
            &[check],
        );
        self.set_words(check, self.net_supporting_wm_check, AtomEnum::WINDOW, &[check]);
    }

    fn set_words(&self, window: Window, property: u32, kind: AtomEnum, words: &[u32]) {
        self.connection
            .change_property32(PropMode::REPLACE, window, property, kind, words)
            .expect("writing a word-list property")
            .check()
            .expect("writing a word-list property");
        self.flush();
    }

    /// Creates an unmapped `InputOutput` window owned by this client.
    ///
    /// Unmapped is deliberate: `Xvfb` has no manager to map anything, and every
    /// property the backend reads is readable regardless of visibility. What is
    /// under test is the property protocol, not compositing.
    fn create_window(&self) -> Window {
        let window = self.connection.generate_id().expect("generating a window id");
        self.connection
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                window,
                self.root,
                0,
                0,
                100,
                100,
                0,
                WindowClass::INPUT_OUTPUT,
                COPY_FROM_PARENT,
                &CreateWindowAux::new(),
            )
            .expect("creating a fixture window")
            .check()
            .expect("creating a fixture window");
        window
    }

    /// Sets `_NET_WM_NAME` as `UTF8_STRING`, the way a modern toolkit does.
    fn set_net_wm_name(&self, window: Window, title: &str) {
        self.connection
            .change_property8(
                PropMode::REPLACE,
                window,
                self.net_wm_name,
                self.utf8_string,
                title.as_bytes(),
            )
            .expect("writing _NET_WM_NAME")
            .check()
            .expect("writing _NET_WM_NAME");
        self.flush();
    }

    /// Publishes `_NET_CLIENT_LIST` as `WINDOW[]/32`, in this order.
    fn publish_client_list(&self, windows: &[Window]) {
        self.connection
            .change_property32(
                PropMode::REPLACE,
                self.root,
                self.net_client_list,
                AtomEnum::WINDOW,
                windows,
            )
            .expect("writing _NET_CLIENT_LIST")
            .check()
            .expect("writing _NET_CLIENT_LIST");
        self.flush();
    }

    fn flush(&self) {
        self.connection.flush().expect("flushing the fixture connection");
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

/// The info for `window`, or a named failure listing what was enumerated.
fn find(infos: &[WindowInfo], window: Window) -> &WindowInfo {
    let handle = WindowHandle(u64::from(window));
    infos
        .iter()
        .find(|info| info.handle == handle)
        .unwrap_or_else(|| panic!("window {window:#x} is missing from the enumeration: {infos:?}"))
}

/// Enumerates through `service`, failing by name rather than unwrapping bare.
fn enumerate(service: &dyn WindowService, what: &str) -> Vec<WindowInfo> {
    service
        .enumerate()
        .unwrap_or_else(|error| panic!("enumerating through {what} failed: {error}"))
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// The accessor hands out a service that really works, caches without
/// degrading, and withholds itself from a session that cannot carry it.
///
/// Every step is one link in the chain behind `capability(WindowEnumeration)
/// == Available`, and each kills a distinct bug:
///
/// 1. `Some` under X11 -- kills the accessor that never connects, or that
///    connects to nothing and caches the failure;
/// 2. `enumerate` through the accessor returns the published window with the
///    exact title -- kills the service that is `Some` but answers with an empty
///    list, which the host would render as "no windows open"; the exact title
///    kills a service reading the wrong property or the root's own;
/// 3. `activate` on that handle is `Ok` -- kills a `Some` whose activation half
///    is broken, and pins that the handle enumeration produced round-trips
///    through the accessor;
/// 4. a second call still enumerates the same window -- kills the `OnceLock`
///    that caches something other than the working service, or hands out a
///    connection that has since been closed;
/// 5. Wayland and Headless are `None` even while `$DISPLAY` still names the
///    live EWMH server -- the discriminating case, because an accessor that
///    connected *before* consulting the session would succeed there and hand
///    out a `Some`. Repeating it against a dead display then shows the answer
///    does not depend on a reachable one, and fresh backends each time keep a
///    populated `OnceLock` from being what is observed;
/// 6. the accessor and [`LinuxBackend::capability`] agree in every one of
///    those sessions. That agreement is the actual claim under review: a
///    capability reported `Available` with no service behind it is the lie
///    this whole file exists to rule out, and so is a withheld service in a
///    session still advertised as `Available`.
///
/// Ordering is load-bearing throughout; see the module comment.
#[test]
fn the_backend_accessor_hands_out_a_working_window_service_under_x11_only() {
    // 1. A server that speaks EWMH *before* the backend exists, with one
    //    published, distinctively titled window.
    let server = XvfbServer::start();
    let fixture = Ewmh::connect(&server);
    fixture.advertise_ewmh();
    let window = fixture.create_window();
    fixture.set_net_wm_name(window, WINDOW_TITLE);
    fixture.publish_client_list(&[window]);

    // 2. Only now does `$DISPLAY` name it, and only now is the backend built:
    //    a backend constructed earlier would have cached a failed connection.
    let display = DisplayVar::set(server.display());
    let backend = LinuxBackend::with_desktop_environment(DesktopEnvironment::X11);
    let service = backend
        .window_service()
        .expect("an X11 backend on an EWMH server must hand out a window service");

    // 3. Use it. This, not the `Some`, is the point of the test.
    let infos = enumerate(service, "the backend accessor");
    let info = find(&infos, window);
    assert_eq!(
        info.title, WINDOW_TITLE,
        "the service behind the accessor read the wrong title for {window:#x}: {infos:?}"
    );
    service.activate(&info.handle).unwrap_or_else(|error| {
        panic!(
            "activating {:?} through the accessor failed: {error}",
            info.handle
        )
    });

    // 4. The cached second answer is the same working service, not a husk.
    let cached = backend
        .window_service()
        .expect("the cached accessor must keep handing out the service it connected");
    let cached_infos = enumerate(cached, "the cached accessor");
    assert_eq!(
        find(&cached_infos, window).title,
        WINDOW_TITLE,
        "the cached service no longer enumerates {window:#x}: {cached_infos:?}"
    );

    // 5/6. A session that cannot carry window control withholds the service
    //      whatever the display says. The live server first -- that is the
    //      case an accessor connecting ahead of its session check would fail
    //      -- then the dead one, which additionally must not stall.
    // `Partial`, not `Available`: `capability` is a pure function of the
    // detected session and cannot see that this particular display happens to
    // carry an EWMH manager. What it must never do is claim more than the
    // session guarantees, and what it must never do here is fall to
    // `Unavailable` while a working service is being handed out.
    for capability in [Capability::WindowEnumeration, Capability::WindowActivation] {
        assert_eq!(
            backend.capability(capability),
            CapabilityState::Partial,
            "an X11 backend handing out a working service must report {capability:?} Partial: the \
             session supports it, subject to the window-manager gate this display passes"
        );
        assert_ne!(
            backend.capability(capability),
            CapabilityState::Unavailable,
            "{capability:?} must not be denied while the accessor hands out a working service"
        );
    }

    for (display_value, why) in [
        (server.display(), "a live EWMH server"),
        (DEAD_DISPLAY, "a display where nothing listens"),
    ] {
        display.point_at(display_value);
        for desktop in [DesktopEnvironment::Wayland, DesktopEnvironment::Headless] {
            let backend = LinuxBackend::with_desktop_environment(desktop);
            let started = Instant::now();
            let answer = backend.window_service();
            let elapsed = started.elapsed();
            assert!(
                answer.is_none(),
                "{desktop:?} must not hand out a window service, even with DISPLAY at {why}"
            );
            // Not a discriminator for the ordering bug -- a refused local
            // connection fails in microseconds -- only a stall guard, so that
            // an accessor blocking on a display becomes a named failure.
            assert!(
                elapsed < SESSION_ANSWER_LIMIT,
                "{desktop:?} blocked for {elapsed:?} before answering None with DISPLAY at {why}"
            );
            for capability in [Capability::WindowEnumeration, Capability::WindowActivation] {
                assert_ne!(
                    backend.capability(capability),
                    CapabilityState::Available,
                    "{desktop:?} withholds the service, so {capability:?} must not be Available"
                );
            }
        }
    }

    // `display` restores `$DISPLAY`, `fixture` closes, `server` dies.
    drop(display);
}
