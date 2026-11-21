//! Global hotkeys under Wayland, through the `org.freedesktop.portal.GlobalShortcuts`
//! XDG desktop portal (spec 6.1, 18.6; ADR-0011).
//!
//! Wayland has no equivalent of X11's `GrabKey`: a client cannot take a key
//! away from the compositor, by design. The portal is the sanctioned inversion
//! of that -- the compositor keeps the grab, the user approves the binding, and
//! the application is told after the fact that its shortcut fired. Everything
//! here is that conversation, held over the session bus.
//!
//! # Why a reader thread, again
//!
//! [`HotkeyService`] is a callback contract, and an activation arrives as a
//! D-Bus signal that somebody has to be reading for. Nothing else in the
//! launcher pumps this connection, so without the reader the portal would hold
//! a live binding whose activations nobody ever sees -- the same failure the
//! X11 backend's reader exists to prevent, arriving over a socket instead of a
//! display.
//!
//! The reader iterates *every* incoming message rather than a filtered signal
//! stream, because it has two jobs. Activations are one. The other is the
//! portal's request/response pattern: `CreateSession` and `BindShortcuts`
//! return an object path immediately and deliver their real answer later as a
//! `Response` signal on that path, so a registration is only complete once the
//! reader has routed that signal back to the thread waiting for it. One reader
//! owning the message stream is what makes those two consumers coexist.
//!
//! Stopping it uses the same shape as the X11 service's throwaway window: the
//! service emits a signal addressed to its own unique bus name, which the bus
//! routes straight back, so the blocked reader wakes on an observable rather
//! than on a poll timeout. `Drop` joins the reader only when that wake was
//! actually sent; a wake that could not be sent detaches the thread instead,
//! because a launcher that hangs on shutdown is worse than a thread that
//! outlives its owner by the length of one connection teardown.
//!
//! Each accelerator owns an independently approved portal session. Acquiring a
//! replacement never releases an old chord, and removing one binding never
//! opens an approval dialog for unrelated bindings.

use std::collections::HashMap;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crikey_core::{CoreError, Result};
use crikey_platform::{
    Accelerator, HotkeyActivationHandler, HotkeyBinding, HotkeyCancellation, HotkeyService,
};
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::message::Type as MessageType;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::MatchRule;

use crate::hotkeys::{invalid, keysym_of, lock, parse};

// ---------------------------------------------------------------------------
// The portal's own names
// ---------------------------------------------------------------------------

/// The portal is always this well-known name on the session bus; a
/// D-Bus-activatable service, so an installed portal answers even when no
/// portal process is running yet.
const PORTAL_BUS_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SHORTCUTS_INTERFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const SESSION_INTERFACE: &str = "org.freedesktop.portal.Session";

/// The prefix every `Request` object path is built from. Predicting the path
/// rather than waiting for the method reply is what the portal's `handle_token`
/// option exists for: the `Response` signal can legitimately arrive before the
/// reply that names the path it arrived on.
const REQUEST_PATH_PREFIX: &str = "/org/freedesktop/portal/desktop/request";

/// The private signal the service sends itself to wake its reader. It is not
/// part of any published interface: the bus routes it back by destination, and
/// nothing else ever sees it.
const WAKE_INTERFACE: &str = "org.crikey.internal.HotkeyReader";
const WAKE_PATH: &str = "/org/crikey/internal/hotkeys";
const WAKE_MEMBER: &str = "Stop";

/// The portal's `Response` codes.
const RESPONSE_SUCCESS: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;

/// How long a request with no user interaction may take. Creating a session is
/// bookkeeping inside the portal, so a portal that has not answered in this
/// long is not going to.
const SESSION_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a binding may take. Generous on purpose: `BindShortcuts` typically
/// puts a dialog in front of the user, and the wait is that person reading it.
const BIND_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// Probing
// ---------------------------------------------------------------------------

