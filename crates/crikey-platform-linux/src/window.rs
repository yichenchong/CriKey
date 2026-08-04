//! Window enumeration and activation over EWMH (spec 18.1, 18.6).
//!
//! # Why EWMH and not the core protocol
//!
//! The X11 core protocol has no notion of an application window. `QueryTree`
//! on the root returns every child, including a window manager's frames, its
//! panels, override-redirect tooltips and unmapped scratch windows, and it says
//! nothing about which of them a user would call "a window". The switchable set
//! is a *window manager* concept, and the window manager publishes it as the
//! `_NET_CLIENT_LIST` property on the root window. Reading that property is
//! therefore not a shortcut; it is the only answer that matches what the user
//! sees.
//!
//! The same reasoning drives activation. `SetInputFocus` moves the X input
//! focus behind the window manager's back: the target stays stacked below other
//! windows, keeps its inactive decorations, and a manager that tracks focus
//! itself will often take the focus straight back. EWMH defines
//! `_NET_ACTIVE_WINDOW` as a `ClientMessage` sent *to the root window* so that
//! the manager performs the raise, the focus and the workspace switch as one
//! operation. [`X11WindowService::activate`] sends exactly that.
//!
//! # Why `connect` refuses a server that cannot prove a manager is running
//!
//! Both properties are written by the window manager, and a bare X server has
//! no window manager. On such a display `_NET_CLIENT_LIST` is simply absent, so
//! an enumeration would return an empty list, and a `_NET_ACTIVE_WINDOW`
//! message would be delivered to nobody and dropped -- both indistinguishable
//! from success. Spec 18.6 makes window control optional on Linux precisely so
//! that this case can be *reported* (spec 18.2) instead of faked, so
//! [`X11WindowService::connect`] runs the EWMH handshake and fails by name.
//!
//! The handshake is three checks, not one, because the property it starts from
//! is an ordinary root-window property that any client may write and that
//! outlives whoever wrote it. `_NET_SUPPORTED` must be the `ATOM[]/32` list
//! EWMH declares; that list must name `_NET_CLIENT_LIST` and
//! `_NET_ACTIVE_WINDOW`, since the list exists to say *which* hints the manager
//! implements; and the two-sided `_NET_SUPPORTING_WM_CHECK` must hold, which is
//! the only part that proves the manager is still alive rather than crashed and
//! survived by its advertisement. A host that gets an error reports the
//! capability unavailable; a host that got a service knows the desktop answers
//! these requests.

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crikey_core::{CoreError, Result};
use crikey_platform::{WindowHandle, WindowInfo, WindowService};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt, CreateWindowAux, EventMask,
    PropMode, Timestamp, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT};

/// Upper bound, in 32-bit words, on how much of a property is read.
///
/// `_NET_CLIENT_LIST` holds one word per window, so this caps enumeration at
/// 4096 windows -- orders of magnitude past any real desktop, while still
/// refusing to allocate whatever a hostile root-window property claims. Titles
/// use the same bound, giving 16 KiB of text.
const MAX_PROPERTY_WORDS: u32 = 4096;

/// EWMH source indication for "a pager", used by [`X11WindowService::activate`].
///
/// The alternative, `1`, means "a normal application", and window managers
/// deliberately apply focus-stealing prevention to it: a background program
/// that shouts for focus is a misbehaving program. A launcher is not that. It
/// is acting on a keystroke the user just pressed, which is what the pager
/// value describes, and managers honour it unconditionally. Claiming to be an
/// application here would make activation silently fail on exactly the
/// well-behaved managers this backend exists to work with.
const SOURCE_PAGER: u32 = 2;

/// `_NET_ACTIVE_WINDOW` carries five 32-bit words.
const CLIENT_MESSAGE_FORMAT: u8 = 32;

