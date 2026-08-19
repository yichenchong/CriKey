//! The session clipboard (spec 18.2).
//!
//! # Why X11 and not a store
//!
//! X11 has no clipboard. It has *selections*, and a selection has an owner: a
//! client calls `SetSelectionOwner` and then has to answer every
//! `SelectionRequest` the server routes to it, for as long as the value is
//! supposed to be readable. Nothing anywhere holds a copy. The consequence
//! that decides this module's shape is that a value "copied" by a process which
//! then exits is gone -- unless a clipboard manager happened to take it over --
//! so a clipboard cannot be opened per copy and dropped afterwards.
//!
//! `arboard` is what does the answering: its X11 backend keeps one
//! process-global connection and one helper thread (`serve_requests`) behind a
//! `static`, hands every `Clipboard` instance an `Arc` of it, and tears the
//! window and thread down when the last instance drops -- at which point it
//! asks the session's clipboard manager, if there is one, to save the value.
//! So this type is only as long-lived as its holder: the launcher keeps one for
//! the lifetime of the process, which is exactly the case that behaves
//! correctly. See `LinuxBackend::clipboard`.
//!
//! Under Wayland the same code path runs through XWayland, because that is the
//! clipboard bridge every compositor this backend targets already operates for
//! X11 clients. `arboard`'s native Wayland backend is behind its
//! `wayland-data-control` feature, which speaks the wlroots-only
//! `wlr_data_control` protocol -- so enabling it would add a dependency tree
//! that helps neither GNOME nor KDE, while XWayland covers all three. What it
//! costs is honesty in reporting: a Wayland session with no XWayland has no
//! clipboard here, which is why the capability is `Partial` there rather than
//! `Available`.

use std::fmt;
use std::sync::{Mutex, PoisonError};

use crikey_core::{CoreError, Result};
use crikey_platform::Clipboard;

/// The clipboard of an X11 or XWayland session, owned while this value lives.
///
/// Not `Clone` and not handed out per call on purpose: see the module
/// documentation for what dropping the last instance does to the selection.
pub struct X11Clipboard {
    /// `arboard` takes `&mut self` for every operation -- reading a selection
    /// means writing a property on its own window and pumping its own events --
    /// while [`Clipboard`] takes `&self`, because a clipboard is shared session
    /// state and not something a caller owns exclusively. The lock reconciles
    /// the two, and is uncontended in the launcher: one holder, one thread.
    inner: Mutex<arboard::Clipboard>,
}

impl X11Clipboard {
    /// The clipboard of the running session, or `None` when no X server answers.
    ///
    /// `None` rather than an error for the same reason [`crate::XdgOpener`]
    /// returns one: the caller's question is whether this session has a
    /// clipboard at all, and a service that can only fail is worse than an
    /// absent one -- the launcher can explain an absent one to the user.
    ///
    /// Connecting is the only side effect: a hidden 1x1 window and a helper
    /// thread. It does not read the selection and it does not take ownership of
    /// it, so probing costs the user's real clipboard nothing.
    pub fn for_session() -> Option<Self> {
        arboard::Clipboard::new().ok().map(|inner| Self {
            inner: Mutex::new(inner),
        })
    }

    /// Runs one operation against the shared `arboard` handle.
    ///
    /// A poisoned lock is recovered rather than propagated: the mutex guards a
    /// connection handle, not an invariant a panic could have broken halfway,
    /// and refusing every later copy because one unrelated caller panicked
    /// would turn a lost paste into a permanently dead feature.
    fn with<T>(
        &self,
        operation: impl FnOnce(&mut arboard::Clipboard) -> std::result::Result<T, arboard::Error>,
    ) -> std::result::Result<T, arboard::Error> {
        let mut clipboard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        operation(&mut clipboard)
    }
}

impl fmt::Debug for X11Clipboard {
    /// Hand written because `arboard::Clipboard` is not `Debug`, and there is
    /// nothing to print anyway: the value is a handle to session state.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("X11Clipboard").finish_non_exhaustive()
    }
}

impl Clipboard for X11Clipboard {
    /// The clipboard's text, or `None` when it holds none.
    ///
    /// An empty clipboard and one holding an image are the same answer here and
    /// neither is an error: there is no text to read. Only a failure to *ask* --
    /// a dead connection, an owner that never answers -- is reported as one.
    fn read_text(&self) -> Result<Option<String>> {
        match self.with(arboard::Clipboard::get_text) {
            Ok(text) => Ok(Some(text)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(error) => Err(CoreError::Invalid(format!("cannot read the clipboard: {error}"))),
        }
    }

    /// Takes ownership of the CLIPBOARD selection and offers `text` from it.
    ///
    /// Returns once the server has been told who the owner is, not once
    /// somebody has pasted: the value is served from this process afterwards,
    /// which is why the holder must stay alive. Empty text is written as
    /// written -- clearing the clipboard is not this method's decision to make.
    fn write_text(&self, text: &str) -> Result<()> {
        self.with(|clipboard| clipboard.set_text(text))
            .map_err(|error| CoreError::Invalid(format!("cannot write to the clipboard: {error}")))
    }
}
