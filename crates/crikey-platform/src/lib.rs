//! Platform service interfaces (spec 18).
//!
//! Platform-independent crates depend on these traits only; concrete desktop
//! APIs live in the per-OS backend crates.
//!
//! Four pieces of behaviour are shared by every backend and therefore belong
//! to none of them: [`Accelerator`], which parses the configurable activation
//! shortcut (spec 6.1); [`encode_target`] and [`decode_target`], which carry a
//! native path through the `String` field of a catalog item without losing a
//! unit (spec 18.3, ADR-0007); [`application_items`], which maps discovered
//! applications onto catalog items (spec 10.2, 10.3); and the [`icon`] module's
//! decoding, size limits and cache, which every backend needs and none of them
//! can test on its own target (spec 6.4, 11.7, 22.1). All are pure functions
//! over data or plain filesystem work: no window system, no key grab, no
//! desktop API.

use std::{collections::BTreeMap, fmt};

use crikey_core::{
    Action, ActionId, ArgumentPolicy, Category, CoreError, ExecutionPolicy, HitPolicy, Item, ItemId,
    PlatformPath, PluginId, Result,
};

mod bundle;
mod directories;
pub mod icon;
pub mod window;

pub use bundle::{bundle_display_name, bundle_icon_path, parse_info_plist, AppBundle};
pub use directories::{DirectoryConvention, DirectoryEnvironment, PluginKind, StandardDirectories};
pub use icon::{
    decode_icon, IconCache, IconCacheKey, IconError, IconFormat, IconImage, IconLoader, IconProvider,
    IconSource, PathIconSource, SourceFingerprint, DEFAULT_ICON_SIZE, ICON_CACHE_SCHEMA_VERSION,
    MAX_ICON_EDGE, MAX_ICON_PAYLOAD_BYTES, MAX_ICON_PIXELS,
};
pub use window::{WindowHandle, WindowInfo, WindowService};

/// Optional platform capabilities and their availability (spec 18.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    ApplicationDiscovery,
    FileSearch,
    Clipboard,
    GlobalHotkeys,
    ProcessLaunch,
    UriOpen,
    WindowEnumeration,
    WindowActivation,
    Notifications,
    Icons,
    FileWatching,
    SecretStorage,
    ShellIntegration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityState {
    Available,
    Unavailable,
    PermissionGated,
    Partial,
    UnsupportedDesktopEnvironment,
}

#[derive(Debug, Clone)]
pub struct DiscoveredApplication {
    pub name: String,
    pub target: PlatformPath,
    pub arguments: Vec<String>,
    pub icon_reference: Option<String>,
    /// Platform native identifier, e.g. a Windows AppUserModelID or a Linux
    /// desktop-entry id.
    pub platform_id: Option<String>,
    /// Directory the process should start in when the platform records one.
    /// `None` means inherit the launcher's working directory.
    pub working_directory: Option<PlatformPath>,
}

pub trait ApplicationDiscovery {
    fn discover(&self) -> Result<Vec<DiscoveredApplication>>;
}

// ---------------------------------------------------------------------------
// Discovered applications as catalog items (spec 10.2, 10.3, 18.3)
// ---------------------------------------------------------------------------

/// Host-mediated action attached to every discovered application item.
pub const APPLICATION_LAUNCH_ACTION_ID: &str = "crikey.application.launch";

const MAX_APPLICATION_ARGUMENTS: usize = 4_096;

/// Metadata key holding how many launch arguments an item records.
const ARGUMENT_COUNT_KEY: &str = "application.argument.count";

/// Metadata key holding the encoded working directory, when one was recorded.
pub const APPLICATION_WORKING_DIRECTORY_KEY: &str = "application.working_directory";

/// Prefix of the per-index launch-argument metadata keys.
///
/// Arguments are recorded one per key rather than as a joined command line, so
/// an argument may itself contain spaces or be empty: [`ProcessLauncher::launch`]
/// takes a slice and never re-splits.
const ARGUMENT_KEY_PREFIX: &str = "application.argument.";

