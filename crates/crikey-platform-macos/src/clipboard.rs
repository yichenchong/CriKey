//! The general pasteboard (spec 18.2).
//!
//! macOS is the one target of the three where the clipboard really is a store:
//! `NSPasteboard` lives in the pasteboard server, a written value outlives the
//! process that wrote it, and reading it is a lookup rather than a request sent
//! to whichever application happens to own a selection. Nothing here therefore
//! has to stay resident the way the Linux backend's X11 clipboard does.
//!
//! What can still be absent is the pasteboard itself. `generalPasteboard` is
//! documented never to return null and does return null anyway for a process
//! with no access to the pasteboard server -- a `launchd` daemon in some
//! configurations -- so this backend probes for one instead of assuming it, and
//! reports [`Capability::Clipboard`] from what the probe found.
//!
//! [`Capability::Clipboard`]: crikey_platform::Capability::Clipboard

use std::fmt;
use std::sync::{Mutex, PoisonError};

use crikey_core::{CoreError, Result};
use crikey_platform::Clipboard;

/// The macOS general pasteboard.
pub struct MacPasteboard {
    /// `arboard` takes `&mut self` for every operation while [`Clipboard`]
    /// takes `&self`, because a pasteboard is shared session state rather than
    /// something a caller owns exclusively. The lock reconciles the two, and is
    /// uncontended in the launcher: one holder, one thread.
    inner: Mutex<arboard::Clipboard>,
}

impl MacPasteboard {
    /// The general pasteboard, or `None` when this process has none.
    ///
    /// Only ever a handle to the pasteboard server: acquiring one neither reads
    /// the user's pasteboard nor writes to it, which is what makes it usable as
    /// the capability probe.
    pub fn for_session() -> Option<Self> {
        arboard::Clipboard::new().ok().map(|inner| Self {
            inner: Mutex::new(inner),
        })
    }

    /// Runs one operation against the `arboard` handle.
    ///
    /// A poisoned lock is recovered rather than propagated: the mutex guards a
    /// pasteboard handle, not an invariant a panic could have left half broken,
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

impl fmt::Debug for MacPasteboard {
    /// Hand written because `arboard::Clipboard` is not `Debug`, and there is
    /// nothing to print anyway: the value is a handle to session state.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("MacPasteboard").finish_non_exhaustive()
    }
}

impl Clipboard for MacPasteboard {
    /// The pasteboard's text, or `None` when it holds none.
    ///
    /// An empty pasteboard and one holding an image are the same answer here
    /// and neither is an error: there is no text to read.
    fn read_text(&self) -> Result<Option<String>> {
        match self.with(arboard::Clipboard::get_text) {
            Ok(text) => Ok(Some(text)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(error) => Err(CoreError::Invalid(format!("cannot read the pasteboard: {error}"))),
        }
    }

    /// Replaces the pasteboard's contents with `text`.
    ///
    /// Empty text is written as written: clearing the pasteboard is not this
    /// method's decision to make.
    fn write_text(&self, text: &str) -> Result<()> {
        self.with(|clipboard| clipboard.set_text(text))
            .map_err(|error| CoreError::Invalid(format!("cannot write to the pasteboard: {error}")))
    }
}
