//! The X11 global-hotkey backend driven against a *real* X server (spec 18.6).
//!
//! # Why a real server
//!
//! The mapping tests in `hotkeys.rs` prove which `(mask, keysym)` pair the
//! backend would hand to X. They cannot prove that the grab is taken, that a
//! re-registration of the same chord takes no second grab, or that releasing
//! it hands the key back — and those are exactly the properties spec 18.6
//! owes: a reactivation path that actually claims the key. An in-process
//! double could not fail a `XGrabKey` round trip, so these tests start their
//! own `Xvfb`.
//!
//! A missing or unusable `Xvfb` is a **test failure**, never a skip: there is
//! no `#[ignore]` and no early return here, following the convention documented
//! at the top of `crates/crikey-python-host/tests/worker.rs`. A skipped test is
//! not evidence.
//!
//! # Isolation
//!
//! Each test owns a private server on its own display number, derived from the
//! process id and a per-test counter so that concurrent test binaries and the
//! developer's own session never collide, and never `:0`. The server is torn
//! down by [`XvfbServer`]'s `Drop`, so it dies on panic too.
//!
//! # Time
//!
//! The server is a real OS process, so waiting for it to accept connections
//! cannot be virtual. The wait is bounded polling against an explicit deadline,
//! never a fixed sleep used as synchronisation: a regression that would hang
//! the run becomes a named failure instead.
//!
//! These tests are written before the implementation. They fail to compile
//! until `crikey_platform_linux` exports `X11HotkeyService`.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crikey_core::CoreError;
use crikey_platform::{HotkeyBinding, HotkeyService};
use crikey_platform_linux::{DesktopEnvironment, LinuxBackend, X11HotkeyService};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, WindowClass};
use x11rb::rust_connection::RustConnection;

/// Ceiling on an `Xvfb` becoming connectable. Not a performance assertion: it
/// turns a server that never comes up into a named failure rather than a stall.
const SERVER_READY_LIMIT: Duration = Duration::from_secs(20);

/// Ceiling on the reader noticing that its server has gone, and on a `Drop`
/// completing. Generous, because neither is being measured: they only turn a
/// hang into a named failure.
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(15);

/// Gap between readiness polls. Polling, not sleeping-as-synchronisation: the
/// loop ends on the observable (the display socket), not on elapsed time.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Hands out a distinct display number per server within this process.
static NEXT_DISPLAY_OFFSET: AtomicU32 = AtomicU32::new(0);

fn binding(text: &str) -> HotkeyBinding {
    HotkeyBinding {
        accelerator: text.to_owned(),
    }
}

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
    /// Panics — loudly and by name — if `Xvfb` is absent or never comes up.
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
    fn service(&self) -> X11HotkeyService {
        X11HotkeyService::connect(Some(self.display()))
            .unwrap_or_else(|error| panic!("connecting to {} failed: {error}", self.display))
    }

    /// Kills the server and waits for it, so every client connection to it is
    /// definitively broken by the time this returns.
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
// Connecting
// ---------------------------------------------------------------------------

/// A service connects to the display it is named, not to the ambient one.
///
/// Kills the bug where `connect` ignores its argument and falls back to
/// `$DISPLAY`: the whole point of the parameter is that a host can drive a
/// specific server, and a backend that silently used another one would grab
/// hotkeys on the wrong session.
#[test]
fn a_service_connects_to_the_display_it_was_named() {
    let server = XvfbServer::start();
    let service = X11HotkeyService::connect(Some(server.display()));
    assert!(
        service.is_ok(),
        "connecting to the private server {} failed: {:?}",
        server.display(),
        service.err()
    );
}

