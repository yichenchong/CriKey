//! Answering the operating system when it asks the launcher to shut down.
//!
//! # Why this module exists
//!
//! A launcher must not quit when it is dismissed. Escape, a click away, and the
//! window manager's own close button all mean "get out of my way", so
//! [`WindowEvent::CloseRequested`](winit::event::WindowEvent::CloseRequested) is
//! delivered to the host as `UiCommand::Dismiss` and the window merely hides.
//! That is deliberate and it is also why the MSI could not update itself: the
//! Restart Manager, and WiX's `util:CloseApplication` after it, ask a resident
//! GUI process to leave by sending its top-level windows `WM_QUERYENDSESSION`
//! and then `WM_ENDSESSION`, wait for the process to exit, and report failure
//! when it does not. `crikey-launcher.exe` is the file the installer has to
//! replace, so "unable to close the applications" was the installer correctly
//! describing a process that had been asked to leave and had not.
//!
//! Those two messages are the only signal that distinguishes a genuine
//! operating-system shutdown request from a dismissal, and winit 0.30.5 does
//! not surface them: its Windows window procedure has arms for `WM_CLOSE` and
//! `WM_DESTROY` and nothing for either session message, so both fall through to
//! `DefWindowProcW`, which consents on the process's behalf and then does
//! nothing about it. `EventLoopBuilderExtWindows::with_msg_hook` is no help
//! either: winit calls that hook on messages it has just pulled off the thread
//! queue with `GetMessageW`/`PeekMessageW`, and both session messages are
//! *sent* rather than posted -- `SendMessageTimeoutW` from the installer's
//! custom action, and the session manager during a real logoff -- so they are
//! dispatched straight into the window procedure and never appear in the queue.
//!
//! A `comctl32` window subclass is therefore the only seam left. It is also the
//! narrow one: two message arms, and everything else is handed to the procedure
//! that was there before.
//!
//! # Where a shutdown request goes
//!
//! Into the exit path that already exists. `notify` is called with the window
//! subclass on the stack, so it must not block; it hands the request to the
//! event loop, which unwinds the same way the settings surface's "Quit CriKey"
//! control unwinds it -- the loop exits, `NativeLauncher::run` returns, and the
//! host persists its selection history and reaps its plugin children on the way
//! out. Nothing here duplicates that shutdown, and nothing here calls
//! `ExitProcess`.
//!
//! # The launcher is usually hidden
//!
//! An idle launcher has `set_visible(false)` on its window, and that window is
//! what receives these messages. It still receives them: a hidden window is
//! still a top-level window, `EnumWindows` enumerates it, and the event loop is
//! blocked in `GetMessageW`, which dispatches sent messages while it waits. A
//! hidden window has no *active session*, though, which is why the request does
//! not travel as a `UiCommand`: `NativeApplication::dispatch_command` drops
//! commands that arrive with no session, so an idle launcher would have thrown
//! the shutdown away.

// The classifier below is a pure function and its tests run on every host, but
// its only *caller* is the Windows subclass. Compiled unconditionally it is
// dead code off Windows, which this workspace treats as an error, so it is
// compiled where it is used and where it is tested.
#![cfg(any(target_os = "windows", test))]

/// `WM_QUERYENDSESSION`: may the session end?
///
/// Declared here rather than imported so that [`classify`] and its tests
/// compile and run on every host. The `windows` crate's own definitions are
/// asserted equal to these at compile time in [`win32`], so the two cannot
/// drift.
const WM_QUERYENDSESSION: u32 = 0x0011;

/// `WM_ENDSESSION`: the session is (`wparam` non-zero) or is not (`wparam`
/// zero) ending after all.
const WM_ENDSESSION: u32 = 0x0016;

/// What a window message means for the launcher's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionEnd {
    /// The caller asked permission to end the session. Answer yes, and do not
    /// leave yet: the sender's next message is the one that means go.
    ///
    /// Answering yes is what the absent winit arm was already doing through
    /// `DefWindowProcW`. Refusing would make the installer report a program
    /// that declined to close, which is worse than the defect being fixed.
    Consent,
    /// The session is ending. Shut down, orderly, now.
    Exit,
    /// The session end was cancelled after this window consented to it. The
    /// message is ours to swallow and there is nothing to do.
    Cancelled,
    /// Not a session message. It belongs to whoever handled it before.
    Unhandled,
}

/// Classifies one window message.
///
/// Separated from the subclass procedure because this mapping is the part that
/// can be silently wrong -- exiting on the query rather than on the decision
/// would close the launcher whenever *anything* merely asked whether the
/// session could end, and exiting on a cancelled session end would close it
/// when the answer turned out to be no -- and a window procedure cannot be
/// called from a test on any host.
pub(crate) fn classify(message: u32, wparam: usize) -> SessionEnd {
    match message {
        WM_QUERYENDSESSION => SessionEnd::Consent,
        // `wparam` is the session-end decision, not a reason code: `TRUE` means
        // the session is ending, `FALSE` means an earlier query was answered
        // and then withdrawn.
        WM_ENDSESSION if wparam != 0 => SessionEnd::Exit,
        WM_ENDSESSION => SessionEnd::Cancelled,
        _ => SessionEnd::Unhandled,
    }
}

#[cfg(target_os = "windows")]
pub(crate) use win32::watch;