/// The event mask a `ClientMessage` to the root window must be sent with.
///
/// The window manager selects `SubstructureRedirect` on the root; naming it
/// here is what routes the message to the manager rather than to whoever else
/// happens to be watching the root window. `EventMask` composes with `|` rather
/// than being a `const`-composable bitflags type, so this is a function.
fn root_message_mask() -> EventMask {
    EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY
}

/// Names the failure with its context, matching the hotkey backend's shape.
fn invalid(context: &str, error: impl fmt::Display) -> CoreError {
    CoreError::Invalid(format!("{context}: {error}"))
}

/// The atoms this backend interns once, so that no request pays for a round trip
/// it already made.
#[derive(Debug, Clone, Copy)]
struct Atoms {
    net_supported: u32,
    net_client_list: u32,
    net_wm_name: u32,
    net_active_window: u32,
    net_supporting_wm_check: u32,
    utf8_string: u32,
    /// Private property the current-server-time probe appends nothing to.
    user_time_probe: u32,
}

impl Atoms {
    /// Interns every atom in one batch.
    ///
    /// The requests are all sent before any reply is awaited, so the whole set
    /// costs one round trip rather than one per name.
    fn intern(connection: &RustConnection) -> Result<Self> {
        const NAMES: [&[u8]; 7] = [
            b"_NET_SUPPORTED",
            b"_NET_CLIENT_LIST",
            b"_NET_WM_NAME",
            b"_NET_ACTIVE_WINDOW",
            b"_NET_SUPPORTING_WM_CHECK",
            b"UTF8_STRING",
            b"CRIKEY_USER_TIME_PROBE",
        ];

        let mut cookies = Vec::with_capacity(NAMES.len());
        for name in NAMES {
            // `only_if_exists = false`: the atom is created when absent. An
            // atom is just a name, and a display that has never mentioned
            // `_NET_ACTIVE_WINDOW` must still be able to receive one.
            cookies.push(
                connection
                    .intern_atom(false, name)
                    .map_err(|error| invalid("interning the EWMH atoms", error))?,
            );
        }

        let mut atoms = [0u32; NAMES.len()];
        for (slot, cookie) in atoms.iter_mut().zip(cookies) {
            *slot = cookie
                .reply()
                .map_err(|error| invalid("interning the EWMH atoms", error))?
                .atom;
        }

        Ok(Self {
            net_supported: atoms[0],
            net_client_list: atoms[1],
            net_wm_name: atoms[2],
            net_active_window: atoms[3],
            net_supporting_wm_check: atoms[4],
            utf8_string: atoms[5],
            user_time_probe: atoms[6],
        })
    }
}

/// Reads a root property EWMH declares as a `<kind>[]/32` list of 32-bit words.
///
/// `None` covers absent, empty, and stored-under-the-wrong-type: the caller
/// treats all three the same way, because a property whose type is not what the
/// spec declares carries something that is not the list it is being read as.
fn word_list(connection: &RustConnection, window: Window, property: u32, kind: AtomEnum) -> Option<Vec<u32>> {
    let reply = connection
        .get_property(false, window, property, kind, 0, MAX_PROPERTY_WORDS)
        .ok()?
        .reply()
        .ok()?;
    if reply.type_ != u32::from(kind) || reply.format != 32 {
        return None;
    }
    reply.value32().map(Iterator::collect)
}

/// Requires `_NET_SUPPORTED` to be an atom list naming the hints this service
/// uses.
fn check_supported_hints(
    connection: &RustConnection,
    root: Window,
    atoms: &Atoms,
    named: &str,
) -> Result<()> {
    let supported = word_list(connection, root, atoms.net_supported, AtomEnum::ATOM).ok_or_else(|| {
        CoreError::Invalid(format!(
            "X display {named} advertises no _NET_SUPPORTED atom list: no EWMH window manager is \
             running, so window enumeration and activation are unavailable there"
        ))
    })?;

    for (atom, name) in [
        (atoms.net_client_list, "_NET_CLIENT_LIST"),
        (atoms.net_active_window, "_NET_ACTIVE_WINDOW"),
    ] {
        if !supported.contains(&atom) {
            return Err(CoreError::Invalid(format!(
                "the window manager on X display {named} does not list {name} in _NET_SUPPORTED, so \
                 window enumeration and activation are unavailable there"
            )));
        }
    }
    Ok(())
}