/// Connecting to a display where no server listens is a named refusal.
///
/// Kills the bug where a failed connection yields a service that accepts
/// registrations and swallows every one of them, so the launcher believes its
/// hotkey is live when nothing will ever deliver it.
#[test]
fn connecting_to_a_display_with_no_server_is_refused_by_name() {
    // A number no server in this process ever takes.
    let absent = ":999";
    assert!(
        !PathBuf::from("/tmp/.X11-unix/X999").exists(),
        "fixture assumes display {absent} is free"
    );

    match X11HotkeyService::connect(Some(absent)) {
        Ok(_) => panic!("connecting to {absent} should not succeed"),
        Err(error) => assert!(
            matches!(error, CoreError::Invalid(_)),
            "unexpected error kind: {error:?}"
        ),
    }
}

// ---------------------------------------------------------------------------
// Registration lifecycle against a live server
// ---------------------------------------------------------------------------

/// A well-formed accelerator can be registered against a live server.
///
/// Kills the bug where the grab request is malformed or is never flushed: with
/// a real server on the other end, a rejected `GrabKey` comes back as an error
/// instead of being invisible.
#[test]
fn registering_an_accelerator_against_a_live_server_succeeds() {
    let server = XvfbServer::start();
    let mut service = server.service();

    assert_eq!(
        service
            .register(&binding("Ctrl+Alt+Space"))
            .map_err(|e| e.to_string()),
        Ok(())
    );
    assert_eq!(
        service.register(&binding("Meta+F5")).map_err(|e| e.to_string()),
        Ok(())
    );
}

#[test]
fn a_grab_conflict_is_reported_instead_of_claiming_the_hotkey() {
    let server = XvfbServer::start();
    let mut first = server.service();
    first
        .register(&binding("Ctrl+Alt+Space"))
        .expect("the first client takes the chord");

    let mut second = server.service();
    assert!(
        matches!(
            second.register(&binding("Ctrl+Alt+Space")),
            Err(CoreError::Invalid(_))
        ),
        "a second X client must see the existing grab conflict"
    );
}

/// Registering the same accelerator twice is idempotent, and takes one grab.
///
/// The shared `HotkeyService` contract, as the Windows backend implements it
/// (`crikey-platform-windows/src/hotkeys.rs:423`), answers a re-registration
/// with `Ok(())`: the caller asked for a live binding and has one. Kills the
/// bug where the second call takes a *second* X grab — the launcher would then
/// fire twice per press, and one `unregister` would leave the key still
/// swallowed. Idempotence is observable exactly there: one release is enough,
/// and the release after it is an error because nothing is live any more.
#[test]
fn registering_the_same_accelerator_twice_is_idempotent() {
    let server = XvfbServer::start();
    let mut service = server.service();

    service
        .register(&binding("Ctrl+Alt+Space"))
        .expect("first registration succeeds");
    service
        .register(&binding("Ctrl+Alt+Space"))
        .expect("re-registering a live accelerator is not an error");

    service
        .unregister(&binding("Ctrl+Alt+Space"))
        .expect("one release is enough: the second register took no second grab");
    assert!(
        matches!(
            service.unregister(&binding("Ctrl+Alt+Space")),
            Err(CoreError::Invalid(_))
        ),
        "a second grab survived the release"
    );
}

/// Every spelling of one chord is one registration.
///
/// This is the bug idempotence exists to prevent: duplicate detection keyed on
/// the raw string rather than the canonical accelerator lets `alt+ctrl+SPACE`
/// double-grab the chord `Ctrl+Alt+Space` already holds, so the user's single
/// configured hotkey fires twice and survives its own release.
#[test]
fn every_spelling_of_one_chord_is_one_registration() {
    let server = XvfbServer::start();
    let mut service = server.service();

    service
        .register(&binding("Ctrl+Alt+Space"))
        .expect("first registration succeeds");

    for spelling in ["alt+ctrl+SPACE", "ctrl+alt+space", " Alt + Ctrl + Space "] {
        service
            .register(&binding(spelling))
            .unwrap_or_else(|error| panic!("{spelling:?} names a live chord and must be Ok: {error}"));
    }

    service
        .unregister(&binding("Ctrl+Alt+Space"))
        .expect("a single release covers every spelling of the one chord");
    assert!(
        matches!(
            service.unregister(&binding("alt+ctrl+SPACE")),
            Err(CoreError::Invalid(_))
        ),
        "the alternative spellings left extra registrations behind"
    );
}

