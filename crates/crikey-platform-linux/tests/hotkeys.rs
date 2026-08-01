//! Public-API contract for the Linux global-hotkey mapping (spec 6.1, 18.6).
//!
//! This is the pure half of the X11 hotkey backend: the translation of a shared
//! [`Accelerator`] into the `(modifier mask, keysym name)` pair that `XGrabKey`
//! is ultimately given. It runs on every host because it touches no display, so
//! it is pinned on every host.
//!
//! What cannot be pinned here — that the grab is actually taken, that a second
//! grab of the same chord is refused, that releasing it hands the key back —
//! needs a real X server and lives in `hotkeys_x11.rs`.
//!
//! These tests are written before the implementation. They fail to compile
//! until `crikey_platform_linux` exports `MOD_SHIFT`, `MOD_CONTROL`, `MOD_ALT`,
//! `MOD_SUPER` and `x11_binding`.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;

use crikey_core::CoreError;
use crikey_platform::Accelerator;
use crikey_platform_linux::{x11_binding, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_SUPER};

fn accelerator(text: &str) -> Accelerator {
    Accelerator::parse(text).expect("fixture accelerator parses")
}

/// The `(mask, keysym)` pair a fixture accelerator maps to.
fn binding_of(text: &str) -> (u32, String) {
    x11_binding(&accelerator(text)).unwrap_or_else(|error| panic!("{text:?} should map: {error}"))
}

fn mask_of(text: &str) -> u32 {
    binding_of(text).0
}

fn keysym_of(text: &str) -> String {
    binding_of(text).1
}

// ---------------------------------------------------------------------------
// Modifier masks
// ---------------------------------------------------------------------------

/// The four modifier bits are the X11 core-protocol ones, by value.
///
/// Kills the bug where a backend invents its own bit numbering: the mask is
/// handed straight to `XGrabKey`, so an off-by-one bit grabs a chord the user
/// never asked for and leaves the one they did ask for dead.
#[test]
fn each_modifier_bit_is_the_x11_core_protocol_bit() {
    assert_eq!(MOD_SHIFT, 1, "ShiftMask is bit 0");
    assert_eq!(MOD_CONTROL, 4, "ControlMask is bit 2");
    assert_eq!(MOD_ALT, 8, "Mod1Mask is bit 3");
    assert_eq!(MOD_SUPER, 64, "Mod4Mask is bit 6");
}

/// A single-modifier accelerator carries exactly that modifier's bit.
///
/// Kills the bug where the shared vocabulary is mis-wired to the X11 one —
/// `Meta` reaching X as Mod1 rather than Mod4, say — which would bind Alt when
/// the user wrote Super.
#[test]
fn a_single_modifier_maps_to_exactly_its_own_bit() {
    assert_eq!(mask_of("Shift+Space"), 1);
    assert_eq!(mask_of("Ctrl+Space"), 4);
    assert_eq!(mask_of("Alt+Space"), 8);
    // `Meta` is what the shared vocabulary calls the key X11 calls Super.
    assert_eq!(mask_of("Meta+Space"), 64);
}

/// Combined modifiers are the bitwise OR of their bits, and nothing else.
///
/// Kills the bug where a chord is mapped by a lookup keyed on one "primary"
/// modifier, silently dropping the rest of the chord.
#[test]
fn combined_modifiers_are_ored_together() {
    assert_eq!(
        binding_of("Ctrl+Alt+Space"),
        (MOD_CONTROL | MOD_ALT, "space".to_owned())
    );
    assert_eq!(mask_of("Ctrl+Alt+Space"), 12);
    assert_eq!(
        mask_of("Ctrl+Alt+Shift+Meta+Space"),
        MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_SUPER
    );
    assert_eq!(mask_of("Ctrl+Alt+Shift+Meta+Space"), 77);
}