/// Whether the `GlobalShortcuts` portal answers on this session bus.
///
/// Reads the interface's `version` property, because that is the cheapest
/// question only a real portal can answer: the well-known name is
/// D-Bus-activatable, so its mere presence in a bus listing would prove
/// nothing about whether a portal implementing *this* interface is installed.
/// A session with no bus, no portal, or a portal too old to carry global
/// shortcuts all answer `false`, which is what a capability claim needs.
pub fn portal_is_available() -> bool {
    let Ok(connection) = Connection::session() else {
        return false;
    };
    shortcuts_proxy(&connection).is_ok_and(|proxy| proxy.get_property::<u32>("version").is_ok())
}

fn shortcuts_proxy(connection: &Connection) -> Result<Proxy<'static>> {
    Proxy::new(connection, PORTAL_BUS_NAME, PORTAL_PATH, SHORTCUTS_INTERFACE)
        .map_err(|error| invalid("the GlobalShortcuts portal could not be addressed", error))
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// One bound accelerator: what the portal calls it, and what the launcher does.
#[derive(Debug, Clone)]
struct Shortcut {
    /// The canonical accelerator rendering, which is what a handler is handed
    /// back and what makes every spelling of one chord one registration.
    canonical: String,
    /// The same chord in the shortcuts-specification syntax the portal takes
    /// as a `preferred_trigger`.
    trigger: String,
    session: OwnedObjectPath,
}

/// The live bindings, keyed by the shortcut id the portal knows them by, which
/// is what an `Activated` signal carries.
type Registrations = HashMap<String, Shortcut>;

/// What a `Response` signal turned out to say.
///
/// A malformed body is its own outcome rather than a discarded message: the
/// caller is blocked on this answer, and "the portal replied with something
/// this backend could not read" is a reason worth reporting, while silence
/// would only be reported as a timeout minutes later.
enum PortalResponse {
    Answered {
        code: u32,
        results: HashMap<String, OwnedValue>,
    },
    Malformed(String),
}

/// Everything the reader thread and the service both touch.
struct Shared {
    /// Behind its own `Arc` so the reader can clone it out of the lock and
    /// release the lock before calling it: a handler that reconfigured the
    /// service would otherwise deadlock against the lock it was invoked under.
    handler: Mutex<Option<Arc<HotkeyActivationHandler>>>,
    registrations: Mutex<Registrations>,
    /// Request paths whose `Response` somebody is still waiting for.
    pending: Mutex<HashMap<String, Sender<PortalResponse>>>,
    stopping: AtomicBool,
}

impl Shared {
    fn new() -> Self {
        Self {
            handler: Mutex::new(None),
            registrations: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            stopping: AtomicBool::new(false),
        }
    }
}

// ---------------------------------------------------------------------------
// Accelerator translation
// ---------------------------------------------------------------------------