/// Requires the two-sided `_NET_SUPPORTING_WM_CHECK` that proves a manager is
/// still running.
fn check_supporting_wm(connection: &RustConnection, root: Window, atoms: &Atoms, named: &str) -> Result<()> {
    let missing = |what: &str| {
        CoreError::Invalid(format!(
            "X display {named} fails the _NET_SUPPORTING_WM_CHECK handshake ({what}), so its \
             _NET_SUPPORTED is stale and no EWMH window manager is running"
        ))
    };

    let on_root = word_list(connection, root, atoms.net_supporting_wm_check, AtomEnum::WINDOW)
        .and_then(|windows| windows.first().copied())
        .ok_or_else(|| missing("the root window names no manager window"))?;

    let on_child = word_list(
        connection,
        on_root,
        atoms.net_supporting_wm_check,
        AtomEnum::WINDOW,
    )
    .and_then(|windows| windows.first().copied())
    .ok_or_else(|| missing("the named manager window is gone or names nothing"))?;

    if on_child != on_root {
        return Err(missing("the manager window does not name itself"));
    }
    Ok(())
}

/// Creates the private `InputOnly` window the server-time probe writes to.
///
/// `PropertyChange` is selected on it and on nothing else, so the only
/// `PropertyNotify` this connection can ever receive is the probe's own.
fn create_probe_window(connection: &RustConnection, root: Window) -> Result<Window> {
    let window = connection
        .generate_id()
        .map_err(|error| invalid("reserving an X window id", error))?;
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
            &CreateWindowAux::new().override_redirect(1),
        )
        .map_err(|error| invalid("creating the timestamp probe window", error))?
        .check()
        .map_err(|error| invalid("creating the timestamp probe window", error))?;
    connection
        .change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|error| invalid("watching the timestamp probe window", error))?
        .check()
        .map_err(|error| invalid("watching the timestamp probe window", error))?;
    Ok(window)
}

/// Window enumeration and activation against one X display.
pub struct X11WindowService {
    connection: RustConnection,
    root: Window,
    atoms: Atoms,
    /// Private `InputOnly` window the current-server-time probe writes to.
    ///
    /// A client cannot ask X what time it is; the documented idiom is to append
    /// zero bytes to a property of one's own window and read the timestamp off
    /// the resulting `PropertyNotify`. This window exists for nothing else, so
    /// the probe never disturbs a property anybody reads.
    probe_window: Window,
    /// The X server time of the last user action this launcher observed.
    ///
    /// Shared with the hotkey service, whose reader thread is the only thing in
    /// the process that sees a real user event: `0` means nothing has been
    /// observed yet and the probe supplies the current server time instead.
    user_time: Arc<AtomicU32>,
}

impl fmt::Debug for X11WindowService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("X11WindowService")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl X11WindowService {
    /// Connects to `display` (or `$DISPLAY`) and verifies it speaks EWMH.
    ///
    /// The service keeps its own user-activity clock, which stays empty: use
    /// [`Self::connect_sharing`] to hand it the clock the hotkey reader fills.
    ///
    /// # Errors
    ///
    /// [`CoreError::Invalid`] when no server answers, when the display names no
    /// usable screen, or when the display fails the EWMH handshake described on
    /// [`Self::connect_sharing`]. A refusal rather than a degraded service on
    /// purpose: see the module documentation. A caller that wants to *report*
    /// the missing capability rather than fail is expected to map this error
    /// onto [`CapabilityState::Unavailable`].
    ///
    /// [`CapabilityState::Unavailable`]: crikey_platform::CapabilityState::Unavailable
    pub fn connect(display: Option<&str>) -> Result<Self> {
        Self::connect_sharing(display, Arc::new(AtomicU32::new(0)))
    }

