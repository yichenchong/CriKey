//! Global hotkeys under Wayland, against a real `GlobalShortcuts` portal
//! (spec 6.1, 18.6; ADR-0011).
//!
//! Every test here runs a private `dbus-daemon` and, in most of them, serves
//! the portal interface on it from inside the test process. That is the point:
//! a Wayland hotkey is a conversation with another process, and a test that
//! stubbed the conversation out would pin nothing about whether the launcher
//! can hold it. Nothing here touches the developer's own session bus, so the
//! suite behaves the same on a GNOME laptop and on a headless runner.
//!
//! An absent `dbus-daemon` is a named panic, never a skip -- the same rule the
//! X11 tests apply to `Xvfb`.
//!
//! Deliberate non-goals: no test asserts what a *particular* desktop's portal
//! does with a binding request. Whether GNOME shows a dialog, remembers an
//! earlier answer or rewrites the trigger is that portal's business; what is
//! pinned here is that this backend asks correctly, believes only what it is
//! told, and never claims a shortcut it was not given.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crikey_platform::{HotkeyBinding, HotkeyService};
use crikey_platform_linux::WaylandHotkeyService;
use zbus::blocking::connection::Builder;
use zbus::blocking::Connection;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

/// The portal's own names, spelled out here rather than imported: a test that
/// borrowed the constants from the code under test would keep passing if the
/// backend started talking to the wrong interface.
const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SHORTCUTS_INTERFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";

/// How long a test waits for something another process has to do. Generous
/// enough for a loaded runner, short enough that a hang is a failure rather
/// than a stalled suite.
const LIMIT: Duration = Duration::from_secs(10);

