//! Translation from the launcher web-input vocabulary to libwpe's input ABI.
//!
//! This module deliberately has no engine dependency. The values handed to the
//! C shim are decided and tested here; the shim only copies them into libwpe
//! structs.

use crikey_native_protocol::message::{
    RawInput, WebInputCode, WEB_MODIFIER_ALT, WEB_MODIFIER_CAPS_LOCK, WEB_MODIFIER_CONTROL,
    WEB_MODIFIER_META, WEB_MODIFIER_NUM_LOCK, WEB_MODIFIER_SHIFT,
};

const KNOWN_MODIFIERS: u32 = WEB_MODIFIER_SHIFT
    | WEB_MODIFIER_CONTROL
    | WEB_MODIFIER_ALT
    | WEB_MODIFIER_META
    | WEB_MODIFIER_CAPS_LOCK
    | WEB_MODIFIER_NUM_LOCK;

// libwpe 1.x's `enum wpe_input_modifier`. Lock state has no corresponding ABI
// bit in that generation; keysyms already reflect it, so the two lock bits are
// accepted and intentionally do not alias an unrelated pointer bit.
const WPE_CONTROL: u32 = 1 << 0;
const WPE_SHIFT: u32 = 1 << 1;
const WPE_ALT: u32 = 1 << 2;
const WPE_META: u32 = 1 << 3;

// libwpe's button state is carried on every pointer event, independently of
// the button whose transition the event reports.
const WPE_BUTTON1: u32 = 1 << 20;

/// One fully validated operation understood by the C shim.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineInput {
    Key {
        time_ms: u32,
        keysym: u32,
        hardware_keycode: u32,
        pressed: bool,
        modifiers: u32,
        repeat: bool,
    },
    PointerMotion {
        time_ms: u32,
        x: i32,
        y: i32,
        modifiers: u32,
    },
    PointerButton {
        time_ms: u32,
        x: i32,
        y: i32,
        button: u32,
        pressed: bool,
        modifiers: u32,
    },
    Axis {
        time_ms: u32,
        x: i32,
        y: i32,
        delta_x: f64,
        delta_y: f64,
        modifiers: u32,
        precise: bool,
    },
    Focus(bool),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InputError {
    #[error("input kind is unspecified")]
    UnspecifiedKind,
    #[error("input carries unknown modifier bits 0x{0:x}")]
    UnknownModifiers(u32),
    #[error("input {field} coordinate is not finite")]
    NonFinite { field: &'static str },
    #[error("input {field} coordinate {value} is outside libwpe's signed pixel range")]
    CoordinateOutOfRange { field: &'static str, value: String },
    #[error("pointer button zero is not a button")]
    MissingButton,
    #[error("repeat is valid only on a key-down input")]
    InvalidRepeat,
}

/// Stateful translator at the protocol-to-engine boundary.
///
/// The wire reports button transitions, while libwpe needs the held-button
/// mask on motion and wheel events as well as on the transition itself.
#[derive(Debug, Default)]
pub struct InputTranslator {
    pointer_buttons: u32,
}

impl InputTranslator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn translate(&mut self, input: &RawInput) -> Result<EngineInput, InputError> {
        // Reject hostile NaNs even in fields that a particular kind does not
        // consume. A protobuf producer may populate every field, and allowing
        // a NaN to hide in a key event makes validation depend on incidental
        // union conventions that the wire format does not have.
        validate_finite(input.x, "x")?;
        validate_finite(input.y, "y")?;
        validate_finite(input.delta_x, "delta_x")?;
        validate_finite(input.delta_y, "delta_y")?;

        let modifiers = translate_modifiers(input.modifiers)?;
        if input.repeat && input.kind != WebInputCode::KeyDown {
            return Err(InputError::InvalidRepeat);
        }
        // libwpe timestamps are the usual wrapping 32-bit millisecond clock.
        let time_ms = input.timestamp_ms as u32;
        match input.kind {
            WebInputCode::KeyDown | WebInputCode::KeyUp => Ok(EngineInput::Key {
                time_ms,
                keysym: input.keysym,
                hardware_keycode: input.hardware_keycode,
                pressed: input.kind == WebInputCode::KeyDown,
                modifiers,
                repeat: input.repeat,
            }),
            WebInputCode::PointerMove => Ok(EngineInput::PointerMotion {
                time_ms,
                x: pixel(input.x, "x")?,
                y: pixel(input.y, "y")?,
                modifiers: modifiers | self.pointer_buttons,
            }),
            WebInputCode::PointerDown | WebInputCode::PointerUp => {
                if input.button == 0 {
                    return Err(InputError::MissingButton);
                }
                let x = pixel(input.x, "x")?;
                let y = pixel(input.y, "y")?;
                let button = translate_button(input.button);
                let mask = pointer_button_mask(button);
                if input.kind == WebInputCode::PointerDown {
                    self.pointer_buttons |= mask;
                } else {
                    self.pointer_buttons &= !mask;
                }
                Ok(EngineInput::PointerButton {
                    time_ms,
                    x,
                    y,
                    button,
                    pressed: input.kind == WebInputCode::PointerDown,
                    modifiers: modifiers | self.pointer_buttons,
                })
            }
            WebInputCode::Scroll => Ok(EngineInput::Axis {
                time_ms,
                x: pixel(input.x, "x")?,
                y: pixel(input.y, "y")?,
                delta_x: f64::from(input.delta_x),
                delta_y: f64::from(input.delta_y),
                precise: input.precise,
                modifiers: modifiers | self.pointer_buttons,
            }),
            WebInputCode::FocusIn => Ok(EngineInput::Focus(true)),
            WebInputCode::FocusOut => {
                self.pointer_buttons = 0;
                Ok(EngineInput::Focus(false))
            }
            WebInputCode::KindUnspecified => Err(InputError::UnspecifiedKind),
        }
    }
}

fn validate_finite(value: f32, field: &'static str) -> Result<(), InputError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(InputError::NonFinite { field })
    }
}