/// Maps discovered applications onto catalog items owned by `plugin`.
///
/// One item per discovery, in discovery order: deduplication is the
/// discoverer's job. Identity is derived from the owning plugin, the category
/// and the encoded target, never from the display label, so renaming a desktop
/// entry keeps an item's recorded history (spec 10.2).
pub fn application_items(plugin: &PluginId, discovered: &[DiscoveredApplication]) -> Vec<Item> {
    discovered
        .iter()
        .map(|application| {
            let target = encode_target(&application.target);
            Item {
                stable_id: ItemId::derived(plugin, &Category::Application, &target),
                plugin_id: plugin.clone(),
                category: Category::Application,
                label: application.name.clone(),
                description: String::new(),
                target,
                search_terms: vec![application.name.clone()],
                icon_reference: application.icon_reference.clone(),
                argument_policy: ArgumentPolicy::Forbidden,
                metadata: argument_metadata(&application.arguments, application.working_directory.as_ref()),
                hit_policy: HitPolicy::Recorded,
                score_hint: 0,
                actions: vec![Action {
                    action_id: ActionId(APPLICATION_LAUNCH_ACTION_ID.to_owned()),
                    label: "Launch".to_owned(),
                    description: "Open this application".to_owned(),
                    applicable_categories: vec![Category::Application],
                    icon_reference: None,
                    execution_policy: ExecutionPolicy::HostMediated,
                }],
            }
        })
        .collect()
}

/// Records launch arguments and an optional encoded working directory.
fn argument_metadata(
    arguments: &[String],
    working_directory: Option<&PlatformPath>,
) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    metadata.insert(ARGUMENT_COUNT_KEY.to_owned(), arguments.len().to_string());
    for (index, argument) in arguments.iter().enumerate() {
        metadata.insert(format!("{ARGUMENT_KEY_PREFIX}{index}"), argument.clone());
    }
    if let Some(working_directory) = working_directory {
        metadata.insert(
            APPLICATION_WORKING_DIRECTORY_KEY.to_owned(),
            encode_target(working_directory),
        );
    }
    metadata
}

/// Rebuilds the exact argument vector recorded by [`application_items`].
///
/// Malformed or hostile metadata is rejected instead of being re-split or
/// partially executed. Empty arguments and arguments containing spaces remain
/// distinct values.
pub fn application_arguments(item: &Item) -> Result<Vec<String>> {
    let count = item
        .metadata
        .get(ARGUMENT_COUNT_KEY)
        .ok_or_else(|| CoreError::Invalid("application item has no argument count".to_owned()))?
        .parse::<usize>()
        .map_err(|_| CoreError::Invalid("application argument count is not a whole number".to_owned()))?;
    if count > MAX_APPLICATION_ARGUMENTS {
        return Err(CoreError::CapacityExceeded("application launch arguments"));
    }

    let mut arguments = Vec::with_capacity(count);
    for index in 0..count {
        let key = format!("{ARGUMENT_KEY_PREFIX}{index}");
        let argument = item
            .metadata
            .get(&key)
            .ok_or_else(|| CoreError::Invalid(format!("application item is missing argument {index}")))?;
        arguments.push(argument.clone());
    }
    Ok(arguments)
}
/// Rebuilds the optional working directory recorded by [`application_items`].
///
/// Missing metadata means the launcher inherits its own working directory.
/// Present metadata is decoded losslessly and malformed values are rejected
/// rather than silently changing where an application starts.
pub fn application_working_directory(item: &Item) -> Result<Option<PlatformPath>> {
    let Some(encoded) = item.metadata.get(APPLICATION_WORKING_DIRECTORY_KEY) else {
        return Ok(None);
    };
    decode_target(encoded)
        .map(Some)
        .map_err(|error| CoreError::Invalid(format!("application working directory is invalid: {error}")))
}