/// An unparseable accelerator is refused, and takes no grab with it.
///
/// Kills the bug where the string is passed to X unvalidated: the following
/// registration of a *valid* chord must still succeed, proving no partial state
/// was left behind.
#[test]
fn an_unparseable_accelerator_is_refused_without_disturbing_the_service() {
    let server = XvfbServer::start();
    let mut service = server.service();

    for text in ["", "Ctrl+", "Ctrl", "Ctrl+Ctrl+A", "Ctrl+A+B", "Ctrl+Nope"] {
        assert!(
            matches!(service.register(&binding(text)), Err(CoreError::Invalid(_))),
            "{text:?} should not be registrable"
        );
    }

    service
        .register(&binding("Ctrl+Alt+Space"))
        .expect("a valid chord still registers");
}

/// Unregistering a registered accelerator succeeds.
#[test]
fn unregistering_a_registered_accelerator_succeeds() {
    let server = XvfbServer::start();
    let mut service = server.service();

    service
        .register(&binding("Ctrl+Alt+Space"))
        .expect("registration succeeds");
    assert_eq!(
        service
            .unregister(&binding("Ctrl+Alt+Space"))
            .map_err(|e| e.to_string()),
        Ok(())
    );
}

/// Unregistering an accelerator that was never registered is a named error.
///
/// Kills the bug where a no-op success leaves the caller believing it handed a
/// key back to the desktop while the backend went on swallowing it.
#[test]
fn unregistering_an_accelerator_that_was_never_registered_is_an_error() {
    let server = XvfbServer::start();
    let mut service = server.service();

    match service.unregister(&binding("Ctrl+Alt+Space")) {
        Ok(()) => panic!("releasing a chord that was never held should be refused"),
        Err(error) => assert!(
            matches!(error, CoreError::Invalid(_)),
            "unexpected error kind: {error:?}"
        ),
    }

    // And still an error once *some other* chord is held: the check must be
    // per-accelerator, not "is anything registered at all".
    service
        .register(&binding("Meta+F5"))
        .expect("registration succeeds");
    assert!(matches!(
        service.unregister(&binding("Ctrl+Alt+Space")),
        Err(CoreError::Invalid(_))
    ));
}

/// Unregistering twice is refused the second time.
///
/// Kills the bug where the bookkeeping entry survives the release, which would
/// hide a leaked grab behind an apparently clean slate.
#[test]
fn releasing_an_accelerator_twice_is_refused_the_second_time() {
    let server = XvfbServer::start();
    let mut service = server.service();

    service
        .register(&binding("Ctrl+Alt+Space"))
        .expect("registration succeeds");
    service
        .unregister(&binding("Ctrl+Alt+Space"))
        .expect("first release succeeds");

    assert!(matches!(
        service.unregister(&binding("Ctrl+Alt+Space")),
        Err(CoreError::Invalid(_))
    ));
}

/// After a release, the same accelerator can be registered again.
///
/// This is the test that distinguishes a real release from a forgotten one: if
/// `unregister` only dropped the bookkeeping entry without issuing `UngrabKey`,
/// the re-grab would still be held by the same client and the chord would be
/// permanently double-grabbed. Reconnecting a *second* service and having it
/// take the chord proves the server no longer holds the original grab.
#[test]
fn a_released_accelerator_can_be_registered_again() {
    let server = XvfbServer::start();
    let mut service = server.service();

    service
        .register(&binding("Ctrl+Alt+Space"))
        .expect("registration succeeds");
    service
        .unregister(&binding("Ctrl+Alt+Space"))
        .expect("release succeeds");
    service
        .register(&binding("Ctrl+Alt+Space"))
        .expect("the released chord is available again");

    // A fresh connection is a different X client, so the server — not the
    // in-process map — is what has to have let the chord go.
    service
        .unregister(&binding("Ctrl+Alt+Space"))
        .expect("release succeeds");
    let mut other = server.service();
    other
        .register(&binding("Ctrl+Alt+Space"))
        .expect("another client can take a genuinely released chord");
}

