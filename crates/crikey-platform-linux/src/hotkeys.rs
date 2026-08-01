//! Global hotkeys over X11 `GrabKey` (spec 6.1, 18.6).
//!
//! Two pieces stack here. [`x11_binding`] turns a parsed [`Accelerator`] into
//! the `(modifier mask, keysym name)` pair `GrabKey` is ultimately given; it
//! touches no display, so it is checkable on any host and is what the mapping
//! tests pin. [`X11HotkeyService`] puts that mapping on the wire: it owns a
//! connection, the grabs taken against the root window, and the reader thread
//! activations arrive on.
//!
//! # Why a reader thread
//!
//! A grabbed key is delivered as an ordinary `KeyPress` on the connection that
//! took the grab. Nothing else in the launcher pumps that connection, so a
//! service without a reader would hold live grabs whose activations nobody ever
//! sees -- the key would be swallowed and the hotkey would still be dead. The
//! thread is therefore part of the registration being real, not an optimisation.
//! It is stopped by a `ClientMessage` addressed to the service's own throwaway
//! window: an event mask of zero means X delivers the message to the client
//! that created the window, so the blocked reader wakes on an observable rather
//! than on a poll timeout.
//!
//! # Why lock modifiers are permuted
//!
//! X reports the *exact* modifier state in a `KeyPress`, and a grab matches only
//! the mask it was taken with. Grabbing `Ctrl+Alt+Space` once would leave the
//! chord dead whenever CapsLock or NumLock happened to be on. The grab is
//! therefore taken for every combination of those two lock bits, while
//! [`x11_binding`] still reports the base mask: the lock bits are an artefact of
//! how X matches, not part of what the user bound.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use crikey_core::{CoreError, Result};
use crikey_platform::{Accelerator, HotkeyActivationHandler, HotkeyBinding, HotkeyService, Modifiers};
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::xproto::{
    ClientMessageEvent, ConnectionExt, CreateWindowAux, EventMask, GrabMode, Keycode, ModMask, Window,
    WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT};

// ---------------------------------------------------------------------------
// Modifier masks
// ---------------------------------------------------------------------------

/// `ShiftMask`, the X11 core-protocol bit.
pub const MOD_SHIFT: u32 = 1 << 0;
/// `ControlMask`.
pub const MOD_CONTROL: u32 = 1 << 2;
/// `Mod1Mask`, which every supported desktop maps Alt onto.
pub const MOD_ALT: u32 = 1 << 3;
/// `Mod4Mask`, which every supported desktop maps Super onto. The shared
/// vocabulary calls that key `Meta`.
pub const MOD_SUPER: u32 = 1 << 6;

/// `LockMask`: CapsLock. Never part of a binding, always permuted over.
const MOD_LOCK: u32 = 1 << 1;

/// The bits a binding can carry, used to strip the incidental state off an
/// incoming `KeyPress` before it is matched against a registration.
const BINDING_MASK: u32 = MOD_SHIFT | MOD_CONTROL | MOD_ALT | MOD_SUPER;

/// The mask an accelerator's modifiers add up to.
fn modifier_mask(modifiers: Modifiers) -> u32 {
    let mut mask = 0;
    if modifiers.shift {
        mask |= MOD_SHIFT;
    }
    if modifiers.ctrl {
        mask |= MOD_CONTROL;
    }
    if modifiers.alt {
        mask |= MOD_ALT;
    }
    if modifiers.meta {
        mask |= MOD_SUPER;
    }
    mask
}

// ---------------------------------------------------------------------------
// Keysyms
// ---------------------------------------------------------------------------