// ---------------------------------------------------------------------------
// Launch targets (spec 18.3, ADR-0007)
// ---------------------------------------------------------------------------

/// Tag introducing a Unix-origin body, whose escapes name raw filesystem bytes.
const UNIX_TAG: &str = "%unix;";

/// Tag introducing a Windows-origin body, whose escapes name UTF-16 code units.
const WINDOWS_TAG: &str = "%windows;";

/// Every tag the encoding defines, so a target written on the other platform is
/// rejected by name instead of being read as this platform's units.
const TAGS: [&str; 2] = [UNIX_TAG, WINDOWS_TAG];

/// The escape for a literal `%`: the one escape every body may carry.
const PERCENT_ESCAPE: &str = "%25";

/// Escape digits are uppercase, so one path has exactly one encoding.
const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

#[cfg(not(any(unix, windows)))]
compile_error!("a lossless launch target needs this platform's path unit type (ADR-0007)");

/// Encodes a launch target so that a `String` item field carries it losslessly.
///
/// A native path is an arbitrary byte string on Unix and potentially ill-formed
/// UTF-16 on Windows, so a lossy conversion would both corrupt the execution
/// payload and collapse distinct applications onto one identity (ADR-0007).
/// The encoding keeps every unit instead:
///
/// * a target that is valid UTF-8 is kept verbatim, except `%`, which becomes
///   `%25`. Such a target names the same path on either platform, so it carries
///   no tag and stays readable wherever a target is printed;
/// * a target that is not valid UTF-8 carries the tag of the platform it came
///   from and escapes the units UTF-8 cannot spell: `%unix;` with one `%XX` per
///   raw byte, or `%windows;` with one `%uXXXX` per unpaired surrogate. The tag
///   is what makes a Windows-origin target distinguishable from a Unix-origin
///   one, so [`decode_target`] refuses the foreign one instead of reconstructing
///   a path from units it cannot read.
///
/// `%` therefore always introduces an escape, which makes the encoding
/// invertible and injective: distinct paths never share a target, an encoded
/// target never contains a replacement character the path did not contain, and
/// the result depends on nothing but the units of the path.
pub fn encode_target(target: &PlatformPath) -> String {
    let target = target.as_os_str();
    let Some(text) = target.to_str() else {
        return platform_target::encode(target);
    };

    // The overwhelmingly common case: UTF-8 with nothing to escape.
    if !text.contains('%') {
        return text.to_owned();
    }

    let mut encoded = String::with_capacity(text.len() + PERCENT_ESCAPE.len());
    push_escaped(&mut encoded, text);
    encoded
}

/// Reconstructs the path [`encode_target`] was given, or reports why it cannot.
///
/// `decode_target(&encode_target(path))` is `path` for every path this platform
/// can name. Everything else is a typed rejection: a target this build cannot
/// reconstruct exactly must never decay into a path that merely resembles it,
/// which is the silent corruption ADR-0007 exists to prevent.
pub fn decode_target(encoded: &str) -> Result<PlatformPath, TargetError> {
    for tag in TAGS {
        let Some(body) = encoded.strip_prefix(tag) else {
            continue;
        };
        return if tag == platform_target::TAG {
            platform_target::decode(body, tag.len())
        } else {
            Err(TargetError::ForeignPlatform { tag })
        };
    }
    decode_untagged(encoded)
}

/// Decodes an untagged body: valid UTF-8 whose only escape is `%25`.
fn decode_untagged(body: &str) -> Result<PlatformPath, TargetError> {
    if !body.contains('%') {
        return Ok(PlatformPath::new(body));
    }

    let mut text = String::with_capacity(body.len());
    let mut rest = body;
    let mut consumed = 0;
    while let Some(index) = rest.find('%') {
        text.push_str(&rest[..index]);
        let escape = &rest[index..];
        let offset = consumed + index;
        if !escape.starts_with(PERCENT_ESCAPE) {
            return Err(untagged_escape_error(escape, offset));
        }
        text.push('%');
        rest = &escape[PERCENT_ESCAPE.len()..];
        consumed = offset + PERCENT_ESCAPE.len();
    }
    text.push_str(rest);
    Ok(PlatformPath::new(text))
}

