//! Global hotkeys over `RegisterHotKey` (spec 6.1, 18.4).
//!
//! Three pieces stack here. [`HotkeyCode`] turns a parsed [`Accelerator`] into
//! the `(fsModifiers, uVirtKey)` pair Win32 wants. [`HotkeyRegistrations`]
//! hands each accelerator a registration id and remembers it. [`WindowsHotkeys`]
//! is the [`HotkeyService`] that puts the two together and, on Windows, owns the
//! message thread the activations arrive on.
//!
//! Only the last of those needs Win32, so only the last is target gated. The
//! mapping and the id allocator are ordinary data structures and are tested as
//! such on every host.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use crikey_core::{CoreError, Result};
use crikey_platform::{Accelerator, HotkeyActivationHandler, HotkeyBinding, HotkeyService, Modifiers};

#[cfg(target_os = "windows")]
mod win32;

// ---------------------------------------------------------------------------
// Accelerators as Win32 hotkey codes
// ---------------------------------------------------------------------------

/// What `RegisterHotKey` is called with: a modifier mask and a virtual key.
///
/// Both are the documented Win32 numbers rather than a re-abstraction of them,
/// because the whole point of this type is to be checkable against `winuser.h`.
/// A Windows-only test asserts every constant here equals the one Microsoft's
/// own metadata generates, so the table can be trusted on hosts that have no
/// `winuser.h` to check it against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HotkeyCode {
    modifiers: u32,
    virtual_key: u16,
}

impl HotkeyCode {
    /// `MOD_ALT`.
    pub const MOD_ALT: u32 = 0x0001;
    /// `MOD_CONTROL`.
    pub const MOD_CONTROL: u32 = 0x0002;
    /// `MOD_SHIFT`.
    pub const MOD_SHIFT: u32 = 0x0004;
    /// `MOD_WIN`, the modifier [`Modifiers::meta`] names on Windows.
    pub const MOD_WIN: u32 = 0x0008;
    /// `MOD_NOREPEAT`.
    ///
    /// Always set. Without it a held-down accelerator repeats at the keyboard
    /// auto-repeat rate, and a launcher that toggles its window on every
    /// activation would flicker for as long as the user leans on the key.
    pub const MOD_NOREPEAT: u32 = 0x4000;

    /// Maps a parsed accelerator onto the Win32 pair.
    ///
    /// Fails rather than guesses. Every key the shared parser accepts today has
    /// an entry below, but if that vocabulary ever grows, an accelerator naming
    /// the new key must be refused loudly here instead of quietly binding
    /// whatever key happens to fall out of a lenient fallback.
    pub fn from_accelerator(accelerator: &Accelerator) -> Result<Self> {
        let key = accelerator.key();
        let virtual_key = virtual_key(key).ok_or_else(|| {
            CoreError::Invalid(format!(
                "the {key} key has no Win32 virtual-key code in this backend"
            ))
        })?;

        Ok(Self {
            modifiers: modifier_mask(accelerator.modifiers()),
            virtual_key,
        })
    }

    /// The `fsModifiers` argument, `MOD_NOREPEAT` included.
    pub fn modifiers(self) -> u32 {
        self.modifiers
    }

    /// The `uVirtKey` argument.
    pub fn virtual_key(self) -> u16 {
        self.virtual_key
    }
}

/// The `MOD_*` mask an accelerator's modifiers add up to.
fn modifier_mask(modifiers: Modifiers) -> u32 {
    let mut mask = HotkeyCode::MOD_NOREPEAT;
    if modifiers.ctrl {
        mask |= HotkeyCode::MOD_CONTROL;
    }
    if modifiers.alt {
        mask |= HotkeyCode::MOD_ALT;
    }
    if modifiers.shift {
        mask |= HotkeyCode::MOD_SHIFT;
    }
    if modifiers.meta {
        mask |= HotkeyCode::MOD_WIN;
    }
    mask
}