/// Every key the shared parser writes as a word, paired with its X11 keysym
/// name and value.
///
/// The first column holds the canonical spellings [`Accelerator::key`] returns,
/// so this table is exactly as long as that vocabulary. Several entries differ
/// from the keysym -- `Enter` is `Return`, the page keys are `Prior` and `Next`
/// -- and a grab on a name X cannot resolve is a hotkey that can only ever be
/// dead.
const NAMED_KEYSYMS: [(&str, &str, u32); 15] = [
    ("Space", "space", 0x0020),
    ("Enter", "Return", 0xff0d),
    ("Tab", "Tab", 0xff09),
    ("Escape", "Escape", 0xff1b),
    ("Backspace", "BackSpace", 0xff08),
    ("Delete", "Delete", 0xffff),
    ("Insert", "Insert", 0xff63),
    ("Home", "Home", 0xff50),
    ("End", "End", 0xff57),
    ("PageUp", "Prior", 0xff55),
    ("PageDown", "Next", 0xff56),
    ("Up", "Up", 0xff52),
    ("Down", "Down", 0xff54),
    ("Left", "Left", 0xff51),
    ("Right", "Right", 0xff53),
];

/// Keysym names for `A` through `Z`, which on X11 are the *unshifted* symbols:
/// the keysym `A` is what the key produces with Shift held, so binding it would
/// make `Ctrl+A` fire only when Shift was down too.
const LETTER_KEYSYM_NAMES: [&str; 26] = [
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s", "t", "u",
    "v", "w", "x", "y", "z",
];

/// Keysym names for `0` through `9`, which spell themselves.
const DIGIT_KEYSYM_NAMES: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];

/// Keysym names for `F1` through `F24`, matching the range the shared parser
/// accepts. The `F` stays uppercase: `f1` names no keysym at all.
const FUNCTION_KEYSYM_NAMES: [&str; 24] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15", "F16",
    "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
];

/// `XK_a`; `b` through `z` follow it without a gap.
const KEYSYM_LOWERCASE_A: u32 = 0x0061;
/// `XK_0`; `1` through `9` follow it without a gap.
const KEYSYM_ZERO: u32 = 0x0030;
/// `XK_F1`; `F2` through `F24` follow it without a gap.
const KEYSYM_F1: u32 = 0xffbe;
/// `XK_Num_Lock`, looked up to find whichever modifier the server puts it on.
const KEYSYM_NUM_LOCK: u32 = 0xff7f;

/// The keysym name and value a canonical key name stands for, or `None` when
/// this backend has no keysym for it.
///
/// Comparison is exact because [`Accelerator`] only ever yields the canonical
/// spelling of a key: matching case insensitively here would only paper over a
/// key name this table has not been taught.
fn keysym(key: &str) -> Option<(&'static str, u32)> {
    if let Some(&(_, name, value)) = NAMED_KEYSYMS.iter().find(|(canonical, _, _)| *canonical == key) {
        return Some((name, value));
    }

    if let [single] = *key.as_bytes() {
        if single.is_ascii_uppercase() {
            let index = usize::from(single - b'A');
            return Some((LETTER_KEYSYM_NAMES[index], KEYSYM_LOWERCASE_A + index as u32));
        }
        if single.is_ascii_digit() {
            let index = usize::from(single - b'0');
            return Some((DIGIT_KEYSYM_NAMES[index], KEYSYM_ZERO + index as u32));
        }
        return None;
    }

    function_keysym(key)
}

/// `F1` to `F24`, contiguous from `XK_F1`.
fn function_keysym(key: &str) -> Option<(&'static str, u32)> {
    let number: usize = key.strip_prefix('F')?.parse().ok()?;
    let index = number.checked_sub(1)?;
    let name = *FUNCTION_KEYSYM_NAMES.get(index)?;
    Some((name, KEYSYM_F1 + index as u32))
}