/// Why a `%` that introduces no `%25` ends an untagged body: an escape shaped
/// like a platform one means the tag that gives it meaning was dropped, and
/// reading it as text would silently corrupt the path.
fn untagged_escape_error(escape: &str, offset: usize) -> TargetError {
    if looks_like_platform_escape(&escape.as_bytes()[1..]) {
        TargetError::MissingPlatformTag { offset }
    } else {
        TargetError::MalformedEscape { offset }
    }
}

/// Whether the text after a `%` is shaped like an escape that only a platform
/// tag gives meaning: `XX` for a Unix byte, `uXXXX` for a Windows code unit.
fn looks_like_platform_escape(after: &[u8]) -> bool {
    match after {
        [b'u', digits @ ..] => opens_with_hex(digits, 4),
        digits => opens_with_hex(digits, 2),
    }
}

/// Whether `digits` opens with `count` uppercase hex digits.
fn opens_with_hex(digits: &[u8], count: usize) -> bool {
    digits
        .get(..count)
        .is_some_and(|digits| digits.iter().all(|digit| hex_digit(*digit).is_some()))
}

/// The value of one uppercase hex digit.
///
/// A lowercase spelling is not a digit: one path has one encoding, so an
/// identity derived from an encoded target is stable whoever wrote the target.
fn hex_digit(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// Appends UTF-8 text, escaping the `%` that introduces every escape.
fn push_escaped(encoded: &mut String, text: &str) {
    let mut rest = text;
    while let Some(index) = rest.find('%') {
        encoded.push_str(&rest[..index]);
        encoded.push_str(PERCENT_ESCAPE);
        rest = &rest[index + '%'.len_utf8()..];
    }
    encoded.push_str(rest);
}

/// Why an encoded launch target names no path on this platform (ADR-0007).
///
/// Every rejection is loud: a target this build cannot reconstruct exactly must
/// never decay into a path that merely resembles it, because a launcher that
/// runs the wrong path is worse than one that reports a broken catalog entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetError {
    /// The target carries the other platform's encoding tag: its escapes name
    /// units this build cannot reconstruct, so it belongs to a catalog written
    /// elsewhere.
    ForeignPlatform { tag: &'static str },
    /// The `%` at this byte offset introduces no complete escape.
    MalformedEscape { offset: usize },
    /// The escape at this byte offset is shaped like a platform escape, but the
    /// target carries no tag saying which platform's unit it names.
    MissingPlatformTag { offset: usize },
}

impl fmt::Display for TargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignPlatform { tag } => write!(
                formatter,
                "the target is tagged {tag:?}, an encoding this build cannot reconstruct"
            ),
            Self::MalformedEscape { offset } => {
                write!(formatter, "the % at byte {offset} introduces no valid escape")
            }
            Self::MissingPlatformTag { offset } => write!(
                formatter,
                "the % at byte {offset} is shaped like a platform escape, but the target has no tag"
            ),
        }
    }
}

impl std::error::Error for TargetError {}

/// Unix-origin targets: a path is a byte string, so an escape names one raw
/// byte and the safe `OsStringExt::from_vec` puts the bytes back. No `unsafe`
/// is involved anywhere in the round trip.
#[cfg(unix)]
mod platform_target {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    use crikey_core::PlatformPath;

    use super::{hex_digit, push_escaped, TargetError, HEX_DIGITS, UNIX_TAG};

    pub(super) const TAG: &str = UNIX_TAG;

    /// `%` and two hex digits.
    const ESCAPE_LEN: usize = 3;

    /// The lowest byte no single-byte UTF-8 sequence covers, and therefore the
    /// lowest byte an escape may name apart from `%` itself.
    const FIRST_ESCAPED_BYTE: u8 = 0x80;