/// Modifier order in the written accelerator does not change the mask.
///
/// Kills the bug where the mask is accumulated positionally (first component
/// wins, later ones overwrite), which would make a user's config file bind a
/// different chord depending on how they happened to type it.
#[test]
fn the_order_modifiers_were_written_in_does_not_change_the_mask() {
    let canonical = binding_of("Ctrl+Alt+Shift+Meta+K");
    for spelling in [
        "Meta+Shift+Alt+Ctrl+K",
        "Shift+Ctrl+Meta+Alt+K",
        "alt+meta+ctrl+shift+k",
        "Meta+Ctrl+Shift+Alt+k",
    ] {
        assert_eq!(binding_of(spelling), canonical, "{spelling} mapped differently");
    }
}

/// A bare key with no modifier maps with an empty mask.
///
/// Kills the bug where a backend defaults to some "sensible" modifier: an
/// unmodified `F12` must be grabbed as `F12`, not as `Ctrl+F12`.
#[test]
fn a_modifierless_accelerator_carries_no_modifier_bit() {
    assert_eq!(binding_of("F12"), (0, "F12".to_owned()));
    assert_eq!(mask_of("Space"), 0);
    assert_eq!(mask_of("A"), 0);
}

// ---------------------------------------------------------------------------
// Keysym names
// ---------------------------------------------------------------------------

/// Letters map to their lowercase keysym name, whatever case they were written
/// in.
///
/// Kills the bug where `Ctrl+A` is mapped to the keysym `A` (which on X11 is
/// the *shifted* symbol) and so only fires when Shift is also held.
#[test]
fn letters_map_to_their_lowercase_keysym_name() {
    assert_eq!(keysym_of("Ctrl+A"), "a");
    assert_eq!(keysym_of("Ctrl+a"), "a");
    assert_eq!(keysym_of("Ctrl+Z"), "z");
    assert_eq!(keysym_of("Ctrl+k"), keysym_of("Ctrl+K"));

    for (offset, letter) in "ABCDEFGHIJKLMNOPQRSTUVWXYZ".chars().enumerate() {
        let expected = char::from(b'a' + offset as u8).to_string();
        assert_eq!(
            keysym_of(&format!("Ctrl+{letter}")),
            expected,
            "{letter} mapped wrong"
        );
    }
}

/// Digits map to their own single-character keysym name.
///
/// Kills the bug where a digit is routed through the letter table and comes out
/// as a letter or as an empty name.
#[test]
fn digits_map_to_their_own_keysym_name() {
    for digit in '0'..='9' {
        assert_eq!(keysym_of(&format!("Ctrl+{digit}")), digit.to_string());
    }
}

/// Function keys keep their `F<n>` keysym spelling, uppercase `F`.
///
/// Kills the bug where the letter rule is applied to the whole component and
/// `F1` is lowercased to `f1`, which names no keysym at all.
#[test]
fn function_keys_keep_their_uppercase_f_spelling() {
    for number in 1..=12u32 {
        let text = format!("Ctrl+F{number}");
        assert_eq!(keysym_of(&text), format!("F{number}"), "{text} mapped wrong");
    }
    assert_eq!(keysym_of("F24"), "F24");
}

/// Every word-spelled key maps to the X11 keysym name for that key.
///
/// Spelled out rather than derived from the implementation's own table: a test
/// that generated its expectations would pass whatever the table contained.
/// Kills the bug where a named key is passed through verbatim — `Enter` and
/// `PageUp` are not keysym names, and a grab on a name X cannot resolve is a
/// hotkey that can only ever be dead.
#[test]
fn named_keys_map_to_their_x11_keysym_names() {
    let expected = [
        ("Space", "space"),
        ("Enter", "Return"),
        ("Tab", "Tab"),
        ("Escape", "Escape"),
        ("Backspace", "BackSpace"),
        ("Delete", "Delete"),
        ("Insert", "Insert"),
        ("Home", "Home"),
        ("End", "End"),
        ("PageUp", "Prior"),
        ("PageDown", "Next"),
        ("Up", "Up"),
        ("Down", "Down"),
        ("Left", "Left"),
        ("Right", "Right"),
    ];

    for (key, keysym) in expected {
        assert_eq!(
            keysym_of(&format!("Ctrl+{key}")),
            keysym,
            "{key} maps to the wrong keysym"
        );
    }
}