/// The id the portal knows one accelerator by.
///
/// Derived from the canonical rendering so that it is stable across runs -- the
/// portal persists a user's answer against it -- and reduced to the characters
/// an identifier safely survives in a configuration file. The canonical
/// vocabulary is alphanumerics and `+`, so replacing everything else keeps
/// distinct chords distinct.
fn shortcut_id(canonical: &str) -> String {
    canonical
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The chord in the syntax of the freedesktop shortcuts specification.
///
/// The key half is the xkbcommon keysym name, which is the same table X11
/// grabs against -- deliberately reused rather than duplicated, so the two
/// Linux backends can never disagree about what `PageUp` means. The modifier
/// half is not shared: the specification spells Super as `LOGO`, and the
/// modifiers are emitted in a fixed order so that one chord always produces one
/// string for the portal to match its stored answer against.
///
/// # Errors
///
/// [`CoreError::Invalid`] when the key names no keysym, which is the same
/// vocabulary-drift guard the X11 mapping applies.
fn portal_trigger(accelerator: &Accelerator) -> Result<String> {
    let (keysym_name, _) = keysym_of(accelerator)?;
    let modifiers = accelerator.modifiers();
    let mut trigger = String::new();
    for (held, name) in [
        (modifiers.ctrl, "CTRL"),
        (modifiers.alt, "ALT"),
        (modifiers.shift, "SHIFT"),
        (modifiers.meta, "LOGO"),
    ] {
        if held {
            trigger.push_str(name);
            trigger.push('+');
        }
    }
    trigger.push_str(keysym_name);
    Ok(trigger)
}

/// What the portal shows the user for one shortcut. The chord is included
/// because a portal dialog lists the description alone, and "CriKey" against
/// three identical rows tells the user nothing about what they are approving.
fn description(canonical: &str) -> String {
    format!("CriKey shortcut ({canonical})")
}

// ---------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------

/// Global hotkeys backed by the XDG `GlobalShortcuts` portal (spec 18.6).
///
/// Constructing one proves the portal answers; it does not prove the user will
/// approve a binding, which is a per-registration outcome and is reported as
/// one.
pub struct WaylandHotkeyService {
    connection: Connection,
    shortcuts: Proxy<'static>,
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
    /// Makes each request and session token unique within this service, which
    /// is what lets a request path be predicted before the call is made.
    next_token: u64,
    sessions: HashMap<String, OwnedObjectPath>,
    retired: Vec<OwnedObjectPath>,
    cancellation: HotkeyCancellation,
    operation_epoch: u64,
    retired_requests: Vec<String>,
}

impl fmt::Debug for WaylandHotkeyService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WaylandHotkeyService")
            .field("sessions", &self.sessions.len())
            .field("registrations", &lock(&self.shared.registrations).len())
            .finish_non_exhaustive()
    }
}

impl WaylandHotkeyService {
    /// Connects to the portal and starts the reader.
    ///
    /// # Errors
    ///
    /// [`CoreError::Invalid`] naming what refused: no session bus, no portal
    /// answering the `GlobalShortcuts` interface, or a portal that would not
    /// open a session. None of those are softened into a service that would
    /// accept registrations nothing could deliver.
    pub fn connect() -> Result<Self> {
        let connection = Connection::session().map_err(|error| {
            invalid(
                "the session bus is unreachable, so no GlobalShortcuts portal can be asked for a hotkey",
                error,
            )
        })?;
        Self::connect_on(connection)
    }

    /// Connects over an already-established session bus connection. Exists so
    /// the tests can point the whole service at a private bus running a portal
    /// they control, rather than at whatever the build host happens to run.
    pub fn connect_on(connection: Connection) -> Result<Self> {
        let shortcuts = shortcuts_proxy(&connection)?;
        shortcuts.get_property::<u32>("version").map_err(|error| {
            invalid(
                "no GlobalShortcuts portal answered on the session bus, so Wayland offers this \
                 launcher no way to bind a global hotkey",
                error,
            )
        })?;

        subscribe(&connection)?;
        // Created before any call is made: the portal's `Response` signals are
        // the answers to those calls, and a stream opened afterwards could miss
        // the first one.
        let messages = MessageIterator::from(&connection);
        let shared = Arc::new(Shared::new());
        let reader = spawn_reader(messages, Arc::clone(&shared));

        let service = Self {
            connection,
            shortcuts,
            shared,
            reader: Some(reader),
            next_token: 0,
            sessions: HashMap::new(),
            retired: Vec::new(),
            cancellation: HotkeyCancellation::default(),
            operation_epoch: 0,
            retired_requests: Vec::new(),
        };
        Ok(service)
    }

    /// A token unique to this service, valid as the last element of an object
    /// path.
    fn token(&mut self) -> Result<String> {
        self.next_token = self
            .next_token
            .checked_add(1)
            .ok_or_else(|| CoreError::Invalid("portal shortcut token exhausted".into()))?;
        Ok(format!("crikey{}_{}", std::process::id(), self.next_token))
    }