    /// Escapes every byte no UTF-8 sequence claims; the rest stays readable.
    pub(super) fn encode(target: &OsStr) -> String {
        let bytes = target.as_bytes();
        let mut encoded = String::with_capacity(TAG.len() + bytes.len() + ESCAPE_LEN);
        encoded.push_str(TAG);
        for chunk in bytes.utf8_chunks() {
            push_escaped(&mut encoded, chunk.valid());
            for byte in chunk.invalid() {
                push_byte_escape(&mut encoded, *byte);
            }
        }
        encoded
    }

    /// Appends one raw byte as `%XX`.
    fn push_byte_escape(encoded: &mut String, byte: u8) {
        encoded.push('%');
        encoded.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }

    /// Rebuilds the byte string, so the decoded path is the one discovery found.
    pub(super) fn decode(body: &str, base: usize) -> Result<PlatformPath, TargetError> {
        let mut bytes = Vec::with_capacity(body.len());
        let mut rest = body;
        let mut consumed = base;
        while let Some(index) = rest.find('%') {
            bytes.extend_from_slice(&rest.as_bytes()[..index]);
            let escape = &rest[index..];
            let offset = consumed + index;
            bytes.push(escaped_byte(escape, offset)?);
            rest = &escape[ESCAPE_LEN..];
            consumed = offset + ESCAPE_LEN;
        }
        bytes.extend_from_slice(rest.as_bytes());
        Ok(PlatformPath::new(OsString::from_vec(bytes)))
    }

    /// The byte one `%XX` names.
    ///
    /// Only a byte with no verbatim spelling may be escaped: `%`, which
    /// introduces every escape, and the bytes from `0x80` up, which no
    /// single-byte UTF-8 sequence covers. Anything else would be a second
    /// spelling of a byte the encoder writes as itself.
    fn escaped_byte(escape: &str, offset: usize) -> Result<u8, TargetError> {
        let malformed = || TargetError::MalformedEscape { offset };
        let Some(&[high, low]) = escape.as_bytes().get(1..ESCAPE_LEN) else {
            return Err(malformed());
        };
        let (Some(high), Some(low)) = (hex_digit(high), hex_digit(low)) else {
            return Err(malformed());
        };

        let byte = (high << 4) | low;
        if byte < FIRST_ESCAPED_BYTE && byte != b'%' {
            return Err(malformed());
        }
        Ok(byte)
    }
}

/// Windows-origin targets: a path is a UTF-16 string that may hold unpaired
/// surrogates, so an escape names one code unit and the safe
/// `OsStringExt::from_wide` puts the units back. No `unsafe` is involved
/// anywhere in the round trip.
#[cfg(windows)]
mod platform_target {
    use std::char::decode_utf16;
    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    use crikey_core::PlatformPath;

    use super::{hex_digit, TargetError, HEX_DIGITS, PERCENT_ESCAPE, WINDOWS_TAG};

    pub(super) const TAG: &str = WINDOWS_TAG;

    /// `%u` and four hex digits.
    const ESCAPE_LEN: usize = 6;

    /// The code units UTF-8 cannot spell, and therefore the only units an
    /// escape may name: a surrogate left unpaired by the filesystem.
    const UNPAIRED_SURROGATES: std::ops::RangeInclusive<u16> = 0xD800..=0xDFFF;

    /// Escapes every code unit UTF-8 cannot spell; the rest stays readable.
    pub(super) fn encode(target: &OsStr) -> String {
        let mut encoded = String::with_capacity(TAG.len() + target.len() + ESCAPE_LEN);
        encoded.push_str(TAG);
        for unit in decode_utf16(target.encode_wide()) {
            match unit {
                Ok('%') => encoded.push_str(PERCENT_ESCAPE),
                Ok(character) => encoded.push(character),
                Err(unpaired) => push_unit_escape(&mut encoded, unpaired.unpaired_surrogate()),
            }
        }
        encoded
    }

