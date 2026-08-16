//! Public-API contract for Windows global hotkeys (spec 6.1, 18.4).
//!
//! This is the mapping and bookkeeping half of the M1 "global hotkey + app
//! discovery" deliverable on Windows: the translation from a shared
//! [`Accelerator`] into the `(fsModifiers, uVirtKey)` pair `RegisterHotKey`
//! takes, and the id allocator that makes `UnregisterHotKey` able to name what
//! was registered.
//!
//! Both run on every host, so both are pinned here on every host. What cannot
//! be pinned without a Windows kernel -- that `RegisterHotKey` actually claims
//! the key, that `WM_HOTKEY` reaches the handler -- is the part this file
//! deliberately does not claim to cover; see `virtual_keys.rs` for the
//! compile-time cross-check that the numbers handed to Win32 are the ones
//! Microsoft's own metadata defines.

use crikey_core::CoreError;
use crikey_platform::{Accelerator, Capability, CapabilityState, HotkeyBinding, HotkeyService, Modifiers};
use crikey_platform_windows::{HotkeyCode, HotkeyRegistrations, WindowsBackend, WindowsHotkeys};

fn accelerator(text: &str) -> Accelerator {
    Accelerator::parse(text).expect("fixture accelerator parses")
}

fn code(text: &str) -> HotkeyCode {
    HotkeyCode::from_accelerator(&accelerator(text)).expect("fixture accelerator maps")
}

