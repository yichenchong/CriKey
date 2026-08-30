//! Rounding the launcher window where the surface in front of it cannot.
//!
//! # The shape is normally drawn
//!
//! The launcher draws its own silhouette: the panel paints a rounded
//! rectangle, the corners outside it are left unpainted, and a compositing
//! desktop shows the wallpaper through them. The compositor blends the arc
//! into whatever is behind it, so the edge is smooth. That is what happens on
//! Wayland, on X11 with a compositing manager running, and on macOS, and it
//! needs nothing from this module.
//!
//! # Windows cannot draw it
//!
//! `wgpu`'s Direct3D 12 backend advertises exactly one composite mode for a
//! swapchain attached to an `HWND`, and it is `Opaque`. The Desktop Window
//! Manager composites the *window* -- which is why `Capability::Compositing`
//! answers `Available` there and says something true -- but the swapchain in
//! front of it has nowhere to put alpha, so the corners cannot be left
//! unpainted.
//!
//! Windows 11 answers this directly: `DWMWA_WINDOW_CORNER_PREFERENCE` asks the
//! compositor to round the window, and being the thing that composites the
//! desktop underneath, it blends the arc properly. The radius is the system's
//! rather than [`crate::theme::RADIUS_WINDOW`], so the window and the query
//! field inside it are not concentric there; that is the price of a smooth
//! edge on a platform that will not blend a drawn one.
//!
//! # Why there is no fallback
//!
//! Both remaining cases -- Windows 10, and X11 with no compositing manager --
//! could be clipped instead: `SetWindowRgn` and the X11 `SHAPE` extension both
//! cut a rounded shape out of a window with no alpha involved. Both shipped
//! briefly, in 0.1.14, and both were removed.
//!
//! A clip is whole pixels, in or out. The stair it leaves is the boundary
//! between the window and the desktop, and nothing the launcher draws inside
//! its own window can antialias against pixels it does not own -- no amount of
//! care in the arc makes a binary mask smooth. Other desktop applications have
//! no third mechanism either: GTK and Qt draw into an ARGB surface exactly as
//! this does, and get square corners on an uncomposited X11 session for the
//! same reason.
//!
//! So a launcher that cannot round its corners smoothly leaves them square. A
//! stepped arc reads as a rendering fault, while a square window reads as a
//! window, and the first was reported as unprofessional within a day of
//! shipping.

/// Asks the platform's compositor to round `window` at the system radius, or
/// to stop.
///
/// Answers whether the window is now rounded by it, which the caller reports:
/// a square launcher looks the same however it got that way, and a user who
/// asked for rounded corners needs to know which half is missing. Answering
/// `false` for `rounded: false` is that same truth -- nothing is rounding the
/// window -- and the caller does not report a shape nobody asked for.
#[cfg(target_os = "windows")]
pub(crate) use win32::round;

/// Nothing to ask: this platform draws its own shape, or cannot round at all.
#[cfg(not(target_os = "windows"))]
pub(crate) fn round(_window: &winit::window::Window, _rounded: bool) -> bool {
    false
}

#[cfg(target_os = "windows")]
mod win32 {
    // One `unsafe` call, and no safe route to a window manager attribute. The
    // workspace warns on unsafe code.
    #![allow(unsafe_code)]

    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND,
        DWM_WINDOW_CORNER_PREFERENCE,
    };
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::Window;

    /// Asks the compositor to round `window` itself, or to leave it square.
    ///
    /// Windows 11 only. The attribute did not exist before it and the call
    /// answers an error on anything older, which is the version check: asking
    /// and reading the answer beats reading a build number, because the
    /// question is whether this DWM honours it rather than which release it
    /// shipped in. Windows 10 keeps square corners, which is the deliberate
    /// alternative to the stepped ones a region would give it.
    ///
    /// `rounded: false` says so explicitly rather than skipping the call: the
    /// preference is a property of the window and outlives the frame that set
    /// it, so a user turning the setting off has to be able to take it back.
    ///
    /// Idempotent, and independent of the window's size, so a caller may
    /// repeat it after a resize without checking.
    pub(crate) fn round(window: &Window, rounded: bool) -> bool {
        let Ok(handle) = window.window_handle() else {
            return false;
        };
        let RawWindowHandle::Win32(win32) = handle.as_ref() else {
            return false;
        };
        let hwnd = HWND(win32.hwnd.get() as *mut _);
        let preference = if rounded { DWMWCP_ROUND } else { DWMWCP_DONOTROUND };
        // SAFETY: the attribute id and the size describe the value pointed at,
        // which is a live local of exactly that type, read and not retained.
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                std::ptr::from_ref(&preference).cast(),
                size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
            )
        }
        // The window is rounded only if the compositor took a request to round
        // it. A successful request to stop is still a square window, and
        // saying otherwise would have the caller report a shape that is not
        // there.
        .is_ok()
            && rounded
    }
}
