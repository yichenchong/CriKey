//! `ShellExecuteExW`, the single dispatch behind every launch.
//!
//! `ShellExecuteExW` rather than `ShellExecuteW` because only the extended form
//! reports a real error: the older one returns an `HINSTANCE`-shaped status
//! that has to be decoded from a table of legacy codes, while this one fails
//! through `GetLastError` and therefore through [`windows::core::Error`], which
//! is what lets a refusal carry Windows' own words and `HRESULT` (spec 18.2).

#![allow(unsafe_code)]

use std::ffi::OsStr;

use windows::core::PCWSTR;
use windows::Win32::UI::Shell::{
    ShellExecuteExW, SEE_MASK_FLAG_DDEWAIT, SEE_MASK_FLAG_NO_UI, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crikey_core::Result;

use crate::win32::{refused, wide, Apartment};

/// Asks the shell to execute `file`, with `parameters` as its command line.
///
/// `file` is whatever the caller wants opened: an executable path, a document,
/// or the `shell:AppsFolder\<AppUserModelID>` moniker of a packaged
/// application. The shell parses all three, which is the whole reason this is
/// one function and not three.
///
/// Returns as soon as the operation has been dispatched, not when the launched
/// program exits: no process handle is requested, so there is none to wait on
/// and none to leak.
pub(super) fn execute(verb: &str, file: &OsStr, parameters: Option<&OsStr>) -> Result<()> {
    // The shell delegates to COM-activated extensions -- verb handlers, data
    // sources -- and several of them require a single-threaded apartment, so
    // the documented preamble to `ShellExecuteEx` is this exact call.
    let _apartment = Apartment::enter("a shell launch")?;

    // Both buffers must outlive the call, so they are named rather than
    // temporaries: a `PCWSTR` into a dropped `Vec` is a dangling pointer.
    let file_units = wide(file);
    let parameter_units = parameters.map(wide);

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        // `SEE_MASK_FLAG_DDEWAIT` is `shellapi.h`'s own older spelling of
        // `SEE_MASK_NOASYNC`: return only once the operation has actually been
        // dispatched, so that a failure is reported here instead of being lost
        // in a shell thread. It does not wait for the launched program.
        //
        // `SEE_MASK_FLAG_NO_UI` keeps the shell from putting its own modal
        // error box on screen: this call has a typed refusal to return, and a
        // launcher that pops a dialog the caller cannot dismiss is worse than
        // one that reports the failure upwards.
        fMask: SEE_MASK_FLAG_DDEWAIT | SEE_MASK_FLAG_NO_UI,
        lpFile: PCWSTR(file_units.as_ptr()),
        lpParameters: parameter_units
            .as_ref()
            .map_or(PCWSTR::null(), |units| PCWSTR(units.as_ptr())),
        // A null verb is the item's default verb, which is `open` for a
        // document, `run` for an executable and activation for a packaged
        // application. Naming one would be this backend guessing at something
        // the shell already knows per target.
        //
        // A null directory is deliberate too: `DiscoveredApplication` does not
        // carry the working directory a shortcut may declare, so synthesising
        // one from the target path would not be restoring the shortcut's answer
        // but inventing a different one.
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    // SAFETY: `info` is a fully initialised structure of the size it declares,
    // and the two pointers it carries are NUL-terminated buffers that outlive
    // this statement.
    unsafe { ShellExecuteExW(&mut info) }
        .map_err(|error| refused(&format!("{verb} {}", file.to_string_lossy()), &error))
}