    /// Connects as [`Self::connect`] does, reading user-activity times from
    /// `user_time` instead of from a private clock.
    ///
    /// # Errors
    ///
    /// The handshake is checked in three parts, because each of them can hold
    /// while the others do not:
    ///
    /// 1. `_NET_SUPPORTED` must be an `ATOM[]/32` list, which is what EWMH
    ///    declares it to be. Any client may write any bytes to a root-window
    ///    property, so a non-empty value of some other type proves nothing.
    /// 2. That list must name `_NET_CLIENT_LIST` and `_NET_ACTIVE_WINDOW`. The
    ///    list exists to say *which* hints the manager implements, and those two
    ///    are the only ones this service uses: a manager that advertises neither
    ///    would answer every enumeration with an empty list and drop every
    ///    activation.
    /// 3. The two-sided `_NET_SUPPORTING_WM_CHECK` must hold. EWMH defines it
    ///    precisely to prove a manager is *alive*: the root names a window, that
    ///    window names itself, and the window belongs to the manager, so it
    ///    vanishes with the manager while a stale `_NET_SUPPORTED` left behind
    ///    by a crashed one does not.
    pub fn connect_sharing(display: Option<&str>, user_time: Arc<AtomicU32>) -> Result<Self> {
        let named = display.unwrap_or("$DISPLAY");
        let (connection, screen) = RustConnection::connect(display)
            .map_err(|error| invalid(&format!("connecting to X display {named}"), error))?;

        let root = connection
            .setup()
            .roots
            .get(screen)
            .ok_or_else(|| CoreError::Invalid(format!("X display {named} has no screen {screen}")))?
            .root;

        let atoms = Atoms::intern(&connection)?;

        check_supported_hints(&connection, root, &atoms, named)?;
        check_supporting_wm(&connection, root, &atoms, named)?;

        let probe_window = create_probe_window(&connection, root)?;

        Ok(Self {
            connection,
            root,
            atoms,
            probe_window,
            user_time,
        })
    }

