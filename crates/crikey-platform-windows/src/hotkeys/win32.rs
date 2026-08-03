//! The `RegisterHotKey` message thread.
//!
//! `WM_HOTKEY` is delivered to the thread that registered the hotkey, and
//! `UnregisterHotKey` must be called from that same thread. Neither fits a
//! launcher whose main thread belongs to a UI event loop with no hook for raw
//! window messages, so this module owns a thread of its own: one message-only
//! window, one `GetMessage` loop, and a synchronous door for the service to
//! marshal registration through.
//!
//! Everything that touches Win32 state -- the registrations, the id-to-binding
//! table -- lives on that thread and is reached only from its window procedure.
//! The one value genuinely shared with the outside is the activation handler,
//! behind its own lock. There is therefore no lock the service holds while it
//! blocks in `SendMessageW`, which is what makes the marshalling deadlock free.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use windows::core::{w, Error, HRESULT, PCWSTR};
use windows::Win32::Foundation::{E_INVALIDARG, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostMessageW,
    PostQuitMessage, RegisterClassW, SendMessageW, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CLOSE, WM_DESTROY, WM_HOTKEY, WNDCLASSW,
};

use crikey_core::{CoreError, Result};
use crikey_platform::HotkeyBinding;

use super::{HandlerSlot, HotkeyRegistration};

/// Window class of the message-only window. Registered once per process.
const CLASS_NAME: PCWSTR = w!("CriKeyHotkeyMessageWindow");

/// Marshalled `RegisterHotKey`. `wParam` carries the reserved registration id,
/// which the hotkey thread uses to retrieve the owned registration record.
const WM_CRIKEY_REGISTER: u32 = WM_APP;

/// Marshalled `UnregisterHotKey`. `wParam` carries the registration id.
const WM_CRIKEY_UNREGISTER: u32 = WM_APP + 1;

/// The reply a marshalled call leaves when Win32 accepted it.
///
/// Deliberately not zero: `SendMessageW` yields zero when it never reached a
/// window procedure, and a failure reply is always a failing `HRESULT`, whose
/// sign bit is set. One is therefore unambiguous.
const MARSHALLED_OK: LRESULT = LRESULT(1);

/// What lives on the hotkey thread.
struct ThreadState {
    handler: Arc<HandlerSlot>,
    /// Registration id to the binding an activation reports, so `WM_HOTKEY`
    /// can name the accelerator that fired without reaching back across the
    /// thread boundary.
    bindings: HashMap<i32, HotkeyBinding>,
    /// Registrations waiting for the synchronous register message. The window
    /// procedure removes a record by id before calling Win32, so it never
    /// trusts message memory as a registration object.
    pending: Arc<Mutex<HashMap<i32, HotkeyRegistration>>>,
}

thread_local! {
    static STATE: RefCell<Option<ThreadState>> = const { RefCell::new(None) };
}

/// An `HWND` handed to the thread that did not create it.
///
/// A window handle is a process-wide value; what has thread affinity is
/// creating and destroying the window and pumping its queue, and this crate
/// does all three on the hotkey thread. The handle crosses the boundary only to
/// be passed to `SendMessageW` and `PostMessageW`, which Win32 documents as
/// callable from any thread and which are precisely the mechanism for getting
/// work back onto the owning one.
struct WindowHandle(HWND);

// SAFETY: see the type's documentation. The handle is never dereferenced and
// never used for an operation Win32 restricts to the creating thread.
unsafe impl Send for WindowHandle {}
// SAFETY: as above; `SendMessageW` and `PostMessageW` are the only uses and are
// both thread safe.
unsafe impl Sync for WindowHandle {}
/// The hotkey thread and the door into it.
pub(super) struct MessageThread {
    window: WindowHandle,
    pending: Arc<Mutex<HashMap<i32, HotkeyRegistration>>>,
    join: Option<JoinHandle<()>>,
}

impl fmt::Debug for MessageThread {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MessageThread")
            .field("window", &self.window.0 .0)
            .field("running", &self.join.is_some())
            .finish()
    }
}

impl MessageThread {
    /// Starts the thread and waits for it to report a usable window.
    ///
    /// Waiting is the point: a service that returned before the window existed
    /// would have to queue registrations against a handle it does not have, and
    /// the first thing a launcher does after building the backend is register
    /// its activation shortcut.
    pub(super) fn start(handler: Arc<HandlerSlot>) -> Result<Self> {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let thread_pending = Arc::clone(&pending);
        let (ready, started) = mpsc::sync_channel::<std::result::Result<WindowHandle, String>>(1);
        let join = thread::Builder::new()
            .name("crikey-hotkeys".to_owned())
            .spawn(move || run(handler, &ready, thread_pending))
            .map_err(|error| {
                CoreError::Invalid(format!("the Windows hotkey thread could not start: {error}"))
            })?;

        match started.recv() {
            Ok(Ok(window)) => Ok(Self {
                window,
                pending,
                join: Some(join),
            }),
            Ok(Err(reason)) => {
                let _ = join.join();
                Err(CoreError::Invalid(reason))
            }
            Err(_) => {
                let _ = join.join();
                Err(CoreError::Invalid(
                    "the Windows hotkey thread stopped before it reported a window".to_owned(),
                ))
            }
        }
    }