/// The virtual key a canonical key name stands for, or `None` when this
/// backend has no code for it.
///
/// Comparison is exact because [`Accelerator`] only ever yields the canonical
/// spelling of a key: matching case insensitively here would only paper over a
/// key name this table has not been taught.
fn virtual_key(key: &str) -> Option<u16> {
    if let Some((_, code)) = NAMED_VIRTUAL_KEYS.iter().find(|(name, _)| *name == key) {
        return Some(*code);
    }

    // Letters and digits are their ASCII code point: `VK_A` is 0x41, `VK_0` is
    // 0x30. This is the documented layout of the virtual-key space, not a
    // coincidence worth spelling out twenty-six more times.
    if let [single] = *key.as_bytes() {
        if single.is_ascii_uppercase() || single.is_ascii_digit() {
            return Some(u16::from(single));
        }
    }

    function_virtual_key(key)
}

/// `F1` to `F24`, contiguous from `VK_F1`.
fn function_virtual_key(key: &str) -> Option<u16> {
    let number: u16 = key.strip_prefix('F')?.parse().ok()?;
    (1..=FUNCTION_KEY_COUNT)
        .contains(&number)
        .then(|| VK_F1 + number - 1)
}

/// `VK_F1`; `VK_F2` through `VK_F24` follow it without a gap.
const VK_F1: u16 = 0x70;

/// How many function keys the virtual-key space defines, matching the range
/// the shared accelerator parser accepts.
const FUNCTION_KEY_COUNT: u16 = 24;

/// Every key the shared parser writes as a word, paired with its virtual key.
///
/// The names are the canonical spellings [`Accelerator::key`] returns, so this
/// table is exactly as long as that vocabulary; a test pins the correspondence
/// in both directions.
const NAMED_VIRTUAL_KEYS: [(&str, u16); 15] = [
    ("Space", 0x20),     // VK_SPACE
    ("Enter", 0x0D),     // VK_RETURN
    ("Tab", 0x09),       // VK_TAB
    ("Escape", 0x1B),    // VK_ESCAPE
    ("Backspace", 0x08), // VK_BACK
    ("Delete", 0x2E),    // VK_DELETE
    ("Insert", 0x2D),    // VK_INSERT
    ("Home", 0x24),      // VK_HOME
    ("End", 0x23),       // VK_END
    ("PageUp", 0x21),    // VK_PRIOR
    ("PageDown", 0x22),  // VK_NEXT
    ("Up", 0x26),        // VK_UP
    ("Down", 0x28),      // VK_DOWN
    ("Left", 0x25),      // VK_LEFT
    ("Right", 0x27),     // VK_RIGHT
];

// ---------------------------------------------------------------------------
// Registration ids
// ---------------------------------------------------------------------------

/// One accelerator's live `RegisterHotKey` registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyRegistration {
    id: i32,
    accelerator: String,
    code: HotkeyCode,
}

impl HotkeyRegistration {
    /// The `id` argument `RegisterHotKey` and `UnregisterHotKey` share.
    pub fn id(&self) -> i32 {
        self.id
    }

    /// The canonical accelerator this registration was made for.
    pub fn accelerator(&self) -> &str {
        &self.accelerator
    }

    /// The modifier mask and virtual key it was registered with.
    pub fn code(&self) -> HotkeyCode {
        self.code
    }
}

/// The registration ids one hotkey service has handed out.
///
/// Ids are stable in the sense that matters to `UnregisterHotKey`: an
/// accelerator keeps the id it was given until it is unregistered, and the
/// allocator is deterministic, so a given sequence of register and unregister
/// calls always produces the same ids. Freed ids are reused, lowest first,
/// which keeps a launcher that rebinds its shortcut on every config reload from
/// walking off the end of the id space.
#[derive(Debug, Default)]
pub struct HotkeyRegistrations {
    /// Sorted by id, so allocation is a single scan and iteration is
    /// reproducible.
    entries: Vec<HotkeyRegistration>,
}

impl HotkeyRegistrations {
    /// The lowest id this allocator hands out.
    ///
    /// Zero is a legal `RegisterHotKey` id but a poor sentinel, and nothing is
    /// gained by using it.
    pub const MIN_ID: i32 = 1;

    /// The highest id an application may use.
    ///
    /// Win32 reserves `0xC000`-`0xFFFF` for ids obtained from `GlobalAddAtom`,
    /// which is a shared-DLL convention this backend has no use for.
    pub const MAX_ID: i32 = 0xBFFF;