/// Independent accelerators are tracked independently.
///
/// Kills the bug where the backend holds a single "current" registration, so
/// binding a second hotkey silently drops the first.
#[test]
fn distinct_accelerators_are_registered_independently() {
    let server = XvfbServer::start();
    let mut service = server.service();

    for text in ["Ctrl+Alt+Space", "Meta+F5", "Ctrl+Shift+K", "F12"] {
        service
            .register(&binding(text))
            .unwrap_or_else(|error| panic!("{text} failed: {error}"));
    }

    // Every one of them is still held, so every one of them can be released
    // exactly once and not twice.
    for text in ["Ctrl+Alt+Space", "Meta+F5", "Ctrl+Shift+K", "F12"] {
        service
            .unregister(&binding(text))
            .unwrap_or_else(|error| panic!("{text} was dropped by a later registration: {error}"));
        assert!(
            matches!(service.unregister(&binding(text)), Err(CoreError::Invalid(_))),
            "{text} was registered more than once"
        );
    }
}

// ---------------------------------------------------------------------------
// The activation handler
// ---------------------------------------------------------------------------

/// Clearing the handler leaves registrations intact.
///
/// The `HotkeyService` doc requires this outright. Kills the bug where
/// `set_activation_handler(None)` is implemented as a teardown that ungrabs
/// everything: the launcher clears its handler while reconfiguring, and would
/// silently lose every hotkey. A dropped registration would make the release
/// below an error, since releasing what was never held is refused by name.
#[test]
fn clearing_the_activation_handler_leaves_registrations_intact() {
    let server = XvfbServer::start();
    let mut service = server.service();

    service.set_activation_handler(Some(Box::new(|_binding| {})));
    service
        .register(&binding("Ctrl+Alt+Space"))
        .expect("registration succeeds");
    service.set_activation_handler(None);

    service
        .unregister(&binding("Ctrl+Alt+Space"))
        .expect("clearing the handler released the registration");
}

/// A handler can be installed and cleared before anything is registered.
///
/// The launcher installs its event-loop wake before it binds anything, and
/// clearing it must not be an error either.
#[test]
fn a_handler_can_be_installed_and_cleared_without_a_registration() {
    let server = XvfbServer::start();
    let mut service = server.service();

    service.set_activation_handler(Some(Box::new(|_binding| {})));
    service.set_activation_handler(None);
    service.set_activation_handler(Some(Box::new(|_binding| {})));

    // The service is still usable afterwards.
    service
        .register(&binding("Ctrl+Alt+Space"))
        .expect("registration succeeds after handler churn");
}

/// Replacing the handler does not disturb registrations either.
///
/// Kills the bug where installing a handler re-runs the grab setup, so the
/// second install double-grabs everything already held.
#[test]
fn replacing_the_activation_handler_does_not_disturb_registrations() {
    let server = XvfbServer::start();
    let mut service = server.service();

    service
        .register(&binding("Meta+F5"))
        .expect("registration succeeds");
    service.set_activation_handler(Some(Box::new(|_binding| {})));
    service.set_activation_handler(Some(Box::new(|_binding| {})));

    service
        .unregister(&binding("Meta+F5"))
        .expect("one release is enough after two handler installs");
    assert!(matches!(
        service.unregister(&binding("Meta+F5")),
        Err(CoreError::Invalid(_))
    ));
}

// ---------------------------------------------------------------------------
// Agreement with the other backend
// ---------------------------------------------------------------------------