    /// Opens a fresh portal session.
    fn create_session(&mut self) -> Result<OwnedObjectPath> {
        let handle_token = self.token()?;
        let session_token = self.token()?;
        let sender = self
            .connection
            .unique_name()
            .ok_or_else(|| CoreError::Invalid("portal connection has no unique name".into()))?
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");
        let predicted_session = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/session/{sender}/{session_token}"
        ))
        .map_err(|error| invalid("predicting portal session", error))?;
        let waiter = self.begin_request(&handle_token)?;

        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("handle_token", Value::from(handle_token.as_str()));
        options.insert("session_handle_token", Value::from(session_token.as_str()));
        let handle: OwnedObjectPath = self
            .shortcuts
            .call("CreateSession", &(options,))
            .map_err(|error| invalid("the GlobalShortcuts portal refused to open a session", error))?;

        let results = match self.await_response(waiter, handle, SESSION_TIMEOUT, "opening a portal session") {
            Ok(results) => results,
            Err(error) => {
                return match self.close_session(&predicted_session) {
                    Ok(()) => Err(error),
                    Err(cleanup) => {
                        self.retired.push(predicted_session);
                        Err(CoreError::Invalid(format!("{error}; {cleanup}")))
                    }
                }
            }
        };
        // The portal returns this as a string rather than an object path: a
        // documented wart of the interface, kept for backwards compatibility.
        let session = results
            .get("session_handle")
            .and_then(|value| Value::try_from(value).ok())
            .and_then(|value| String::try_from(value).ok())
            .ok_or_else(|| {
                CoreError::Invalid(
                    "the GlobalShortcuts portal opened a session without naming it, so nothing can \
                     be bound to it"
                        .to_owned(),
                )
            })?;
        ObjectPath::try_from(session)
            .map(OwnedObjectPath::from)
            .map_err(|error| {
                invalid(
                    "the portal named its session with something that is not an object path",
                    error,
                )
            })
    }

    /// Asks the portal to bind every shortcut in `desired`, and confirms the
    /// complete set before the previous session can be retired.
    fn bind(&mut self, session: &OwnedObjectPath, desired: &Registrations) -> Result<()> {
        if desired.is_empty() {
            // Nothing left to bind. The empty session still replaces the old
            // one, which is what actually releases the shortcuts.
            return Ok(());
        }

        let shortcuts: Vec<(String, HashMap<String, Value<'_>>)> = desired
            .iter()
            .map(|(id, shortcut)| {
                let mut fields: HashMap<String, Value<'_>> = HashMap::new();
                fields.insert(
                    "description".to_owned(),
                    Value::from(description(&shortcut.canonical)),
                );
                fields.insert(
                    "preferred_trigger".to_owned(),
                    Value::from(shortcut.trigger.clone()),
                );
                (id.clone(), fields)
            })
            .collect();

        let handle_token = self.token()?;
        let waiter = self.begin_request(&handle_token)?;
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("handle_token", Value::from(handle_token.as_str()));

        // An empty parent window: the launcher has no window to parent a
        // portal dialog to at the moment it registers its activation chord.
        let handle: OwnedObjectPath = self
            .shortcuts
            .call("BindShortcuts", &(session, shortcuts, "", options))
            .map_err(|error| invalid("the GlobalShortcuts portal refused the binding request", error))?;

        let results = self.await_response(waiter, handle, BIND_TIMEOUT, "binding a global shortcut")?;
        let bound = bound_ids(&results).ok_or_else(|| CoreError::Invalid(
            "the GlobalShortcuts portal did not confirm its bound shortcuts; the previous session remains active".into()
        ))?;
        for id in desired.keys() {
            if !bound.iter().any(|bound_id| bound_id == id) {
                return Err(CoreError::Invalid(format!(
                    "the GlobalShortcuts portal did not bind {id}; the previous session remains active"
                )));
            }
        }
        Ok(())
    }

    /// Registers interest in the `Response` for a request that is about to be
    /// made, returning what to wait on.
    fn begin_request(&self, handle_token: &str) -> Result<RequestWaiter> {
        if self.cancellation.cancelled(self.operation_epoch) {
            return Err(CoreError::Invalid("hotkey service is stopping".into()));
        }
        let unique = self.connection.unique_name().ok_or_else(|| {
            CoreError::Invalid(
                "this connection has no unique bus name, so a portal request could not be addressed"
                    .to_owned(),
            )
        })?;
        let sender = unique.as_str().trim_start_matches(':').replace('.', "_");
        let path = format!("{REQUEST_PATH_PREFIX}/{sender}/{handle_token}");
        let (responses, receiver) = mpsc::channel();
        lock(&self.shared.pending).insert(path.clone(), responses);
        Ok(RequestWaiter {
            path,
            receiver,
            epoch: self.operation_epoch,
            shared: Arc::clone(&self.shared),
        })
    }

    /// Waits for the `Response` the reader routes back, and unwraps its code.
    fn await_response(
        &mut self,
        waiter: RequestWaiter,
        handle: OwnedObjectPath,
        timeout: Duration,
        attempt: &str,
    ) -> Result<HashMap<String, OwnedValue>> {
        let RequestWaiter {
            ref path,
            ref receiver,
            epoch,
            ..
        } = waiter;
        // A portal is entitled to hand back a path other than the predicted
        // one. Listening on both costs one map entry and is the difference
        // between a working registration and a timeout.
        let handle = handle.as_str().to_owned();
        if handle != *path {
            let mut pending = lock(&self.shared.pending);
            if let Some(responses) = pending.get(path).cloned() {
                pending.insert(handle.clone(), responses);
            }
        }

        let deadline = Instant::now() + timeout;
        let outcome = loop {
            if self.cancellation.cancelled(epoch) {
                let close = self.close_request(&handle);
                if close.is_err() {
                    self.retired_requests.push(handle.clone());
                }
                lock(&self.shared.pending).remove(path);
                lock(&self.shared.pending).remove(&handle);
                return Err(CoreError::Invalid(match close {
                    Ok(()) => format!("cancelled {attempt}; portal request closed"),
                    Err(error) => format!("cancelled {attempt}; portal request cleanup failed: {error}"),
                }));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break Err(RecvTimeoutError::Timeout);
            }
            match receiver.recv_timeout(remaining.min(Duration::from_millis(20))) {
                Err(RecvTimeoutError::Timeout) => continue,
                outcome => break outcome,
            }
        };
        let mut pending = lock(&self.shared.pending);
        pending.remove(path);
        pending.remove(&handle);
        drop(pending);

        match outcome {
            Ok(PortalResponse::Answered {
                code: RESPONSE_SUCCESS,
                results,
            }) => Ok(results),
            Ok(PortalResponse::Answered {
                code: RESPONSE_CANCELLED,
                ..
            }) => Err(CoreError::Invalid(format!(
                "the user dismissed the portal dialog while {attempt}"
            ))),
            Ok(PortalResponse::Answered { code, .. }) => Err(CoreError::Invalid(format!(
                "the GlobalShortcuts portal ended {attempt} with response code {code}"
            ))),
            Ok(PortalResponse::Malformed(reason)) => Err(CoreError::Invalid(format!(
                "the GlobalShortcuts portal answered {attempt} with a body this backend cannot \
                 read: {reason}"
            ))),
            Err(RecvTimeoutError::Timeout) => {
                let cleanup = self.close_request(&handle);
                if cleanup.is_err() {
                    self.retired_requests.push(handle.clone());
                }
                Err(CoreError::Invalid(format!(
                    "the GlobalShortcuts portal did not answer {attempt} within {} seconds; request cleanup: {}",
                    timeout.as_secs(), cleanup.err().map(|error| error.to_string()).unwrap_or_else(|| "closed".into())
                )))
            }
            // The reader is gone, so no answer can ever arrive on this
            // connection again, whatever the portal does next.
            Err(RecvTimeoutError::Disconnected) => Err(CoreError::Invalid(format!(
                "the portal reader stopped while {attempt}, so no shortcut this service lists can \
                 fire"
            ))),
        }
    }

    fn close_request(&self, path: &str) -> Result<()> {
        self.close_portal_object(path, REQUEST_INTERFACE, "closing portal shortcut request")
    }

    fn close_portal_object(&self, path: &str, interface: &str, context: &str) -> Result<()> {
        let proxy = Proxy::new(&self.connection, PORTAL_BUS_NAME, path, interface)
            .map_err(|error| invalid(context, error))?;
        match proxy.call::<_, _, ()>("Close", &()) {
            Ok(()) => Ok(()),
            // An explicit absent-object answer discharges cleanup. Transport
            // failures and Close refusals remain unresolved obligations.
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.DBus.Error.UnknownObject" =>
            {
                Ok(())
            }
            Err(error) => Err(invalid(context, error)),
        }
    }

    fn retry_retired(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        let mut requests = Vec::new();
        for path in std::mem::take(&mut self.retired_requests) {
            if let Err(error) = self.close_request(&path) {
                errors.push(error.to_string());
                requests.push(path);
            }
        }
        self.retired_requests = requests;
        let mut held = Vec::new();
        for session in std::mem::take(&mut self.retired) {
            if let Err(error) = self.close_session(&session) {
                errors.push(error.to_string());
                held.push(session);
            }
        }
        self.retired = held;
        if errors.is_empty() {
            Ok(())
        } else {
            Err(CoreError::Invalid(errors.join("; ")))
        }
    }

    /// An acknowledged Close or explicit UnknownObject confirms retirement.
    /// Every other refusal remains an unresolved physical cleanup obligation.
    fn close_session(&self, session: &OwnedObjectPath) -> Result<()> {
        self.close_portal_object(
            session.as_str(),
            SESSION_INTERFACE,
            "closing portal shortcut session",
        )
    }

    /// Gets the reader out of its blocking read, reporting whether joining it
    /// is now safe.
    ///
    /// The signal is addressed to this connection's own unique name, so the
    /// bus routes it straight back and the reader sees it as an ordinary
    /// incoming message. A send that fails means the connection is already
    /// gone -- in which case the reader's iterator has ended on its own -- or
    /// that the bus refused it, in which case joining could block forever and
    /// the caller is told not to.
    fn wake_reader(&self) -> bool {
        let Some(unique) = self.connection.unique_name().cloned() else {
            return false;
        };
        self.connection
            .emit_signal(Some(unique), WAKE_PATH, WAKE_INTERFACE, WAKE_MEMBER, &())
            .is_ok()
    }
}

