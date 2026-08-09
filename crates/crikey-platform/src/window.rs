//! Window enumeration and activation (spec 18.1).
//!
//! Two of the required platform interfaces are about *other* applications'
//! windows: listing them, and raising one. A launcher needs both, because
//! "switch to the window you already have open" is a different action from
//! "start another copy of the program".
//!
//! # Why this is a separate trait
//!
//! Spec 18.6 makes window control *optional* on Linux: X11 gives it, a Wayland
//! session may refuse it outright, and neither answer is a bug. A backend
//! therefore hands out a [`WindowService`] only when it actually has one, and
//! reports [`Capability::WindowEnumeration`]/[`Capability::WindowActivation`]
//! per session (spec 18.2). Folding these methods into [`PlatformBackend`]
//! would force every backend to answer, and the only thing an unwilling backend
//! could answer with is a lie.
//!
//! [`Capability::WindowEnumeration`]: crate::Capability::WindowEnumeration
//! [`Capability::WindowActivation`]: crate::Capability::WindowActivation
//! [`PlatformBackend`]: crate::PlatformBackend

use crikey_core::Result;

/// An opaque, platform-defined identity for one window.
///
/// The inner value is whatever the window system calls a window -- an X11
/// `Window` id, a Win32 `HWND`, an Accessibility element's id -- widened to
/// `u64` so that one type spans every backend. Nothing outside the backend that
/// produced it may interpret it: a handle is only ever a token to hand back to
/// the same service, and it may name a window that has since been destroyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowHandle(pub u64);

/// One window as a catalog-facing description.
///
/// The title is what the window says it is called, which may be empty: a window
/// with no title is still switchable, so an unnamed window is listed rather than
/// dropped. `application` is the owning program where the window system reveals
/// it, and `None` where it does not -- guessing it from the title would put a
/// fabricated program name in front of the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub handle: WindowHandle,
    pub title: String,
    pub application: Option<String>,
}

/// Listing and raising windows that belong to other applications.
pub trait WindowService {
    /// Every window the desktop currently presents as switchable.
    ///
    /// Ordering is the window system's own; callers that need a stable order
    /// impose one. A window that disappears mid-scan is omitted rather than
    /// turned into an error: windows are destroyed asynchronously by the
    /// programs that own them, so a torn read is the normal case, and failing
    /// the whole enumeration would make the switcher intermittently empty.
    fn enumerate(&self) -> Result<Vec<WindowInfo>>;

    /// The window the desktop currently focuses, or `None` when there is none.
    ///
    /// Ranking uses this as an application-context signal, so the distinction
    /// between the two negative answers matters and both are `None` here: an
    /// empty desktop and a session whose focus this backend cannot observe are
    /// equally "no evidence". What must never happen is a *positive* answer
    /// that was inferred rather than read — a fabricated foreground window
    /// would silently promote the wrong category of result on every query, and
    /// nothing downstream could tell it from a real one.
    ///
    /// # Errors
    ///
    /// Only when the connection to the window system itself failed. A focus
    /// property that is absent, malformed, or names a window that has since
    /// been destroyed is `Ok(None)`: other programs mutate focus concurrently,
    /// so a torn read is the normal case and not an error.
    fn foreground_window(&self) -> Result<Option<WindowInfo>>;

    /// Raises and focuses one window.
    ///
    /// An error means the request could not be *delivered* -- the handle names
    /// no live window, or the connection broke. Delivery is all a client can
    /// observe: the window manager owns focus policy and may legitimately
    /// decline, so `Ok(())` promises the request was made, not that focus
    /// moved. Implementations must not report success without putting the
    /// request on the wire.
    fn activate(&self, handle: &WindowHandle) -> Result<()>;
}
