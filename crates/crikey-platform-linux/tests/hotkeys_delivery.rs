//! The X11 global-hotkey backend proven to *deliver* (spec 18.6).
//!
//! # Why this file exists beside `hotkeys_x11.rs`
//!
//! `hotkeys_x11.rs` proves the registration lifecycle: the grab is taken, one
//! chord is one grab, and a release hands the key back. Every one of those
//! tests would still pass against a service whose reader thread never called
//! the activation handler at all — the grabs would be live, the key would be
//! swallowed, and the launcher would be silent. That is exactly the failure a
//! reader-thread implementation hides, so it needs its own evidence: a *real*
//! key press, synthesised through the XTEST extension against the very display
//! the service is connected to, and observed arriving at the handler.
//!
//! # Why XTEST
//!
//! A grabbed key is only delivered when the server actually processes a key
//! event. Nothing short of the server generating one exercises the path under
//! test: keycode resolution, the lock-bit permutations the grab is taken for,
//! the stripping of those bits before matching, and the handler dispatch.
//! `xtest_fake_input` makes the server generate one for real, so the whole
//! path runs. An absent XTEST extension is a named panic, never a skip.
//!
//! # Isolation and time
//!
//! Each test owns a private `Xvfb` on its own display number, torn down by
//! [`XvfbServer`]'s `Drop` so a panicking test leaks nothing. The `XvfbServer`
//! pattern is the one `hotkeys_x11.rs` established and is reproduced here
//! rather than shared, because integration test binaries do not share modules.
//!
//! Nothing here sleeps as synchronisation. A delivery is awaited by blocking on
//! a channel against a bounded deadline, so a reader thread that never fires
//! fails by name instead of stalling the suite. The two silence assertions are
//! the unavoidable exception — proving a *non*-event needs a window — and each
//! is backed by a positive sentinel afterwards, so a globally broken pipeline
//! cannot make them pass vacuously.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crikey_platform::{Accelerator, HotkeyBinding, HotkeyService};
use crikey_platform_linux::X11HotkeyService;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as XprotoExt, Keycode, Window};
use x11rb::protocol::xtest::ConnectionExt as XtestExt;
use x11rb::rust_connection::RustConnection;

/// Ceiling on an `Xvfb` becoming connectable. Not a performance assertion: it
/// turns a server that never comes up into a named failure rather than a stall.
const SERVER_READY_LIMIT: Duration = Duration::from_secs(20);

/// Gap between readiness polls. Polling, not sleeping-as-synchronisation: the
/// loop ends on the observable (the display socket), not on elapsed time.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Ceiling on a synthesised press reaching the handler. Deliberately generous:
/// it exists so a reader thread that never delivers fails by name, not to
/// assert how fast delivery is.
const DELIVERY_LIMIT: Duration = Duration::from_secs(10);

/// How long "nothing arrives" is watched for. A negative claim needs a window;
/// every use of it is paired with a positive sentinel through the same channel,
/// so a pipeline that is broken outright cannot satisfy the test by silence.
const SILENCE_WINDOW: Duration = Duration::from_millis(750);

/// Hands out a distinct display number per server within this process.
static NEXT_DISPLAY_OFFSET: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// The two chords under test
// ---------------------------------------------------------------------------

/// `xproto` event type for a key press, as `xtest_fake_input` wants it.
const KEY_PRESS: u8 = 2;
/// `xproto` event type for a key release.
const KEY_RELEASE: u8 = 3;

const KEYSYM_SHIFT_L: u32 = 0xffe1;
const KEYSYM_CONTROL_L: u32 = 0xffe3;
const KEYSYM_CAPS_LOCK: u32 = 0xffe5;
const KEYSYM_ALT_L: u32 = 0xffe9;
const KEYSYM_NUM_LOCK: u32 = 0xff7f;
const KEYSYM_SPACE: u32 = 0x0020;
const KEYSYM_F5: u32 = 0xffc2;

/// The first chord: `Ctrl+Alt+Space`.
const CHORD_A: &str = "Ctrl+Alt+Space";
/// The keys that produce [`CHORD_A`]: held modifiers, then the key itself.
const CHORD_A_KEYS: (&[u32], u32) = (&[KEYSYM_CONTROL_L, KEYSYM_ALT_L], KEYSYM_SPACE);

