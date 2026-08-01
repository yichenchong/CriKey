//! The X11 window backend driven against a *real* X server (spec 18.1, 18.6).
//!
//! # Why a real server
//!
//! Every claim `X11WindowService` makes is a claim about bytes on an X
//! connection: that `_NET_CLIENT_LIST` is read from the root window with the
//! type EWMH declares, that a title comes from `_NET_WM_NAME` and falls back to
//! `WM_NAME`, and that `activate` puts a `_NET_ACTIVE_WINDOW` `ClientMessage`
//! on the wire addressed to the root with the mask a window manager selects. An
//! in-process double could not fail any of those, so these tests start their own
//! `Xvfb` and inspect the wire.
//!
//! A missing or unusable `Xvfb` is a **test failure**, never a skip: there is no
//! `#[ignore]` and no early return here. A skipped test is not evidence. The
//! `XvfbServer` guard follows `hotkeys_x11.rs`, which established the pattern.
//!
//! # Why the tests act as the window manager
//!
//! A bare `Xvfb` has no window manager, so nothing writes `_NET_SUPPORTED`,
//! nothing writes `_NET_CLIENT_LIST`, and nothing consumes a
//! `_NET_ACTIVE_WINDOW` message. Those properties are *just properties* and that
//! message is *just an event*, so a test can play the manager's part with
//! [`Ewmh`]: it advertises `_NET_SUPPORTED`, creates real windows, publishes the
//! client list, and selects the substructure bits on the root that a manager
//! selects. What the service does with all of that is then observed rather than
//! assumed.
//!
//! Two things genuinely cannot be proven this way, and are not claimed anywhere:
//!
//! * that focus actually *moves*. `_NET_ACTIVE_WINDOW` is a request to the
//!   window manager, and focus policy is the manager's. With no manager running
//!   there is nobody to honour it, which is exactly why `WindowService::activate`
//!   promises delivery rather than focus. The tests below prove delivery, to the
//!   right window, with the right mask;
//! * that a *real* manager's `_NET_CLIENT_LIST` contains what a user would call
//!   their windows. That is the manager's editorial judgement, not this
//!   backend's: the backend's whole contract is to report the list verbatim, and
//!   that is what is tested.
//!
//! # Time
//!
//! Waits are bounded polling against an explicit deadline, never a fixed sleep
//! used as synchronisation: the loops end on an observable -- the display
//! socket, or an event arriving -- so a regression becomes a named failure
//! instead of a stall or a flake.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crikey_core::CoreError;
use crikey_platform::{WindowHandle, WindowInfo, WindowService};
use crikey_platform_linux::X11WindowService;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt, CreateWindowAux, EventMask,
    PropMode, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT};

/// Ceiling on an `Xvfb` becoming connectable. Not a performance assertion: it
/// turns a server that never comes up into a named failure rather than a stall.
const SERVER_READY_LIMIT: Duration = Duration::from_secs(20);

/// Ceiling on a sent event reaching a client that selected it. Generous, because
/// it is not measuring anything: it only bounds the failure.
const EVENT_LIMIT: Duration = Duration::from_secs(10);

/// Gap between polls. Polling, not sleeping-as-synchronisation: every loop here
/// ends on its observable, never on elapsed time.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Hands out a distinct display number per server within this process.
static NEXT_DISPLAY_OFFSET: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