    /// The clock this service reads activation timestamps from, so that the
    /// hotkey reader can fill it.
    pub fn user_activity_clock(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.user_time)
    }

    /// The timestamp `_NET_ACTIVE_WINDOW` is sent with.
    ///
    /// The last observed user action when there is one -- EWMH asks for exactly
    /// that, and a manager compares it against the focused window's
    /// `_NET_USER_TIME` to decide whether the request is a real user intent or a
    /// background program shouting. Failing that, the current server time, which
    /// is the closest true statement available: `CurrentTime` is what a manager
    /// cannot reason about at all.
    fn activation_time(&self) -> Timestamp {
        match self.user_time.load(Ordering::Relaxed) {
            0 => self.server_time().unwrap_or(0),
            observed => observed,
        }
    }

    /// Asks the server what time it is by appending nothing to a private
    /// property and reading the timestamp off the `PropertyNotify` that causes.
    ///
    /// `None` only when the connection is broken, in which case the activation
    /// that follows fails on its own.
    fn server_time(&self) -> Option<Timestamp> {
        self.connection
            .change_property8(
                PropMode::APPEND,
                self.probe_window,
                self.atoms.user_time_probe,
                u32::from(AtomEnum::STRING),
                &[],
            )
            .ok()?
            .check()
            .ok()?;
        self.connection.flush().ok()?;

        // `PropertyChange` is selected on the probe window and on nothing else,
        // so this loop is waiting for an event only this request can produce.
        loop {
            match self.connection.wait_for_event().ok()? {
                Event::PropertyNotify(notify)
                    if notify.window == self.probe_window && notify.atom == self.atoms.user_time_probe =>
                {
                    return Some(notify.time);
                }
                _ => {}
            }
        }
    }

    /// Whether `window` still exists on this display.
    ///
    /// A round trip, and deliberately the *last* thing an enumeration does with
    /// a window: a window destroyed at any point up to here is then reported as
    /// gone rather than as an untitled live one.
    fn is_alive(&self, window: Window) -> bool {
        self.connection
            .get_window_attributes(window)
            .is_ok_and(|cookie| cookie.reply().is_ok())
    }

    /// Reads one property of exactly `kind`, or `None` when it is absent.
    ///
    /// A property that does not exist, one stored under a different type, and a
    /// window that no longer exists are all `None`: the caller is scanning a set
    /// that other programs mutate concurrently, so none of them is exceptional.
    /// If the connection breaks during a per-window read it is also treated as
    /// an omitted window; the root list request in [`enumerate`] still reports
    /// a connection failure before this helper is reached.
    ///
    /// Naming `kind` rather than passing `AnyPropertyType` is what makes the
    /// answer trustworthy: the server filters on it, so a client that stored
    /// text where this backend expects a window id yields nothing instead of
    /// having its bytes reinterpreted.
    fn property(&self, window: Window, property: impl Into<u32>, kind: impl Into<u32>) -> Option<Vec<u8>> {
        let kind = kind.into();
        let reply = self
            .connection
            .get_property(false, window, property, kind, 0, MAX_PROPERTY_WORDS)
            .ok()?
            .reply()
            .ok()?;
        // XGetProperty returns a property's actual type when the requested
        // type does not match. Treating those bytes as the requested text
        // would turn arbitrary client data into a title or WM_CLASS.
        if reply.type_ != kind || reply.format != 8 {
            return None;
        }
        if reply.value.is_empty() {
            return None;
        }
        Some(reply.value)
    }

    /// The window's title: `_NET_WM_NAME` if it has one, else `WM_NAME`.
    ///
    /// `_NET_WM_NAME` is `UTF8_STRING` and is what a modern toolkit sets.
    /// `WM_NAME` is the core-protocol `STRING` fallback that older clients still
    /// set alone, so skipping it would show blank rows for them. A window with
    /// neither is titled `""` and still listed: an unnamed window is switchable.
    fn title(&self, window: Window) -> String {
        if let Some(bytes) = self.property(window, self.atoms.net_wm_name, self.atoms.utf8_string) {
            return String::from_utf8_lossy(&bytes).into_owned();
        }
        match self.property(window, AtomEnum::WM_NAME, AtomEnum::STRING) {
            // `STRING` is Latin-1, where every byte is one code point. Decoding
            // it as UTF-8 would mangle every accented title.
            Some(bytes) => bytes.iter().map(|&byte| char::from(byte)).collect(),
            None => String::new(),
        }
    }

    /// The owning program's name from `WM_CLASS`, or `None`.
    ///
    /// `WM_CLASS` is two NUL-terminated `STRING`s, instance then class. The
    /// class is the human-facing one (`Firefox`, not `navigator`), so it is
    /// preferred; a client that set only one string falls back to that. A window
    /// without the property yields `None` rather than a guess derived from its
    /// title.
    fn application(&self, window: Window) -> Option<String> {
        let bytes = self.property(window, AtomEnum::WM_CLASS, AtomEnum::STRING)?;
        let mut fields = bytes
            .split(|&byte| byte == 0)
            .filter(|field| !field.is_empty())
            .map(|field| String::from_utf8_lossy(field).into_owned());
        let instance = fields.next();
        fields.next().or(instance)
    }
}