/// The second chord: `Ctrl+Shift+F5`. Both its keycode *and* its modifier mask
/// differ from [`CHORD_A`], so a handler told the wrong registration cannot be
/// right by coincidence.
const CHORD_B: &str = "Ctrl+Shift+F5";
/// The keys that produce [`CHORD_B`].
const CHORD_B_KEYS: (&[u32], u32) = (&[KEYSYM_CONTROL_L, KEYSYM_SHIFT_L], KEYSYM_F5);

fn binding(text: &str) -> HotkeyBinding {
    HotkeyBinding {
        accelerator: text.to_owned(),
    }
}

/// The rendering the backend reports an activation of `text` as.
///
/// The handler is handed the *canonical* accelerator, so the expectation is
/// derived the same way rather than hard-coded: this asserts which binding
/// fired, not how the shared parser spells it.
fn canonical(text: &str) -> String {
    Accelerator::parse(text)
        .unwrap_or_else(|error| panic!("the test's own chord {text:?} does not parse: {error}"))
        .canonical()
}

// ---------------------------------------------------------------------------
// A private server
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

    /// A second client on this server, used to type at it.
    fn keyboard(&self) -> Keyboard {
        Keyboard::open(self.display())
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
// Typing at the server
// ---------------------------------------------------------------------------

/// A separate client that synthesises real key events through XTEST.
///
/// It is a *different* connection from the service's on purpose: the press has
/// to travel through the server and back out to the grabbing client, which is
/// precisely the path a physical key takes.
struct Keyboard {
    connection: RustConnection,
    root: Window,
    display: String,
}

impl Keyboard {
    /// Connects and confirms the server offers XTEST.
    ///
    /// A server without XTEST is a named panic: these tests would otherwise
    /// silently stop proving anything about delivery.
    fn open(display: &str) -> Self {
        let (connection, screen) = RustConnection::connect(Some(display))
            .unwrap_or_else(|error| panic!("the test keyboard could not connect to {display}: {error}"));
        let root = connection
            .setup()
            .roots
            .get(screen)
            .unwrap_or_else(|| panic!("X display {display} has no screen {screen}"))
            .root;

        connection
            .xtest_get_version(2, 2)
            .map(|cookie| cookie.reply())
            .unwrap_or_else(|error| {
                panic!(
                    "these tests need the XTEST extension to synthesise a key press; \
                     asking {display} for it failed: {error}"
                )
            })
            .unwrap_or_else(|error| {
                panic!("the X server on {display} does not offer a usable XTEST extension: {error}")
            });

        Self {
            connection,
            root,
            display: display.to_owned(),
        }
    }

    /// The keycode carrying `keysym`, resolved the way the backend resolves it:
    /// lowest shift level first, so both agree on which key to use.
    fn keycode(&self, keysym: u32) -> Keycode {
        self.keycode_of(keysym).unwrap_or_else(|| {
            panic!(
                "the keyboard mapping on {} has no key for keysym {keysym:#x}",
                self.display
            )
        })
    }

    /// As [`Keyboard::keycode`], but `None` when the server maps no such key.
    fn keycode_of(&self, keysym: u32) -> Option<Keycode> {
        let setup = self.connection.setup();
        let first = setup.min_keycode;
        let count = setup.max_keycode - first + 1;
        let mapping = self
            .connection
            .get_keyboard_mapping(first, count)
            .map(|cookie| cookie.reply())
            .unwrap_or_else(|error| {
                panic!("asking {} for its keyboard mapping failed: {error}", self.display)
            })
            .unwrap_or_else(|error| {
                panic!("asking {} for its keyboard mapping failed: {error}", self.display)
            });

        let per_keycode = usize::from(mapping.keysyms_per_keycode);
        if per_keycode == 0 {
            return None;
        }
        for column in 0..per_keycode {
            for (index, symbols) in mapping.keysyms.chunks(per_keycode).enumerate() {
                if symbols.get(column) == Some(&keysym) {
                    return Some(first.saturating_add(index as u8));
                }
            }
        }
        None
    }

    /// The modifier bit this server puts NumLock on, or `None` when it has no
    /// NumLock key at all.
    ///
    /// Determined exactly the way `hotkeys.rs` determines it, because the point
    /// of the lock test is to exercise the permutation the backend actually
    /// grabbed — assuming `Mod2Mask` would test a guess instead.
    fn num_lock_mask(&self) -> Option<u32> {
        let keycode = self.keycode_of(KEYSYM_NUM_LOCK)?;
        let modifiers = self
            .connection
            .get_modifier_mapping()
            .map(|cookie| cookie.reply())
            .unwrap_or_else(|error| {
                panic!("asking {} for its modifier mapping failed: {error}", self.display)
            })
            .unwrap_or_else(|error| {
                panic!("asking {} for its modifier mapping failed: {error}", self.display)
            });
        let per_modifier = usize::from(modifiers.keycodes_per_modifier()).max(1);
        modifiers
            .keycodes
            .chunks(per_modifier)
            .position(|codes| codes.contains(&keycode))
            .map(|index| 1u32 << index)
            .filter(|mask| *mask != 0)
    }

    /// Sends one synthetic key event and waits for the server to acknowledge
    /// it, so the events of a chord cannot be reordered against each other.
    fn fake(&self, type_: u8, keycode: Keycode) {
        self.connection
            .xtest_fake_input(type_, keycode, 0, self.root, 0, 0, 0)
            .map(|cookie| cookie.check())
            .unwrap_or_else(|error| {
                panic!(
                    "synthesising key event {type_} on {} failed: {error}",
                    self.display
                )
            })
            .unwrap_or_else(|error| {
                panic!(
                    "synthesising key event {type_} on {} failed: {error}",
                    self.display
                )
            });
    }

    /// Presses and releases one key, which is how a lock is toggled.
    fn tap(&self, keysym: u32) {
        let keycode = self.keycode(keysym);
        self.fake(KEY_PRESS, keycode);
        self.fake(KEY_RELEASE, keycode);
    }

    /// Types a whole chord: modifiers down, key down, key up, modifiers up.
    ///
    /// Both halves are synthesised because a grab that is never released would
    /// leave the server holding the keyboard for every later test in the file.
    fn chord(&self, chord: (&[u32], u32)) {
        let (modifiers, key) = chord;
        let held: Vec<Keycode> = modifiers.iter().map(|keysym| self.keycode(*keysym)).collect();
        let key = self.keycode(key);

        for keycode in &held {
            self.fake(KEY_PRESS, *keycode);
        }
        self.fake(KEY_PRESS, key);
        self.fake(KEY_RELEASE, key);
        for keycode in held.iter().rev() {
            self.fake(KEY_RELEASE, *keycode);
        }
    }
}

// ---------------------------------------------------------------------------
// Observing activations
// ---------------------------------------------------------------------------

/// The channel an installed handler reports activations down.
///
/// The test keeps its own [`Sender`] alive so that clearing the handler — which
/// drops the handler's clone — cannot turn a silence assertion into a
/// `Disconnected` that would pass for the wrong reason.
struct Activations {
    sender: Sender<String>,
    receiver: Receiver<String>,
}

impl Activations {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self { sender, receiver }
    }

    /// Installs a handler that reports the accelerator it was handed.
    fn install(&self, service: &mut X11HotkeyService) {
        let sender = self.sender.clone();
        service.set_activation_handler(Some(Box::new(move |binding: &HotkeyBinding| {
            let _ = sender.send(binding.accelerator.clone());
        })));
    }

    /// The next activation, or a named failure once the deadline passes.
    fn next(&self, what: &str) -> String {
        match self.receiver.recv_timeout(DELIVERY_LIMIT) {
            Ok(accelerator) => accelerator,
            Err(RecvTimeoutError::Timeout) => panic!(
                "no activation reached the handler within {DELIVERY_LIMIT:?} after synthesising {what}: \
                 the grab is live but the reader thread delivered nothing"
            ),
            Err(RecvTimeoutError::Disconnected) => {
                panic!("the activation channel closed before {what} was delivered")
            }
        }
    }

    /// Fails unless nothing arrives within the bounded silence window.
    fn expect_silence(&self, what: &str) {
        match self.receiver.recv_timeout(SILENCE_WINDOW) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(accelerator) => panic!("{what} but {accelerator} was delivered anyway"),
            Err(RecvTimeoutError::Disconnected) => {
                panic!("the activation channel closed while checking that {what}")
            }
        }
    }

    /// Fails unless no *further* activation follows one already received.
    fn expect_no_more(&self, what: &str) {
        if let Ok(extra) = self.receiver.recv_timeout(SILENCE_WINDOW) {
            panic!("{what} was delivered more than once; the repeat named {extra}");
        }
    }
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