/// What a caller holds while a portal request is in flight.
struct RequestWaiter {
    path: String,
    receiver: Receiver<PortalResponse>,
    epoch: u64,
    shared: Arc<Shared>,
}

impl Drop for RequestWaiter {
    fn drop(&mut self) {
        lock(&self.shared.pending).remove(&self.path);
    }
}

impl HotkeyService for WaylandHotkeyService {
    /// Binds an accelerator through the portal, or reports why it is not live.
    ///
    /// Re-registering an accelerator this service already holds succeeds
    /// without touching the portal: the caller asked for a live binding and has
    /// one, and rebinding would put a second approval dialog in front of the
    /// user for a shortcut they already approved. Duplicate detection is keyed
    /// on the canonical rendering, so every spelling of one chord is one
    /// registration.
    fn register(&mut self, binding: &HotkeyBinding) -> Result<()> {
        self.register_at(binding, self.cancellation.epoch())
    }

    fn register_at(&mut self, binding: &HotkeyBinding, epoch: u64) -> Result<()> {
        self.operation_epoch = epoch;
        if self.cancellation.cancelled(self.operation_epoch) {
            return Err(CoreError::Invalid("hotkey service is stopping".into()));
        }
        self.retry_retired()?;
        let accelerator = parse(binding)?;
        let canonical = accelerator.canonical();
        let id = shortcut_id(&canonical);
        if self.sessions.contains_key(&id) {
            return Ok(());
        }
        let trigger = portal_trigger(&accelerator)?;
        let session = self.create_session()?;
        let desired = HashMap::from([(
            id.clone(),
            Shortcut {
                canonical,
                trigger,
                session: session.clone(),
            },
        )]);
        if let Err(error) = self.bind(&session, &desired) {
            return match self.close_session(&session) {
                Ok(()) => Err(error),
                Err(cleanup) => {
                    self.retired.push(session);
                    Err(CoreError::Invalid(format!("{error}; {cleanup}")))
                }
            };
        }
        self.sessions.insert(id, session);
        lock(&self.shared.registrations).extend(desired);
        Ok(())
    }

