//! The Win32 pieces more than one service in this crate needs.
//!
//! Discovery reads the shell's object model and launching drives the shell's
//! execute verb, but both cross the same three boundaries: a Rust string has to
//! become a NUL-terminated `PCWSTR`, a failing `HRESULT` has to become a
//! [`CoreError`] that still names what Windows said, and the calling thread has
//! to be in a COM apartment first. One copy of each, so a fix to any of them is
//! a fix everywhere.
//!
//! The module exists only on Windows: everything in it is either an FFI call or
//! the exact shape of one.

#![allow(unsafe_code)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows::core::Error;
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};

use crikey_core::{CoreError, Result};

/// A NUL-terminated UTF-16 copy of a native string, for the shell APIs that
/// take one.
///
/// Lossless: an [`OsStr`] on Windows is already UTF-16 held as WTF-8, so the
/// code units that come out are the ones the shell reported, unpaired
/// surrogates included (spec 18.3). Callers are responsible for having refused
/// an interior NUL beforehand -- this function cannot, because it has no way to
/// report one.
pub(crate) fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// A Win32 refusal, with the operating system's own words and code kept.
pub(crate) fn refused(action: &str, error: &Error) -> CoreError {
    CoreError::Invalid(format!(
        "Windows would not {action}: {} ({})",
        error.message(),
        error.code()
    ))
}

/// The COM apartment a shell call runs in.
///
/// Entering is conditional because the caller may already have. A thread that
/// was put in a multi-threaded apartment by someone else keeps it: the shell
/// interfaces used here still work from one, and leaving an apartment this code
/// did not enter would break whoever did.
///
/// `purpose` completes "COM would not start for ...", so it reads as a noun
/// phrase: `"application discovery"`, `"a shell launch"`.
pub(crate) struct Apartment {
    /// Whether this guard owes a `CoUninitialize`.
    owned: bool,
}

impl Apartment {
    pub(crate) fn enter(purpose: &str) -> Result<Self> {
        // SAFETY: an ordinary initialisation of the calling thread.
        let outcome = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };

        // `S_FALSE` means the thread was already in this apartment, and each
        // successful call -- including that one -- owes a `CoUninitialize`.
        if outcome == S_OK || outcome == S_FALSE {
            return Ok(Self { owned: true });
        }
        if outcome == RPC_E_CHANGED_MODE {
            return Ok(Self { owned: false });
        }

        Err(CoreError::Invalid(format!(
            "COM would not start for {purpose}: {} ({outcome})",
            outcome.message()
        )))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: balances the successful `CoInitializeEx` on this thread.
            unsafe { CoUninitialize() };
        }
    }
}
