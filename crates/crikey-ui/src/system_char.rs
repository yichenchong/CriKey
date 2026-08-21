//! Keeping the Windows alert sound out of the launcher's keystrokes.
//!
//! Users of the shipped Windows build reported one alert sound per typed
//! character. The sound is `MessageBeep`, and on Windows there is exactly one
//! route from a keystroke to it: a `WM_SYSCHAR` that reaches `DefWindowProc`.
//! `DefWindowProc` treats a system character as a menu mnemonic -- it forwards
//! it as `WM_SYSCOMMAND`/`SC_KEYMENU`, which beeps when no menu item matches,
//! and beeps directly when the character arrived without the Alt-down flag
//! (ReactOS `win32ss/user/ntuser/defwnd.c`, the `WM_SYSCHAR` arm; the two
//! branches are `SC_KEYMENU` and the bare `MessageBeep`). The launcher window
//! is created undecorated (`native.rs`, `with_decorations(false)`), so it has
//! neither a menu bar nor a system menu and *no* mnemonic can ever match: every
//! system character it is handed is a beep.
//!
//! `WM_SYSCHAR` arrives instead of `WM_CHAR` whenever `TranslateMessage`
//! translates a `WM_SYSKEYDOWN` -- which the system posts while Alt is held,
//! and also when no window holds the keyboard focus, in which case the
//! keystroke goes to the active window as a system key. The launcher is
//! activated by a hotkey that contains Alt by default (`Ctrl+Alt+Space`), so
//! both are reachable from an ordinary activation.
//!
//! winit *usually* swallows the message before `DefWindowProc` sees it, but
//! only conditionally: `winit-0.30.5`'s `platform_impl/windows/keyboard.rs`
//! handles `WM_CHAR | WM_SYSCHAR` at line 168 and consumes it with
//! `ProcResult::Value(0)` only when its `KeyEventBuilder` is holding the
//! `event_info` built by the matching key-down (line 170 returns
//! `MatchResult::Nothing` otherwise, logging "Received a CHAR message but no
//! `event_info` was available"). Nothing else in winit claims a character
//! message -- `platform_impl/windows/event_loop.rs` matches only key *down* and
//! *up* messages when it forces `ProcResult::Value(0)` (line 1046) and its
//! message match has no character arm -- so an unmatched character message
//! keeps the `ProcResult::DefWindowProc` the window procedure starts with and
//! is beeped.
//!
//! This module removes the whole class of beeps rather than betting on which
//! of those preconditions the reporter hit: the launcher's window procedure is
//! handed `WM_CHAR` in place of `WM_SYSCHAR`, so no system character can reach
//! `DefWindowProc` however it was produced. winit treats the two identically
//! when it builds a key event (`keyboard.rs:168` matches both, and so does the
//! "more characters coming" lookahead at line 212, which peeks the untouched
//! queue), so the character is still typed. What is given up is menu mnemonics
//! and the Alt+Space system menu, neither of which an undecorated window has.
//!
//! Key *down* messages are deliberately left alone. `WM_SYSKEYDOWN` is what
//! `DefWindowProc` turns into Alt+F4 (`SC_CLOSE`) and Alt+Tab/Alt+Esc
//! (`SC_NEXTWINDOW`), and winit relies on exactly that: `keyboard.rs:107`
//! refuses to dispatch Alt+F4 to the application because
//! `event_loop.rs:1661-1664` forwards the message to `DefWindowProc` for it.
//! Rewriting or swallowing key-down messages would take Alt+F4 away; rewriting
//! character messages costs nothing, because `DefWindowProc` does not beep for
//! a key down at all -- its `WM_SYSKEYDOWN` arm only inspects F4, PrintScreen,
//! Escape, Tab and F10.
//!
//! The mapping is a pure function of the message id and is therefore compiled
//! and tested on every host, the way `crikey-platform-windows` keeps its
//! virtual-key table testable off target. Only the caller -- the window
//! subclass that owns the launcher `HWND` -- is Windows gated.

/// `WM_CHAR`: the character message an ordinary keystroke produces.
pub const WM_CHAR: u32 = 0x0102;

/// `WM_SYSCHAR`: the character message a *system* keystroke produces, and the
/// only keyboard message `DefWindowProc` answers with an alert sound.
pub const WM_SYSCHAR: u32 = 0x0106;

/// The message id the launcher's window procedure should be given in place of
/// `message`.
///
/// Every id but [`WM_SYSCHAR`] is returned unchanged: this is the narrowest
/// edit that keeps a system character away from `DefWindowProc`, and in
/// particular it leaves `WM_SYSKEYDOWN` -- Alt+F4 and Alt+Tab -- alone. See the
/// module documentation for why the character message is the one that beeps.
///
/// A caller must use the returned id both for its own dispatch and for the
/// procedure it forwards to; rewriting one and not the other would leave the
/// original message on the path to `DefWindowProc`.
pub fn beep_free_message(message: u32) -> u32 {
    if message == WM_SYSCHAR {
        WM_CHAR
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The message numbers are spelled out here rather than reusing the
    /// constants, so that a wrong constant fails on every host instead of only
    /// where `windows` metadata is available to compare against.
    #[test]
    fn a_system_character_is_handed_over_as_an_ordinary_character() {
        assert_eq!(beep_free_message(0x0106), 0x0102);
    }

    /// Alt+F4 and Alt+Tab are `DefWindowProc`'s job, reached through
    /// `WM_SYSKEYDOWN`; the dead-key messages winit unwraps unconditionally are
    /// not ours to move either. Nothing in the keyboard message block may be
    /// rewritten except the one message that beeps.
    #[test]
    fn every_other_keyboard_message_is_left_exactly_as_it_arrived() {
        for message in 0x0100..=0x0109_u32 {
            if message == 0x0106 {
                continue;
            }
            assert_eq!(
                beep_free_message(message),
                message,
                "message 0x{message:04x} must not be rewritten"
            );
        }
        // The two that would actually break something, named for the record.
        assert_eq!(beep_free_message(0x0104), 0x0104, "WM_SYSKEYDOWN");
        assert_eq!(beep_free_message(0x0107), 0x0107, "WM_SYSDEADCHAR");
    }

    /// A subclass that is installed twice, or a caller that maps a message it
    /// has already mapped, must not turn a character into something else again.
    #[test]
    fn the_rewrite_is_idempotent() {
        let once = beep_free_message(WM_SYSCHAR);
        assert_eq!(beep_free_message(once), once);
    }

    /// Non-keyboard traffic vastly outnumbers keyboard traffic on a window
    /// procedure; none of it may be touched.
    #[test]
    fn messages_outside_the_keyboard_block_pass_through() {
        for message in [
            0x0000_u32,
            0x0002,
            0x0010,
            0x0011,
            0x0086,
            0x0100 - 1,
            0x010a,
            0x8000,
        ] {
            assert_eq!(beep_free_message(message), message);
        }
    }

    /// The hand written numbers against Microsoft's own metadata, as `const`
    /// assertions rather than a runtime test: a cross-compile check of this
    /// crate is then enough to prove the constants, which matters because the
    /// only host that could *run* such a test is the one nobody develops on.
    #[cfg(target_os = "windows")]
    const _: () = assert!(WM_CHAR == windows::Win32::UI::WindowsAndMessaging::WM_CHAR);
    #[cfg(target_os = "windows")]
    const _: () = assert!(WM_SYSCHAR == windows::Win32::UI::WindowsAndMessaging::WM_SYSCHAR);
}