fn binding(text: &str) -> HotkeyBinding {
    HotkeyBinding {
        accelerator: text.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Accelerators as Win32 hotkey codes
// ---------------------------------------------------------------------------

#[test]
fn modifiers_become_their_win32_flags() {
    assert_eq!(
        code("Ctrl+Space").modifiers(),
        HotkeyCode::MOD_NOREPEAT | HotkeyCode::MOD_CONTROL
    );
    assert_eq!(
        code("Alt+Space").modifiers(),
        HotkeyCode::MOD_NOREPEAT | HotkeyCode::MOD_ALT
    );
    assert_eq!(
        code("Shift+Space").modifiers(),
        HotkeyCode::MOD_NOREPEAT | HotkeyCode::MOD_SHIFT
    );
    // Meta is what the shared vocabulary calls the key Windows calls Win.
    assert_eq!(
        code("Meta+Space").modifiers(),
        HotkeyCode::MOD_NOREPEAT | HotkeyCode::MOD_WIN
    );
    assert_eq!(
        code("Ctrl+Alt+Shift+Meta+Space").modifiers(),
        HotkeyCode::MOD_NOREPEAT
            | HotkeyCode::MOD_CONTROL
            | HotkeyCode::MOD_ALT
            | HotkeyCode::MOD_SHIFT
            | HotkeyCode::MOD_WIN
    );
}

#[test]
fn every_registration_suppresses_auto_repeat() {
    // A launcher that toggles its window on activation must not toggle it
    // sixty times because the user leaned on the key.
    for text in ["Ctrl+Space", "Alt+F4", "Meta+R", "Ctrl+Shift+Escape"] {
        let modifiers = code(text).modifiers();
        assert_eq!(
            modifiers & HotkeyCode::MOD_NOREPEAT,
            HotkeyCode::MOD_NOREPEAT,
            "{text} was mapped without MOD_NOREPEAT"
        );
    }
}

#[test]
fn a_modifierless_accelerator_carries_no_modifier_flag() {
    assert_eq!(code("F12").modifiers(), HotkeyCode::MOD_NOREPEAT);
    assert_eq!(Modifiers::default(), accelerator("F12").modifiers());
}

#[test]
fn letters_and_digits_are_their_ascii_code_point() {
    assert_eq!(code("Ctrl+A").virtual_key(), 0x41);
    assert_eq!(code("Ctrl+Z").virtual_key(), 0x5A);
    assert_eq!(code("Ctrl+0").virtual_key(), 0x30);
    assert_eq!(code("Ctrl+9").virtual_key(), 0x39);
}

#[test]
fn a_letter_maps_the_same_however_it_was_spelled() {
    // The shared parser canonicalises case, so both spellings are one hotkey
    // and must reach Win32 as one virtual key.
    assert_eq!(code("Ctrl+k"), code("Ctrl+K"));
    assert_eq!(code("ctrl+alt+space"), code("Ctrl+Alt+Space"));
}

#[test]
fn function_keys_are_contiguous_from_f1() {
    assert_eq!(code("Ctrl+F1").virtual_key(), 0x70);
    assert_eq!(code("Ctrl+F12").virtual_key(), 0x7B);
    assert_eq!(code("Ctrl+F24").virtual_key(), 0x87);

    for number in 1..=24u16 {
        let key = format!("Ctrl+F{number}");
        assert_eq!(
            code(&key).virtual_key(),
            0x70 + number - 1,
            "{key} is not where the virtual-key space puts it"
        );
    }
}

#[test]
fn named_keys_map_to_their_documented_virtual_keys() {
    // Spelled out rather than derived: a table that generated its own
    // expectations would pass whatever it happened to contain.
    let expected = [
        ("Space", 0x20u16),
        ("Enter", 0x0D),
        ("Tab", 0x09),
        ("Escape", 0x1B),
        ("Backspace", 0x08),
        ("Delete", 0x2E),
        ("Insert", 0x2D),
        ("Home", 0x24),
        ("End", 0x23),
        ("PageUp", 0x21),
        ("PageDown", 0x22),
        ("Up", 0x26),
        ("Down", 0x28),
        ("Left", 0x25),
        ("Right", 0x27),
    ];

    for (key, virtual_key) in expected {
        assert_eq!(
            code(&format!("Ctrl+{key}")).virtual_key(),
            virtual_key,
            "{key} maps to the wrong virtual key"
        );
    }
}

#[test]
fn every_key_the_shared_parser_accepts_has_a_virtual_key() {
    // The mapping refuses rather than guesses, so a key the shared vocabulary
    // gains and this backend has not been taught would be a loud failure at
    // registration time. This is the test that would notice first.
    let mut candidates: Vec<String> = Vec::new();
    candidates.extend((0x21u8..=0x7E).map(|byte| (byte as char).to_string()));
    candidates.extend((0..=30).map(|number| format!("F{number}")));
    candidates.extend(
        [
            "Space",
            "Enter",
            "Tab",
            "Escape",
            "Backspace",
            "Delete",
            "Insert",
            "Home",
            "End",
            "PageUp",
            "PageDown",
            "Up",
            "Down",
            "Left",
            "Right",
        ]
        .map(str::to_owned),
    );

    let mut accepted = 0usize;
    for candidate in candidates {
        // `+` is the component separator and can never itself be a key.
        let Ok(parsed) = Accelerator::parse(&format!("Ctrl+{candidate}")) else {
            continue;
        };
        accepted += 1;
        assert!(
            HotkeyCode::from_accelerator(&parsed).is_ok(),
            "{candidate} parses as a key but has no Win32 virtual-key code"
        );
    }

    // 26 letters + 10 digits + 24 function keys + 15 named keys, each reached
    // by at least one candidate above.
    assert!(accepted >= 75, "the sweep only exercised {accepted} keys");
}

// ---------------------------------------------------------------------------
// Registration ids
// ---------------------------------------------------------------------------

#[test]
fn ids_start_at_one_and_count_up() {
    let mut registrations = HotkeyRegistrations::new();
    let first = registrations
        .insert("Ctrl+Space".to_owned(), code("Ctrl+Space"))
        .expect("first id is available");
    let second = registrations
        .insert("Alt+Space".to_owned(), code("Alt+Space"))
        .expect("second id is available");

    assert_eq!(first.id(), HotkeyRegistrations::MIN_ID);
    assert_eq!(second.id(), HotkeyRegistrations::MIN_ID + 1);
    assert_eq!(registrations.len(), 2);
}

#[test]
fn a_registration_keeps_its_id_and_its_accelerator() {
    let mut registrations = HotkeyRegistrations::new();
    let registration = registrations
        .insert("Ctrl+Alt+K".to_owned(), code("Ctrl+Alt+K"))
        .expect("id is available");

    let found = registrations
        .find("Ctrl+Alt+K")
        .expect("the registration is findable by accelerator");
    assert_eq!(found.id(), registration.id());
    assert_eq!(found.accelerator(), "Ctrl+Alt+K");
    assert_eq!(found.code(), code("Ctrl+Alt+K"));
}

#[test]
fn a_released_id_is_reused_lowest_first() {
    // Reuse is what keeps a launcher that rebinds its shortcut on every config
    // reload from walking off the end of the id space.
    let mut registrations = HotkeyRegistrations::new();
    for accelerator in ["Ctrl+1", "Ctrl+2", "Ctrl+3"] {
        registrations
            .insert(accelerator.to_owned(), code(accelerator))
            .expect("id is available");
    }

    let released = registrations.remove("Ctrl+2").expect("Ctrl+2 was registered");
    assert_eq!(released.id(), 2);

    let replacement = registrations
        .insert("Ctrl+4".to_owned(), code("Ctrl+4"))
        .expect("the freed id is available");
    assert_eq!(replacement.id(), 2);
}

#[test]
fn ids_stay_in_order_after_a_gap_is_filled() {
    let mut registrations = HotkeyRegistrations::new();
    for accelerator in ["Ctrl+1", "Ctrl+2", "Ctrl+3"] {
        registrations
            .insert(accelerator.to_owned(), code(accelerator))
            .expect("id is available");
    }
    registrations.remove("Ctrl+2");
    registrations
        .insert("Ctrl+4".to_owned(), code("Ctrl+4"))
        .expect("id is available");

    let ids: Vec<i32> = registrations.iter().map(|entry| entry.id()).collect();
    assert_eq!(ids, vec![1, 2, 3]);
    let accelerators: Vec<&str> = registrations.iter().map(|entry| entry.accelerator()).collect();
    assert_eq!(accelerators, vec!["Ctrl+1", "Ctrl+4", "Ctrl+3"]);
}

#[test]
fn the_same_accelerator_cannot_hold_two_ids() {
    let mut registrations = HotkeyRegistrations::new();
    registrations
        .insert("Ctrl+Space".to_owned(), code("Ctrl+Space"))
        .expect("id is available");

    let again = registrations.insert("Ctrl+Space".to_owned(), code("Ctrl+Space"));
    assert!(matches!(again, Err(CoreError::Invalid(_))));
    assert_eq!(registrations.len(), 1);
}

#[test]
fn removing_an_unregistered_accelerator_reports_nothing() {
    let mut registrations = HotkeyRegistrations::new();
    assert!(registrations.remove("Ctrl+Space").is_none());
    assert!(registrations.is_empty());
}

// ---------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------

#[test]
fn an_unparseable_accelerator_is_refused_before_any_id_is_spent() {
    let mut hotkeys = WindowsHotkeys::new();

    for text in ["", "Ctrl+", "Ctrl", "Ctrl+Ctrl+A", "Ctrl+A+B", "Ctrl+Nope"] {
        let refusal = hotkeys.register(&binding(text));
        assert!(
            matches!(refusal, Err(CoreError::Invalid(_))),
            "{text:?} should not be registrable"
        );
    }
    assert!(hotkeys.registrations().is_empty());
}

#[test]
fn releasing_an_accelerator_that_was_never_registered_is_an_error() {
    // Quietly succeeding would leave the caller believing it had given a key
    // back to the system while the backend went on swallowing it.
    let mut hotkeys = WindowsHotkeys::new();
    let refusal = hotkeys.unregister(&binding("Ctrl+Space"));
    assert!(matches!(refusal, Err(CoreError::Invalid(_))));
}

#[test]
fn a_handler_can_be_installed_and_cleared_without_a_registration() {
    // The launcher installs its event-loop wake before it binds anything, and
    // clearing it must not be an error either.
    let mut hotkeys = WindowsHotkeys::new();
    hotkeys.set_activation_handler(Some(Box::new(|_binding| {})));
    hotkeys.set_activation_handler(None);
    assert!(hotkeys.registrations().is_empty());
}

#[cfg(not(target_os = "windows"))]
#[test]
fn off_target_registration_fails_instead_of_pretending() {
    // The point of the crate compiling here is that its logic is testable, not
    // that it works. A hotkey nothing can deliver must be reported as refused.
    let mut hotkeys = WindowsHotkeys::new();
    let refusal = hotkeys.register(&binding("Ctrl+Space"));

    match refusal {
        Err(CoreError::Invalid(reason)) => {
            assert!(
                reason.contains("does not target Windows"),
                "the refusal should say why: {reason}"
            );
        }
        other => panic!("expected a typed refusal, got {other:?}"),
    }

    // A refused registration must not leave its id reserved.
    assert!(hotkeys.registrations().is_empty());
}

/// Everything this backend stands behind, and therefore claims on Windows and
/// nowhere else.
const IMPLEMENTED: [Capability; 4] = [
    Capability::ApplicationDiscovery,
    Capability::GlobalHotkeys,
    Capability::ProcessLaunch,
    Capability::UriOpen,
];

#[cfg(not(target_os = "windows"))]
#[test]
fn off_target_capabilities_are_not_claimed() {
    let backend = WindowsBackend::new();
    for capability in IMPLEMENTED {
        assert_eq!(
            backend.capability(capability),
            CapabilityState::Unavailable,
            "{capability:?} has no Windows underneath it here and must not be claimed"
        );
    }
}

#[cfg(target_os = "windows")]
#[test]
fn on_target_the_implemented_capabilities_are_claimed() {
    let backend = WindowsBackend::new();
    for capability in IMPLEMENTED {
        assert_eq!(
            backend.capability(capability),
            CapabilityState::Available,
            "{capability:?} is implemented here and should be claimed"
        );
    }
}

#[test]
fn unimplemented_capabilities_are_never_claimed() {
    let backend = WindowsBackend::new();
    for capability in [
        Capability::Clipboard,
        Capability::WindowEnumeration,
        Capability::WindowActivation,
        Capability::Notifications,
        Capability::FileWatching,
        Capability::SecretStorage,
        Capability::ShellIntegration,
    ] {
        assert_eq!(
            backend.capability(capability),
            CapabilityState::Unavailable,
            "{capability:?} is claimed but nothing implements it"
        );
    }
}

/// Icons are the one capability this backend half-implements, and the claim
/// has to say so on the platform where the half exists.
///
/// A shortcut naming a real image file gets pixels; one naming a PE resource
/// or a packaged application gets nothing, and `Partial` is the only honest
/// word for that. Off Windows there is no half at all, so the same backend
/// claims `Unavailable` — which is what this test asserted for every host
/// until the suite first ran on a Windows one.
#[test]
fn the_half_implemented_icon_capability_is_claimed_as_partial_only_where_it_exists() {
    let expected = if cfg!(target_os = "windows") {
        CapabilityState::Partial
    } else {
        CapabilityState::Unavailable
    };
    assert_eq!(WindowsBackend::new().capability(Capability::Icons), expected);
}

/// File search is the second half-implemented capability, and half for a reason
/// that is not this crate's to fix.
///
/// The `SystemIndex` catalog holds the locations Windows Search is configured to
/// index, which on a clean install is Documents, Pictures, Music and the Desktop
/// rather than the drive, and the directory walk that covers the rest -- notably
/// Downloads -- reaches a handful of profile folders. Real results from a subset
/// is `Partial`, and it stays `Partial` on Windows: the missing part is the
/// user's indexing configuration, not missing code. Off Windows there is neither
/// catalog nor profile, so there is nothing to claim.
#[test]
fn file_search_is_claimed_as_partial_only_where_windows_search_exists() {
    let expected = if cfg!(target_os = "windows") {
        CapabilityState::Partial
    } else {
        CapabilityState::Unavailable
    };
    assert_eq!(WindowsBackend::new().capability(Capability::FileSearch), expected);
}

/// A backend hands out a service only for the session it is running in.
#[test]
fn the_file_search_service_exists_only_on_windows() {
    let backend = WindowsBackend::new();
    assert_eq!(
        backend.file_search().is_some(),
        cfg!(target_os = "windows"),
        "the service must be offered exactly where Windows Search and a Windows profile are"
    );
}

#[test]
fn the_backend_names_itself() {
    assert_eq!(WindowsBackend::NAME, "windows");
}