/// The Linux backend answers the two asymmetric cases the way the Windows one
/// does: duplicate `register` is `Ok`, unknown `unregister` is an error.
///
/// `HotkeyService` is one trait with two implementations, and a host that
/// works on Windows must work here. The reference is
/// `crikey-platform-windows/src/hotkeys.rs:423` (a live accelerator
/// re-registers as `Ok(())`) and the `unregister` doc immediately below it (an
/// accelerator that was never registered is an error, because caller and
/// backend disagreeing about what is live means the launcher keeps swallowing
/// a key it believes it released). The Windows crate is target-gated, so the
/// shape is asserted here rather than imported. Kills the bug where the two
/// backends drift apart and shared host code has to branch on the platform.
#[test]
fn the_linux_backend_answers_duplicates_and_unknowns_like_the_windows_one() {
    let server = XvfbServer::start();
    let mut service = server.service();

    // Unknown release: error, before anything is live and after.
    assert!(matches!(
        service.unregister(&binding("Ctrl+Alt+Space")),
        Err(CoreError::Invalid(_))
    ));
    service
        .register(&binding("Meta+F5"))
        .expect("registration succeeds");
    assert!(matches!(
        service.unregister(&binding("Ctrl+Alt+Space")),
        Err(CoreError::Invalid(_))
    ));

    // Duplicate registration: Ok, and asymmetric with the release above.
    assert!(
        service.register(&binding("Meta+F5")).is_ok(),
        "a duplicate register must not be an error"
    );
}

// ---------------------------------------------------------------------------
// Losing the server
// ---------------------------------------------------------------------------

/// Once the X server is gone, a registration is refused -- including for a
/// chord the service still lists as registered.
///
/// The duplicate-registration shortcut above answers `Ok(())` without touching
/// X, which is sound only while the connection is alive. When the server dies
/// the reader exits, but the `registrations` map is untouched, so the shortcut
/// keeps reporting success for a grab that died with the connection. Kills
/// exactly that: the launcher would rebind its activation shortcut after a
/// display restart, be told the hotkey is live, and never see another press.
/// The already-held chord is the discriminating one -- a fresh chord would have
/// failed on its X request anyway.
#[test]
fn a_registration_after_the_server_dies_is_refused_even_for_a_chord_already_held() {
    let mut server = XvfbServer::start();
    let mut service = server.service();
    let held = binding("Ctrl+Alt+Space");
    service
        .register(&held)
        .expect("the chord registers against a live server");

    server.kill();

    let deadline = Instant::now() + SHUTDOWN_LIMIT;
    while !service.has_failed() {
        assert!(
            Instant::now() < deadline,
            "the reader did not notice its server had gone within {SHUTDOWN_LIMIT:?}"
        );
        thread::sleep(POLL_INTERVAL);
    }

    match service.register(&held) {
        Ok(()) => panic!(
            "re-registering Ctrl+Alt+Space on a dead connection reported success: the grab went \
             with the server, so no press can ever be delivered"
        ),
        Err(CoreError::Invalid(message)) => assert!(
            message.contains("Ctrl+Alt+Space"),
            "the refusal must name the chord it refused, got: {message}"
        ),
        Err(other) => panic!("unexpected error kind: {other:?}"),
    }
}

/// Dropping a service whose private wake-up window was destroyed still
/// terminates.
///
/// The reader is woken by a `ClientMessage` addressed to a private `InputOnly`
/// child of the root. Private is not protected: X has no per-client window
/// ownership, so any other client can find that window with `QueryTree` and
/// destroy it -- which this test does, exactly as a hostile or merely
/// overzealous program would. The send is then a `BadWindow` that an *unchecked*
/// request reports asynchronously and nobody reads, so the reader stays blocked
/// in `wait_for_event` and `Drop`'s `join()` never returns: the launcher hangs
/// on exit, forever, with no diagnostic. Kills that.
///
/// The drop runs on its own thread with a deadline, because a test that hangs
/// is not a test that fails.
#[test]
fn dropping_a_service_whose_wake_window_was_destroyed_still_terminates() {
    let server = XvfbServer::start();
    let mut service = server.service();
    service
        .register(&binding("Ctrl+Alt+Space"))
        .expect("the chord registers against a live server");

    let destroyed = destroy_input_only_children(server.display());
    assert!(
        destroyed > 0,
        "the service's wake-up window was not found on the root: this test proves nothing unless \
         it actually takes the window away"
    );

    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        drop(service);
        let _ = sender.send(());
    });
    receiver.recv_timeout(SHUTDOWN_LIMIT).unwrap_or_else(|_| {
        panic!(
            "dropping the service did not finish within {SHUTDOWN_LIMIT:?}: the reader was never \
             woken and Drop is blocked in join()"
        )
    });
}