    /// Releases an accelerator this service bound.
    ///
    /// An accelerator that was never registered is an error rather than a quiet
    /// success, exactly as on X11: it means the caller and the backend disagree
    /// about what is live.
    fn unregister(&mut self, binding: &HotkeyBinding) -> Result<()> {
        let accelerator = parse(binding)?;
        let canonical = accelerator.canonical();
        let id = shortcut_id(&canonical);
        let session = self
            .sessions
            .get(&id)
            .ok_or_else(|| CoreError::Invalid(format!("{canonical} holds no portal shortcut to release")))?;
        self.close_session(session)?;
        self.sessions.remove(&id);
        lock(&self.shared.registrations).remove(&id);
        Ok(())
    }

    /// Installs the callback the reader invokes on an activation.
    ///
    /// Bindings are untouched, so a handler may be swapped or cleared while
    /// shortcuts stay live -- and clearing it must not cost the user a second
    /// approval dialog to get them back.
    fn set_activation_handler(&mut self, handler: Option<HotkeyActivationHandler>) {
        *lock(&self.shared.handler) = handler.map(Arc::new);
    }

    fn cancellation_handle(&self) -> Option<HotkeyCancellation> {
        Some(self.cancellation.clone())
    }

    fn shutdown(&mut self, _bindings: &[HotkeyBinding]) -> Result<()> {
        self.set_activation_handler(None);
        self.cancellation.stop();
        let mut errors = Vec::new();
        for (id, session) in self.sessions.clone() {
            match self.close_session(&session) {
                Ok(()) => {
                    self.sessions.remove(&id);
                    lock(&self.shared.registrations).remove(&id);
                }
                Err(error) => errors.push(error.to_string()),
            }
        }
        if let Err(error) = self.retry_retired() {
            errors.push(error.to_string());
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(CoreError::Invalid(errors.join("; ")))
        }
    }
}