/// The `(modifier mask, keysym name)` pair an accelerator is grabbed as.
///
/// Pure: no server is needed and none is consulted. The mask is the base one,
/// without the lock-modifier permutations a real grab also takes, because those
/// describe how X matches rather than what the user bound.
///
/// # Errors
///
/// [`CoreError::Invalid`] when the key names no keysym this backend knows.
/// Every key the shared parser accepts maps, so this guards against the two
/// vocabularies drifting apart; it is not a routine outcome.
pub fn x11_binding(accelerator: &Accelerator) -> Result<(u32, String)> {
    let (name, _) = keysym_of(accelerator)?;
    Ok((modifier_mask(accelerator.modifiers()), name.to_owned()))
}

/// The keysym an accelerator's key names, or the shared refusal.
fn keysym_of(accelerator: &Accelerator) -> Result<(&'static str, u32)> {
    keysym(accelerator.key()).ok_or_else(|| {
        CoreError::Invalid(format!(
            "{} names no X11 keysym this backend can grab",
            accelerator.key()
        ))
    })
}

// ---------------------------------------------------------------------------
// Live grabs
// ---------------------------------------------------------------------------

/// One accelerator's live grab: what has to be handed back to release it, and
/// what an incoming `KeyPress` is matched against.
#[derive(Debug, Clone)]
struct Grab {
    keycode: Keycode,
    /// The base mask, without lock bits: what a stripped `KeyPress` state
    /// equals.
    mask: u32,
    /// Every mask actually grabbed, so the release gives all of them back.
    grabbed: Vec<u32>,
}

/// The accelerators one service holds, keyed by canonical rendering so that
/// every spelling of one chord is one registration.
type Registrations = HashMap<String, Grab>;

/// Everything the reader thread needs to turn a `KeyPress` into a handler call.
///
/// The handler sits behind its own `Arc` so the reader can clone it out of the
/// lock and release the lock *before* calling it: a handler that reconfigured
/// the service would otherwise deadlock against the lock it was invoked under.
struct Shared {
    registrations: Mutex<Registrations>,
    handler: Mutex<Option<Arc<HotkeyActivationHandler>>>,
    /// Set before the wake-up `ClientMessage` is sent, so the reader can tell a
    /// shutdown from any other message.
    stopping: AtomicBool,
    /// Set by the reader when the connection dies under it.
    ///
    /// Without this a service whose X server has gone keeps its `registrations`
    /// map intact, and re-registering an accelerator already in that map would
    /// return `Ok(())` having made no X request at all -- a hotkey the launcher
    /// believes is live and that nothing can ever deliver.
    failed: AtomicBool,
    /// The X server time of the last key press this reader saw, shared with the
    /// window service so that `_NET_ACTIVE_WINDOW` can carry a real
    /// user-activity timestamp instead of `CurrentTime`.
    user_time: Arc<AtomicU32>,
}

/// Locks through poisoning: a panicking handler must not turn every later
/// registration into a panic of its own.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Reports an X failure as the shared error type, naming what was attempted.
fn invalid(context: &str, error: impl fmt::Display) -> CoreError {
    CoreError::Invalid(format!("{context}: {error}"))
}

// ---------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------

/// Global hotkeys backed by X11 `GrabKey` (spec 18.6).
///
/// Grabs are taken against the root window of the connection's default screen,
/// which is what makes them global: they fire whatever window has focus.
///
/// Dropping the service stops the reader thread and closes the connection, and
/// closing the connection is what makes the server drop every grab this client
/// still held.
pub struct X11HotkeyService {
    connection: Arc<RustConnection>,
    root: Window,
    /// The throwaway window the shutdown `ClientMessage` is addressed to.
    wake_window: Window,
    wake_atom: u32,
    /// Lock-bit combinations every grab is taken for.
    lock_permutations: Vec<u32>,
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
}

impl fmt::Debug for X11HotkeyService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("X11HotkeyService")
            .field("root", &self.root)
            .field("registrations", &lock(&self.shared.registrations).len())
            .finish_non_exhaustive()
    }
}

