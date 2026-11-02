//! Engine-independent one-surface protocol state machine.
//!
//! The transport and WPE adapter are intentionally dumb consumers of the
//! actions produced here. All untrusted wire fields are validated before an
//! engine call can observe them.

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crikey_native_protocol::message::{
    Envelope, ImeEvent, OpenSurface, Payload, WebImeCode, WebStorageMode, MAX_WEB_FRAME_BYTES,
    MAX_WEB_FRAME_EDGE,
};

use crate::input::{EngineInput, InputError, InputTranslator};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSpec {
    pub surface_id: u64,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub url: String,
    pub storage_mode: WebStorageMode,
    pub storage_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HostAction {
    Open(OpenSpec),
    Navigate(String),
    Resize { pixel_width: u32, pixel_height: u32 },
    Input(EngineInput),
    Ime(ImeEvent),
    Close { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    #[error("the first web-host command must be OpenSurface, not {0}")]
    FirstCommand(&'static str),
    #[error("the launcher sent an empty protocol envelope")]
    EmptyEnvelope,
    #[error("{0} travels from the web host to the launcher, not in this direction")]
    WrongDirection(&'static str),
    #[error("a second OpenSurface was sent to a one-surface host")]
    DuplicateOpen,
    #[error("surface id zero is reserved")]
    ZeroSurfaceId,
    #[error("command names surface {actual}, but this host owns surface {expected}")]
    WrongSurface { expected: u64, actual: u64 },
    #[error("the surface is already closed")]
    AlreadyClosed,
    #[error("surface dimensions {width}x{height} are outside the web-frame bounds")]
    InvalidDimensions { width: u32, height: u32 },
    #[error("web URL must be an absolute http or https URL")]
    InvalidUrl,
    #[error("web URL contains a control character")]
    UrlControlCharacter,
    #[error("persistent storage was requested without a storage directory")]
    MissingStorageDirectory,
    #[error("storage directory must be an absolute path without NUL bytes")]
    InvalidStorageDirectory,
    #[error("IME event kind is unspecified")]
    UnspecifiedImeKind,
    #[error("IME cursor range is inconsistent with its text")]
    InvalidImeCursor,
    #[error("IME text contains a NUL byte")]
    ImeNul,
    #[error(transparent)]
    Input(#[from] InputError),
}

#[derive(Debug)]
pub struct HostState {
    storage_dir: Option<PathBuf>,
    surface_id: Option<u64>,
    closed: bool,
    input: InputTranslator,
}

impl HostState {
    pub fn new(storage_dir: Option<PathBuf>) -> Result<Self, StateError> {
        if let Some(path) = storage_dir.as_deref() {
            validate_storage_dir(path)?;
        }
        Ok(Self {
            storage_dir,
            surface_id: None,
            closed: false,
            input: InputTranslator::new(),
        })
    }

    pub fn surface_id(&self) -> Option<u64> {
        self.surface_id
    }

    pub fn receive(&mut self, envelope: Envelope) -> Result<HostAction, StateError> {
        let payload = envelope.payload.ok_or(StateError::EmptyEnvelope)?;
        if self.surface_id.is_none() {
            return match payload {
                Payload::OpenSurface(open) => self.open(open),
                other => Err(StateError::FirstCommand(other.kind())),
            };
        }
        if self.closed {
            return Err(StateError::AlreadyClosed);
        }

        match payload {
            Payload::OpenSurface(_) => Err(StateError::DuplicateOpen),
            Payload::Navigate(navigate) => {
                self.check_surface(navigate.surface_id)?;
                validate_url(&navigate.url)?;
                Ok(HostAction::Navigate(navigate.url))
            }
            Payload::Resize(resize) => {
                self.check_surface(resize.surface_id)?;
                validate_dimensions(resize.pixel_width, resize.pixel_height)?;
                Ok(HostAction::Resize {
                    pixel_width: resize.pixel_width,
                    pixel_height: resize.pixel_height,
                })
            }
            Payload::RawInput(input) => {
                self.check_surface(input.surface_id)?;
                Ok(HostAction::Input(self.input.translate(&input)?))
            }
            Payload::ImeEvent(event) => {
                self.check_surface(event.surface_id)?;
                validate_ime(&event)?;
                Ok(HostAction::Ime(event))
            }
            Payload::CloseSurface(close) => {
                self.check_surface(close.surface_id)?;
                self.closed = true;
                Ok(HostAction::Close { reason: close.reason })
            }
            other => Err(StateError::WrongDirection(other.kind())),
        }
    }

    fn open(&mut self, open: OpenSurface) -> Result<HostAction, StateError> {
        if open.surface_id == 0 {
            return Err(StateError::ZeroSurfaceId);
        }
        validate_dimensions(open.pixel_width, open.pixel_height)?;
        validate_url(&open.url)?;
        let (storage_mode, storage_dir) = match open.storage_mode {
            WebStorageMode::Ephemeral | WebStorageMode::ModeUnspecified => (WebStorageMode::Ephemeral, None),
            WebStorageMode::Persistent => (
                WebStorageMode::Persistent,
                Some(
                    self.storage_dir
                        .clone()
                        .ok_or(StateError::MissingStorageDirectory)?,
                ),
            ),
        };
        self.surface_id = Some(open.surface_id);
        Ok(HostAction::Open(OpenSpec {
            surface_id: open.surface_id,
            pixel_width: open.pixel_width,
            pixel_height: open.pixel_height,
            url: open.url,
            storage_mode,
            storage_dir,
        }))
    }

    fn check_surface(&self, actual: u64) -> Result<(), StateError> {
        let expected = self.surface_id.expect("surface checked before commands");
        if actual == 0 || actual != expected {
            Err(StateError::WrongSurface { expected, actual })
        } else {
            Ok(())
        }
    }
}

pub fn validate_storage_dir(path: &Path) -> Result<(), StateError> {
    if !path.is_absolute() || path.as_os_str().is_empty() || path_has_nul(path) {
        Err(StateError::InvalidStorageDirectory)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn path_has_nul(path: &Path) -> bool {
    path.as_os_str().as_bytes().contains(&0)
}

#[cfg(not(unix))]
fn path_has_nul(path: &Path) -> bool {
    path.to_string_lossy().contains('\0')
}

fn validate_dimensions(width: u32, height: u32) -> Result<(), StateError> {
    let bytes = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4));
    if width == 0
        || height == 0
        || width > MAX_WEB_FRAME_EDGE
        || height > MAX_WEB_FRAME_EDGE
        || bytes.is_none_or(|bytes| bytes > MAX_WEB_FRAME_BYTES)
    {
        Err(StateError::InvalidDimensions { width, height })
    } else {
        Ok(())
    }
}

fn validate_url(url: &str) -> Result<(), StateError> {
    if url.chars().any(char::is_control) {
        return Err(StateError::UrlControlCharacter);
    }
    let Some((scheme, remainder)) = url.split_once(':') else {
        return Err(StateError::InvalidUrl);
    };
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        || !remainder.starts_with("//")
    {
        return Err(StateError::InvalidUrl);
    }
    let authority = remainder[2..].split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.chars().any(char::is_whitespace) {
        return Err(StateError::InvalidUrl);
    }
    Ok(())
}

fn validate_ime(event: &ImeEvent) -> Result<(), StateError> {
    if event.text.contains('\0') {
        return Err(StateError::ImeNul);
    }
    match event.kind {
        WebImeCode::Preedit => {
            let chars = u32::try_from(event.text.chars().count()).unwrap_or(u32::MAX);
            if event.has_cursor {
                if event.cursor_begin_chars > event.cursor_end_chars || event.cursor_end_chars > chars {
                    return Err(StateError::InvalidImeCursor);
                }
            } else if event.cursor_begin_chars != 0 || event.cursor_end_chars != 0 {
                return Err(StateError::InvalidImeCursor);
            }
            Ok(())
        }
        WebImeCode::Commit => {
            if event.has_cursor || event.cursor_begin_chars != 0 || event.cursor_end_chars != 0 {
                Err(StateError::InvalidImeCursor)
            } else {
                Ok(())
            }
        }
        WebImeCode::KindUnspecified => Err(StateError::UnspecifiedImeKind),
    }
}

#[cfg(test)]
mod tests {
    use crikey_native_protocol::message::{
        CloseSurface, Gone, Navigate, RawInput, Resize, WebGoneCode, WebInputCode,
    };

    use super::*;

    fn envelope(payload: Payload) -> Envelope {
        Envelope {
            payload: Some(payload),
            ..Default::default()
        }
    }

    fn open(id: u64) -> Envelope {
        envelope(Payload::OpenSurface(OpenSurface {
            surface_id: id,
            pixel_width: 696,
            pixel_height: 410,
            url: "https://example.test/page".to_owned(),
            storage_mode: WebStorageMode::Ephemeral,
            ..Default::default()
        }))
    }

    #[test]
    fn first_command_opens_exactly_one_valid_surface() {
        let mut state = HostState::new(None).unwrap();
        assert!(matches!(state.receive(open(42)), Ok(HostAction::Open(_))));
        assert_eq!(state.surface_id(), Some(42));
        assert_eq!(state.receive(open(43)), Err(StateError::DuplicateOpen));
    }

    #[test]
    fn requires_open_first_and_rejects_output_direction() {
        let mut state = HostState::new(None).unwrap();
        assert_eq!(
            state.receive(envelope(Payload::Navigate(Navigate {
                surface_id: 3,
                url: "https://example.test".to_owned(),
                ..Default::default()
            }))),
            Err(StateError::FirstCommand("navigate"))
        );
        state.receive(open(3)).unwrap();
        assert_eq!(
            state.receive(envelope(Payload::Gone(Gone {
                surface_id: 3,
                reason: WebGoneCode::Closed,
                ..Default::default()
            }))),
            Err(StateError::WrongDirection("gone"))
        );
    }

    #[test]
    fn validates_ids_urls_dimensions_and_storage_before_actions() {
        let mut zero = HostState::new(None).unwrap();
        assert_eq!(zero.receive(open(0)), Err(StateError::ZeroSurfaceId));

        let mut persistent = HostState::new(None).unwrap();
        let mut request = match open(1).payload.unwrap() {
            Payload::OpenSurface(request) => request,
            _ => unreachable!(),
        };
        request.storage_mode = WebStorageMode::Persistent;
        assert_eq!(
            persistent.receive(envelope(Payload::OpenSurface(request))),
            Err(StateError::MissingStorageDirectory)
        );

        let mut state = HostState::new(None).unwrap();
        state.receive(open(7)).unwrap();
        assert!(matches!(
            state.receive(envelope(Payload::Navigate(Navigate {
                surface_id: 7,
                url: "file:///etc/passwd".to_owned(),
                ..Default::default()
            }))),
            Err(StateError::InvalidUrl)
        ));
        assert_eq!(
            state.receive(envelope(Payload::Resize(Resize {
                surface_id: 8,
                pixel_width: 1,
                pixel_height: 1,
                ..Default::default()
            }))),
            Err(StateError::WrongSurface {
                expected: 7,
                actual: 8
            })
        );
        assert!(matches!(
            state.receive(envelope(Payload::Resize(Resize {
                surface_id: 7,
                pixel_width: 4096,
                pixel_height: 4096,
                ..Default::default()
            }))),
            Err(StateError::InvalidDimensions { .. })
        ));
    }

    #[test]
    fn validates_input_and_ime_enums_and_finite_values() {
        let mut state = HostState::new(None).unwrap();
        state.receive(open(7)).unwrap();
        assert!(matches!(
            state.receive(envelope(Payload::RawInput(RawInput {
                surface_id: 7,
                kind: WebInputCode::PointerMove,
                x: f32::INFINITY,
                ..Default::default()
            }))),
            Err(StateError::Input(InputError::NonFinite { field: "x" }))
        ));
        assert_eq!(
            state.receive(envelope(Payload::ImeEvent(ImeEvent {
                surface_id: 7,
                kind: WebImeCode::KindUnspecified,
                ..Default::default()
            }))),
            Err(StateError::UnspecifiedImeKind)
        );
    }

    #[test]
    fn close_is_a_terminal_transition() {
        let mut state = HostState::new(None).unwrap();
        state.receive(open(9)).unwrap();
        assert_eq!(
            state.receive(envelope(Payload::CloseSurface(CloseSurface {
                surface_id: 9,
                reason: "page closed".to_owned(),
                ..Default::default()
            }))),
            Ok(HostAction::Close {
                reason: "page closed".to_owned()
            })
        );
        assert_eq!(state.receive(open(9)), Err(StateError::AlreadyClosed));
    }

    #[test]
    fn unspecified_storage_is_ephemeral_even_with_a_persistent_directory() {
        let directory = std::env::temp_dir().join("crikey-web-host-storage");
        for configured in [None, Some(directory)] {
            let mut state = HostState::new(configured).unwrap();
            let Payload::OpenSurface(mut request) = open(7).payload.unwrap() else {
                unreachable!()
            };
            request.storage_mode = WebStorageMode::ModeUnspecified;
            let HostAction::Open(spec) = state.receive(envelope(Payload::OpenSurface(request))).unwrap()
            else {
                unreachable!()
            };
            assert_eq!(spec.storage_mode, WebStorageMode::Ephemeral);
            assert_eq!(spec.storage_dir, None);
        }
    }

    #[test]
    fn persistent_storage_uses_only_the_configured_absolute_directory() {
        assert!(matches!(
            HostState::new(Some(PathBuf::from("relative/storage"))),
            Err(StateError::InvalidStorageDirectory)
        ));
        let directory = std::env::temp_dir().join("crikey-web-host-storage");
        let mut state = HostState::new(Some(directory.clone())).unwrap();
        let Payload::OpenSurface(mut request) = open(7).payload.unwrap() else {
            unreachable!()
        };
        request.storage_mode = WebStorageMode::Persistent;
        let HostAction::Open(spec) = state.receive(envelope(Payload::OpenSurface(request))).unwrap() else {
            unreachable!()
        };
        assert_eq!(spec.storage_mode, WebStorageMode::Persistent);
        assert_eq!(spec.storage_dir, Some(directory));
    }

    #[cfg(unix)]
    #[test]
    fn storage_directory_rejects_embedded_nul_before_ffi() {
        let directory = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/storage\0other"));
        assert_eq!(
            validate_storage_dir(directory),
            Err(StateError::InvalidStorageDirectory)
        );
    }

    #[test]
    fn rejected_open_does_not_claim_a_surface_and_valid_open_can_follow() {
        let mut state = HostState::new(None).unwrap();
        let Payload::OpenSurface(mut request) = open(7).payload.unwrap() else {
            unreachable!()
        };
        request.url = "https:///missing-host".to_owned();
        assert_eq!(
            state.receive(envelope(Payload::OpenSurface(request))),
            Err(StateError::InvalidUrl)
        );
        assert_eq!(state.surface_id(), None);
        assert!(matches!(state.receive(open(8)), Ok(HostAction::Open(_))));
        assert_eq!(state.surface_id(), Some(8));
    }

    #[test]
    fn dimensions_accept_the_exact_byte_limit_but_not_the_next_row() {
        assert_eq!(validate_dimensions(1024, 512), Ok(()));
        assert_eq!(
            validate_dimensions(1024, 513),
            Err(StateError::InvalidDimensions {
                width: 1024,
                height: 513
            })
        );
        assert!(validate_dimensions(u32::MAX, u32::MAX).is_err());
    }

    #[test]
    fn ime_cursor_validation_counts_characters_not_utf8_bytes() {
        let mut event = ImeEvent {
            kind: WebImeCode::Preedit,
            text: "\u{4f60}\u{20bb7}".to_owned(),
            has_cursor: true,
            cursor_begin_chars: 1,
            cursor_end_chars: 2,
            ..Default::default()
        };
        assert_eq!(validate_ime(&event), Ok(()));
        event.cursor_end_chars = 3;
        assert_eq!(validate_ime(&event), Err(StateError::InvalidImeCursor));
        event.cursor_begin_chars = 2;
        event.cursor_end_chars = 1;
        assert_eq!(validate_ime(&event), Err(StateError::InvalidImeCursor));
        event.has_cursor = false;
        assert_eq!(validate_ime(&event), Err(StateError::InvalidImeCursor));
        event.cursor_begin_chars = 0;
        event.cursor_end_chars = 0;
        event.kind = WebImeCode::Commit;
        assert_eq!(validate_ime(&event), Ok(()));
        event.has_cursor = true;
        assert_eq!(validate_ime(&event), Err(StateError::InvalidImeCursor));
        event.has_cursor = false;
        event.text.push('\0');
        assert_eq!(validate_ime(&event), Err(StateError::ImeNul));
    }

    #[test]
    fn navigation_uses_the_same_absolute_http_url_boundary_as_open() {
        let mut state = HostState::new(None).unwrap();
        state.receive(open(7)).unwrap();
        for url in [
            "",
            "https:relative",
            "https:///path",
            "https://bad host/",
            "file:///tmp/page",
        ] {
            assert_eq!(
                state.receive(envelope(Payload::Navigate(Navigate {
                    surface_id: 7,
                    url: url.to_owned(),
                    ..Default::default()
                }))),
                Err(StateError::InvalidUrl),
                "{url:?}"
            );
        }
        assert_eq!(
            validate_url("https://example.test/\n"),
            Err(StateError::UrlControlCharacter)
        );
        assert_eq!(validate_url("HTTPS://example.test/path?q=1#fragment"), Ok(()));
    }
}
