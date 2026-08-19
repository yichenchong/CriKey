//! The Win32 clipboard (spec 18.2).
//!
//! Windows keeps the value, not the writer: `SetClipboardData` copies the text
//! into memory the system owns, so a copy survives the launcher exiting and
//! nothing here has to stay resident the way the Linux backend's X11 selection
//! owner does.
//!
//! What it does need is exclusive access for the duration of a transfer.
//! `OpenClipboard` fails while another process holds the clipboard open, which
//! is an ordinary, transient condition -- some other application is mid-copy --
//! rather than a missing capability, so `arboard` retries a bounded number of
//! times before reporting it. A retried-out copy surfaces as a diagnostic on
//! the failing action and nothing else: the clipboard is still there.
//!
//! Whether there is a clipboard at all is a property of the window station the
//! process runs in. An interactive session always has one; a service in an
//! isolated station has its own, which no user can paste from. That is not
//! detectable from inside a process without judging what its station is *for*,
//! so this backend claims the capability for the interactive launcher it is and
//! does not invent a probe whose answer it could not trust.

use std::fmt;
use std::sync::{Mutex, PoisonError};

use crikey_core::{CoreError, Result};
use crikey_platform::Clipboard;

/// The Windows clipboard.
pub struct WindowsClipboard {
    /// `arboard` takes `&mut self` for every operation -- each one opens the
    /// clipboard, transfers and closes it -- while [`Clipboard`] takes `&self`,
    /// because a clipboard is shared session state rather than something a
    /// caller owns exclusively. The lock reconciles the two, and is uncontended
    /// in the launcher: one holder, one thread.
    inner: Mutex<arboard::Clipboard>,
}

impl WindowsClipboard {
    /// The clipboard of the running session.
    ///
    /// `Option` rather than a plain value because the three backends' accessors
    /// have that in common, and because it is a real answer off Windows and in a
    /// station with no clipboard: constructing this holds no handle, so nothing
    /// is opened, read or written until a caller asks for a transfer.
    pub fn for_session() -> Option<Self> {
        arboard::Clipboard::new().ok().map(|inner| Self {
            inner: Mutex::new(inner),
        })
    }

    /// Runs one operation against the `arboard` handle.
    ///
    /// A poisoned lock is recovered rather than propagated: the mutex serialises
    /// transfers, it does not guard an invariant a panic could have left half
    /// broken, and refusing every later copy because one unrelated caller
    /// panicked would turn a lost paste into a permanently dead feature.
    fn with<T>(
        &self,
        operation: impl FnOnce(&mut arboard::Clipboard) -> std::result::Result<T, arboard::Error>,
    ) -> std::result::Result<T, arboard::Error> {
        let mut clipboard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        operation(&mut clipboard)
    }
}

impl fmt::Debug for WindowsClipboard {
    /// Hand written because `arboard::Clipboard` is not `Debug`, and there is
    /// nothing to print anyway: the value is a handle to session state.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WindowsClipboard").finish_non_exhaustive()
    }
}

impl Clipboard for WindowsClipboard {
    /// The clipboard's text, or `None` when it holds none.
    ///
    /// An empty clipboard and one holding a bitmap are the same answer here and
    /// neither is an error: there is no text to read. A clipboard held open by
    /// another process is reported as the failure it is, because retrying it is
    /// `arboard`'s job and it has already been done by the time this returns.
    fn read_text(&self) -> Result<Option<String>> {
        match self.with(arboard::Clipboard::get_text) {
            Ok(text) => Ok(Some(text)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(error) => Err(CoreError::Invalid(format!("cannot read the clipboard: {error}"))),
        }
    }

    /// Replaces the clipboard's contents with `text`.
    ///
    /// Empty text is written as written: clearing the clipboard is not this
    /// method's decision to make.
    fn write_text(&self, text: &str) -> Result<()> {
        self.with(|clipboard| clipboard.set_text(text))
            .map_err(|error| CoreError::Invalid(format!("cannot write to the clipboard: {error}")))
    }
}