impl Drop for WaylandHotkeyService {
    /// Stops the reader and gives the portal its session back.
    ///
    /// The reader is joined only when the wake was actually sent. The X11
    /// service learned this the hard way: a `Drop` that unconditionally joins a
    /// thread blocked on a socket hangs the whole launcher on shutdown, and a
    /// detached reader costs a thread that ends with the connection anyway.
    fn drop(&mut self) {
        let _ = self.shutdown(&[]);

        let Some(reader) = self.reader.take() else {
            return;
        };
        self.shared.stopping.store(true, Ordering::Release);
        if self.wake_reader() {
            let _ = reader.join();
        }
    }
}

// ---------------------------------------------------------------------------
// The reader
// ---------------------------------------------------------------------------

/// Asks the bus for the portal's signals.
///
/// Both signals this backend reads are addressed to this connection, and the
/// bus delivers unicast messages without a match rule -- but only some portal
/// implementations address them, and a broadcast signal with no matching rule
/// is dropped by the bus before it is ever written to this socket. Registering
/// costs one round trip at startup and removes that whole class of "works on
/// one desktop" failure.
fn subscribe(connection: &Connection) -> Result<()> {
    let dbus = zbus::blocking::fdo::DBusProxy::new(connection)
        .map_err(|error| invalid("the session bus would not accept a match rule", error))?;
    for interface in [SHORTCUTS_INTERFACE, REQUEST_INTERFACE] {
        let rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .interface(interface)
            .map_err(|error| invalid("a portal match rule was rejected", error))?
            .build();
        dbus.add_match_rule(rule)
            .map_err(|error| invalid("the session bus refused a portal match rule", error))?;
    }
    Ok(())
}

