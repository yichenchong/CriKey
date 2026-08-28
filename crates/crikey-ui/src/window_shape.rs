//! Cutting the launcher's rounded shape out of its window, where the surface
//! in front of it cannot be trusted to carry alpha.
//!
//! # Two ways to have a shape
//!
//! The launcher would rather *draw* its shape: the panel paints a rounded
//! rectangle, the corners outside it are left unpainted, and a compositing
//! desktop shows the wallpaper through them. That is smooth-edged and costs
//! nothing, and it is what happens wherever the surface states that it blends
//! -- a Wayland session, and macOS.
//!
//! Two platforms never say so, and a corner left unpainted on either of them
//! is presented as solid black rather than as nothing:
//!
//! * **Windows.** `wgpu`'s Direct3D 12 backend advertises exactly one
//!   composite mode for a swapchain attached to an `HWND`, and it is `Opaque`.
//!   The Desktop Window Manager composites the *window*, which is why
//!   `Capability::Compositing` answers `Available` there and says something
//!   true; the swapchain in front of it still has nowhere to put alpha.
//! * **X11.** Mesa's surfaces advertise `Opaque` and `Inherit`, and `Inherit`
//!   is defined as the alpha behaviour being unknown to Vulkan and settable
//!   only through native window-system calls. A surface offering it has
//!   promised nothing, so taking it for a blending mode is a guess, and the
//!   cost of the guess being wrong is four black notches.
//!
//! On both, the shape is cut instead of drawn: the window manager is told to
//! remove the corner pixels from the window, which needs no promise about
//! alpha because there is no alpha involved. A clip is quantised to whole
//! pixels, so its arc is stepped where a composited one is smooth. That is the
//! price of having the shape at all on these two, and it is not paid anywhere
//! else.

use crate::theme;

/// A run of solid pixels: `width` wide and `height` tall at (`x`, `y`).
///
/// The unit both window systems take. X11 wants a list of them and Windows
/// wants only the corner size, so the shared shape is described this way and
/// each platform reads what it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// The corner radius in physical pixels, for a window at `scale`.
///
/// Compiled and tested on every host rather than only where it is used: the
/// scale factor is the whole of the translation between the logical radius the
/// theme states and the physical pixels a window system clips in, and a
/// developer's own machine is the one that reports 1.0 and hides the mistake.
pub fn corner_radius(scale: f64) -> u32 {
    let radius = (f64::from(theme::RADIUS_WINDOW) * scale).round();
    // A scale that is negative, NaN or infinite is not a rounder window, it is
    // a broken one, and a radius of zero is the square every other refusal in
    // this feature already falls back to.
    if radius.is_finite() && radius >= 0.0 {
        radius.min(f64::from(u32::MAX)) as u32
    } else {
        0
    }
}

/// The solid runs of a `width` x `height` rectangle with `radius` corners.
///
/// One span per row across the two corner bands and a single span for
/// everything between them, so a tall window costs the same as a short one:
/// what the list grows with is the radius, not the height.
///
/// The radius is clamped to half of the shorter side, because a corner larger
/// than that is not a rounded rectangle -- the two arcs on an edge would cross
/// -- and a window system handed the crossing spans would clip something the
/// caller did not describe.
pub fn rounded_spans(width: u32, height: u32, radius: u32) -> Vec<Span> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let radius = radius.min(width / 2).min(height / 2);
    if radius == 0 {
        return vec![Span {
            x: 0,
            y: 0,
            width,
            height,
        }];
    }

    let mut spans = Vec::with_capacity(2 * radius as usize + 1);
    let centre = f64::from(radius);
    for row in 0..radius {
        // The arc is measured to the middle of the row rather than its edge,
        // which is what keeps the first and last rows from being a pixel wide
        // or a pixel short: a row is a band, and the circle crosses it
        // somewhere inside.
        let dy = centre - (f64::from(row) + 0.5);
        let dx = (centre * centre - dy * dy).max(0.0).sqrt();
        let inset = (centre - dx).round().max(0.0) as u32;
        // Both corners of the row are cut, so a row whose insets meet has no
        // solid pixels at all and must not become a zero-width span.
        if inset * 2 >= width {
            continue;
        }
        let run = width - inset * 2;
        spans.push(Span {
            x: inset as i32,
            y: row as i32,
            width: run,
            height: 1,
        });
        spans.push(Span {
            x: inset as i32,
            y: (height - 1 - row) as i32,
            width: run,
            height: 1,
        });
    }
    // Everything between the two bands is the full width, and it is one span
    // however tall the window is.
    if height > radius * 2 {
        spans.push(Span {
            x: 0,
            y: radius as i32,
            width,
            height: height - radius * 2,
        });
    }
    spans
}