#[cfg(target_os = "windows")]
mod win32 {
    // Two `unsafe` calls and one `extern "system"` callback: installing the
    // subclass and chaining to the procedure underneath it. The workspace warns
    // on unsafe code, and there is no safe route to a window procedure.
    #![allow(unsafe_code)]

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::WM_NCDESTROY;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::Window;

    use super::{classify, SessionEnd};

    /// The constants [`super`] declares are the constants Windows means.
    const _: () = {
        assert!(super::WM_QUERYENDSESSION == windows::Win32::UI::WindowsAndMessaging::WM_QUERYENDSESSION);
        assert!(super::WM_ENDSESSION == windows::Win32::UI::WindowsAndMessaging::WM_ENDSESSION);
    };

    /// Distinguishes this subclass from any other on the same window. One
    /// window, one subclass, so the value only has to be stable.
    const SUBCLASS_ID: usize = 1;

    /// The boxed callback, behind the `usize` of reference data a subclass is
    /// allowed to carry. Freed when the window is destroyed.
    type Notify = Box<dyn Fn()>;

    /// Starts answering session-end messages on `window`.
    ///
    /// `notify` runs on the event-loop thread with the sender blocked in
    /// `SendMessageTimeoutW`, so it must return promptly and must not wait for
    /// the shutdown it requests.
    ///
    /// Failing to install is not a launcher failure: it costs the polite exit
    /// this module adds and leaves the behaviour that shipped, so it is reported
    /// by the return value and not by refusing to start.
    pub(crate) fn watch(window: &Window, notify: Notify) -> bool {
        let Ok(handle) = window.window_handle() else {
            return false;
        };
        let RawWindowHandle::Win32(win32) = handle.as_ref() else {
            return false;
        };
        let hwnd = HWND(win32.hwnd.get() as *mut _);
        let reference = Box::into_raw(Box::new(notify));
        // SAFETY: `hwnd` is this process's live window, `subclass_proc` has the
        // signature `SUBCLASSPROC` requires, and `reference` is a live
        // allocation that only `subclass_proc` dereferences and only until it
        // frees it on `WM_NCDESTROY`.
        let installed =
            unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, reference as usize) }
                .as_bool();
        if !installed {
            // SAFETY: nothing took ownership of the box, and the procedure that
            // would have freed it was never installed.
            drop(unsafe { Box::from_raw(reference) });
        }
        installed
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        // The subclass id `watch` registered. There is exactly one subclass on
        // this window, so it identifies nothing this procedure has to branch on.
        _id: usize,
        reference: usize,
    ) -> LRESULT {
        // The one message rewrite the launcher performs, applied before this
        // procedure and everything under it sees the id: `WM_SYSCHAR` reaching
        // `DefWindowProcW` is the Windows alert sound on every keystroke. See
        // `crate::system_char`.
        let message = crate::system_char::beep_free_message(message);

        match classify(message, wparam.0) {
            // TRUE: this window agrees the session may end.
            SessionEnd::Consent => return LRESULT(1),
            SessionEnd::Exit => {
                // SAFETY: `reference` is the box `watch` leaked for this
                // subclass, and `WM_NCDESTROY` -- the only thing that frees it
                // -- cannot have run yet, because it removes the subclass this
                // call arrived through.
                let notify = unsafe { &*(reference as *const Notify) };
                notify();
                return LRESULT(0);
            }
            SessionEnd::Cancelled => return LRESULT(0),
            SessionEnd::Unhandled => {}
        }

        // SAFETY: forwarding the message this procedure was handed to the
        // procedure that was installed before it, which is what a subclass is
        // required to do with everything it does not handle.
        let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };

        if message == WM_NCDESTROY {
            // Last message a window receives. Detach before freeing, so that
            // nothing can arrive at a procedure whose reference data is gone.
            //
            // SAFETY: the subclass is this one, and `reference` is its box,
            // which no other code owns.
            unsafe {
                let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID);
                drop(Box::from_raw(reference as *mut Notify));
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::{classify, SessionEnd, WM_ENDSESSION, WM_QUERYENDSESSION};

    /// `WM_CLOSE`, which is the dismissal this module must not turn into an
    /// exit.
    const WM_CLOSE: u32 = 0x0010;

    #[test]
    fn asking_whether_the_session_may_end_is_answered_without_leaving() {
        // The installer sends the query first and only sends the decision if
        // the answer was yes. Exiting here would close the launcher for anyone
        // who merely asked -- and would leave nothing alive to receive the
        // decision, including a `FALSE` one.
        assert_eq!(classify(WM_QUERYENDSESSION, 0), SessionEnd::Consent);
        assert_eq!(classify(WM_QUERYENDSESSION, 1), SessionEnd::Consent);
    }

    #[test]
    fn a_session_that_is_really_ending_shuts_the_launcher_down() {
        assert_eq!(classify(WM_ENDSESSION, 1), SessionEnd::Exit);
    }

    #[test]
    fn a_withdrawn_session_end_leaves_the_launcher_running() {
        assert_eq!(classify(WM_ENDSESSION, 0), SessionEnd::Cancelled);
    }

    #[test]
    fn a_close_request_is_not_a_session_end() {
        // Escape, a click away and the close button all arrive as `WM_CLOSE`,
        // and all three mean dismiss. If this module claimed `WM_CLOSE` the
        // launcher would exit on the first Escape.
        assert_eq!(classify(WM_CLOSE, 0), SessionEnd::Unhandled);
        assert_eq!(classify(WM_CLOSE, 1), SessionEnd::Unhandled);
    }
}