    pub fn new() -> Self {
        Self::default()
    }

    /// How many accelerators are registered.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The registration for a canonical accelerator, if it has one.
    pub fn find(&self, accelerator: &str) -> Option<&HotkeyRegistration> {
        self.entries.iter().find(|entry| entry.accelerator == accelerator)
    }

    /// Every registration, in id order.
    pub fn iter(&self) -> impl Iterator<Item = &HotkeyRegistration> {
        self.entries.iter()
    }

    /// Reserves the lowest free id for a canonical accelerator.
    ///
    /// Rejects an accelerator that already holds an id: the caller decides
    /// whether that is an error or a no-op, and this allocator will not quietly
    /// hand out a second id for a hotkey Win32 would refuse to register twice.
    pub fn insert(&mut self, accelerator: String, code: HotkeyCode) -> Result<HotkeyRegistration> {
        if self.find(&accelerator).is_some() {
            return Err(CoreError::Invalid(format!(
                "{accelerator} already holds a Windows hotkey registration id"
            )));
        }

        let (index, id) = self.free_slot()?;

        let registration = HotkeyRegistration {
            id,
            accelerator,
            code,
        };
        self.entries.insert(index, registration.clone());
        Ok(registration)
    }

    /// Releases a canonical accelerator's id.
    pub fn remove(&mut self, accelerator: &str) -> Option<HotkeyRegistration> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.accelerator == accelerator)?;
        Some(self.entries.remove(index))
    }

    /// Where the next registration belongs and which id it gets: the front of
    /// the first gap in the id sequence, or the end when the ids are
    /// contiguous.
    fn free_slot(&self) -> Result<(usize, i32)> {
        let mut candidate = Self::MIN_ID;
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.id > candidate {
                return Ok((index, candidate));
            }
            candidate = entry.id + 1;
        }

        if candidate > Self::MAX_ID {
            return Err(CoreError::CapacityExceeded("windows hotkey registration ids"));
        }
        Ok((self.entries.len(), candidate))
    }
}

// ---------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------

/// The activation callback, shared with the platform message thread.
///
/// The handler lives behind a second `Arc` so the message thread can clone it
/// out of the lock and release the lock *before* calling it. A handler that
/// touched the service again -- clearing itself on the last activation, say --
/// would otherwise deadlock against the lock it was invoked under.
#[derive(Default)]
struct HandlerSlot(Mutex<Option<Arc<HotkeyActivationHandler>>>);

impl HandlerSlot {
    fn set(&self, handler: Option<HotkeyActivationHandler>) {
        // A panicking handler poisons nothing worth protecting: the slot holds
        // one callback, and refusing to ever replace it again would strand the
        // launcher with a hotkey it cannot rebind.
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = handler.map(Arc::new);
    }

    /// The installed handler, if any, with the lock already released.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    fn handler(&self) -> Option<Arc<HotkeyActivationHandler>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl fmt::Debug for HandlerSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `try_lock` because a `Debug` that can block -- or that deadlocks when
        // something logs from inside the handler -- is worse than one that
        // admits it could not look.
        let installed = match self.0.try_lock() {
            Ok(handler) => match *handler {
                Some(_) => "installed",
                None => "none",
            },
            Err(_) => "busy",
        };
        formatter.debug_tuple("HandlerSlot").field(&installed).finish()
    }
}

/// Global hotkeys backed by `RegisterHotKey` (spec 18.4).
///
/// On Windows the first successful registration starts a dedicated thread
/// owning a message-only window; the thread is what makes activation delivery
/// possible at all, because `WM_HOTKEY` goes to the thread that registered the
/// hotkey and the launcher's own event loop offers no hook for it. A backend
/// that is built and never registers anything never starts that thread.
///
/// Off target every registration is refused. Nothing is faked: a hotkey that
/// cannot reach `RegisterHotKey` is not registered, and the caller is told.
#[derive(Debug)]
pub struct WindowsHotkeys {
    registrations: HotkeyRegistrations,
    handler: Arc<HandlerSlot>,
    #[cfg(target_os = "windows")]
    thread: Option<win32::MessageThread>,
}