/// A synthesised press of a registered chord reaches the activation handler.
///
/// This is the guarantee every other test in `hotkeys_x11.rs` assumes and none
/// of them can see: a service whose reader thread never called the handler, or
/// which grabbed a keycode the chord does not produce, passes the entire
/// lifecycle suite while the hotkey is stone dead. Here the server generates a
/// real `KeyPress`, so keycode resolution, the grab, the reader thread and the
/// dispatch all have to work for the channel to receive anything.
#[test]
fn a_synthesised_press_of_a_registered_chord_reaches_the_handler() {
    let server = XvfbServer::start();
    let keyboard = server.keyboard();
    let mut service = server.service();
    let activations = Activations::new();

    activations.install(&mut service);
    service
        .register(&binding(CHORD_A))
        .unwrap_or_else(|error| panic!("registering {CHORD_A} failed: {error}"));

    keyboard.chord(CHORD_A_KEYS);

    assert_eq!(
        activations.next(CHORD_A),
        canonical(CHORD_A),
        "the handler was told about a chord other than the one that was pressed"
    );
    activations.expect_no_more(CHORD_A);
}

/// The handler is told *which* chord fired, not merely that one did.
///
/// With two registrations live, a reader thread that hands the handler whatever
/// entry it finds first would still deliver on every press — and the launcher
/// would launch the wrong thing. Pressing the second chord must name the second
/// binding. The two differ in keycode *and* in modifier mask, so neither a
/// mask-blind nor a keycode-blind match can pass by accident.
#[test]
fn the_delivered_binding_names_the_chord_that_was_actually_pressed() {
    let server = XvfbServer::start();
    let keyboard = server.keyboard();
    let mut service = server.service();
    let activations = Activations::new();

    activations.install(&mut service);
    service
        .register(&binding(CHORD_A))
        .unwrap_or_else(|error| panic!("registering {CHORD_A} failed: {error}"));
    service
        .register(&binding(CHORD_B))
        .unwrap_or_else(|error| panic!("registering {CHORD_B} failed: {error}"));

    keyboard.chord(CHORD_B_KEYS);
    assert_eq!(
        activations.next(CHORD_B),
        canonical(CHORD_B),
        "pressing the second chord delivered the wrong registration"
    );
    activations.expect_no_more(CHORD_B);

    // And the first is still its own binding, so this is a real discrimination
    // rather than a service that always answers with the last thing registered.
    keyboard.chord(CHORD_A_KEYS);
    assert_eq!(
        activations.next(CHORD_A),
        canonical(CHORD_A),
        "pressing the first chord delivered the wrong registration"
    );
}