impl X11HotkeyService {
    /// Connects to `display`, or to `$DISPLAY` when it is `None`.
    ///
    /// The service keeps its own user-activity clock, which nothing else reads:
    /// use [`Self::connect_sharing`] to hand it the clock the window service
    /// stamps `_NET_ACTIVE_WINDOW` from.
    ///
    /// # Errors
    ///
    /// [`CoreError::Invalid`] when no server answers, when the display name is
    /// unusable, or when the connection cannot be set up far enough to take a
    /// grab. A failed connection is never softened into a service that accepts
    /// registrations and swallows them: the launcher would believe its hotkey
    /// was live when nothing could ever deliver it.
    pub fn connect(display: Option<&str>) -> Result<Self> {
        Self::connect_sharing(display, Arc::new(AtomicU32::new(0)))
    }

    /// Connects as [`Self::connect`] does, publishing every key-press time it
    /// sees into `user_time`.
    ///
    /// # Errors
    ///
    /// As [`Self::connect`].
    pub fn connect_sharing(display: Option<&str>, user_time: Arc<AtomicU32>) -> Result<Self> {
        let named = display.unwrap_or("$DISPLAY");
        let (connection, screen) = RustConnection::connect(display)
            .map_err(|error| invalid(&format!("connecting to X display {named}"), error))?;
        let connection = Arc::new(connection);

        let root = connection
            .setup()
            .roots
            .get(screen)
            .ok_or_else(|| CoreError::Invalid(format!("X display {named} has no screen {screen}")))?
            .root;

        let lock_permutations = lock_permutations(&connection)?;
        let (wake_window, wake_atom) = create_wake_target(&connection, root)?;

        let shared = Arc::new(Shared {
            registrations: Mutex::new(Registrations::new()),
            handler: Mutex::new(None),
            stopping: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            user_time,
        });
        let reader = spawn_reader(Arc::clone(&connection), Arc::clone(&shared), &lock_permutations);

        Ok(Self {
            connection,
            root,
            wake_window,
            wake_atom,
            lock_permutations,
            shared,
            reader: Some(reader),
        })
    }

    /// Whether the reader has seen this service's connection die.
    ///
    /// A failed service can no longer take, hold or deliver a grab, so nothing
    /// it still lists as registered is live.
    pub fn has_failed(&self) -> bool {
        self.shared.failed.load(Ordering::Acquire)
    }

    /// The keycode the server currently maps `keysym` to.
    ///
    /// Resolved per registration rather than cached: a user who switches
    /// keyboard layout while the launcher runs would otherwise keep a grab on
    /// whatever key used to carry the symbol.
    fn keycode_for(&self, keysym_name: &str, keysym_value: u32) -> Result<Keycode> {
        let (first, mapping) = keyboard_mapping(&self.connection)?;
        let per_keycode = usize::from(mapping.keysyms_per_keycode);
        if per_keycode == 0 {
            return Err(CoreError::Invalid(
                "the X server reports an empty keyboard mapping".to_owned(),
            ));
        }

        // A column is a shift level, so the lowest column carrying the symbol
        // is the plainest way to press it.
        for column in 0..per_keycode {
            for (index, symbols) in mapping.keysyms.chunks(per_keycode).enumerate() {
                if symbols.get(column) == Some(&keysym_value) {
                    return Ok(first.saturating_add(index as u8));
                }
            }
        }

        Err(CoreError::Invalid(format!(
            "the X server's keyboard mapping has no key for keysym {keysym_name}"
        )))
    }