impl WindowService for X11WindowService {
    /// Reads `_NET_CLIENT_LIST` from the root window and describes each entry.
    ///
    /// # Errors
    ///
    /// [`CoreError::Invalid`] when the root property cannot be read at all,
    /// which means the connection is gone. An individual window that has gone
    /// away since the manager published the list is *omitted*, which is what
    /// [`WindowService::enumerate`] requires: a destroyed window has no
    /// properties left, so keeping it would put an untitled, unswitchable row in
    /// front of the user and hand the host a handle whose activation can only
    /// fail. A window whose *properties* merely cannot be read is still listed.
    fn enumerate(&self) -> Result<Vec<WindowInfo>> {
        let reply = self
            .connection
            .get_property(
                false,
                self.root,
                self.atoms.net_client_list,
                AtomEnum::WINDOW,
                0,
                MAX_PROPERTY_WORDS,
            )
            .map_err(|error| invalid("reading _NET_CLIENT_LIST", error))?
            .reply()
            .map_err(|error| invalid("reading _NET_CLIENT_LIST", error))?;

        // EWMH declares the property `WINDOW[]/32`. Both halves are checked
        // before the bytes are trusted: an absent property has type `None`, and
        // a property of some other type or width holds something that is not a
        // list of window ids, which must never be reinterpreted as one.
        if reply.type_ != u32::from(AtomEnum::WINDOW) || reply.format != 32 {
            // Not an error: a manager writes the list only once it has a client
            // to list, so an unset property is simply an empty desktop.
            return Ok(Vec::new());
        }
        let Some(windows) = reply.value32() else {
            return Ok(Vec::new());
        };

        let mut infos = Vec::new();
        for window in windows {
            let title = self.title(window);
            let application = self.application(window);
            // Last, so that a window destroyed at any point during its own
            // description is caught: the properties above answer emptily for a
            // dead window rather than failing.
            if !self.is_alive(window) {
                continue;
            }
            infos.push(WindowInfo {
                handle: WindowHandle(u64::from(window)),
                title,
                application,
            });
        }
        Ok(infos)
    }

    /// Sends `_NET_ACTIVE_WINDOW` to the root window and flushes it.
    ///
    /// # Errors
    ///
    /// [`CoreError::Invalid`] when the handle names no window this display can
    /// address, or when the message cannot be put on the wire. The liveness
    /// check is a real round trip rather than a formality: a `ClientMessage`
    /// naming a dead window is accepted by the server and dropped by the
    /// manager, so without it every stale handle would activate successfully and
    /// do nothing.
    fn activate(&self, handle: &WindowHandle) -> Result<()> {
        let window = u32::try_from(handle.0)
            .map_err(|_| CoreError::Invalid(format!("window handle {} is not an X11 window id", handle.0)))?;

        self.connection
            .get_window_attributes(window)
            .map_err(|error| invalid("checking the window to activate", error))?
            .reply()
            .map_err(|error| invalid(&format!("window {window:#x} cannot be activated"), error))?;

        // data[0] source indication, data[1] the timestamp of the user action
        // that asked for this: EWMH requires the client's last user-activity
        // time, which a manager compares against the focused window's
        // `_NET_USER_TIME` before honouring the raise. `CurrentTime` -- a zero
        // here -- is the one value that carries no information at all, and
        // managers are entitled to treat it as a background program shouting.
        // data[2] would name the window losing focus, which this client does
        // not know.
        let message = ClientMessageEvent::new(
            CLIENT_MESSAGE_FORMAT,
            window,
            self.atoms.net_active_window,
            [SOURCE_PAGER, self.activation_time(), 0, 0, 0],
        );

        self.connection
            .send_event(false, self.root, root_message_mask(), message)
            .map_err(|error| invalid("sending _NET_ACTIVE_WINDOW", error))?
            .check()
            .map_err(|error| invalid("sending _NET_ACTIVE_WINDOW", error))?;

        // Without this the request sits in the output buffer until some later
        // request happens to flush it, so an activation followed by no further
        // X traffic would never reach the manager.
        self.connection
            .flush()
            .map_err(|error| invalid("flushing _NET_ACTIVE_WINDOW", error))?;
        Ok(())
    }
}