/// A private `Xvfb` instance, killed when the guard is dropped.
///
/// Dropping is what makes the tests full-suite safe: a panicking test still
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
                    "these tests require a real X server; spawning `Xvfb {display}` failed: {error}. \
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

    /// A service connected to this server, or a named failure.
    fn service(&self) -> X11WindowService {
        X11WindowService::connect(Some(self.display()))
            .unwrap_or_else(|error| panic!("connecting to {} failed: {error}", self.display))
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

/// A second X client that plays the parts of an EWMH window manager the tests
/// need: it advertises support, owns the windows, publishes the client list and
/// watches the root window.
///
/// It must outlive each assertion, because a window belongs to the client that
/// created it: dropping this closes the connection and the server destroys every
/// window with it.
struct Ewmh {
    connection: RustConnection,
    display: String,
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
            display: server.display().to_owned(),
            connection,
            root,
        }
    }

    /// The X server's current clock reading.
    ///
    /// A client cannot ask X what time it is; the documented idiom is to append
    /// zero bytes to a property of a window one watches and read the timestamp
    /// off the `PropertyNotify` that causes. It runs on a *separate* connection
    /// on purpose: draining events on the fixture's own connection would eat the
    /// `_NET_ACTIVE_WINDOW` message a test is about to wait for.
    fn server_time(&self) -> u32 {
        let (connection, screen) = RustConnection::connect(Some(&self.display))
            .unwrap_or_else(|error| panic!("time probe could not reach {}: {error}", self.display));
        let root = connection.setup().roots[screen].root;
        let window = connection.generate_id().expect("generating a probe window id");
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
                WindowClass::INPUT_ONLY,
                COPY_FROM_PARENT,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .expect("creating a probe window")
            .check()
            .expect("creating a probe window");
        connection
            .change_property8(
                PropMode::APPEND,
                window,
                u32::from(AtomEnum::WM_NAME),
                u32::from(AtomEnum::STRING),
                &[],
            )
            .expect("appending nothing to the probe property")
            .check()
            .expect("appending nothing to the probe property");
        connection.flush().expect("flushing the time probe");

        let deadline = Instant::now() + EVENT_LIMIT;
        loop {
            if let Event::PropertyNotify(notify) =
                connection.wait_for_event().expect("waiting for the probe reply")
            {
                if notify.window == window {
                    return notify.time;
                }
            }
            assert!(
                Instant::now() < deadline,
                "the X server never answered the timestamp probe within {EVENT_LIMIT:?}"
            );
        }
    }

    /// Presents everything a live EWMH manager presents: the hint list, and the
    /// two-sided `_NET_SUPPORTING_WM_CHECK` that proves the manager is running.
    ///
    /// Returns the check window, so a test can take the proof away again.
    fn advertise_ewmh(&self) -> Window {
        self.advertise_supported(&[self.net_client_list, self.net_active_window]);
        self.advertise_supporting_wm()
    }

    /// Writes `_NET_SUPPORTED` as the `ATOM[]/32` list EWMH declares.
    fn advertise_supported(&self, hints: &[u32]) {
        self.set_words(self.root, self.net_supported, AtomEnum::ATOM, hints);
    }

    /// Writes the two-sided `_NET_SUPPORTING_WM_CHECK`: the root names a window
    /// and that window names itself.
    fn advertise_supporting_wm(&self) -> Window {
        let window = self.create_window();
        self.point_supporting_wm_at(self.root, window);
        self.point_supporting_wm_at(window, window);
        window
    }

    /// Sets `_NET_SUPPORTING_WM_CHECK` on `window` to `target`.
    fn point_supporting_wm_at(&self, window: Window, target: Window) {
        self.set_words(window, self.net_supporting_wm_check, AtomEnum::WINDOW, &[target]);
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
        self.set_text(window, self.net_wm_name, self.utf8_string, title.as_bytes());
    }

    /// Sets `WM_NAME` as `STRING`, the way a client that predates EWMH does.
    fn set_wm_name(&self, window: Window, title: &[u8]) {
        self.set_text(window, AtomEnum::WM_NAME.into(), AtomEnum::STRING.into(), title);
    }

    /// Sets `WM_CLASS` to the instance/class pair, NUL terminated as the core
    /// protocol specifies.
    fn set_wm_class(&self, window: Window, instance: &str, class: &str) {
        let mut value = Vec::new();
        value.extend_from_slice(instance.as_bytes());
        value.push(0);
        value.extend_from_slice(class.as_bytes());
        value.push(0);
        self.set_text(window, AtomEnum::WM_CLASS.into(), AtomEnum::STRING.into(), &value);
    }

    fn set_text(&self, window: Window, property: u32, kind: u32, value: &[u8]) {
        self.connection
            .change_property8(PropMode::REPLACE, window, property, kind, value)
            .expect("writing a text property")
            .check()
            .expect("writing a text property");
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

    /// Selects `mask` on the root window, as a manager does, so that events sent
    /// to the root naming any of those bits reach this client.
    fn watch_root(&self, mask: EventMask) {
        self.connection
            .change_window_attributes(self.root, &ChangeWindowAttributesAux::new().event_mask(mask))
            .expect("selecting root events")
            .check()
            .unwrap_or_else(|error| panic!("selecting {mask:?} on the root window failed: {error}"));
        self.flush();
    }

    /// Destroys a window, so its id names nothing.
    fn destroy_window(&self, window: Window) {
        self.connection
            .destroy_window(window)
            .expect("destroying a fixture window")
            .check()
            .expect("destroying a fixture window");
        self.flush();
    }

    fn flush(&self) {
        self.connection.flush().expect("flushing the fixture connection");
    }

    /// Polls for a `_NET_ACTIVE_WINDOW` client message naming `expected`.
    ///
    /// Returns the whole message -- format and data words both matter -- or
    /// `None` if none arrives before the deadline. Other events are ignored
    /// rather than failing the wait: the server is free to send this client
    /// anything else it selected.
    fn await_active_window_message(&self, expected: Window) -> Option<ClientMessageEvent> {
        let deadline = Instant::now() + EVENT_LIMIT;
        loop {
            while let Some(event) = self
                .connection
                .poll_for_event()
                .expect("polling the fixture connection")
            {
                if let Event::ClientMessage(message) = event {
                    if message.type_ == self.net_active_window && message.window == expected {
                        return Some(message);
                    }
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

/// A server that already speaks EWMH, plus its fixture client.
fn ewmh_server() -> (XvfbServer, Ewmh) {
    let server = XvfbServer::start();
    let fixture = Ewmh::connect(&server);
    fixture.advertise_ewmh();
    (server, fixture)
}

/// The info for `window`, if it was enumerated at all.
fn find_opt(infos: &[WindowInfo], window: Window) -> Option<&WindowInfo> {
    let handle = WindowHandle(u64::from(window));
    infos.iter().find(|info| info.handle == handle)
}

/// The info for `window`, or a named failure listing what was enumerated.
fn find(infos: &[WindowInfo], window: Window) -> &WindowInfo {
    find_opt(infos, window)
        .unwrap_or_else(|| panic!("window {window:#x} is missing from the enumeration: {infos:?}"))
}

// ---------------------------------------------------------------------------
// Connecting
// ---------------------------------------------------------------------------

/// A service connects to the display it is named, not to the ambient one.
///
/// Kills the bug where `connect` ignores its argument and falls back to
/// `$DISPLAY`: a host driving a specific server would silently enumerate and
/// activate windows on another session.
#[test]
fn a_service_connects_to_the_display_it_was_named() {
    let (server, _fixture) = ewmh_server();
    let service = X11WindowService::connect(Some(server.display()));
    assert!(
        service.is_ok(),
        "connecting to the private server {} failed: {:?}",
        server.display(),
        service.err()
    );
}

/// Connecting to a display where no server listens is a named refusal.
///
/// Kills the bug where a failed connection still yields a service, whose
/// enumeration would then be an empty list the host reads as "no windows open".
#[test]
fn connecting_to_a_display_with_no_server_is_refused_by_name() {
    // A number no server in this process ever takes.
    let absent = ":998";
    assert!(
        !PathBuf::from("/tmp/.X11-unix/X998").exists(),
        "fixture assumes display {absent} is free"
    );

    match X11WindowService::connect(Some(absent)) {
        Ok(_) => panic!("connecting to {absent} should not succeed"),
        Err(error) => assert!(
            matches!(error, CoreError::Invalid(_)),
            "unexpected error kind: {error:?}"
        ),
    }
}

/// A server advertising no `_NET_SUPPORTED` is refused, not tolerated.
///
/// This is the honesty test for spec 18.6, and it is why the fixture has to
/// write the handshake at all. On a bare server -- no window manager, which is
/// what this test uses -- `_NET_CLIENT_LIST` is absent and a
/// `_NET_ACTIVE_WINDOW` message is delivered to nobody. A service that connected
/// anyway would answer every enumeration with an empty list and every activation
/// with `Ok(())`, so the host would report the capability `Available` and the
/// user would press the switch key forever. The refusal is what lets the host
/// report `Unavailable` instead.
#[test]
fn connecting_to_a_server_without_ewmh_support_is_refused_by_name() {
    let server = XvfbServer::start();
    // Deliberately no `Ewmh::advertise_ewmh`: this is a bare X server.
    match X11WindowService::connect(Some(server.display())) {
        Ok(_) => panic!(
            "a server with no _NET_SUPPORTED must not yield a service: it would report success \
             for enumerations and activations that reach no window manager"
        ),
        Err(CoreError::Invalid(message)) => assert!(
            message.contains("_NET_SUPPORTED"),
            "the refusal must name the missing handshake, got: {message}"
        ),
        Err(other) => panic!("unexpected error kind: {other:?}"),
    }
}

/// Once `_NET_SUPPORTED` appears, the same server is accepted.
///
/// The pair with the test above: it pins that the refusal is caused by the
/// handshake and nothing else about `Xvfb`, so neither test can pass for an
/// accidental reason.
#[test]
fn advertising_ewmh_support_is_what_makes_a_server_acceptable() {
    let server = XvfbServer::start();
    let fixture = Ewmh::connect(&server);
    assert!(
        X11WindowService::connect(Some(server.display())).is_err(),
        "the bare server should be refused before the handshake is written"
    );
    fixture.advertise_ewmh();
    assert!(
        X11WindowService::connect(Some(server.display())).is_ok(),
        "the same server should be accepted once _NET_SUPPORTED is present"
    );
}

/// A non-empty `_NET_SUPPORTED` of the wrong type is not a handshake.
///
/// EWMH declares the property `ATOM[]/32`, and any client at all may write any
/// bytes to a root-window property. Kills the bug where the check is
/// `!value.is_empty()`: a stray `STRING` left on the root by some other program
/// would then be read as a window manager's advertisement, and the host would
/// be handed a service whose enumeration and activation reach nobody.
#[test]
fn a_net_supported_of_the_wrong_type_is_not_a_handshake() {
    let server = XvfbServer::start();
    let fixture = Ewmh::connect(&server);
    fixture.set_text(
        fixture.root,
        fixture.net_supported,
        AtomEnum::STRING.into(),
        b"_NET_CLIENT_LIST _NET_ACTIVE_WINDOW",
    );
    fixture.advertise_supporting_wm();

    match X11WindowService::connect(Some(server.display())) {
        Ok(_) => panic!("a STRING _NET_SUPPORTED is not an ATOM[]/32 hint list and must be refused"),
        Err(CoreError::Invalid(message)) => assert!(
            message.contains("_NET_SUPPORTED"),
            "the refusal must name the property it rejected, got: {message}"
        ),
        Err(other) => panic!("unexpected error kind: {other:?}"),
    }
}

/// A hint list that omits a hint this service uses is refused, by name.
///
/// `_NET_SUPPORTED` exists to say *which* hints the manager implements, so a
/// manager listing neither `_NET_CLIENT_LIST` nor `_NET_ACTIVE_WINDOW` -- a
/// tiling manager that publishes no client list is a real example -- would
/// answer every enumeration with an empty list and drop every activation. Kills
/// the bug where the list is treated as an opaque non-empty blob. Each hint is
/// removed separately, so a check that looks for only one of them fails here.
#[test]
fn a_hint_list_missing_a_hint_this_service_uses_is_refused_by_name() {
    for (present, missing) in [
        ("_NET_ACTIVE_WINDOW", "_NET_CLIENT_LIST"),
        ("_NET_CLIENT_LIST", "_NET_ACTIVE_WINDOW"),
    ] {
        let server = XvfbServer::start();
        let fixture = Ewmh::connect(&server);
        let hint = if present == "_NET_ACTIVE_WINDOW" {
            fixture.net_active_window
        } else {
            fixture.net_client_list
        };
        fixture.advertise_supported(&[hint]);
        fixture.advertise_supporting_wm();

        match X11WindowService::connect(Some(server.display())) {
            Ok(_) => panic!("a manager that does not list {missing} cannot serve window control"),
            Err(CoreError::Invalid(message)) => assert!(
                message.contains(missing),
                "the refusal must name the hint that was missing ({missing}), got: {message}"
            ),
            Err(other) => panic!("unexpected error kind: {other:?}"),
        }
    }
}

/// A complete hint list with no `_NET_SUPPORTING_WM_CHECK` is refused.
///
/// `_NET_SUPPORTED` is an ordinary root property: it outlives the manager that
/// wrote it, so a manager that crashed leaves a perfectly well-formed hint list
/// behind on a display where nothing implements any of it. EWMH defines
/// `_NET_SUPPORTING_WM_CHECK` precisely to tell those apart, because the check
/// window is owned by the manager and dies with it. Kills the bug where the hint
/// list alone is taken as proof of life.
#[test]
fn a_hint_list_without_a_living_manager_is_refused() {
    let server = XvfbServer::start();
    let fixture = Ewmh::connect(&server);
    fixture.advertise_supported(&[fixture.net_client_list, fixture.net_active_window]);
    // Deliberately no `advertise_supporting_wm`: this is a stale advertisement.

    match X11WindowService::connect(Some(server.display())) {
        Ok(_) => panic!("a hint list with no living manager behind it must not yield a service"),
        Err(CoreError::Invalid(message)) => assert!(
            message.contains("_NET_SUPPORTING_WM_CHECK"),
            "the refusal must name the missing proof of life, got: {message}"
        ),
        Err(other) => panic!("unexpected error kind: {other:?}"),
    }
}

/// The check is two-sided: the named window has to name itself back.
///
/// One side alone is forgeable and is exactly what a half-written or stale root
/// property looks like. Both failure modes are covered: a check window that
/// names something else, and one that has been destroyed since the root named
/// it -- which is what an EWMH manager exiting actually leaves behind.
#[test]
fn a_supporting_wm_check_that_is_not_two_sided_is_refused() {
    let server = XvfbServer::start();
    let fixture = Ewmh::connect(&server);
    fixture.advertise_supported(&[fixture.net_client_list, fixture.net_active_window]);

    let claimed = fixture.create_window();
    let other = fixture.create_window();
    fixture.point_supporting_wm_at(fixture.root, claimed);
    fixture.point_supporting_wm_at(claimed, other);
    assert!(
        X11WindowService::connect(Some(server.display())).is_err(),
        "a check window naming some other window does not prove a manager is running"
    );

    // Now make it two-sided, and only then does the same display pass: the
    // refusal above cannot have been caused by anything else.
    fixture.point_supporting_wm_at(claimed, claimed);
    assert!(
        X11WindowService::connect(Some(server.display())).is_ok(),
        "a two-sided _NET_SUPPORTING_WM_CHECK is what makes the same display acceptable"
    );

    // And a manager that exits takes its check window with it.
    fixture.destroy_window(claimed);
    match X11WindowService::connect(Some(server.display())) {
        Ok(_) => panic!("a destroyed check window must not still prove a manager is running"),
        Err(CoreError::Invalid(message)) => assert!(
            message.contains("_NET_SUPPORTING_WM_CHECK"),
            "the refusal must name the handshake that failed, got: {message}"
        ),
        Err(other) => panic!("unexpected error kind: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

/// An EWMH desktop with no clients enumerates to nothing, without erroring.
///
/// Kills the bug where an absent `_NET_CLIENT_LIST` -- which is what a manager
/// publishes before its first client appears -- is reported as a failure, making
/// the switcher error out on an empty session.
#[test]
fn an_empty_client_list_enumerates_to_nothing() {
    let (server, _fixture) = ewmh_server();

    let infos = server
        .service()
        .enumerate()
        .expect("enumerating an empty desktop");
    assert!(infos.is_empty(), "expected no windows, got {infos:?}");
}

/// Every window in `_NET_CLIENT_LIST` is enumerated, with its own title.
///
/// Two windows with *different* titles is the point: an implementation that
/// returns only the first entry, that returns the root's own properties for
/// every entry, or that reads one title and repeats it, all fail here. `WM_CLASS`
/// is set on one window only, so the application name is proven to be read
/// per-window too rather than copied.
#[test]
fn enumeration_reports_every_listed_window_with_its_own_title() {
    let (server, fixture) = ewmh_server();

    let first = fixture.create_window();
    let second = fixture.create_window();
    fixture.set_net_wm_name(first, "Editor — draft.txt");
    fixture.set_net_wm_name(second, "Terminal");
    fixture.set_wm_class(first, "editor", "Editor");
    fixture.publish_client_list(&[first, second]);

    let infos = server.service().enumerate().expect("enumerating two windows");

    assert_eq!(
        infos.len(),
        2,
        "both listed windows must be enumerated, got {infos:?}"
    );
    assert_eq!(find(&infos, first).title, "Editor — draft.txt");
    assert_eq!(find(&infos, second).title, "Terminal");
    assert_eq!(
        find(&infos, first).application.as_deref(),
        Some("Editor"),
        "WM_CLASS's class field is the application name"
    );
    assert_eq!(
        find(&infos, second).application,
        None,
        "a window without WM_CLASS must not inherit another window's application name"
    );
}

/// A window that set only the pre-EWMH `WM_NAME` is still titled.
///
/// Kills the bug where `_NET_WM_NAME` is the only property consulted: such a
/// window would appear as a blank row, indistinguishable from an untitled one.
/// The Latin-1 byte proves the fallback decodes `STRING` rather than running the
/// bytes through UTF-8, where `0xE9` alone is invalid and would become `U+FFFD`.
#[test]
fn a_window_with_only_wm_name_is_titled_from_it() {
    let (server, fixture) = ewmh_server();

    let window = fixture.create_window();
    // "Café" in Latin-1: the encoding `WM_NAME`'s `STRING` type mandates.
    fixture.set_wm_name(window, b"Caf\xe9");
    fixture.publish_client_list(&[window]);

    let infos = server.service().enumerate().expect("enumerating");
    assert_eq!(find(&infos, window).title, "Café");
}

/// `_NET_WM_NAME` wins when a window sets both.
///
/// Both are commonly set, and they disagree in practice -- `WM_NAME` is the
/// truncated ASCII-safe version. Kills the bug where the fallback is consulted
/// first, or where the two are concatenated.
#[test]
fn net_wm_name_takes_precedence_over_wm_name() {
    let (server, fixture) = ewmh_server();

    let window = fixture.create_window();
    fixture.set_wm_name(window, b"legacy title");
    fixture.set_net_wm_name(window, "modern title");
    fixture.publish_client_list(&[window]);

    let infos = server.service().enumerate().expect("enumerating");
    assert_eq!(find(&infos, window).title, "modern title");
}

/// A window with no title property is listed with an empty title, and does not
/// stop the scan.
///
/// This is the "skip the property, not the window" contract. The untitled window
/// is placed *between* two titled ones, so an implementation that aborts on the
/// first unreadable property, or that stops early, loses the third window and
/// fails. A window is switchable whether or not it has a name.
#[test]
fn a_window_without_a_title_is_still_enumerated() {
    let (server, fixture) = ewmh_server();

    let before = fixture.create_window();
    let untitled = fixture.create_window();
    let after = fixture.create_window();
    fixture.set_net_wm_name(before, "before");
    fixture.set_net_wm_name(after, "after");
    fixture.publish_client_list(&[before, untitled, after]);

    let infos = server.service().enumerate().expect("enumerating");

    assert_eq!(
        infos.len(),
        3,
        "the untitled window must not be dropped: {infos:?}"
    );
    assert_eq!(find(&infos, untitled).title, "");
    assert_eq!(find(&infos, untitled).application, None);
    assert_eq!(find(&infos, before).title, "before");
    assert_eq!(
        find(&infos, after).title,
        "after",
        "the scan must continue past a window with no readable properties"
    );
}

/// A destroyed window still listed by the manager is omitted, and the rest of
/// the scan survives.
///
/// Racing a real desktop is the reason the contract is written this way: a
/// program exits between the manager publishing its list and the backend
/// reading a title, and every property request for that window is then a
/// `BadWindow`. Two distinct bugs die here. One such error must not abort the
/// whole scan, which would make the switcher intermittently empty for reasons
/// the user cannot see -- and the dead window must not be *listed* either.
/// `WindowService::enumerate` requires it to be omitted, because a listed one
/// is an untitled, unswitchable row in front of the user and a handle whose
/// activation can only fail.
#[test]
fn a_window_destroyed_after_being_listed_is_omitted_without_failing_the_scan() {
    let (server, fixture) = ewmh_server();

    let alive = fixture.create_window();
    let doomed = fixture.create_window();
    let after = fixture.create_window();
    fixture.set_net_wm_name(alive, "still here");
    fixture.set_net_wm_name(doomed, "about to go");
    fixture.set_net_wm_name(after, "also still here");
    fixture.publish_client_list(&[alive, doomed, after]);
    fixture.destroy_window(doomed);

    let infos = server
        .service()
        .enumerate()
        .expect("a destroyed window must not fail the enumeration");

    assert_eq!(find(&infos, alive).title, "still here");
    // Placed after the doomed one, so a scan that stopped there fails too.
    assert_eq!(find(&infos, after).title, "also still here");
    assert!(
        find_opt(&infos, doomed).is_none(),
        "the destroyed window {doomed:#x} must be omitted, not listed untitled: {infos:?}"
    );
}

/// Enumeration preserves the order the manager published.
///
/// The manager orders `_NET_CLIENT_LIST` by initial mapping, and a switcher's
/// list is meaningless if the backend permutes it. Kills the bug where the
/// implementation collects into a set or reverses while iterating.
#[test]
fn enumeration_preserves_the_published_order() {
    let (server, fixture) = ewmh_server();

    let windows: Vec<Window> = (0..4).map(|_| fixture.create_window()).collect();
    fixture.publish_client_list(&windows);

    let infos = server.service().enumerate().expect("enumerating");
    let handles: Vec<u64> = infos.iter().map(|info| info.handle.0).collect();
    let expected: Vec<u64> = windows.iter().copied().map(u64::from).collect();
    assert_eq!(handles, expected);
}

/// A `_NET_CLIENT_LIST` of the wrong type is not reinterpreted as window ids.
///
/// EWMH declares the property `WINDOW[]/32`. Any client can write anything to
/// the root window, so a backend that reads the bytes without checking the type
/// would hand the host handles synthesised from text -- and `activate` on those
/// would address arbitrary window ids. Kills exactly that.
#[test]
fn a_client_list_of_the_wrong_type_yields_no_windows() {
    let (server, fixture) = ewmh_server();

    let window = fixture.create_window();
    fixture.publish_client_list(&[window]);
    // Overwrite with text under the same property name.
    fixture.set_text(
        fixture.root,
        fixture.net_client_list,
        AtomEnum::STRING.into(),
        b"not a window list",
    );

    let infos = server
        .service()
        .enumerate()
        .expect("a wrongly-typed client list is an empty desktop, not an error");
    assert!(
        infos.is_empty(),
        "text stored under _NET_CLIENT_LIST must not become window handles: {infos:?}"
    );
}

// ---------------------------------------------------------------------------
// Activation
// ---------------------------------------------------------------------------

/// `activate` puts a format-32 `_NET_ACTIVE_WINDOW` message for the right window
/// on the wire, to the root, with `SubstructureRedirect` named.
///
/// This is the test that distinguishes a real activation from `Ok(())`. The
/// fixture selects **only** `SubstructureRedirect` on the root -- the bit a
/// window manager selects there, and the reason EWMH routes the request through
/// the root at all. `SendEvent` delivers to clients selecting a bit the sender
/// named, so receiving the message proves the sender named this bit. A message
/// sent with a zero mask, or to the target window instead of the root, reaches
/// nobody and fails here while a bare `Ok(())` check would have passed.
#[test]
fn activate_sends_a_net_active_window_message_with_substructure_redirect() {
    let (server, fixture) = ewmh_server();

    let window = fixture.create_window();
    fixture.publish_client_list(&[window]);
    fixture.watch_root(EventMask::SUBSTRUCTURE_REDIRECT);

    server
        .service()
        .activate(&WindowHandle(u64::from(window)))
        .expect("activating a live window");

    let message = fixture.await_active_window_message(window).unwrap_or_else(|| {
        panic!(
            "no _NET_ACTIVE_WINDOW message for {window:#x} reached a client selecting \
                 SubstructureRedirect on the root within {EVENT_LIMIT:?}"
        )
    });
    assert_eq!(
        message.format, 32,
        "EWMH requires a format-32 client message; a manager ignores any other width"
    );
}

/// The activation carries a real timestamp, not `CurrentTime`.
///
/// EWMH puts the requesting client's last user-activity time in `data.l[1]`, and
/// a manager compares it against the focused window's `_NET_USER_TIME` to tell a
/// user-driven raise from a background program shouting for focus. Zero is
/// `CurrentTime`, which carries no information at all and which managers are
/// entitled to treat as the second case -- so a launcher that always sends zero
/// has an activation that silently loses to focus-stealing prevention on exactly
/// the well-behaved managers this backend exists to work with. Kills the
/// hard-coded zero.
///
/// The service has seen no key press here, which is the harder case: it has to
/// go and ask the server what time it is rather than sending nothing. The bound
/// is the server's own clock, read independently through the fixture, so the
/// assertion is not "non-zero" alone -- an implementation sending a constant, or
/// a wall-clock value, or the source indication again all fail it.
#[test]
fn activate_carries_a_real_server_timestamp_rather_than_current_time() {
    let (server, fixture) = ewmh_server();

    let window = fixture.create_window();
    fixture.publish_client_list(&[window]);
    fixture.watch_root(EventMask::SUBSTRUCTURE_REDIRECT);

    let before = fixture.server_time();
    server
        .service()
        .activate(&WindowHandle(u64::from(window)))
        .expect("activating a live window");
    let after = fixture.server_time();

    let message = fixture
        .await_active_window_message(window)
        .unwrap_or_else(|| panic!("no _NET_ACTIVE_WINDOW message for {window:#x} arrived"));
    let stamp = message.data.as_data32()[1];

    assert_ne!(
        stamp, 0,
        "data.l[1] is CurrentTime, which tells the manager nothing about user intent"
    );
    assert!(
        (before..=after).contains(&stamp),
        "the activation timestamp {stamp} is not a time from this server's clock, which ran from \
         {before} to {after} across the call"
    );
}

/// The same message also names `SubstructureNotify`.
///
/// EWMH specifies the mask as `SubstructureNotify | SubstructureRedirect`, and
/// the two bits are separately observable: this fixture selects **only** the
/// notify bit, so it receives the message only if the sender named that bit too.
/// Together with the test above, both halves of the required mask are proven to
/// have reached the wire rather than being inferred from one delivery.
#[test]
fn activate_names_substructure_notify_in_the_same_mask() {
    let (server, fixture) = ewmh_server();

    let window = fixture.create_window();
    fixture.publish_client_list(&[window]);
    fixture.watch_root(EventMask::SUBSTRUCTURE_NOTIFY);

    server
        .service()
        .activate(&WindowHandle(u64::from(window)))
        .expect("activating a live window");

    assert!(
        fixture.await_active_window_message(window).is_some(),
        "no _NET_ACTIVE_WINDOW message for {window:#x} reached a client selecting \
         SubstructureNotify on the root within {EVENT_LIMIT:?}"
    );
}

/// Activating a window that no longer exists is a named error.
///
/// Kills the bug where the client message is sent blind: the server accepts a
/// message naming a dead window and the manager drops it, so the host would be
/// told the switch succeeded and the user would watch nothing happen. A stale
/// handle -- the entry the user clicked a moment after the program quit -- is the
/// normal way this arises.
#[test]
fn activating_a_window_that_does_not_exist_is_an_error() {
    let (server, fixture) = ewmh_server();

    let window = fixture.create_window();
    fixture.destroy_window(window);

    match server.service().activate(&WindowHandle(u64::from(window))) {
        Ok(()) => panic!("activating the destroyed window {window:#x} must not report success"),
        Err(error) => assert!(
            matches!(error, CoreError::Invalid(_)),
            "unexpected error kind: {error:?}"
        ),
    }
}

/// A handle too wide to be an X11 window id is refused, not truncated.
///
/// `WindowHandle` is `u64` so that one type spans every backend, and X11 ids are
/// 32-bit. Kills the bug where the value is cast: the low bits of a foreign
/// handle would name some *other*, possibly live, window on this display, and
/// activating the wrong window is worse than refusing.
#[test]
fn a_handle_wider_than_an_x11_window_id_is_refused() {
    let (server, fixture) = ewmh_server();

    let window = fixture.create_window();
    fixture.publish_client_list(&[window]);

    let disguised = WindowHandle(u64::from(window) | (1 << 40));
    match server.service().activate(&disguised) {
        Ok(()) => panic!("a 40-bit handle must not be truncated onto window {window:#x}"),
        Err(error) => assert!(
            matches!(error, CoreError::Invalid(_)),
            "unexpected error kind: {error:?}"
        ),
    }
}

/// An enumerated handle can be activated as-is.
///
/// The end-to-end contract the host relies on: whatever `enumerate` hands back is
/// a token `activate` accepts. Kills the bug where the two disagree about how a
/// window id is encoded in the `u64` -- a mismatch no single-method test could
/// catch.
#[test]
fn a_handle_from_enumeration_round_trips_into_activation() {
    let (server, fixture) = ewmh_server();

    let window = fixture.create_window();
    fixture.set_net_wm_name(window, "round trip");
    fixture.publish_client_list(&[window]);
    fixture.watch_root(EventMask::SUBSTRUCTURE_REDIRECT);

    let service = server.service();
    let infos = service.enumerate().expect("enumerating");
    let handle = infos
        .first()
        .map(|info| info.handle)
        .unwrap_or_else(|| panic!("the published window was not enumerated"));

    service
        .activate(&handle)
        .expect("activating an enumerated handle");
    assert!(
        fixture.await_active_window_message(window).is_some(),
        "the handle enumeration returned must address the window it described"
    );
}