    /// Registers one reserved id on the hotkey thread.
    pub(super) fn register(&self, registration: &HotkeyRegistration) -> Result<()> {
        let id = registration.id();
        {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            if pending.insert(id, registration.clone()).is_some() {
                return Err(CoreError::Invalid(format!(
                    "hotkey registration {id} is already being registered"
                )));
            }
        }

        let result = self.send(
            WM_CRIKEY_REGISTER,
            WPARAM(id as usize),
            LPARAM(0),
            registration.accelerator(),
            "register",
        );
        if result.is_err() {
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
        }
        result
    }

    /// Releases one id on the hotkey thread.
    pub(super) fn unregister(&self, registration: &HotkeyRegistration) -> Result<()> {
        self.send(
            WM_CRIKEY_UNREGISTER,
            WPARAM(registration.id() as usize),
            LPARAM(0),
            registration.accelerator(),
            "release",
        )
    }

    /// Runs one marshalled call and reads the reply the window procedure left.
    ///
    /// Success is a positive sentinel rather than zero, because zero is also
    /// what `SendMessageW` itself returns when it never reached a window
    /// procedure at all. Reading that as success would turn a hotkey thread
    /// that had gone away into a registration the launcher believes it holds.
    fn send(
        &self,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        accelerator: &str,
        action: &str,
    ) -> Result<()> {
        // SAFETY: the window is alive for as long as `self` is -- `Drop` is
        // what tears it down -- and cross-thread `SendMessageW` is documented.
        let reply = unsafe { SendMessageW(self.window.0, message, Some(wparam), Some(lparam)) };
        if reply == MARSHALLED_OK {
            return Ok(());
        }

        let reason = if reply.0 == 0 {
            "the hotkey message thread did not answer".to_owned()
        } else {
            // Every failure reply is a failing `HRESULT`, so it can never be
            // mistaken for the sentinel above.
            let code = HRESULT(reply.0 as i32);
            format!("{} ({code})", Error::from_hresult(code).message())
        };

        Err(CoreError::Invalid(format!(
            "Windows would not {action} the {accelerator} hotkey: {reason}"
        )))
    }
}

impl Drop for MessageThread {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        // `WM_CLOSE` reaches `DefWindowProcW`, which destroys the window;
        // `WM_DESTROY` then releases every registration and posts `WM_QUIT`,
        // which ends the loop. Posting rather than sending keeps the common
        // drop path off the hook if it is momentarily busy.
        // SAFETY: the handle is still ours; nothing else can have destroyed it.
        let posted = unsafe { PostMessageW(Some(self.window.0), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        if posted.is_err() {
            // An exhausted queue can reject the asynchronous post even while
            // the window is alive. A synchronous fallback makes shutdown
            // deterministic; if the window is already gone this returns
            // immediately and the join below observes the exited thread.
            // SAFETY: the handle is still ours and the message carries no
            // borrowed data.
            unsafe {
                let _ = SendMessageW(self.window.0, WM_CLOSE, Some(WPARAM(0)), Some(LPARAM(0)));
            }
        }

        // Never detach a live hotkey thread. Detaching here would leave the
        // thread's registrations owned by a window nobody can reach again.
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// The hotkey thread body.
fn run(
    handler: Arc<HandlerSlot>,
    ready: &SyncSender<std::result::Result<WindowHandle, String>>,
    pending: Arc<Mutex<HashMap<i32, HotkeyRegistration>>>,
) {
    let window = match create_window() {
        Ok(window) => window,
        Err(reason) => {
            let _ = ready.send(Err(reason));
            return;
        }
    };

    STATE.with(|state| {
        *state.borrow_mut() = Some(ThreadState {
            handler,
            bindings: HashMap::new(),
            pending,
        });
    });

    if ready.send(Ok(WindowHandle(window))).is_err() {
        // Nobody is left to hand the window to, so there is nobody to stop the
        // loop either. Tear it down instead of parking a thread forever.
        // SAFETY: called on the thread that created the window.
        unsafe {
            let _ = DestroyWindow(window);
        }
        STATE.with(|state| state.borrow_mut().take());
        return;
    }

    let mut message = MSG::default();
    // `GetMessageW` yields 0 on `WM_QUIT` and -1 on error; both end the loop.
    // No `TranslateMessage`: a message-only window receives no keyboard input
    // that would need translating.
    // SAFETY: `message` is a live, correctly typed buffer for the duration.
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        // SAFETY: `message` was filled by the call above.
        unsafe { DispatchMessageW(&message) };
    }

    // A `GetMessageW` error does not deliver `WM_DESTROY`, so clean up here as
    // well as in the normal window-destruction path. This keeps registrations
    // from surviving a message-queue failure.
    release_all(window);
    // SAFETY: this is still the thread that created the window. If normal
    // shutdown already destroyed it, Win32 simply reports failure.
    unsafe {
        let _ = DestroyWindow(window);
    }
    STATE.with(|state| state.borrow_mut().take());
}

/// The window class, registered the first time a hotkey thread needs it.
///
/// A class belongs to the process, not to a backend instance, so a second
/// `WindowsHotkeys` reuses this one rather than colliding with it. It is never
/// unregistered: it outlives every window that uses it and is reclaimed when
/// the process exits.
///
/// The module handle is kept as a plain integer because a Win32 handle is a
/// raw pointer and therefore not `Sync`, while a `static` must be. Nothing is
/// lost: a module base is an opaque process-wide value that this code only
/// ever hands straight back to Win32.
static CLASS: LazyLock<std::result::Result<usize, String>> = LazyLock::new(|| {
    // SAFETY: `None` asks for the handle of the running executable, which is
    // always loaded.
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| format!("the hotkey window has no module handle: {}", error.message()))?;
    let instance = HINSTANCE::from(module);

    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };

    // SAFETY: `class` outlives the call, and `RegisterClassW` copies it.
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(format!(
            "the hotkey window class could not be registered: {}",
            Error::from_win32().message()
        ));
    }
    Ok(instance.0 as usize)
});