    /// Appends one code unit as `%uXXXX`.
    fn push_unit_escape(encoded: &mut String, unit: u16) {
        encoded.push_str("%u");
        for shift in [12, 8, 4, 0] {
            let digit = usize::from((unit >> shift) & 0x0f);
            encoded.push(char::from(HEX_DIGITS[digit]));
        }
    }

    /// Rebuilds the code units, so the decoded path is the one discovery found.
    pub(super) fn decode(body: &str, base: usize) -> Result<PlatformPath, TargetError> {
        let mut units = Vec::with_capacity(body.len());
        let mut rest = body;
        let mut consumed = base;
        while let Some(index) = rest.find('%') {
            units.extend(rest[..index].encode_utf16());
            let escape = &rest[index..];
            let offset = consumed + index;
            let length = if escape.starts_with(PERCENT_ESCAPE) {
                units.push(u16::from(b'%'));
                PERCENT_ESCAPE.len()
            } else {
                units.push(escaped_unit(escape, offset)?);
                ESCAPE_LEN
            };
            rest = &escape[length..];
            consumed = offset + length;
        }
        units.extend(rest.encode_utf16());
        Ok(PlatformPath::new(OsString::from_wide(&units)))
    }

    /// The code unit one `%uXXXX` names.
    ///
    /// Only an unpaired surrogate may be escaped: every other unit is part of a
    /// character UTF-8 can spell verbatim, so escaping it would be a second
    /// spelling of one path.
    fn escaped_unit(escape: &str, offset: usize) -> Result<u16, TargetError> {
        let malformed = || TargetError::MalformedEscape { offset };
        let Some([b'u', digits @ ..]) = escape.as_bytes().get(1..ESCAPE_LEN) else {
            return Err(malformed());
        };

        let mut unit = 0u16;
        for digit in digits {
            let Some(value) = hex_digit(*digit) else {
                return Err(malformed());
            };
            unit = (unit << 4) | u16::from(value);
        }
        if !UNPAIRED_SURROGATES.contains(&unit) {
            return Err(malformed());
        }
        Ok(unit)
    }
}

pub trait Clipboard {
    fn read_text(&self) -> Result<Option<String>>;
    fn write_text(&self, text: &str) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyBinding {
    pub accelerator: String,
}

/// Callback installed by a hotkey backend to wake the launcher event loop.
///
/// It runs on a platform message thread and therefore must hand the event off
/// without doing query, rendering, or plugin work itself.
pub type HotkeyActivationHandler = Box<dyn Fn(&HotkeyBinding) + Send + Sync + 'static>;

pub trait HotkeyService {
    fn register(&mut self, binding: &HotkeyBinding) -> Result<()>;
    fn unregister(&mut self, binding: &HotkeyBinding) -> Result<()>;

    /// Replaces the handler invoked when a registered accelerator fires.
    ///
    /// Clearing the handler must leave registrations intact. Implementations
    /// invoke it on their platform message thread, so hosts should do no more
    /// than send an event through the native UI loop's wake-up mechanism.
    fn set_activation_handler(&mut self, handler: Option<HotkeyActivationHandler>);
}

// ---------------------------------------------------------------------------
// Accelerators (spec 6.1, 18.1)
// ---------------------------------------------------------------------------

/// The modifier keys an accelerator requires.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

/// A single modifier, so that parsing, duplicate detection and rendering share
/// one canonical order and one canonical spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Modifier {
    Ctrl,
    Alt,
    Shift,
    Meta,
}

impl Modifier {
    /// Canonical rendering order: `Ctrl+Alt+Shift+Meta+Key`.
    const ORDER: [Self; 4] = [Self::Ctrl, Self::Alt, Self::Shift, Self::Meta];