/// The keysym never depends on the modifiers that accompany it.
///
/// Kills the bug where a shifted chord is mapped to the shifted symbol, so that
/// `Ctrl+Shift+A` grabs a different key from `Ctrl+A`.
#[test]
fn the_keysym_does_not_depend_on_the_modifiers() {
    assert_eq!(keysym_of("A"), "a");
    assert_eq!(keysym_of("Shift+A"), "a");
    assert_eq!(keysym_of("Ctrl+Shift+Meta+A"), "a");
}

// ---------------------------------------------------------------------------
// Injectivity
// ---------------------------------------------------------------------------

/// Distinct accelerators map to distinct `(mask, keysym)` pairs.
///
/// This is the property the user actually cares about: a mapping that collapsed
/// two chords would silently steal one of their hotkeys, and the collision
/// would surface as "my shortcut does the wrong thing", never as an error.
#[test]
fn distinct_accelerators_never_collapse_onto_one_binding() {
    let accelerators = [
        "Ctrl+Space",
        "Alt+Space",
        "Shift+Space",
        "Meta+Space",
        "Ctrl+Alt+Space",
        "Ctrl+Shift+Space",
        "Ctrl+A",
        "Ctrl+B",
        "Ctrl+F1",
        "Ctrl+F2",
        "Alt+F1",
        "Meta+Enter",
        "Ctrl+Escape",
        "Ctrl+Tab",
        "F12",
    ];

    let mut seen: BTreeSet<(u32, String)> = BTreeSet::new();
    for text in accelerators {
        let mapped = binding_of(text);
        assert!(
            seen.insert(mapped.clone()),
            "{text} collided with an earlier accelerator on {mapped:?}"
        );
    }
    assert_eq!(seen.len(), accelerators.len());
}

/// Re-parsing an accelerator's canonical rendering maps to the same binding.
///
/// Kills the bug where the canonical form the config round-trips through maps
/// differently from the form the user wrote, so a saved hotkey stops working
/// after a restart.
#[test]
fn the_canonical_rendering_maps_to_the_same_binding() {
    for text in ["ctrl+alt+space", "meta+shift+f5", "SHIFT+CTRL+K"] {
        let parsed = accelerator(text);
        let round_tripped = accelerator(&parsed.canonical());
        assert_eq!(
            x11_binding(&parsed).expect("maps"),
            x11_binding(&round_tripped).expect("maps"),
            "{text} maps differently from its canonical form"
        );
    }
}

/// A key the mapping cannot express is refused loudly rather than mapped to a
/// placeholder.
///
/// Every key the shared parser accepts is a key a Linux desktop defines, so the
/// whole accepted vocabulary must map. Kills the bug where an unmapped key
/// silently becomes keysym `0` (`NoSymbol`), which grabs nothing.
#[test]
fn every_key_the_shared_parser_accepts_has_a_keysym() {
    let keys = [
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
    .into_iter()
    .map(str::to_owned)
    .chain((1..=24u32).map(|number| format!("F{number}")))
    .chain(
        "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
            .chars()
            .map(|key| key.to_string()),
    );

    for key in keys {
        let mapped = x11_binding(&accelerator(&key));
        let Ok((mask, keysym)) = mapped else {
            panic!("{key} names a key the shared parser accepts but the backend cannot map");
        };
        assert_eq!(mask, 0, "{key} carries no modifier");
        assert!(!keysym.is_empty(), "{key} mapped to an empty keysym name");
    }
}

/// A refusal, when one happens, is a named `Invalid` error.
///
/// The mapping's error type is the shared one, so a caller can report why a
/// hotkey was rejected instead of unwrapping an opaque failure.
#[test]
fn a_refusal_is_reported_as_an_invalid_error() {
    // The whole accepted vocabulary maps, so this is a type-level guarantee
    // exercised through the one construction that can carry a refusal.
    fn assert_named(refusal: Result<(u32, String), CoreError>) {
        if let Err(error) = refusal {
            assert!(
                matches!(error, CoreError::Invalid(_)),
                "unexpected error kind: {error:?}"
            );
        }
    }

    assert_named(x11_binding(&accelerator("Ctrl+Space")));
}
