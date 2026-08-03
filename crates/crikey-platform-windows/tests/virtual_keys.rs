//! Cross-check of the backend's Win32 numbers against Microsoft's metadata.
//!
//! [`crikey_platform_windows::HotkeyCode`] writes the `MOD_*` and `VK_*` values
//! out by hand so the mapping stays testable on a host that has no `winuser.h`.
//! That is only safe if the hand-written numbers are the real ones, which is a
//! claim only a Windows build can settle -- so it is settled here, and settled
//! at compile time: every assertion below is a `const` item, so a Windows build
//! of this crate's tests fails outright rather than reporting a wrong key at
//! runtime.

#![cfg(target_os = "windows")]

use crikey_platform::Accelerator;
use crikey_platform_windows::HotkeyCode;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, VK_0, VK_9, VK_A, VK_BACK, VK_DELETE, VK_DOWN,
    VK_END, VK_ESCAPE, VK_F1, VK_F12, VK_F24, VK_HOME, VK_INSERT, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN,
    VK_RIGHT, VK_SPACE, VK_TAB, VK_UP, VK_Z,
};

const _: () = assert!(HotkeyCode::MOD_ALT == MOD_ALT.0);
const _: () = assert!(HotkeyCode::MOD_CONTROL == MOD_CONTROL.0);
const _: () = assert!(HotkeyCode::MOD_SHIFT == MOD_SHIFT.0);
const _: () = assert!(HotkeyCode::MOD_WIN == MOD_WIN.0);
const _: () = assert!(HotkeyCode::MOD_NOREPEAT == MOD_NOREPEAT.0);

/// The virtual keys the backend spells out, in the order its table lists them.
const _: () = {
    assert!(VK_SPACE.0 == 0x20);
    assert!(VK_RETURN.0 == 0x0D);
    assert!(VK_TAB.0 == 0x09);
    assert!(VK_ESCAPE.0 == 0x1B);
    assert!(VK_BACK.0 == 0x08);
    assert!(VK_DELETE.0 == 0x2E);
    assert!(VK_INSERT.0 == 0x2D);
    assert!(VK_HOME.0 == 0x24);
    assert!(VK_END.0 == 0x23);
    assert!(VK_PRIOR.0 == 0x21);
    assert!(VK_NEXT.0 == 0x22);
    assert!(VK_UP.0 == 0x26);
    assert!(VK_DOWN.0 == 0x28);
    assert!(VK_LEFT.0 == 0x25);
    assert!(VK_RIGHT.0 == 0x27);
};

/// The ranges the backend derives instead of tabulating.
const _: () = {
    assert!(VK_A.0 == b'A' as u16);
    assert!(VK_Z.0 == b'Z' as u16);
    assert!(VK_0.0 == b'0' as u16);
    assert!(VK_9.0 == b'9' as u16);
    assert!(VK_F1.0 == 0x70);
    assert!(VK_F12.0 == VK_F1.0 + 11);
    assert!(VK_F24.0 == VK_F1.0 + 23);
};

/// The same values, reached the way the backend reaches them.
///
/// The `const` items above pin the constants; this pins the function that is
/// supposed to produce them.
#[test]
fn the_mapping_produces_every_metadata_value() {
    let virtual_key = |text: &str| {
        HotkeyCode::from_accelerator(&Accelerator::parse(text).expect("fixture parses"))
            .expect("fixture maps")
            .virtual_key()
    };

    let named = [
        ("Space", VK_SPACE.0),
        ("Enter", VK_RETURN.0),
        ("Tab", VK_TAB.0),
        ("Escape", VK_ESCAPE.0),
        ("Backspace", VK_BACK.0),
        ("Delete", VK_DELETE.0),
        ("Insert", VK_INSERT.0),
        ("Home", VK_HOME.0),
        ("End", VK_END.0),
        ("PageUp", VK_PRIOR.0),
        ("PageDown", VK_NEXT.0),
        ("Up", VK_UP.0),
        ("Down", VK_DOWN.0),
        ("Left", VK_LEFT.0),
        ("Right", VK_RIGHT.0),
    ];
    for (key, expected) in named {
        assert_eq!(
            virtual_key(&format!("Ctrl+{key}")),
            expected,
            "{key} maps incorrectly"
        );
    }

    for expected in b'A'..=b'Z' {
        let key = char::from(expected);
        assert_eq!(
            virtual_key(&format!("Ctrl+{key}")),
            u16::from(expected),
            "{key} maps incorrectly"
        );
    }
    for expected in b'0'..=b'9' {
        let key = char::from(expected);
        assert_eq!(
            virtual_key(&format!("Ctrl+{key}")),
            u16::from(expected),
            "{key} maps incorrectly"
        );
    }
    for number in 1..=24u16 {
        assert_eq!(
            virtual_key(&format!("Ctrl+F{number}")),
            VK_F1.0 + number - 1,
            "F{number} maps incorrectly"
        );
    }
}
