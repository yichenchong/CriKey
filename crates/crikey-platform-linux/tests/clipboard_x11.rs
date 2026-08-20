//! The session clipboard driven against a *real* X server (spec 18.2).
//!
//! # Why a real server, and why a second client
//!
//! On X11 a clipboard is not a store. `X11Clipboard::write_text` takes
//! ownership of the `CLIPBOARD` selection and then has to answer every
//! `SelectionRequest` the server routes to it; nothing anywhere holds a copy.
//! So the only claim worth pinning is the one another application makes when it
//! pastes -- that a `ConvertSelection` for `UTF8_STRING` comes back with the
//! bytes that were copied -- and that claim cannot be checked from inside the
//! writer. Reading it back through the same handle proves nothing at all: the
//! implementation short-circuits a read while it is the owner and never touches
//! the wire.
//!
//! [`Reader`] is therefore a separate X client on its own connection, doing
//! exactly what a pasting application does: it creates its own window, asks the
//! server to convert the selection into a property on it, waits for the
//! `SelectionNotify` and reads the property. Every byte it sees travelled over
//! the wire, through the server, and out of the writer's selection-serving
//! thread.
//!
//! A missing or unusable `Xvfb` is a **test failure**, never a skip: there is no
//! `#[ignore]` and no early return here. A skipped test is not evidence. The
//! `XvfbServer` guard follows `hotkeys_x11.rs` and `window_x11.rs`, which
//! established the pattern.
//!
//! # One test, one display
//!
//! The clipboard implementation connects to whatever `DISPLAY` names, because
//! that is what an X client does and there is no seam to inject in the middle of
//! one. `DISPLAY` is process-wide state, so this file holds a single test: two
//! would race over which server the third-party crate's process-global
//! connection was made against.
//!
//! # Time
//!
//! Waits are bounded polling against an explicit deadline, never a fixed sleep
//! used as synchronisation: every loop ends on an observable -- the display
//! socket, or the event that answers a request -- so a regression becomes a
//! named failure instead of a stall or a flake.

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
use crikey_platform_linux::{DesktopEnvironment, LinuxBackend};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt, CreateWindowAux, EventMask, Time, WindowClass};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, NONE};

/// Ceiling on an `Xvfb` becoming connectable. Not a performance assertion: it
/// turns a server that never comes up into a named failure rather than a stall.
const SERVER_READY_LIMIT: Duration = Duration::from_secs(20);

/// Ceiling on the answer to one `ConvertSelection` arriving. Generous, because
/// it is not measuring anything: it only bounds the failure.
const REPLY_LIMIT: Duration = Duration::from_secs(10);

/// Gap between polls. Polling, not sleeping-as-synchronisation: every loop here
/// ends on its observable, never on elapsed time.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The largest selection this test ever asks for, in 32-bit words: the values
/// copied here are a handful of bytes, and a cap keeps a wrong answer a failure
/// rather than an allocation.
const MAX_REPLY_WORDS: u32 = 1_024;

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

/// A private `Xvfb`, killed when the test ends.
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
        Self {
            display: format!(":{number}"),
            socket: PathBuf::from(format!("/tmp/.X11-unix/X{number}")),
            child,
        }
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

// ---------------------------------------------------------------------------
// The pasting application
// ---------------------------------------------------------------------------

/// An independent X client that reads the `CLIPBOARD` selection the way an
/// application being pasted into does.
struct Reader {
    connection: RustConnection,
    window: u32,
    clipboard: Atom,
    utf8_string: Atom,
    /// The property the converted selection is delivered into. Named after this
    /// test so it cannot collide with the one the implementation uses on its own
    /// window.
    destination: Atom,
}

impl Reader {
    /// Connects to `server` and creates the window a selection is delivered on.
    fn connect(server: &XvfbServer) -> Self {
        let (connection, screen) = RustConnection::connect(Some(server.display()))
            .unwrap_or_else(|error| panic!("reader could not reach {}: {error}", server.display()));
        let root = connection.setup().roots[screen].root;
        let window = connection.generate_id().expect("generating a reader window id");
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
                // The selection arrives as a property change on this window.
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .expect("creating a reader window")
            .check()
            .expect("creating a reader window");

        let atom = |name: &str| -> Atom {
            connection
                .intern_atom(false, name.as_bytes())
                .unwrap_or_else(|error| panic!("interning {name}: {error}"))
                .reply()
                .unwrap_or_else(|error| panic!("interning {name}: {error}"))
                .atom
        };

        Self {
            clipboard: atom("CLIPBOARD"),
            utf8_string: atom("UTF8_STRING"),
            destination: atom("CRIKEY_TEST_SELECTION"),
            connection,
            window,
        }
    }