/// An active lock modifier does not defeat delivery.
///
/// The backend grabs every combination of CapsLock and NumLock and strips those
/// bits before matching, precisely so a user with CapsLock on does not lose the
/// hotkey. Nothing else in the suite exercises those extra grabs: they could be
/// taken against the wrong masks, or the stripping could be wrong, and every
/// other test would stay green. NumLock's bit is looked up rather than assumed,
/// the same way the backend looks it up, so this drives the permutation that
/// was actually grabbed.
#[test]
fn a_chord_still_delivers_with_the_lock_modifiers_on() {
    let server = XvfbServer::start();
    let keyboard = server.keyboard();
    let mut service = server.service();
    let activations = Activations::new();

    activations.install(&mut service);
    service
        .register(&binding(CHORD_A))
        .unwrap_or_else(|error| panic!("registering {CHORD_A} failed: {error}"));

    keyboard.tap(KEYSYM_CAPS_LOCK);
    keyboard.chord(CHORD_A_KEYS);
    assert_eq!(
        activations.next("the chord with CapsLock on"),
        canonical(CHORD_A),
        "CapsLock changed which binding the press was matched against"
    );

    if keyboard.num_lock_mask().is_some() {
        keyboard.tap(KEYSYM_NUM_LOCK);
        keyboard.chord(CHORD_A_KEYS);
        assert_eq!(
            activations.next("the chord with CapsLock and NumLock on"),
            canonical(CHORD_A),
            "the CapsLock+NumLock permutation is not delivered"
        );

        keyboard.tap(KEYSYM_CAPS_LOCK);
        keyboard.chord(CHORD_A_KEYS);
        assert_eq!(
            activations.next("the chord with NumLock alone on"),
            canonical(CHORD_A),
            "the NumLock permutation is not delivered"
        );
        keyboard.tap(KEYSYM_NUM_LOCK);
    } else {
        keyboard.tap(KEYSYM_CAPS_LOCK);
    }

    // Back to no locks at all: the plain permutation still works, which is what
    // makes the assertions above about the lock bits rather than about luck.
    keyboard.chord(CHORD_A_KEYS);
    assert_eq!(
        activations.next("the chord with the locks off again"),
        canonical(CHORD_A),
        "turning the locks back off broke delivery"
    );
}