    /// Takes the grab for every lock permutation, undoing the ones already
    /// taken if any of them is refused.
    fn grab(&self, grab: &Grab) -> Result<()> {
        for (taken, mask) in grab.grabbed.iter().enumerate() {
            let outcome = self
                .connection
                .grab_key(
                    false,
                    self.root,
                    ModMask::from(*mask as u16),
                    grab.keycode,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )
                .map_err(|error| invalid("grabbing the hotkey", error))
                .and_then(|cookie| {
                    cookie
                        .check()
                        .map_err(|error| invalid("grabbing the hotkey", error))
                });

            if let Err(error) = outcome {
                self.release_masks(grab.keycode, &grab.grabbed[..taken]);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Hands every permutation of a grab back to the server.
    ///
    /// A refusal is reported rather than swallowed: it means the key is still
    /// being eaten, which the caller has to be able to see.
    fn ungrab(&self, grab: &Grab) -> Result<()> {
        let mut failure = None;
        for mask in &grab.grabbed {
            let outcome = self
                .connection
                .ungrab_key(grab.keycode, self.root, ModMask::from(*mask as u16))
                .map_err(|error| invalid("releasing the hotkey", error))
                .and_then(|cookie| {
                    cookie
                        .check()
                        .map_err(|error| invalid("releasing the hotkey", error))
                });
            if let Err(error) = outcome {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Rolls back a grab that was refused part way through its permutations.
    fn release_masks(&self, keycode: Keycode, masks: &[u32]) {
        for mask in masks {
            if let Ok(cookie) = self
                .connection
                .ungrab_key(keycode, self.root, ModMask::from(*mask as u16))
            {
                let _ = cookie.check();
            }
        }
    }
}

/// The whole keyboard mapping, with the keycode its first row describes.
fn keyboard_mapping(
    connection: &RustConnection,
) -> Result<(Keycode, x11rb::protocol::xproto::GetKeyboardMappingReply)> {
    let setup = connection.setup();
    let first = setup.min_keycode;
    let count = setup.max_keycode.saturating_sub(first).saturating_add(1);
    let mapping = connection
        .get_keyboard_mapping(first, count)
        .map_err(|error| invalid("asking X for the keyboard mapping", error))?
        .reply()
        .map_err(|error| invalid("asking X for the keyboard mapping", error))?;
    Ok((first, mapping))
}

/// Creates the window and atom the shutdown `ClientMessage` travels on.
///
/// The window is `InputOnly` and never mapped: it exists only to be addressed,
/// so it must not appear on screen or take input away from anything.
fn create_wake_target(connection: &RustConnection, root: Window) -> Result<(Window, u32)> {
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
        .map_err(|error| invalid("creating the hotkey wake-up window", error))?
        .check()
        .map_err(|error| invalid("creating the hotkey wake-up window", error))?;

    let atom = connection
        .intern_atom(false, b"CRIKEY_HOTKEY_WAKE")
        .map_err(|error| invalid("interning the hotkey wake-up atom", error))?
        .reply()
        .map_err(|error| invalid("interning the hotkey wake-up atom", error))?
        .atom;
    Ok((window, atom))
}

/// Starts the thread that turns `KeyPress` events into handler calls.
fn spawn_reader(
    connection: Arc<RustConnection>,
    shared: Arc<Shared>,
    lock_permutations: &[u32],
) -> JoinHandle<()> {
    // Whatever the lock bits are on this server, they are never part of a
    // binding, so they are stripped before a press is matched.
    let lock_bits = lock_permutations.iter().fold(0, |bits, mask| bits | mask);

    std::thread::spawn(move || loop {
        let Ok(event) = connection.wait_for_event() else {
            // The connection is gone; there is nothing left to read, and every
            // grab this service still lists went with it. Saying so is what
            // stops a later `register` of an already-listed accelerator from
            // reporting a live hotkey nothing can deliver.
            shared.failed.store(true, Ordering::Release);
            return;
        };
        match event {
            Event::ClientMessage(_) if shared.stopping.load(Ordering::Acquire) => return,
            Event::KeyPress(press) => {
                // The only real user action this process ever observes, and so
                // the timestamp EWMH wants on an activation request.
                shared.user_time.store(press.time, Ordering::Relaxed);
                let state = u32::from(u16::from(press.state)) & !lock_bits & BINDING_MASK;
                let activated = lock(&shared.registrations)
                    .iter()
                    .find(|(_, grab)| grab.keycode == press.detail && grab.mask == state)
                    .map(|(canonical, _)| canonical.clone());
                let Some(accelerator) = activated else {
                    continue;
                };
                let handler = lock(&shared.handler).clone();
                if let Some(handler) = handler {
                    handler(&HotkeyBinding { accelerator });
                }
            }
            _ => {}
        }
    })
}

/// The lock-modifier combinations a grab is taken for.
///
/// CapsLock is always `LockMask`; NumLock is wherever the server's modifier map
/// puts it, so it is looked up rather than assumed. A server with no NumLock key
/// yields just the two CapsLock states.
fn lock_permutations(connection: &RustConnection) -> Result<Vec<u32>> {
    let (first, mapping) = keyboard_mapping(connection)?;
    let per_keycode = usize::from(mapping.keysyms_per_keycode).max(1);
    let num_lock_keycode = mapping
        .keysyms
        .chunks(per_keycode)
        .position(|symbols| symbols.contains(&KEYSYM_NUM_LOCK))
        .map(|index| first.saturating_add(index as u8));

    let mut permutations = vec![0, MOD_LOCK];
    let Some(keycode) = num_lock_keycode else {
        return Ok(permutations);
    };

    let modifiers = connection
        .get_modifier_mapping()
        .map_err(|error| invalid("asking X for the modifier mapping", error))?
        .reply()
        .map_err(|error| invalid("asking X for the modifier mapping", error))?;
    let per_modifier = usize::from(modifiers.keycodes_per_modifier()).max(1);
    let num_lock_mask = modifiers
        .keycodes
        .chunks(per_modifier)
        .position(|codes| codes.contains(&keycode))
        .map_or(0u32, |index| 1 << index);

    if num_lock_mask != 0 {
        permutations.push(num_lock_mask);
        permutations.push(num_lock_mask | MOD_LOCK);
    }
    Ok(permutations)
}

impl HotkeyService for X11HotkeyService {
    /// Registers an accelerator, or reports why X would not take the grab.
    ///
    /// Re-registering an accelerator this service already holds succeeds
    /// without touching X: the caller asked for a live binding and has one.
    /// Duplicate detection is keyed on the canonical rendering, so every
    /// spelling of one chord is one registration -- keying it on the raw string
    /// would let `alt+ctrl+SPACE` take a second grab on the chord
    /// `Ctrl+Alt+Space` already holds, and the hotkey would then fire twice and
    /// survive its own release.
    ///
    /// That shortcut is only sound while the connection is alive, so a service
    /// whose reader has seen the server go refuses every registration by name
    /// instead: its `registrations` map still lists chords, but the grabs behind
    /// them died with the connection and no press can ever be delivered again.
    fn register(&mut self, binding: &HotkeyBinding) -> Result<()> {
        let accelerator = parse(binding)?;
        let canonical = accelerator.canonical();
        if self.has_failed() {
            return Err(CoreError::Invalid(format!(
                "the X11 hotkey connection is gone, so {canonical} cannot be registered and no \
                 accelerator this service still lists can fire"
            )));
        }
        if lock(&self.shared.registrations).contains_key(&canonical) {
            return Ok(());
        }

        let (name, value) = keysym_of(&accelerator)?;
        let mask = modifier_mask(accelerator.modifiers());
        let grab = Grab {
            keycode: self.keycode_for(name, value)?,
            mask,
            grabbed: self.lock_permutations.iter().map(|locks| mask | locks).collect(),
        };

        self.grab(&grab)?;
        lock(&self.shared.registrations).insert(canonical, grab);
        Ok(())
    }

    /// Releases an accelerator this service registered.
    ///
    /// An accelerator that was never registered is an error rather than a quiet
    /// success: it means the caller and the backend disagree about what is live,
    /// and a launcher that thinks it released a hotkey it never held will keep
    /// swallowing that key press.
    ///
    /// The bookkeeping entry is dropped even when X refuses the ungrab. Keeping
    /// it would describe a registration nobody can act on any more, and the
    /// error still reaches the caller.
    fn unregister(&mut self, binding: &HotkeyBinding) -> Result<()> {
        let accelerator = parse(binding)?;
        let canonical = accelerator.canonical();
        let grab = lock(&self.shared.registrations)
            .remove(&canonical)
            .ok_or_else(|| CoreError::Invalid(format!("{canonical} holds no X11 hotkey grab to release")))?;
        self.ungrab(&grab)
    }

    /// Installs the callback the reader thread invokes on an activation.
    ///
    /// Registrations are untouched, so a handler may be swapped or cleared while
    /// hotkeys stay live: the launcher clears its handler while reconfiguring
    /// and must not lose its grabs to that.
    fn set_activation_handler(&mut self, handler: Option<HotkeyActivationHandler>) {
        *lock(&self.shared.handler) = handler.map(Arc::new);
    }
}

impl X11HotkeyService {
    /// Sends the reader its wake-up `ClientMessage`, checked.
    ///
    /// An event mask of zero means X delivers this to the client that created
    /// `target`, which is exactly the blocked reader.
    fn send_wake(&self, target: Window, atom: u32) -> std::result::Result<(), ReplyError> {
        let wake = ClientMessageEvent::new(32, target, atom, [0u32; 5]);
        self.connection
            .send_event(false, target, EventMask::NO_EVENT, wake)?
            .check()?;
        self.connection.flush()?;
        Ok(())
    }

    /// Gets the reader out of `wait_for_event`, reporting whether joining it is
    /// now safe.
    ///
    /// The wake target is a private `InputOnly` child of the root, and privacy
    /// is not protection: any other client can find it with `QueryTree` and
    /// destroy it. The send then fails with `BadWindow`, which an unchecked
    /// request reports asynchronously and nobody sees -- so the reader stays
    /// blocked and `join` never returns. Checking the send is what turns that
    /// into a case with an answer.
    fn wake_reader(&self) -> bool {
        match self.send_wake(self.wake_window, self.wake_atom) {
            Ok(()) => true,
            // The connection is gone, so the reader has already left
            // `wait_for_event` on its own and joining it cannot block.
            Err(ReplyError::ConnectionError(_)) => true,
            // The target was destroyed under us. A fresh one does the same job:
            // all that matters is that the message comes back to this client.
            // It is left for the connection close to reap, because destroying
            // it here would race the reader that still has to see the message.
            Err(_) => match create_wake_target(&self.connection, self.root) {
                Ok((window, atom)) => self.send_wake(window, atom).is_ok(),
                Err(_) => false,
            },
        }
    }
}

impl Drop for X11HotkeyService {
    /// Stops the reader thread, then lets the connection close -- which is what
    /// makes the server drop every grab this client still held.
    ///
    /// The reader is only joined once the wake-up is known to have been
    /// delivered. When it cannot be, the thread is detached rather than waited
    /// on: a launcher that hangs forever in a destructor is a worse outcome than
    /// one leaked thread on a connection that is about to close underneath it.
    fn drop(&mut self) {
        let Some(reader) = self.reader.take() else {
            return;
        };

        self.shared.stopping.store(true, Ordering::Release);
        if self.wake_reader() {
            let _ = reader.join();
        }
        if let Ok(cookie) = self.connection.destroy_window(self.wake_window) {
            let _ = cookie.check();
        }
    }
}

/// Parses the accelerator a binding names, reporting the shared parser's own
/// reason for refusing it.
fn parse(binding: &HotkeyBinding) -> Result<Accelerator> {
    Accelerator::parse(&binding.accelerator).map_err(|error| {
        CoreError::Invalid(format!(
            "{:?} is not a usable hotkey: {error}",
            binding.accelerator
        ))
    })
}