    const fn name(self) -> &'static str {
        match self {
            Self::Ctrl => "Ctrl",
            Self::Alt => "Alt",
            Self::Shift => "Shift",
            Self::Meta => "Meta",
        }
    }

    fn parse(component: &str) -> Option<Self> {
        Self::ORDER
            .into_iter()
            .find(|modifier| component.eq_ignore_ascii_case(modifier.name()))
    }

    fn is_set(self, modifiers: &Modifiers) -> bool {
        match self {
            Self::Ctrl => modifiers.ctrl,
            Self::Alt => modifiers.alt,
            Self::Shift => modifiers.shift,
            Self::Meta => modifiers.meta,
        }
    }

    fn set(self, modifiers: &mut Modifiers) {
        let slot = match self {
            Self::Ctrl => &mut modifiers.ctrl,
            Self::Alt => &mut modifiers.alt,
            Self::Shift => &mut modifiers.shift,
            Self::Meta => &mut modifiers.meta,
        };
        *slot = true;
    }
}

/// Why an accelerator string names no usable shortcut (spec 6.1).
///
/// Every rejection is loud: a shortcut the user meant to bind must never
/// degrade into a binding that can only ever be dead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyError {
    /// The accelerator, or one of its `+` separated components, is empty.
    EmptyComponent,
    /// Every component named a modifier; a hotkey with no key can never fire.
    MissingKey,
    /// The same modifier is named more than once.
    DuplicateModifier { modifier: &'static str },
    /// A component follows the key: an accelerator names exactly one key, last.
    TrailingComponent { component: String },
    /// A component is neither a known modifier nor a known key.
    UnknownComponent { component: String },
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyComponent => formatter.write_str("an accelerator component is empty"),
            Self::MissingKey => formatter.write_str("an accelerator names modifiers but no key"),
            Self::DuplicateModifier { modifier } => {
                write!(formatter, "the {modifier} modifier is named more than once")
            }
            Self::TrailingComponent { component } => write!(
                formatter,
                "{component:?} follows the key; an accelerator names exactly one key, last"
            ),
            Self::UnknownComponent { component } => {
                write!(formatter, "{component:?} is neither a modifier nor a known key")
            }
        }
    }
}

impl std::error::Error for HotkeyError {}

/// A parsed global-hotkey accelerator: a set of modifiers plus exactly one key.
///
/// Values are canonical by construction, so every accepted spelling of one
/// shortcut compares equal and renders identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Accelerator {
    modifiers: Modifiers,
    /// Canonical key name, always borrowed from the key tables below.
    key: &'static str,
}

impl Accelerator {
    /// Parses an accelerator such as `"Ctrl+Alt+Space"`.
    ///
    /// Components are separated by `+`; whitespace around a component and
    /// ASCII case are both insignificant. Modifiers may appear in any order but
    /// only once each, and the key is the single non-modifier component.
    pub fn parse(text: &str) -> Result<Self, HotkeyError> {
        let mut modifiers = Modifiers::default();
        let mut key: Option<&'static str> = None;

        for component in text.split('+') {
            let component = component.trim();
            if component.is_empty() {
                return Err(HotkeyError::EmptyComponent);
            }
            if key.is_some() {
                return Err(HotkeyError::TrailingComponent {
                    component: component.to_owned(),
                });
            }

            if let Some(modifier) = Modifier::parse(component) {
                if modifier.is_set(&modifiers) {
                    return Err(HotkeyError::DuplicateModifier {
                        modifier: modifier.name(),
                    });
                }
                modifier.set(&mut modifiers);
            } else if let Some(canonical) = canonical_key(component) {
                key = Some(canonical);
            } else {
                return Err(HotkeyError::UnknownComponent {
                    component: component.to_owned(),
                });
            }
        }

        match key {
            Some(key) => Ok(Self { modifiers, key }),
            None => Err(HotkeyError::MissingKey),
        }
    }

    /// The modifiers the shortcut requires.
    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    /// The canonical name of the non-modifier key.
    pub fn key(&self) -> &str {
        self.key
    }