/// Starts the thread that turns portal signals into handler calls and request
/// answers.
fn spawn_reader(messages: MessageIterator, shared: Arc<Shared>) -> JoinHandle<()> {
    thread::spawn(move || {
        for message in messages {
            if shared.stopping.load(Ordering::Acquire) {
                break;
            }
            // A message this connection could not decode says nothing about
            // the next one, and the portal is not the only thing that can
            // address this connection.
            let Ok(message) = message else {
                continue;
            };
            let header = message.header();
            let Some(interface) = header.interface().map(|interface| interface.as_str().to_owned()) else {
                continue;
            };
            let member = header
                .member()
                .map(|member| member.as_str().to_owned())
                .unwrap_or_default();
            let path = header.path().map(|path| path.as_str().to_owned());

            match (interface.as_str(), member.as_str()) {
                (REQUEST_INTERFACE, "Response") => deliver_response(&shared, path, &message),
                (SHORTCUTS_INTERFACE, "Activated") => deliver_activation(&shared, &message),
                (WAKE_INTERFACE, _) => break,
                _ => {}
            }
        }
    })
}

/// Routes one `Response` back to the thread blocked on that request.
fn deliver_response(shared: &Shared, path: Option<String>, message: &zbus::Message) {
    let Some(path) = path else {
        return;
    };
    let Some(responses) = lock(&shared.pending).remove(&path) else {
        return;
    };
    let response = match message.body().deserialize::<(u32, HashMap<String, OwnedValue>)>() {
        Ok((code, results)) => PortalResponse::Answered { code, results },
        Err(error) => PortalResponse::Malformed(error.to_string()),
    };
    // The waiter may already have timed out and gone; that is its decision,
    // not an error here.
    let _ = responses.send(response);
}

/// Turns one `Activated` signal into a handler call.
fn deliver_activation(shared: &Shared, message: &zbus::Message) {
    let Ok((session, id, _timestamp, _options)) =
        message
            .body()
            .deserialize::<(OwnedObjectPath, String, u64, HashMap<String, OwnedValue>)>()
    else {
        return;
    };
    let Some(shortcut) = lock(&shared.registrations).get(&id).cloned() else {
        return;
    };
    if shortcut.session != session {
        return;
    }
    let Some(handler) = lock(&shared.handler).clone() else {
        return;
    };

    let binding = HotkeyBinding {
        accelerator: shortcut.canonical,
    };
    // A panicking handler is the plugin host's problem, not a reason for this
    // thread to die and take every remaining shortcut with it.
    let _ = catch_unwind(AssertUnwindSafe(|| handler(&binding)));
}

/// The shortcut ids a `BindShortcuts` response says were bound.
///
/// `None` when the portal reported no subset at all, which is different from
/// reporting an empty one: the first is an implementation that does not answer
/// the question, the second is a refusal of everything.
fn bound_ids(results: &HashMap<String, OwnedValue>) -> Option<Vec<String>> {
    let value = Value::try_from(results.get("shortcuts")?).ok()?;
    let Value::Array(entries) = value else {
        return None;
    };
    let mut ids = Vec::with_capacity(entries.len());
    for entry in entries.iter() {
        if let Value::Structure(fields) = entry {
            if let Some(Value::Str(id)) = fields.fields().first() {
                ids.push(id.as_str().to_owned());
            }
        }
    }
    Some(ids)
}