impl WindowsHotkeys {
    pub fn new() -> Self {
        Self {
            registrations: HotkeyRegistrations::new(),
            handler: Arc::new(HandlerSlot::default()),
            #[cfg(target_os = "windows")]
            thread: None,
        }
    }

    /// The ids currently held, for diagnostics.
    pub fn registrations(&self) -> &HotkeyRegistrations {
        &self.registrations
    }

    /// Hands one reserved registration to `RegisterHotKey`.
    #[cfg(target_os = "windows")]
    fn bind(&mut self, registration: &HotkeyRegistration) -> Result<()> {
        if self.thread.is_none() {
            self.thread = Some(win32::MessageThread::start(Arc::clone(&self.handler))?);
        }
        let thread = self.thread.as_ref().expect("the message thread was just started");
        thread.register(registration)
    }

    #[cfg(not(target_os = "windows"))]
    fn bind(&mut self, _registration: &HotkeyRegistration) -> Result<()> {
        Err(crate::off_target("register a global hotkey"))
    }

    /// Hands one released registration to `UnregisterHotKey`.
    #[cfg(target_os = "windows")]
    fn unbind(&mut self, registration: &HotkeyRegistration) -> Result<()> {
        match self.thread.as_ref() {
            Some(thread) => thread.unregister(registration),
            // Unreachable through `unregister`, which only ever passes a
            // registration this service made, and a registration can only have
            // been made through a running thread.
            None => Ok(()),
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn unbind(&mut self, _registration: &HotkeyRegistration) -> Result<()> {
        Err(crate::off_target("release a global hotkey"))
    }
}

impl Default for WindowsHotkeys {
    fn default() -> Self {
        Self::new()
    }
}

impl HotkeyService for WindowsHotkeys {
    /// Registers an accelerator, or reports why Windows would not take it.
    ///
    /// Re-registering an accelerator this service already holds succeeds
    /// without touching Win32: the caller asked for a live binding and has one.
    /// A failed registration releases the id it reserved, so a retry after the
    /// conflicting owner goes away starts from the same state as the first
    /// attempt.
    fn register(&mut self, binding: &HotkeyBinding) -> Result<()> {
        let accelerator = parse(binding)?;
        let canonical = accelerator.canonical();
        if self.registrations.find(&canonical).is_some() {
            return Ok(());
        }

        let code = HotkeyCode::from_accelerator(&accelerator)?;
        let registration = self.registrations.insert(canonical, code)?;
        match self.bind(&registration) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.registrations.remove(registration.accelerator());
                #[cfg(target_os = "windows")]
                if self.registrations.is_empty() {
                    // `bind` starts the message thread before asking Win32 to
                    // reserve the first id. If that reservation is refused,
                    // do not leave an idle thread behind for a service that
                    // owns no native hotkeys.
                    self.thread.take();
                }
                Err(error)
            }
        }
    }

    /// Releases an accelerator this service registered.
    ///
    /// An accelerator that was never registered is an error rather than a quiet
    /// success: it means the caller and the backend disagree about what is
    /// live, and a launcher that thinks it released a hotkey it never held will
    /// keep swallowing that key press.
    ///
    /// The logical registration is removed only after Win32 accepts the
    /// unregister request. If the native call fails, retaining the record lets
    /// shutdown retry cleanup and avoids losing track of a still-live global
    /// hotkey.
    fn unregister(&mut self, binding: &HotkeyBinding) -> Result<()> {
        let accelerator = parse(binding)?;
        let canonical = accelerator.canonical();
        let registration = self.registrations.find(&canonical).cloned().ok_or_else(|| {
            CoreError::Invalid(format!(
                "{canonical} holds no Windows hotkey registration to release"
            ))
        })?;
        self.unbind(&registration)?;
        self.registrations.remove(&canonical);
        Ok(())
    }

    /// Installs the callback the message thread invokes on `WM_HOTKEY`.
    ///
    /// Registrations are untouched, so a handler may be swapped or cleared
    /// while hotkeys stay live. Off target the handler is stored and can never
    /// fire, because nothing can ever be registered for it to fire for.
    fn set_activation_handler(&mut self, handler: Option<HotkeyActivationHandler>) {
        self.handler.set(handler);
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