    /// The canonical rendering, `Ctrl+Alt+Shift+Meta+Key`, carrying only the
    /// modifiers the shortcut requires. Re-parsing it yields an equal value.
    pub fn canonical(&self) -> String {
        let capacity = self
            .components()
            .map(|component| component.len() + '+'.len_utf8())
            .sum::<usize>()
            .saturating_sub('+'.len_utf8());
        let mut canonical = String::with_capacity(capacity);
        for component in self.components() {
            if !canonical.is_empty() {
                canonical.push('+');
            }
            canonical.push_str(component);
        }
        canonical
    }

    /// The canonical components, modifiers first and the key last.
    fn components(&self) -> impl Iterator<Item = &'static str> {
        let modifiers = self.modifiers;
        let key = self.key;
        Modifier::ORDER
            .into_iter()
            .filter(move |modifier| modifier.is_set(&modifiers))
            .map(Modifier::name)
            .chain(std::iter::once(key))
    }
}

/// Keys written as a word. Exactly one spelling each: parsing is case
/// insensitive but accepts no abbreviation, so a typo fails instead of binding
/// a different key.
const NAMED_KEYS: [&str; 15] = [
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
];

/// The function-key range every supported desktop defines.
const FUNCTION_KEYS: [&str; 24] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15", "F16",
    "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
];

/// Canonical single-character keys, indexed by their offset from `A` and `0`.
const LETTER_KEYS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGIT_KEYS: &str = "0123456789";

/// The canonical spelling of a key component, or `None` when it names no key.
fn canonical_key(component: &str) -> Option<&'static str> {
    if let Some(named) = NAMED_KEYS
        .iter()
        .copied()
        .find(|name| component.eq_ignore_ascii_case(name))
    {
        return Some(named);
    }

    if let [single] = *component.as_bytes() {
        if single.is_ascii_alphabetic() {
            let index = usize::from(single.to_ascii_uppercase() - b'A');
            return LETTER_KEYS.get(index..=index);
        }
        if single.is_ascii_digit() {
            let index = usize::from(single - b'0');
            return DIGIT_KEYS.get(index..=index);
        }
    }
    function_key(component)
}

/// `F1` to `F24` in one spelling each: no `F0`, no `F01`, no `F99`.
fn function_key(component: &str) -> Option<&'static str> {
    let mut characters = component.chars();
    if !matches!(characters.next(), Some('f' | 'F')) {
        return None;
    }

    let digits = characters.as_str();
    let plausible = matches!(digits.len(), 1 | 2)
        && !digits.starts_with('0')
        && digits.bytes().all(|digit| digit.is_ascii_digit());
    if !plausible {
        return None;
    }

    let number: usize = digits.parse().ok()?;
    FUNCTION_KEYS.get(number.checked_sub(1)?).copied()
}

pub trait ProcessLauncher {
    fn launch(&self, target: &PlatformPath, args: &[String]) -> Result<()>;
    /// Starts a target in an optional working directory.
    ///
    /// The default preserves existing backends by ignoring the directory. A
    /// backend that can honor it should override this method.
    fn launch_in(
        &self,
        target: &PlatformPath,
        args: &[String],
        working_directory: Option<&PlatformPath>,
    ) -> Result<()> {
        let _ = working_directory;
        self.launch(target, args)
    }
    fn open_uri(&self, uri: &str) -> Result<()>;
}

pub trait Notifications {
    fn notify(&self, title: &str, body: &str) -> Result<()>;
}

pub trait SecretStore {
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn set(&self, key: &str, value: &str) -> Result<()>;
}

/// The aggregate a backend crate implements and the app wires in.
pub trait PlatformBackend {
    fn name(&self) -> &'static str;
    fn capability(&self, capability: Capability) -> CapabilityState;
    fn application_discovery(&self) -> &dyn ApplicationDiscovery;
    fn clipboard(&self) -> &dyn Clipboard;
    fn process_launcher(&self) -> &dyn ProcessLauncher;
}
