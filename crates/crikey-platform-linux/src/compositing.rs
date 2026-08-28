//! Whether the session composites window transparency (spec 18.2).
//!
//! # Why a selection owner
//!
//! X11 has no request that asks "is a compositor running". What it has is the
//! convention the EWMH companion specification defines: a compositing manager
//! acquires ownership of the `_NET_WM_CM_S<screen>` selection on every screen
//! it composites, and releases it when it stops. That selection is not a hint
//! about the compositor, it *is* how compositing managers announce themselves
//! -- it is what a second one reads to discover that a first is already there
//! and refuse to start beside it -- so reading its owner is the standard check
//! rather than an approximation of one.
//!
//! A selection is also the one piece of X11 state that cannot go stale. The
//! server drops the ownership when the owning client's connection closes, so
//! unlike a root-window property -- which any client may write and which
//! outlives whoever wrote it, the reason [`window`] runs a three-part EWMH
//! handshake -- an owned `_NET_WM_CM_S0` proves a compositor is alive right
//! now.
//!
//! The screen number is part of the name, and taking it from the connection
//! rather than assuming `0` is what makes the answer right on a multi-screen
//! display: `DISPLAY=:0.1` composites or does not composite independently of
//! screen `0`, and a probe hard-coding the first screen would report the wrong
//! one's state.
//!
//! # Why every failure answers "no compositor"
//!
//! The caller is deciding whether to put an alpha channel into the corners of
//! the launcher window. A wrong "composited" leaves solid black notches cut out
//! of those corners on the user's screen; a wrong "not composited" leaves
//! square corners on a desktop that could have rounded them. Only the first is
//! a defect a user has to look at, so an unreachable display, a refused atom
//! and a broken connection all answer `false` here rather than propagating: the
//! caller has no better move than the safe shape, and there is no error it
//! could report that a squared-off window does not already say.
//!
//! [`window`]: crate::window

use x11rb::protocol::xproto::ConnectionExt;
use x11rb::rust_connection::RustConnection;

/// Whether a compositing manager owns `_NET_WM_CM_S<screen>` on `display`.
///
/// `display` is the `:N` string to connect to, or `None` for `$DISPLAY`.
///
/// Fresh on every call and cached nowhere, unlike the session probes behind
/// [`Capability::GlobalHotkeys`] and [`Capability::FileOpen`]: a portal and a
/// helper binary are installed or not for the whole run, while a user really
/// does start and stop a compositor mid-session, and a cached answer would
/// outlive the fact it reported. Two round trips against a socket is what a
/// capability query can afford, being asked once before a window is built.
///
/// Never panics and never blocks on a reply that is not coming: both requests
/// are ordinary round trips, and a server that has gone away breaks the
/// connection rather than leaving one outstanding.
///
/// [`Capability::GlobalHotkeys`]: crikey_platform::Capability::GlobalHotkeys
/// [`Capability::FileOpen`]: crikey_platform::Capability::FileOpen
pub fn compositor_is_running(display: Option<&str>) -> bool {
    selection_is_owned(display).unwrap_or(false)
}

/// The owner lookup, with every X11 failure collapsed into `None` by `?`.
///
/// Split out so that the failure policy is written once, at the one call site
/// above, instead of once per request: there is no failure here that a caller
/// could do anything with, and no failure that should read as "composited".
fn selection_is_owned(display: Option<&str>) -> Option<bool> {
    let (connection, screen) = RustConnection::connect(display).ok()?;
    let name = format!("_NET_WM_CM_S{screen}");

    // `only_if_exists = true`: this is a question, not a claim. A display where
    // the name has never been interned has certainly never had an owner for it,
    // and creating the atom to find that out would leave a probe's mark on a
    // display it only meant to read.
    let interned = connection.intern_atom(true, name.as_bytes()).ok()?.reply().ok()?;
    if interned.atom == x11rb::NONE {
        return Some(false);
    }

    // `GetSelectionOwner` answers `None` -- window 0 -- for a selection nobody
    // holds, which is exactly a screen with no compositing manager on it.
    let owner = connection.get_selection_owner(interned.atom).ok()?.reply().ok()?;
    Some(owner.owner != x11rb::NONE)
}