/// The longest a `Drop` may take. A service that joins a reader nobody woke
/// blocks forever, so anything in this neighbourhood is the bug, not slowness.
const DROP_LIMIT: Duration = Duration::from_secs(5);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn binding(accelerator: &str) -> HotkeyBinding {
    HotkeyBinding {
        accelerator: accelerator.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// A private bus
// ---------------------------------------------------------------------------

/// A `dbus-daemon` of this test's own, torn down with the fixture so no
/// daemon and no bound name leaks to the next test.
struct PrivateBus {
    child: Child,
    address: String,
}

impl PrivateBus {
    /// Panics -- loudly and by name -- if `dbus-daemon` is absent or never
    /// prints an address.
    fn start() -> Self {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--print-address", "--nofork"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| {
                panic!(
                    "these tests require a real message bus; spawning `dbus-daemon` failed: \
                     {error}. A missing dbus-daemon is a test failure, never a skip."
                )
            });

        let stdout = child
            .stdout
            .take()
            .expect("the daemon was spawned with a piped stdout");
        let mut address = String::new();
        BufReader::new(stdout)
            .read_line(&mut address)
            .expect("dbus-daemon prints its address on the first line of stdout");
        let address = address.trim().to_owned();
        assert!(
            !address.is_empty(),
            "dbus-daemon started but named no address, so nothing can connect to it"
        );

        Self { child, address }
    }

    fn connect(&self) -> Connection {
        Builder::address(self.address.as_str())
            .expect("the address dbus-daemon printed is a usable bus address")
            .build()
            .expect("the private bus accepts a client")
    }

    /// Connects and serves the portal interface, returning the connection that
    /// owns it. Dropping that connection retires the portal.
    fn serve_portal(&self, log: PortalLog) -> Connection {
        Builder::address(self.address.as_str())
            .expect("the address dbus-daemon printed is a usable bus address")
            .name(PORTAL_NAME)
            .expect("the portal's well-known name is valid")
            .serve_at(PORTAL_PATH, GlobalShortcuts { log })
            .expect("the portal object can be served")
            .build()
            .expect("the private bus accepts the portal")
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// A portal to talk to
// ---------------------------------------------------------------------------

/// What the fake portal saw, so a test can assert about the conversation
/// rather than only about its outcome.
#[derive(Debug, Default)]
struct PortalState {
    /// One entry per `BindShortcuts` call, holding the ids it asked for. The
    /// *number* of entries is what proves a duplicate registration did not
    /// reach the portal.
    binds: Vec<Vec<String>>,
    sessions: Vec<String>,
    closed: Vec<String>,
    client: Option<String>,
    /// When set, every bind is answered with a refusal instead of a success.
    refuse: bool,
    /// When set, a bind succeeds but reports an empty bound set: the portal
    /// took the request and honoured none of it.
    bind_nothing: bool,
}

#[derive(Clone, Debug, Default)]
struct PortalLog {
    state: Arc<Mutex<PortalState>>,
}

impl PortalLog {
    fn refuse_binds(&self) {
        lock(&self.state).refuse = true;
    }

    fn bind_nothing(&self) {
        lock(&self.state).bind_nothing = true;
    }

    fn binds(&self) -> Vec<Vec<String>> {
        lock(&self.state).binds.clone()
    }

    fn sessions(&self) -> Vec<String> {
        lock(&self.state).sessions.clone()
    }

    fn client(&self) -> String {
        lock(&self.state)
            .client
            .clone()
            .expect("the portal has been called at least once")
    }

    /// Blocks until `ready` holds, so a test can wait for something the portal
    /// is told about asynchronously without sleeping blind.
    fn wait_for(&self, what: &str, ready: impl Fn(&PortalState) -> bool) {
        let deadline = Instant::now() + LIMIT;
        while Instant::now() < deadline {
            if ready(&lock(&self.state)) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("the portal never saw {what} within {LIMIT:?}");
    }
}

struct GlobalShortcuts {
    log: PortalLog,
}

#[zbus::interface(name = "org.freedesktop.portal.GlobalShortcuts")]
impl GlobalShortcuts {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }

    async fn create_session(
        &self,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let client = sender_of(&header);
        let request = request_path(&client, &text(&options, "handle_token"));
        let session = format!(
            "/org/freedesktop/portal/desktop/session/{}/{}",
            escaped(&client),
            text(&options, "session_handle_token")
        );

        {
            let mut state = lock(&self.log.state);
            state.client = Some(client.clone());
            state.sessions.push(session.clone());
        }

        // A real portal answers `Close` on the session it just handed out, and
        // this backend closes every session it replaces.
        server
            .at(
                path(&session),
                PortalSession {
                    log: self.log.clone(),
                    path: session.clone(),
                },
            )
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;

        let mut results = HashMap::new();
        results.insert("session_handle".to_owned(), owned(Value::from(session)));
        respond(emitter.connection(), &client, &request, 0, results).await;
        Ok(path(&request))
    }

    async fn bind_shortcuts(
        &self,
        _session: OwnedObjectPath,
        shortcuts: Vec<(String, HashMap<String, OwnedValue>)>,
        _parent_window: String,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let client = sender_of(&header);
        let request = request_path(&client, &text(&options, "handle_token"));
        let asked: Vec<String> = shortcuts.iter().map(|(id, _)| id.clone()).collect();

        let (refuse, bind_nothing) = {
            let mut state = lock(&self.log.state);
            state.binds.push(asked.clone());
            (state.refuse, state.bind_nothing)
        };

        let (code, bound) = match (refuse, bind_nothing) {
            (true, _) => (1, Vec::new()),
            (false, true) => (0, Vec::new()),
            (false, false) => (0, asked),
        };
        let mut results = HashMap::new();
        let listed: Vec<(String, HashMap<String, Value<'_>>)> = bound
            .into_iter()
            .map(|id| {
                let mut fields = HashMap::new();
                fields.insert("trigger_description".to_owned(), Value::from("a chord"));
                (id, fields)
            })
            .collect();
        results.insert("shortcuts".to_owned(), owned(Value::from(listed)));
        respond(emitter.connection(), &client, &request, code, results).await;
        Ok(path(&request))
    }
}

/// The `Session` object a created session hands out, present so that closing
/// one is observable: releasing a portal session is how a rebinding retires
/// the shortcuts it replaced, and a test cannot see that happen otherwise.
struct PortalSession {
    log: PortalLog,
    path: String,
}

#[zbus::interface(name = "org.freedesktop.portal.Session")]
impl PortalSession {
    fn close(&self) {
        lock(&self.log.state).closed.push(self.path.clone());
    }
}

/// Emits the `Response` a portal request is really answered by.
async fn respond(
    connection: &zbus::Connection,
    client: &str,
    request: &str,
    code: u32,
    results: HashMap<String, OwnedValue>,
) {
    connection
        .emit_signal(
            Some(client),
            request,
            REQUEST_INTERFACE,
            "Response",
            &(code, results),
        )
        .await
        .expect("the portal can answer a request it was sent");
}

fn sender_of(header: &Header<'_>) -> String {
    header
        .sender()
        .expect("a bus message carries its sender")
        .as_str()
        .to_owned()
}

/// The object path a request with this token is answered on, built exactly as
/// the portal specification says: the caller's unique name with its punctuation
/// flattened, then the token the caller chose.
fn request_path(client: &str, token: &str) -> String {
    format!(
        "/org/freedesktop/portal/desktop/request/{}/{token}",
        escaped(client)
    )
}

fn escaped(unique_name: &str) -> String {
    unique_name.trim_start_matches(':').replace('.', "_")
}

fn path(value: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(value.to_owned()).expect("the portal builds valid object paths")
}

fn owned(value: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(value).expect("a portal result value can be owned")
}

/// One string out of an options vardict, or the empty string when the caller
/// omitted it -- which is itself a legitimate thing for a caller to do.
fn text(options: &HashMap<String, OwnedValue>, key: &str) -> String {
    options
        .get(key)
        .and_then(|value| Value::try_from(value).ok())
        .and_then(|value| String::try_from(value).ok())
        .unwrap_or_default()
}

/// A portal, a client connection and a live service, which is what most of
/// these tests need before they can assert anything.
struct Fixture {
    _bus: PrivateBus,
    _portal: Connection,
    log: PortalLog,
    service: Option<WaylandHotkeyService>,
}

impl Fixture {
    fn start() -> Self {
        let bus = PrivateBus::start();
        let log = PortalLog::default();
        let portal = bus.serve_portal(log.clone());
        let service = WaylandHotkeyService::connect_on(bus.connect())
            .expect("a portal that answers its version property carries a hotkey service");
        Self {
            _bus: bus,
            _portal: portal,
            log,
            service: Some(service),
        }
    }

    fn service(&mut self) -> &mut WaylandHotkeyService {
        self.service.as_mut().expect("the service is still held")
    }

    /// Fires an activation the way the portal does: a unicast signal naming the
    /// shortcut id, addressed to the client that bound it.
    fn activate(&self, shortcut_id: &str) {
        let session = self
            .log
            .sessions()
            .last()
            .cloned()
            .expect("a session was created before anything could be bound");
        self._portal
            .emit_signal(
                Some(self.log.client()),
                PORTAL_PATH,
                SHORTCUTS_INTERFACE,
                "Activated",
                &(
                    path(&session),
                    shortcut_id,
                    0u64,
                    HashMap::<String, Value<'_>>::new(),
                ),
            )
            .expect("the portal can signal an activation");
    }
}

// ---------------------------------------------------------------------------
// Without a portal
// ---------------------------------------------------------------------------

/// A bus with no portal on it refuses by name instead of handing out a service.
///
/// Kills the honesty bug this whole backend exists to avoid: a service that
/// constructs anyway, accepts registrations and swallows them, leaving a
/// launcher that reports a working hotkey and never activates. The refusal has
/// to name the portal, because "hotkeys did not work" is not something a user
/// can act on and "no GlobalShortcuts portal answered" is.
#[test]
fn a_session_bus_with_no_portal_refuses_to_hand_out_a_hotkey_service() {
    let bus = PrivateBus::start();

    let outcome = WaylandHotkeyService::connect_on(bus.connect());

    let Err(error) = outcome else {
        panic!("a bus with no GlobalShortcuts portal must not yield a hotkey service");
    };
    let message = error.to_string();
    assert!(
        message.contains("GlobalShortcuts"),
        "the refusal must name what was missing, got: {message}"
    );
}

// ---------------------------------------------------------------------------
// With one
// ---------------------------------------------------------------------------

/// Registering a chord really binds it through the portal, and an activation
/// comes back through the ordinary handler.
///
/// Kills the backend that talks to the portal but never delivers: the binding
/// is only worth having if `Activated` reaches the callback the launcher
/// installed, spelled the way the launcher registered it.
#[test]
fn a_registered_chord_is_bound_through_the_portal_and_activates_the_handler() {
    let mut fixture = Fixture::start();
    let (activations, received) = mpsc::channel();
    fixture
        .service()
        .set_activation_handler(Some(Box::new(move |binding: &HotkeyBinding| {
            let _ = activations.send(binding.accelerator.clone());
        })));

    fixture
        .service()
        .register(&binding("Ctrl+Alt+Space"))
        .expect("a portal that accepts the binding makes the chord live");

    let binds = fixture.log.binds();
    assert_eq!(
        binds.len(),
        1,
        "one registration is one bind request, got {binds:?}"
    );
    let shortcut_id = binds[0]
        .first()
        .cloned()
        .expect("the bind request names the shortcut it wants");

    fixture.activate(&shortcut_id);

    let delivered = received
        .recv_timeout(LIMIT)
        .expect("an Activated signal reaches the installed handler");
    assert_eq!(
        delivered, "Ctrl+Alt+Space",
        "the handler is handed the accelerator that was registered, not the portal's id for it"
    );
}

/// Registering the same chord twice is one binding, however it is spelled.
///
/// Kills two bugs with one assertion. A second bind request for a chord the
/// portal already granted puts a second approval dialog in front of the user
/// for something they already approved, and on the portals that grant it, the
/// chord then fires twice per press. Keying the check on the canonical
/// rendering is what makes `alt+ctrl+SPACE` the same registration as
/// `Ctrl+Alt+Space` rather than a second one.
#[test]
fn registering_one_chord_twice_binds_it_once_however_it_is_spelled() {
    let mut fixture = Fixture::start();
    fixture
        .service()
        .register(&binding("Ctrl+Alt+Space"))
        .expect("the first registration binds");

    fixture
        .service()
        .register(&binding("alt+ctrl+SPACE"))
        .expect("re-registering a live chord succeeds: the caller asked for a binding and has one");

    let binds = fixture.log.binds();
    assert_eq!(
        binds.len(),
        1,
        "the duplicate must not reach the portal at all, got {binds:?}"
    );
}

/// A second, different chord is bound alongside the first.
///
/// Kills the shortcut a bind-once-per-session portal invites: dropping the
/// accelerators already held when a new one is added. The portal permits one
/// `BindShortcuts` per session, so the second registration has to carry the
/// whole set on a new session, and both ids have to be in it.
#[test]
fn a_second_chord_is_bound_alongside_the_first_rather_than_replacing_it() {
    let mut fixture = Fixture::start();
    fixture
        .service()
        .register(&binding("Ctrl+Alt+Space"))
        .expect("the first registration binds");

    fixture
        .service()
        .register(&binding("Ctrl+Alt+K"))
        .expect("the second registration binds");

    let binds = fixture.log.binds();
    assert_eq!(binds.len(), 2, "a new chord needs a new bind, got {binds:?}");
    assert_eq!(
        binds[1].len(),
        2,
        "the second bind must carry both chords, or the first one is silently released: {binds:?}"
    );
    assert!(
        binds[1].iter().any(|id| binds[0].contains(id)),
        "the second bind must still include the first chord: {binds:?}"
    );
}

/// A portal that refuses the request leaves nothing claimed.
///
/// Kills the optimistic registration: a user who dismisses the dialog must get
/// an error naming the dismissal, and the accelerator must not be listed as
/// live afterwards -- otherwise `unregister` would "succeed" against a binding
/// that never existed and the launcher would believe it had a hotkey.
#[test]
fn a_refused_binding_is_reported_and_leaves_the_chord_unregistered() {
    let mut fixture = Fixture::start();
    fixture.log.refuse_binds();

    let refusal = fixture.service().register(&binding("Ctrl+Alt+Space"));

    let Err(error) = refusal else {
        panic!("a portal refusal must not be reported as a live hotkey");
    };
    let message = error.to_string();
    assert!(
        message.contains("dismissed"),
        "the refusal must say the user turned it down, got: {message}"
    );
    assert!(
        fixture.service().unregister(&binding("Ctrl+Alt+Space")).is_err(),
        "a chord the portal refused must not be listed as registered"
    );
}

/// A portal that answers "bound: nothing" is not a successful registration.
///
/// Kills the trusting reader: response code 0 means the request completed, not
/// that the shortcut is live. The portal reports the subset it actually bound,
/// and a subset without the requested chord is a refusal wearing a success
/// code.
#[test]
fn a_binding_the_portal_left_out_of_its_answer_is_not_reported_as_live() {
    let mut fixture = Fixture::start();
    fixture.log.bind_nothing();

    let outcome = fixture.service().register(&binding("Ctrl+Alt+Space"));

    let Err(error) = outcome else {
        panic!("a shortcut the portal did not bind must not be reported as registered");
    };
    let message = error.to_string();
    assert!(
        message.contains("did not bind"),
        "the failure must say the portal left the chord out, got: {message}"
    );
}

/// Unregistering releases the portal session the chord was bound on.
///
/// Kills the bookkeeping-only release: dropping the entry from a map while the
/// portal session stays open leaves the compositor still holding the key, so
/// the chord keeps firing at a launcher that thinks it let go of it.
#[test]
fn unregistering_a_chord_retires_the_session_it_was_bound_on() {
    let mut fixture = Fixture::start();
    fixture
        .service()
        .register(&binding("Ctrl+Alt+Space"))
        .expect("the registration binds");
    let bound_session = fixture
        .log
        .sessions()
        .last()
        .cloned()
        .expect("the registration created a session");

    fixture
        .service()
        .unregister(&binding("Ctrl+Alt+Space"))
        .expect("a chord this service bound can be released");

    fixture.log.wait_for("the bound session closed", |state| {
        state.closed.contains(&bound_session)
    });
}

/// Dropping the service stops its reader promptly.
///
/// Kills the `Drop` that hangs. The reader blocks on a bus socket, so a `Drop`
/// that joins it without first sending something the reader will see waits
/// forever, and the launcher never exits. This test fails by timing out at
/// exactly the point that regression is introduced.
#[test]
fn dropping_the_service_stops_its_reader_without_blocking() {
    let mut fixture = Fixture::start();
    fixture
        .service()
        .register(&binding("Ctrl+Alt+Space"))
        .expect("the registration binds");

    let service = fixture.service.take().expect("the service is still held");
    let started = Instant::now();
    drop(service);
    let elapsed = started.elapsed();

    assert!(
        elapsed < DROP_LIMIT,
        "dropping the service took {elapsed:?}, which means its reader was joined without being \
         woken"
    );
}

/// Dropping the service also gives the portal its session back.
///
/// Kills the shutdown that leaks: a session left open holds the compositor's
/// binding for as long as the portal keeps the session alive, so a restarted
/// launcher meets its own stale shortcut.
#[test]
fn dropping_the_service_closes_the_session_it_still_held() {
    let mut fixture = Fixture::start();
    fixture
        .service()
        .register(&binding("Ctrl+Alt+Space"))
        .expect("the registration binds");
    let live_session = fixture
        .log
        .sessions()
        .last()
        .cloned()
        .expect("the registration created a session");

    drop(fixture.service.take().expect("the service is still held"));

    fixture.log.wait_for("the live session closed", |state| {
        state.closed.contains(&live_session)
    });
}