/// Creates the message-only window the hotkeys hang off.
fn create_window() -> std::result::Result<HWND, String> {
    let instance = HINSTANCE((*CLASS).clone()? as *mut core::ffi::c_void);

    // SAFETY: the class is registered against this instance, and every other
    // argument is a plain value.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            CLASS_NAME,
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
    }
    .map_err(|error| {
        format!(
            "the hotkey message window could not be created: {}",
            error.message()
        )
    })
}

/// The window procedure, always on the hotkey thread.
unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_CRIKEY_REGISTER => {
            let id = wparam.0 as i32;
            let registration = STATE.with(|state| {
                let state = state.borrow();
                let Some(state) = state.as_ref() else {
                    return None;
                };
                let registration = state
                    .pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                registration
            });
            match registration {
                Some(registration) => bind(window, &registration),
                None => LRESULT(E_INVALIDARG.0 as isize),
            }
        }
        WM_CRIKEY_UNREGISTER => unbind(window, wparam.0 as i32),
        WM_HOTKEY => {
            activate(wparam.0 as i32);
            LRESULT(0)
        }
        WM_DESTROY => {
            release_all(window);
            // SAFETY: ends this thread's `GetMessageW` loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: forwarding untouched arguments is what the default procedure
        // is for, and `WM_CLOSE` reaching it is how the window gets destroyed.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

/// `RegisterHotKey` plus the id-to-binding entry an activation needs.
fn bind(window: HWND, registration: &HotkeyRegistration) -> LRESULT {
    let code = registration.code();
    // SAFETY: called on the thread that owns `window`, as Win32 requires.
    let registered = unsafe {
        RegisterHotKey(
            Some(window),
            registration.id(),
            HOT_KEY_MODIFIERS(code.modifiers()),
            u32::from(code.virtual_key()),
        )
    };

    match registered {
        Ok(()) => {
            STATE.with(|state| {
                if let Some(state) = state.borrow_mut().as_mut() {
                    state.bindings.insert(
                        registration.id(),
                        HotkeyBinding {
                            accelerator: registration.accelerator().to_owned(),
                        },
                    );
                }
            });
            MARSHALLED_OK
        }
        Err(error) => LRESULT(error.code().0 as isize),
    }
}
fn unbind(window: HWND, id: i32) -> LRESULT {
    let known = STATE.with(|state| {
        state
            .borrow_mut()
            .as_mut()
            .is_some_and(|state| state.bindings.remove(&id).is_some())
    });
    if !known {
        return LRESULT(E_INVALIDARG.0 as isize);
    }

    // SAFETY: called on the registering thread, as Win32 requires.
    match unsafe { UnregisterHotKey(Some(window), id) } {
        Ok(()) => MARSHALLED_OK,
        Err(error) => LRESULT(error.code().0 as isize),
    }
}
/// Hands one activation to the installed handler.
fn activate(id: i32) {
    // Both the handler and the binding are cloned out before the call, so
    // neither the `RefCell` nor the handler lock is held while foreign code
    // runs -- a handler that pumps messages, or that reconfigures the service,
    // must not deadlock the message thread.
    let call = STATE.with(|state| {
        let state = state.borrow();
        let state = state.as_ref()?;
        let binding = state.bindings.get(&id)?.clone();
        Some((state.handler.handler()?, binding))
    });

    if let Some((handler, binding)) = call {
        // The callback is application code running from an `extern "system"`
        // window procedure. Letting a panic unwind through that ABI would
        // terminate or violate the thread's cleanup path, so contain it here.
        let _ = catch_unwind(AssertUnwindSafe(|| (**handler)(&binding)));
    }
}

/// Releases every registration this thread still holds.
fn release_all(window: HWND) {
    STATE.with(|state| {
        if let Some(state) = state.borrow_mut().as_mut() {
            for (id, _) in state.bindings.drain() {
                // SAFETY: called on the registering thread. A failure here has
                // nowhere to go and nothing to fix: the window is being
                // destroyed, which drops the registration with it.
                unsafe {
                    let _ = UnregisterHotKey(Some(window), id);
                }
            }
        }
    });
}