/// Destroys every `InputOnly` child of the root, the way any other X client
/// can, and reports how many went.
fn destroy_input_only_children(display: &str) -> usize {
    let (connection, screen) = RustConnection::connect(Some(display))
        .unwrap_or_else(|error| panic!("the hostile client could not reach {display}: {error}"));
    let root = connection.setup().roots[screen].root;
    let children = connection
        .query_tree(root)
        .expect("querying the root's children")
        .reply()
        .expect("querying the root's children")
        .children;

    let mut destroyed = 0;
    for child in children {
        let Ok(cookie) = connection.get_window_attributes(child) else {
            continue;
        };
        let Ok(attributes) = cookie.reply() else {
            continue;
        };
        if attributes.class != WindowClass::INPUT_ONLY {
            continue;
        }
        if let Ok(cookie) = connection.destroy_window(child) {
            if cookie.check().is_ok() {
                destroyed += 1;
            }
        }
    }
    connection.flush().expect("flushing the hostile client");
    destroyed
}

// ---------------------------------------------------------------------------
// Reaching the service through the backend
// ---------------------------------------------------------------------------

/// The backend hands out a hotkey service that really grabs, and only in a
/// session that can carry one.
///
/// This is the link the capability claim rests on. `capability(GlobalHotkeys)`
/// answers `Available` under X11, and until there is an accessor on
/// [`LinuxBackend`] that reaches a real [`X11HotkeyService`], that claim is
/// backed by nothing a host can call: the shortcut is unreachable in the live
/// app however well the service itself works. Kills the accessor that does not
/// exist, the one that never connects, and the one that hands out a service in
/// a session with nothing at all to bind against.
#[test]
fn the_backend_hands_out_a_working_hotkey_service_under_x11_and_refuses_a_headless_session() {
    let server = XvfbServer::start();
    let previous = std::env::var("DISPLAY").ok();
    std::env::set_var("DISPLAY", server.display());

    let mut backend = LinuxBackend::with_desktop_environment(DesktopEnvironment::X11);
    let outcome = backend.hotkeys().map(|hotkeys| {
        hotkeys
            .register(&binding("Ctrl+Alt+Space"))
            .expect("the chord registers through the backend accessor");
    });

    // Restore before asserting, so a failure does not leak the display.
    match previous {
        Some(value) => std::env::set_var("DISPLAY", value),
        None => std::env::remove_var("DISPLAY"),
    }
    outcome.unwrap_or_else(|error| panic!("an X11 backend must hand out a hotkey service: {error}"));

    // A session with nothing to offer refuses, naming itself, rather than
    // handing out a service whose registrations could only be swallowed.
    // Wayland is deliberately not in this loop any more: it has a real route
    // through the GlobalShortcuts portal (ADR-0011), so whether it refuses
    // depends on the portal rather than on the session, and `wayland_portal.rs`
    // pins both answers against a portal it controls.
    let desktop = DesktopEnvironment::Headless;
    let mut backend = LinuxBackend::with_desktop_environment(desktop);
    match backend.hotkeys() {
        Ok(_) => panic!("{desktop:?} has no display and no portal and must not hand out a hotkey service"),
        Err(CoreError::Invalid(message)) => assert!(
            message.contains(&format!("{desktop:?}")),
            "the refusal must name the session it refused for, got: {message}"
        ),
        Err(other) => panic!("unexpected error kind: {other:?}"),
    }
}