#[cfg(target_os = "windows")]
pub(crate) use win32::clip;

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) use x11::clip;

/// Nothing to cut: this platform draws its shape, or has no window system this
/// module knows how to clip. Answers `false`, which is the truth -- no clip
/// was applied -- and lets the caller report a square window honestly.
#[cfg(not(any(target_os = "windows", all(unix, not(target_os = "macos")))))]
pub(crate) fn clip(_window: &winit::window::Window, _width: u32, _height: u32, _scale: f64) -> bool {
    false
}

#[cfg(target_os = "windows")]
mod win32 {
    // Two `unsafe` calls: creating the region and handing it to the window.
    // The workspace warns on unsafe code, and there is no safe route to a
    // window manager call.
    #![allow(unsafe_code)]

    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{CreateRoundRectRgn, SetWindowRgn};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::Window;

    use super::corner_radius;

    /// Clips `window` to a rounded rectangle `width` x `height` physical
    /// pixels across.
    ///
    /// Answers whether the window is now clipped, so a caller can say why the
    /// corners are square when nothing worked. No failure here is worth
    /// recovering from -- the shape is decoration and the launcher is entirely
    /// usable without it -- but a silent one leaves a user with nothing to
    /// report.
    pub(crate) fn clip(window: &Window, width: u32, height: u32, scale: f64) -> bool {
        if width == 0 || height == 0 {
            return false;
        }
        let Ok(handle) = window.window_handle() else {
            return false;
        };
        let RawWindowHandle::Win32(win32) = handle.as_ref() else {
            return false;
        };
        let hwnd = HWND(win32.hwnd.get() as *mut _);
        // `CreateRoundRectRgn` takes the width and height of the ellipse it
        // corners the rectangle with, which is twice the radius, and an
        // exclusive bottom-right corner -- so the rectangle covering the whole
        // window is one past its last pixel on each axis. One short and the
        // window loses its right column and bottom row, which reads as a
        // hairline gap against the desktop.
        let diameter = corner_radius(scale).min(i32::MAX as u32 / 2) as i32 * 2;
        let right = width.min(i32::MAX as u32) as i32 + 1;
        let bottom = height.min(i32::MAX as u32) as i32 + 1;
        // SAFETY: a GDI object constructor taking six integers by value. It
        // allocates or it answers a null handle.
        let region = unsafe { CreateRoundRectRgn(0, 0, right, bottom, diameter, diameter) };
        if region.is_invalid() {
            return false;
        }
        // SAFETY: `hwnd` is this process's live window and `region` is the
        // handle just created, which nothing else refers to.
        //
        // The window takes ownership of a region it accepts, so it must not be
        // deleted here. A rejected one is left rather than freed through a
        // second fallible call on a path that has already failed:
        // `SetWindowRgn` failing at all means the window is on its way out.
        // `None` would clear the region and unshape the window, so the handle
        // is always passed as `Some`.
        unsafe { SetWindowRgn(hwnd, Some(region), true) != 0 }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod x11 {
    //! The X11 half, which is also the Wayland half by doing nothing.
    //!
    //! A Wayland surface states that it blends, so the launcher draws its
    //! shape there and this is never reached with a Wayland handle. The check
    //! is on the handle rather than on an environment variable because one
    //! binary serves both and the window itself is the only thing that knows
    //! which it got.
    //!
    //! The connection is this module's own rather than the one `winit` holds:
    //! `winit` does not lend it out, X11 is a protocol rather than a library
    //! handle, and a second client may shape a window it knows the id of. It
    //! is opened once and kept, because the launcher resizes as results
    //! arrive and a connection per keystroke would be absurd.

    use std::sync::LazyLock;

    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::Window;
    use x11rb::connection::{Connection, RequestConnection as _};
    use x11rb::protocol::shape::{self, ConnectionExt as _, SK, SO};
    use x11rb::protocol::xproto::Rectangle;
    use x11rb::rust_connection::RustConnection;

    use super::{corner_radius, rounded_spans};

    /// The shared connection, or `None` if the display refused one or cannot
    /// clip.
    ///
    /// `None` is cached as deliberately as a connection is: a display that
    /// would not answer once will not answer on the next keystroke either, and
    /// retrying per resize would spend a socket connect on every result that
    /// arrives.
    static CONNECTION: LazyLock<Option<RustConnection>> = LazyLock::new(|| {
        let (connection, _) = RustConnection::connect(None).ok()?;
        // A server without the SHAPE extension cannot clip anything, and
        // asking it to would be a protocol error rather than a square window.
        let present = connection
            .extension_information(shape::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_some();
        present.then_some(connection)
    });

    /// Clips `window` to a rounded rectangle `width` x `height` physical
    /// pixels across.
    ///
    /// Answers whether the window is now clipped, for the same reason the
    /// Windows half does: a square window with no explanation is what sent a
    /// user here in the first place.
    pub(crate) fn clip(window: &Window, width: u32, height: u32, scale: f64) -> bool {
        if width == 0 || height == 0 {
            return false;
        }
        let Ok(handle) = window.window_handle() else {
            return false;
        };
        let id = match handle.as_ref() {
            RawWindowHandle::Xlib(xlib) => xlib.window as u32,
            RawWindowHandle::Xcb(xcb) => xcb.window.get(),
            // Wayland draws its shape; anything else is not an X server.
            _ => return false,
        };
        let Some(connection) = CONNECTION.as_ref() else {
            return false;
        };
        let rectangles: Vec<Rectangle> = rounded_spans(width, height, corner_radius(scale))
            .into_iter()
            .filter_map(|span| {
                Some(Rectangle {
                    x: i16::try_from(span.x).ok()?,
                    y: i16::try_from(span.y).ok()?,
                    width: u16::try_from(span.width).ok()?,
                    height: u16::try_from(span.height).ok()?,
                })
            })
            .collect();
        if rectangles.is_empty() {
            return false;
        }
        // `Bounding` is the shape of the window itself -- what the server
        // clips it to and what a compositor reads. `Set` replaces whatever was
        // there, which is what makes this safe to call again on every resize.
        if connection
            .shape_rectangles(SO::SET, SK::BOUNDING, 0u8.into(), id, 0, 0, &rectangles)
            .is_err()
        {
            return false;
        }
        connection.flush().is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clip is in physical pixels and the radius is in logical ones, so
    /// the scale factor is the whole of the translation between them.
    #[test]
    fn the_corner_follows_the_window_scale() {
        assert_eq!(corner_radius(1.0), theme::RADIUS_WINDOW as u32);
        assert_eq!(corner_radius(2.0), theme::RADIUS_WINDOW as u32 * 2);
        // 150 % is the common laptop scale and the one that is not a whole
        // multiple: 20 logical pixels of radius is 30 physical.
        assert_eq!(corner_radius(1.5), 30);
    }

    /// A scale factor the window system should never report still has to
    /// produce a usable number rather than a panic or a negative dimension.
    #[test]
    fn an_impossible_scale_squares_the_window_instead_of_failing() {
        assert_eq!(corner_radius(0.0), 0);
        assert_eq!(corner_radius(-1.0), 0);
        assert_eq!(corner_radius(f64::NAN), 0);
        assert_eq!(corner_radius(f64::INFINITY), 0);
    }

    /// Whether a pixel is inside the described shape.
    fn covered(spans: &[Span], x: i32, y: i32) -> bool {
        spans.iter().any(|span| {
            x >= span.x && y >= span.y && x < span.x + span.width as i32 && y < span.y + span.height as i32
        })
    }

    /// The corners are the whole point: the pixel at each of the four extreme
    /// corners must be outside the shape, and the middle of each edge must be
    /// inside it. A shape that cut the edges instead would be a lozenge, and
    /// one that cut nothing would be the square this replaces.
    #[test]
    fn the_corners_are_cut_and_the_edges_are_not() {
        let (width, height) = (200u32, 100u32);
        let spans = rounded_spans(width, height, 20);
        let (last_x, last_y) = (width as i32 - 1, height as i32 - 1);

        for corner in [(0, 0), (last_x, 0), (0, last_y), (last_x, last_y)] {
            assert!(
                !covered(&spans, corner.0, corner.1),
                "the pixel at {corner:?} is a corner and must be cut away"
            );
        }
        for edge in [
            (width as i32 / 2, 0),
            (width as i32 / 2, last_y),
            (0, height as i32 / 2),
            (last_x, height as i32 / 2),
        ] {
            assert!(
                covered(&spans, edge.0, edge.1),
                "the pixel at {edge:?} is the middle of an edge and must survive"
            );
        }
    }

    /// The area removed is the area of a square minus its inscribed circle,
    /// once per corner. Pinning the total rather than individual rows is what
    /// makes this a test of the arc and not of the arithmetic that drew it: an
    /// off-by-one in the inset changes the area, a different rounding rule
    /// does not.
    #[test]
    fn the_shape_removes_one_circle_of_pixels() {
        let (width, height, radius) = (300u32, 200u32, 24u32);
        let spans = rounded_spans(width, height, radius);
        let covered: u32 = spans.iter().map(|span| span.width * span.height).sum();

        let area = f64::from(width * height);
        let circle = std::f64::consts::PI * f64::from(radius * radius);
        let expected = area - (4.0 * f64::from(radius * radius) - circle);
        // One pixel of slack per row of the two corner bands, which is what
        // rounding each inset to a whole pixel can cost.
        let slack = f64::from(radius * 2);
        assert!(
            (f64::from(covered) - expected).abs() <= slack,
            "covered {covered} pixels, expected about {expected}"
        );
    }

    /// Rows are emitted in pairs about the horizontal centre line, so a
    /// window's top and bottom cannot come out differently rounded.
    #[test]
    fn the_top_and_bottom_are_cut_alike() {
        let (width, height) = (120u32, 80u32);
        let spans = rounded_spans(width, height, 16);
        for y in 0..height as i32 {
            let mirrored = height as i32 - 1 - y;
            for x in 0..width as i32 {
                assert_eq!(
                    covered(&spans, x, y),
                    covered(&spans, x, mirrored),
                    "row {y} and row {mirrored} disagree at {x}"
                );
            }
        }
    }

    /// A radius larger than the window is a caller error that must still
    /// describe a shape: clamped to half the shorter side, which is a circle
    /// or a stadium rather than crossing arcs.
    #[test]
    fn an_oversized_radius_is_clamped_rather_than_crossed() {
        let spans = rounded_spans(40, 40, 400);
        assert!(!spans.is_empty(), "an oversized radius must still be a shape");
        assert!(covered(&spans, 20, 20), "the centre survives any radius");
        assert!(!covered(&spans, 0, 0), "the corner is cut at any radius");
        let covered_pixels: u32 = spans.iter().map(|span| span.width * span.height).sum();
        assert!(
            covered_pixels < 40 * 40,
            "a clamped radius still removes the corners"
        );
    }

    /// No radius is the square window, described as one span rather than as
    /// one per row.
    #[test]
    fn no_radius_is_one_span() {
        assert_eq!(
            rounded_spans(64, 32, 0),
            vec![Span {
                x: 0,
                y: 0,
                width: 64,
                height: 32
            }]
        );
    }
}