fn pixel(value: f32, field: &'static str) -> Result<i32, InputError> {
    let rounded = f64::from(value).round();
    if rounded < f64::from(i32::MIN) || rounded > f64::from(i32::MAX) {
        return Err(InputError::CoordinateOutOfRange {
            field,
            value: value.to_string(),
        });
    }
    Ok(rounded as i32)
}

fn translate_modifiers(modifiers: u32) -> Result<u32, InputError> {
    let unknown = modifiers & !KNOWN_MODIFIERS;
    if unknown != 0 {
        return Err(InputError::UnknownModifiers(unknown));
    }
    let mut translated = 0;
    if modifiers & WEB_MODIFIER_CONTROL != 0 {
        translated |= WPE_CONTROL;
    }
    if modifiers & WEB_MODIFIER_SHIFT != 0 {
        translated |= WPE_SHIFT;
    }
    if modifiers & WEB_MODIFIER_ALT != 0 {
        translated |= WPE_ALT;
    }
    if modifiers & WEB_MODIFIER_META != 0 {
        translated |= WPE_META;
    }
    Ok(translated)
}

fn translate_button(button: u32) -> u32 {
    // The launcher uses 1=left, 2=middle, 3=right. The libwpe backend's
    // button-event ABI uses 1=left, 2=right, 3=middle.
    match button {
        2 => 3,
        3 => 2,
        other => other,
    }
}