    /// The clipboard's text once it has settled on `expected`, or the last
    /// answer seen before [`REPLY_LIMIT`] ran out.
    ///
    /// A copy is not visible to another client the instant `write_text`
    /// returns. X orders requests within one connection, not across two: the
    /// writer's `SetSelectionOwner` and this reader's `ConvertSelection` are on
    /// separate connections, so the server may answer the request before it
    /// processes the claim, and the honest answer at that moment is that nobody
    /// owns the selection. Waiting for the handover is what makes the copy
    /// observable; asking once measures the scheduler.
    ///
    /// This still fails a writer that never serves the value, and still fails
    /// one that keeps serving a previous value, because the wait ends on the
    /// value rather than on a clock and the caller compares what it returns.
    fn pasted_once_settled(&self, expected: &str) -> Option<String> {
        let deadline = Instant::now() + REPLY_LIMIT;
        loop {
            let seen = self.clipboard_text();
            if seen.as_deref() == Some(expected) || Instant::now() >= deadline {
                return seen;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// The clipboard's text as this client sees it, or `None` when the server
    /// reports that nobody would convert the selection -- which is what an
    /// unowned selection looks like from the outside.
    fn clipboard_text(&self) -> Option<String> {
        // Cleared first so a stale value from an earlier request can never be
        // mistaken for a fresh answer.
        self.connection
            .delete_property(self.window, self.destination)
            .expect("clearing the destination property")
            .check()
            .expect("clearing the destination property");
        self.connection
            .convert_selection(
                self.window,
                self.clipboard,
                self.utf8_string,
                self.destination,
                Time::CURRENT_TIME,
            )
            .expect("requesting the selection");
        self.connection.flush().expect("flushing the request");

        let notify = self.await_selection_notify();
        if notify == NONE {
            return None;
        }
        assert_eq!(
            notify, self.destination,
            "the owner answered into a property this client never named"
        );

        let reply = self
            .connection
            .get_property(
                false,
                self.window,
                self.destination,
                AtomEnum::ANY,
                0,
                MAX_REPLY_WORDS,
            )
            .expect("reading the delivered selection")
            .reply()
            .expect("reading the delivered selection");
        assert_eq!(
            reply.bytes_after, 0,
            "the selection did not fit in one read; this test copies a handful of bytes"
        );
        Some(String::from_utf8(reply.value).expect("the clipboard delivered UTF-8"))
    }

    /// The property named by the `SelectionNotify` answering the last request,
    /// or [`NONE`] when the request was refused.
    fn await_selection_notify(&self) -> Atom {
        let deadline = Instant::now() + REPLY_LIMIT;
        loop {
            match self.connection.poll_for_event().expect("polling for the answer") {
                Some(Event::SelectionNotify(event)) => return event.property,
                // Anything else on this connection is not an answer to the one
                // request this client makes.
                Some(_) => {}
                None => thread::sleep(POLL_INTERVAL),
            }
            assert!(
                Instant::now() < deadline,
                "no SelectionNotify arrived within {REPLY_LIMIT:?}: the copy took ownership of nothing, \
                 or nothing is serving the selection"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The contract
// ---------------------------------------------------------------------------

/// A copy made through the backend's clipboard is what another application
/// pastes, and stays that way when a second copy replaces it.
///
/// Kills the bugs that a same-process read-back cannot see, because the
/// implementation answers those from its own memory without going near the
/// server:
///
/// * a copy that never takes ownership of `CLIPBOARD` at all -- the reader would
///   get no `SelectionNotify` with a property, exactly as it does before the
///   first write;
/// * a copy that takes ownership but serves nothing, or serves the wrong bytes
///   or the wrong encoding, to a real `ConvertSelection` for `UTF8_STRING`;
/// * a second copy that updates the value the writer remembers without
///   re-asserting ownership, so pasting still yields the first value (ICCCM
///   requires ownership to be re-asserted whenever the data changes).
///
/// It also pins the three answers around the write: an X11 session hands out a
/// clipboard and claims the capability, an unowned selection reads as *no text*
/// rather than as a failure, and a backend that detected no display server hands
/// out nothing even here, with a live server one connect call away -- the bug
/// where availability is decided by whatever `DISPLAY` a unit inherited instead
/// of by the session that was detected.
#[test]
fn a_copy_is_served_to_an_independent_client_that_pastes_it() {
    let server = XvfbServer::start();
    // The implementation is an X client and connects to whatever `DISPLAY`
    // names; there is no display argument to pass it, because a clipboard is not
    // a per-connection service the way a window query is. Safe to set process
    // wide because this file holds exactly one test -- see the module docs.
    env::set_var("DISPLAY", server.display());

    assert!(
        LinuxBackend::with_desktop_environment(DesktopEnvironment::Headless)
            .clipboard()
            .is_none(),
        "a backend that detected no display server must hand out no clipboard, \
         even with a reachable DISPLAY to connect to"
    );

    let backend = LinuxBackend::with_desktop_environment(DesktopEnvironment::X11);
    assert_eq!(
        backend.capability(Capability::Clipboard),
        CapabilityState::Available,
        "an X11 session must claim the clipboard it is about to use"
    );
    let clipboard = backend
        .clipboard()
        .expect("an X11 session with a running server has a clipboard");

    let reader = Reader::connect(&server);
    assert_eq!(
        reader.clipboard_text(),
        None,
        "nothing has taken the selection yet, so a pasting client must be told there is nothing"
    );
    assert_eq!(
        clipboard
            .read_text()
            .expect("reading an unowned selection is not an error"),
        None,
        "an unowned selection is empty, not broken"
    );

    clipboard.write_text("42").expect("an X11 session accepts a copy");
    assert_eq!(
        reader.pasted_once_settled("42").as_deref(),
        Some("42"),
        "the copied text must be what another application pastes"
    );

    // The second copy is the interesting one: ICCCM requires ownership to be
    // re-asserted when the data changes, and a writer that only updates its own
    // memory keeps serving the first value.
    clipboard
        .write_text("6 * 7 = 42")
        .expect("an X11 session accepts a second copy");
    assert_eq!(
        reader.pasted_once_settled("6 * 7 = 42").as_deref(),
        Some("6 * 7 = 42"),
        "a second copy must replace what a pasting application sees"
    );
    assert_eq!(
        clipboard
            .read_text()
            .expect("reading back the value just written"),
        Some("6 * 7 = 42".to_owned()),
        "the clipboard reports the text it is serving"
    );
}