/// Clearing the handler stops delivery without dropping the registration.
///
/// `set_activation_handler(None)` must detach the callback and nothing else:
/// the launcher clears its handler while reconfiguring. This separates the two
/// failure modes that look alike from outside — a press that still fires a
/// stale handler, and a clear implemented as a teardown that quietly ungrabs.
/// Re-installing a handler and getting exactly one delivery proves the press
/// made during the gap was dropped rather than queued, and the `unregister`
/// afterwards proves the grab was live throughout.
#[test]
fn a_cleared_handler_stops_delivery_while_the_registration_stays_live() {
    let server = XvfbServer::start();
    let keyboard = server.keyboard();
    let mut service = server.service();
    let activations = Activations::new();

    activations.install(&mut service);
    service
        .register(&binding(CHORD_A))
        .unwrap_or_else(|error| panic!("registering {CHORD_A} failed: {error}"));

    // Delivery works before the handler is cleared, so the silence below is
    // about the clear and not about a pipeline that never worked.
    keyboard.chord(CHORD_A_KEYS);
    assert_eq!(activations.next(CHORD_A), canonical(CHORD_A));

    service.set_activation_handler(None);
    keyboard.chord(CHORD_A_KEYS);
    activations.expect_silence("the activation handler was cleared");

    activations.install(&mut service);
    keyboard.chord(CHORD_A_KEYS);
    assert_eq!(
        activations.next(CHORD_A),
        canonical(CHORD_A),
        "a re-installed handler receives nothing, so the clear tore the registration down"
    );
    activations.expect_no_more("the press made while the handler was cleared");

    service.unregister(&binding(CHORD_A)).unwrap_or_else(|error| {
        panic!("{CHORD_A} was not still registered after the handler was cleared: {error}")
    });
}

/// An unregistered chord no longer fires.
///
/// `hotkeys_x11.rs` proves a release makes the *bookkeeping* forget the chord
/// and lets another client take the grab. It cannot prove this client stopped
/// receiving the key, which is the user-visible half: a backend that forgot to
/// ungrab, or one that ungrabbed only the bare mask and left the lock
/// permutations behind, would go on swallowing and firing. A second chord stays
/// registered as a sentinel, so the silence is a statement about the released
/// chord rather than about a service that has stopped delivering anything.
#[test]
fn a_released_chord_no_longer_reaches_the_handler() {
    let server = XvfbServer::start();
    let keyboard = server.keyboard();
    let mut service = server.service();
    let activations = Activations::new();

    activations.install(&mut service);
    service
        .register(&binding(CHORD_A))
        .unwrap_or_else(|error| panic!("registering {CHORD_A} failed: {error}"));
    service
        .register(&binding(CHORD_B))
        .unwrap_or_else(|error| panic!("registering {CHORD_B} failed: {error}"));

    keyboard.chord(CHORD_A_KEYS);
    assert_eq!(activations.next(CHORD_A), canonical(CHORD_A));

    service
        .unregister(&binding(CHORD_A))
        .unwrap_or_else(|error| panic!("releasing {CHORD_A} failed: {error}"));

    keyboard.chord(CHORD_A_KEYS);
    activations.expect_silence("the chord was released");

    // The sentinel: the service is still delivering, so the silence above was
    // the ungrab and not a dead reader thread.
    keyboard.chord(CHORD_B_KEYS);
    assert_eq!(
        activations.next(CHORD_B),
        canonical(CHORD_B),
        "releasing one chord stopped delivery of another"
    );
}