fn pointer_button_mask(button: u32) -> u32 {
    match button {
        1..=5 => WPE_BUTTON1 << (button - 1),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(kind: WebInputCode) -> RawInput {
        RawInput {
            surface_id: 7,
            kind,
            ..Default::default()
        }
    }

    #[test]
    fn translates_keyboard_modifiers_to_libwpe_bits() {
        let mut raw = input(WebInputCode::KeyDown);
        raw.timestamp_ms = u64::from(u32::MAX) + 3;
        raw.keysym = 0x61;
        raw.hardware_keycode = 38;
        raw.modifiers = WEB_MODIFIER_SHIFT
            | WEB_MODIFIER_CONTROL
            | WEB_MODIFIER_ALT
            | WEB_MODIFIER_META
            | WEB_MODIFIER_CAPS_LOCK
            | WEB_MODIFIER_NUM_LOCK;

        assert_eq!(
            InputTranslator::new().translate(&raw),
            Ok(EngineInput::Key {
                time_ms: 2,
                keysym: 0x61,
                hardware_keycode: 38,
                pressed: true,
                repeat: false,
                modifiers: WPE_CONTROL | WPE_SHIFT | WPE_ALT | WPE_META,
            })
        );
    }

    #[test]
    fn pointer_coordinates_round_and_button_state_is_explicit() {
        let mut raw = input(WebInputCode::PointerUp);
        raw.x = 10.6;
        raw.y = -2.6;
        raw.button = 8;
        assert_eq!(
            InputTranslator::new().translate(&raw),
            Ok(EngineInput::PointerButton {
                time_ms: 0,
                x: 11,
                y: -3,
                button: 8,
                pressed: false,
                modifiers: 0,
            })
        );
    }

    #[test]
    fn rejects_non_finite_coordinates_for_every_input_kind() {
        let mut raw = input(WebInputCode::FocusIn);
        raw.delta_y = f32::NAN;
        assert_eq!(
            InputTranslator::new().translate(&raw),
            Err(InputError::NonFinite { field: "delta_y" })
        );
    }

    #[test]
    fn rejects_unspecified_kinds_unknown_modifiers_and_button_zero() {
        assert_eq!(
            InputTranslator::new().translate(&input(WebInputCode::KindUnspecified)),
            Err(InputError::UnspecifiedKind)
        );
        let mut unknown = input(WebInputCode::FocusOut);
        unknown.modifiers = 1 << 31;
        assert_eq!(
            InputTranslator::new().translate(&unknown),
            Err(InputError::UnknownModifiers(1 << 31))
        );
        assert_eq!(
            InputTranslator::new().translate(&input(WebInputCode::PointerDown)),
            Err(InputError::MissingButton)
        );
        let mut invalid_repeat = input(WebInputCode::KeyUp);
        invalid_repeat.repeat = true;
        assert_eq!(
            InputTranslator::new().translate(&invalid_repeat),
            Err(InputError::InvalidRepeat)
        );
    }

    #[test]
    fn middle_and_right_buttons_use_the_legacy_wpe_numbering() {
        for (wire, engine) in [(1, 1), (2, 3), (3, 2), (8, 8)] {
            let mut raw = input(WebInputCode::PointerDown);
            raw.button = wire;
            assert!(matches!(
                InputTranslator::new().translate(&raw),
                Ok(EngineInput::PointerButton { button, .. }) if button == engine
            ));
        }
    }

    #[test]
    fn held_buttons_survive_motion_and_release_independently() {
        let mut translator = InputTranslator::new();
        let mut left = input(WebInputCode::PointerDown);
        left.button = 1;
        let mut right = left.clone();
        right.button = 3;
        assert!(matches!(
            translator.translate(&left),
            Ok(EngineInput::PointerButton {
                modifiers: WPE_BUTTON1,
                ..
            })
        ));
        translator.translate(&right).unwrap();
        let mut motion = input(WebInputCode::PointerMove);
        motion.modifiers = WEB_MODIFIER_SHIFT;
        assert!(matches!(
            translator.translate(&motion),
            Ok(EngineInput::PointerMotion { modifiers, .. })
                if modifiers == WPE_SHIFT | WPE_BUTTON1 | (WPE_BUTTON1 << 1)
        ));
        left.kind = WebInputCode::PointerUp;
        assert!(matches!(
            translator.translate(&left),
            Ok(EngineInput::PointerButton { modifiers, .. }) if modifiers == WPE_BUTTON1 << 1
        ));
        right.kind = WebInputCode::PointerUp;
        translator.translate(&right).unwrap();
        assert!(matches!(
            translator.translate(&input(WebInputCode::PointerMove)),
            Ok(EngineInput::PointerMotion { modifiers: 0, .. })
        ));
    }

    #[test]
    fn focus_loss_clears_buttons_and_invalid_transitions_do_not_change_them() {
        let mut translator = InputTranslator::new();
        let mut down = input(WebInputCode::PointerDown);
        down.button = 1;
        down.x = f32::MAX;
        assert!(matches!(
            translator.translate(&down),
            Err(InputError::CoordinateOutOfRange { field: "x", .. })
        ));
        assert!(matches!(
            translator.translate(&input(WebInputCode::PointerMove)),
            Ok(EngineInput::PointerMotion { modifiers: 0, .. })
        ));
        down.x = 0.0;
        translator.translate(&down).unwrap();
        translator.translate(&input(WebInputCode::FocusOut)).unwrap();
        assert!(matches!(
            translator.translate(&input(WebInputCode::PointerMove)),
            Ok(EngineInput::PointerMotion { modifiers: 0, .. })
        ));
    }

    #[test]
    fn scroll_preserves_fractional_deltas_units_and_held_buttons() {
        let mut translator = InputTranslator::new();
        let mut down = input(WebInputCode::PointerDown);
        down.button = 1;
        translator.translate(&down).unwrap();
        let mut scroll = input(WebInputCode::Scroll);
        scroll.delta_x = 0.25;
        scroll.delta_y = -1.5;
        for precise in [false, true] {
            scroll.precise = precise;
            assert_eq!(
                translator.translate(&scroll),
                Ok(EngineInput::Axis {
                    time_ms: 0,
                    x: 0,
                    y: 0,
                    delta_x: 0.25,
                    delta_y: -1.5,
                    precise,
                    modifiers: WPE_BUTTON1,
                })
            );
        }
    }

    #[test]
    fn signed_pixel_boundary_is_checked_before_casting() {
        assert_eq!(pixel(i32::MIN as f32, "x"), Ok(i32::MIN));
        // i32::MAX rounds up when represented as f32; accepting it saturates.
        assert!(matches!(
            pixel(i32::MAX as f32, "x"),
            Err(InputError::CoordinateOutOfRange { .. })
        ));
        assert_eq!(pixel(2_147_483_520.0, "x"), Ok(2_147_483_520));
    }
}
