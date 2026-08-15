//! Public-API contract for global-hotkey accelerators, the lossless launch
//! target codec and the application item mapping (spec 6.1, 10.2, 10.3,
//! 18.1-18.3; ADR-0007; roadmap M1 "Global hotkey + app discovery").
//!
//! All three are pure functions over data: no window system, no key grab, no
//! filesystem. Backend-specific discovery lives in the per-OS crates.
//!
//! The surface these tests require of `crikey-platform`:
//!
//! ```text
//! pub struct Modifiers { pub ctrl: bool, pub alt: bool, pub shift: bool, pub meta: bool }
//!     // Debug + PartialEq
//! pub enum HotkeyError { .. }                       // Debug
//! pub struct Accelerator { .. }                     // Debug + PartialEq
//! impl Accelerator {
//!     pub fn parse(text: &str) -> Result<Accelerator, HotkeyError>;
//!     pub fn modifiers(&self) -> Modifiers;
//!     pub fn key(&self) -> &str;                    // canonical key name
//!     pub fn canonical(&self) -> String;            // "Ctrl+Alt+Shift+Meta+Key"
//! }
//! pub enum TargetError { .. }                      // Debug + PartialEq + Display
//! pub fn encode_target(target: &PlatformPath) -> String;
//! pub fn decode_target(encoded: &str) -> Result<PlatformPath, TargetError>;
//! pub fn application_items(plugin: &PluginId, discovered: &[DiscoveredApplication]) -> Vec<Item>;
//! ```

use std::ffi::OsString;

use crikey_core::{Category, Item, ItemId, PlatformPath, PluginId};
use crikey_platform::{
    application_arguments, application_items, application_working_directory, decode_target, encode_target,
    Accelerator, DiscoveredApplication, HotkeyError, Modifiers, TargetError, APPLICATION_LAUNCH_ACTION_ID,
};

#[cfg(unix)]
use std::collections::BTreeSet;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

// ---------------------------------------------------------------------------
// Accelerators (spec 6.1, 18.1)
// ---------------------------------------------------------------------------

fn accelerator(text: &str) -> Accelerator {
    Accelerator::parse(text).unwrap_or_else(|error| panic!("{text:?} must parse: {error:?}"))
}

/// Parameters follow the canonical modifier order: ctrl, alt, shift, meta.
fn modifiers(ctrl: bool, alt: bool, shift: bool, meta: bool) -> Modifiers {
    Modifiers {
        ctrl,
        alt,
        shift,
        meta,
    }
}

fn assert_rejected(text: &str, reason: &str) {
    let parsed: Result<Accelerator, HotkeyError> = Accelerator::parse(text);
    if let Ok(accepted) = parsed {
        panic!(
            "{text:?} must be rejected ({reason}), but parsed as {:?}",
            accepted.canonical()
        );
    }
}

#[test]
fn parses_the_documented_activation_shortcut() {
    let parsed = accelerator("Ctrl+Alt+Space");

    assert_eq!(
        parsed.canonical(),
        "Ctrl+Alt+Space",
        "the specification's example accelerator round-trips unchanged"
    );
    assert_eq!(
        parsed.modifiers(),
        modifiers(true, true, false, false),
        "only the modifiers actually present are set"
    );
    assert_eq!(parsed.key(), "Space", "the trailing component is the key");
}

#[test]
fn each_modifier_token_sets_exactly_its_own_flag() {
    for (text, expected) in [
        ("Ctrl+Space", modifiers(true, false, false, false)),
        ("Alt+Space", modifiers(false, true, false, false)),
        ("Shift+Space", modifiers(false, false, true, false)),
        ("Meta+Space", modifiers(false, false, false, true)),
        ("Ctrl+Alt+Shift+Meta+Space", modifiers(true, true, true, true)),
    ] {
        assert_eq!(
            accelerator(text).modifiers(),
            expected,
            "{text:?} sets exactly the modifiers it names"
        );
    }
}

#[test]
fn parsing_is_case_insensitive() {
    let expected = accelerator("Ctrl+Alt+Space");

    for text in ["ctrl+alt+space", "CTRL+ALT+SPACE", "cTrL+aLt+SpAcE"] {
        assert_eq!(
            accelerator(text),
            expected,
            "{text:?} names the same accelerator as Ctrl+Alt+Space"
        );
        assert_eq!(
            accelerator(text).canonical(),
            "Ctrl+Alt+Space",
            "{text:?} canonicalizes to the documented spelling"
        );
    }
}

#[test]
fn parsing_ignores_surrounding_and_interior_spaces() {
    let expected = accelerator("Ctrl+Alt+Space");

    for text in [
        " Ctrl+Alt+Space",
        "Ctrl+Alt+Space ",
        "Ctrl +Alt+Space",
        "Ctrl+ Alt +Space",
        "  ctrl  +  alt  +  space  ",
    ] {
        assert_eq!(
            accelerator(text),
            expected,
            "{text:?} differs from Ctrl+Alt+Space only in spacing"
        );
    }
}

#[test]
fn canonical_form_uses_a_fixed_modifier_order() {
    for (text, canonical) in [
        ("Shift+Meta+Ctrl+Alt+Space", "Ctrl+Alt+Shift+Meta+Space"),
        ("Meta+Shift+Alt+Ctrl+Space", "Ctrl+Alt+Shift+Meta+Space"),
        ("Shift+Ctrl+K", "Ctrl+Shift+K"),
        ("meta+alt+f12", "Alt+Meta+F12"),
        ("shift+meta+enter", "Shift+Meta+Enter"),
        ("alt+ctrl+tab", "Ctrl+Alt+Tab"),
    ] {
        assert_eq!(
            accelerator(text).canonical(),
            canonical,
            "{text:?} renders modifiers in Ctrl, Alt, Shift, Meta order"
        );
    }
}

#[test]
fn keys_canonicalize_to_one_spelling() {
    for (text, canonical) in [
        ("ctrl+space", "Ctrl+Space"),
        ("ctrl+enter", "Ctrl+Enter"),
        ("ctrl+tab", "Ctrl+Tab"),
        ("ctrl+escape", "Ctrl+Escape"),
        ("ctrl+f1", "Ctrl+F1"),
        ("ctrl+f12", "Ctrl+F12"),
        ("ctrl+k", "Ctrl+K"),
        ("ctrl+9", "Ctrl+9"),
    ] {
        let parsed = accelerator(text);
        assert_eq!(
            parsed.canonical(),
            canonical,
            "{text:?} canonicalizes to a single documented spelling"
        );
        let (_, key) = canonical
            .rsplit_once('+')
            .expect("every canonical form in this table carries a modifier");
        assert_eq!(
            parsed.key(),
            key,
            "{text:?} exposes the same key name the canonical form renders"
        );
    }
}

#[test]
fn canonical_form_round_trips_through_parse() {
    for text in [
        "Ctrl+Alt+Space",
        "ctrl+k",
        "meta+shift+alt+ctrl+space",
        " alt +  f12 ",
        "SHIFT+META+ENTER",
        "ctrl+9",
        "ctrl+tab",
        "alt+escape",
    ] {
        let parsed = accelerator(text);
        let canonical = parsed.canonical();
        let reparsed = accelerator(&canonical);

        assert_eq!(
            reparsed, parsed,
            "parsing the canonical form of {text:?} yields the same accelerator"
        );
        assert_eq!(
            reparsed.canonical(),
            canonical,
            "canonicalization of {text:?} is idempotent"
        );
    }
}

#[test]
fn rejects_empty_input() {
    for text in ["", " ", "   ", "+"] {
        assert_rejected(text, "an accelerator without components names no key");
    }
}

#[test]
fn rejects_modifier_only_input() {
    for text in [
        "Ctrl",
        "Ctrl+Alt",
        "ctrl+shift+meta",
        "Ctrl+Alt+Shift+Meta",
        "Ctrl+",
    ] {
        assert_rejected(text, "a hotkey with no key can never fire");
    }
}

#[test]
fn rejects_duplicate_modifiers() {
    for text in [
        "Ctrl+Ctrl+Space",
        "ctrl+CTRL+space",
        "Ctrl+Alt+ctrl+Space",
        "Shift+Shift+F1",
        "Meta+Alt+Meta+Enter",
    ] {
        assert_rejected(text, "a repeated modifier is a typo, not a stronger modifier");
    }
}

#[test]
fn rejects_unknown_keys() {
    for text in [
        "Ctrl+Frobnicate",
        "Ctrl+Alt+Sapce",
        "Ctrl+ThisIsNotAKey",
        "Kontrol+Space",
        "Ctrl+F0",
        "Ctrl+F99",
    ] {
        assert_rejected(
            text,
            "an unrecognized token must fail loudly instead of binding nothing",
        );
    }
}

// ---------------------------------------------------------------------------
// Application items (spec 10.2, 10.3, 18.1-18.3)
// ---------------------------------------------------------------------------

/// Arguments are recorded losslessly: a count plus one key per argument, so an
/// argument may itself contain spaces (`ProcessLauncher::launch` takes a slice,
/// never a joined command line).
const ARGUMENT_COUNT_KEY: &str = "application.argument.count";
const ARGUMENT_KEY_PREFIX: &str = "application.argument.";

fn plugin() -> PluginId {
    PluginId("dev.crikey.applications".to_owned())
}

fn discovered(name: &str, target: impl Into<OsString>) -> DiscoveredApplication {
    DiscoveredApplication {
        name: name.to_owned(),
        target: PlatformPath::new(target),
        arguments: Vec::new(),
        working_directory: None,
        icon_reference: None,
        platform_id: None,
    }
}

fn recorded_arguments(item: &Item) -> Vec<String> {
    let raw = item
        .metadata
        .get(ARGUMENT_COUNT_KEY)
        .unwrap_or_else(|| panic!("{ARGUMENT_COUNT_KEY:?} is always recorded"));
    let count: usize = raw
        .parse()
        .unwrap_or_else(|_| panic!("{ARGUMENT_COUNT_KEY:?} is a decimal count, got {raw:?}"));

    (0..count)
        .map(|index| {
            let key = format!("{ARGUMENT_KEY_PREFIX}{index}");
            item.metadata
                .get(&key)
                .unwrap_or_else(|| panic!("{key:?} is recorded when the count is {count}"))
                .clone()
        })
        .collect()
}

fn indexed_argument_keys(item: &Item) -> Vec<&str> {
    item.metadata
        .keys()
        .map(String::as_str)
        .filter(|key| key.starts_with(ARGUMENT_KEY_PREFIX) && *key != ARGUMENT_COUNT_KEY)
        .collect()
}

#[test]
fn discovered_applications_become_application_items_in_order() {
    let owner = plugin();
    let discoveries = [
        discovered("Firefox", "/usr/bin/firefox"),
        discovered("GNU Image Manipulation Program", "/usr/bin/gimp"),
        discovered("Terminal", "/usr/bin/xterm"),
    ];

    let items = application_items(&owner, &discoveries);

    assert_eq!(
        items.len(),
        discoveries.len(),
        "the mapping is one item per discovered application; deduplication belongs to discovery"
    );
    for (item, discovery) in items.iter().zip(&discoveries) {
        assert_eq!(
            item.category,
            Category::Application,
            "a discovered application is an Application item (spec 10.3)"
        );
        assert_eq!(
            item.label, discovery.name,
            "the label is the discovered display name"
        );
        assert_eq!(
            item.plugin_id, owner,
            "the item is owned by the plugin the caller named"
        );
        assert!(
            item.search_terms.contains(&discovery.name),
            "{:?} is searchable by its name, found {:?}",
            discovery.name,
            item.search_terms
        );
    }
}

#[test]
fn no_discoveries_produce_no_items() {
    assert!(
        application_items(&plugin(), &[]).is_empty(),
        "an empty discovery run publishes an empty catalog slice, not a placeholder"
    );
}

#[test]
fn arguments_are_recorded_in_metadata_in_order() {
    let mut app = discovered("Terminal", "/usr/bin/xterm");
    app.arguments = vec![
        "--title".to_owned(),
        "My Shell".to_owned(),
        String::new(),
        "café ☕".to_owned(),
        "-e=/bin/sh -l".to_owned(),
    ];

    let items = application_items(&plugin(), std::slice::from_ref(&app));

    assert_eq!(
        recorded_arguments(&items[0]),
        app.arguments,
        "arguments survive conversion in order, including empty ones and ones containing spaces"
    );
    assert_eq!(
        indexed_argument_keys(&items[0]).len(),
        app.arguments.len(),
        "one metadata key per argument, no stragglers"
    );
}

#[test]
fn application_arguments_rebuilds_exact_argument_boundaries() {
    let mut app = discovered("Terminal", "/usr/bin/xterm");
    app.arguments = vec!["".to_owned(), "two words".to_owned(), "\"quoted\"".to_owned()];
    let item = application_items(&plugin(), &[app])
        .pop()
        .expect("one discovery produces one item");

    assert_eq!(
        application_arguments(&item).expect("host-owned metadata is valid"),
        ["", "two words", "\"quoted\""]
    );
}

#[test]
fn working_directory_metadata_round_trips_when_present_and_absent() {
    let mut with_directory = discovered("Editor", "/usr/bin/editor");
    let directory = PlatformPath::new("/home/tester/project");
    with_directory.working_directory = Some(directory.clone());
    let with_item = application_items(&plugin(), &[with_directory])
        .pop()
        .expect("one discovery produces one item");
    assert_eq!(
        application_working_directory(&with_item).expect("working-directory metadata is valid"),
        Some(directory)
    );

    let without_item = application_items(&plugin(), &[discovered("Editor", "/usr/bin/editor")])
        .pop()
        .expect("one discovery produces one item");
    assert_eq!(
        application_working_directory(&without_item).expect("missing working-directory metadata is valid"),
        None
    );
}

#[test]
fn discovered_applications_have_one_host_launch_action() {
    let item = application_items(&plugin(), &[discovered("Firefox", "/usr/bin/firefox")])
        .pop()
        .expect("one discovery produces one item");

    assert_eq!(item.actions.len(), 1);
    assert_eq!(item.actions[0].action_id.0, APPLICATION_LAUNCH_ACTION_ID);
    assert_eq!(item.actions[0].label, "Launch");
}

#[test]
fn applications_without_arguments_record_an_empty_argument_list() {
    let items = application_items(&plugin(), &[discovered("Firefox", "/usr/bin/firefox")]);

    let arguments = recorded_arguments(&items[0]);
    assert!(
        arguments.is_empty(),
        "an application launched with no arguments records none, got {arguments:?}"
    );
    let stray = indexed_argument_keys(&items[0]);
    assert!(
        stray.is_empty(),
        "no indexed argument keys are written when there are no arguments, got {stray:?}"
    );
}

#[test]
fn icon_references_are_propagated_and_stay_absent_when_undiscovered() {
    let mut with_icon = discovered("Firefox", "/usr/bin/firefox");
    with_icon.icon_reference = Some("firefox".to_owned());
    let without_icon = discovered("Terminal", "/usr/bin/xterm");

    let items = application_items(&plugin(), &[with_icon, without_icon]);

    assert_eq!(
        items[0].icon_reference.as_deref(),
        Some("firefox"),
        "the discovered icon reference is carried through verbatim"
    );
    assert_eq!(
        items[1].icon_reference, None,
        "a missing icon stays missing rather than becoming an empty reference"
    );
}

#[test]
fn stable_ids_are_derived_from_the_item_target() {
    let owner = plugin();
    let discoveries = [discovered("Firefox", "/usr/bin/firefox")];

    let items = application_items(&owner, &discoveries);
    let item = &items[0];

    assert_eq!(
        item.stable_id,
        ItemId::derived(&owner, &Category::Application, &item.target),
        "the host derivation over the item target is the item identity"
    );

    let elsewhere = application_items(&PluginId("dev.crikey.other".to_owned()), &discoveries);
    assert_ne!(
        item.stable_id, elsewhere[0].stable_id,
        "identity is scoped to the owning plugin"
    );
}

#[test]
fn stable_ids_distinguish_targets_and_ignore_the_display_name() {
    let items = application_items(
        &plugin(),
        &[
            discovered("Firefox", "/usr/bin/firefox"),
            discovered("Firefox", "/opt/firefox/firefox"),
            discovered("Web Browser", "/usr/bin/firefox"),
        ],
    );

    assert_ne!(
        items[0].target, items[1].target,
        "two install locations are two distinct targets"
    );
    assert_ne!(
        items[0].stable_id, items[1].stable_id,
        "two install locations are two distinct items"
    );
    assert_eq!(
        items[0].stable_id, items[2].stable_id,
        "renaming a desktop entry must not change item identity (spec 10.2)"
    );
}

#[test]
fn conversion_is_deterministic() {
    let owner = plugin();
    let mut app = discovered("Terminal", "/usr/bin/xterm");
    app.arguments = vec!["-e".to_owned(), "/bin/sh".to_owned()];
    app.icon_reference = Some("utilities-terminal".to_owned());
    let discoveries = [app];

    let first = application_items(&owner, &discoveries);
    let second = application_items(&owner, &discoveries);

    assert_eq!(
        first[0].stable_id, second[0].stable_id,
        "identity does not drift between discovery runs"
    );
    assert_eq!(
        first[0].target, second[0].target,
        "the target encoding does not drift between discovery runs"
    );
    assert_eq!(
        first[0].metadata, second[0].metadata,
        "recorded metadata does not drift between discovery runs"
    );
}

#[test]
fn valid_utf8_targets_are_preserved_verbatim() {
    let items = application_items(
        &plugin(),
        &[
            discovered("Firefox", "/usr/bin/firefox"),
            discovered("Café", "/opt/café/bin/app"),
        ],
    );

    assert_eq!(
        items[0].target, "/usr/bin/firefox",
        "a UTF-8 target is the execution payload as-is, not an escaped rendering"
    );
    assert_eq!(
        items[1].target, "/opt/café/bin/app",
        "non-ASCII UTF-8 is still a valid execution payload and stays readable"
    );
}

#[cfg(unix)]
fn raw_target(bytes: &[u8]) -> OsString {
    OsString::from_vec(bytes.to_vec())
}

#[cfg(unix)]
#[test]
fn non_utf8_targets_are_never_flattened_by_a_lossy_conversion() {
    let owner = plugin();
    let discoveries = [discovered("Legacy App", raw_target(b"/usr/bin/app-\xFF"))];

    let items = application_items(&owner, &discoveries);
    let item = &items[0];

    assert!(
        !item.target.contains('\u{FFFD}'),
        "a non-UTF-8 target must not pass through a lossy conversion (ADR-0007), got {:?}",
        item.target
    );
    assert_eq!(
        item.stable_id,
        ItemId::derived(&owner, &Category::Application, &item.target),
        "identity is derived from the preserved target, not from a display rendering"
    );
    assert!(
        item.search_terms.iter().any(|term| term == "Legacy App"),
        "an application on a non-UTF-8 path is still searchable by name, found {:?}",
        item.search_terms
    );
    assert_eq!(
        application_items(&owner, &discoveries)[0].target,
        item.target,
        "the encoding of a non-UTF-8 target is deterministic"
    );
}

#[cfg(unix)]
#[test]
fn targets_that_differ_only_in_non_utf8_bytes_stay_distinct() {
    let discoveries = [
        discovered("App", raw_target(b"/usr/bin/app-\xFF")),
        discovered("App", raw_target(b"/usr/bin/app-\xFE")),
        discovered("App", "/usr/bin/app-\u{FFFD}"),
        discovered("App", "/usr/bin/app-%FF"),
    ];

    let items = application_items(&plugin(), &discoveries);

    let targets: BTreeSet<&str> = items.iter().map(|item| item.target.as_str()).collect();
    assert_eq!(
        targets.len(),
        discoveries.len(),
        "distinct paths must never share an item target: {targets:?}"
    );

    let ids: BTreeSet<&ItemId> = items.iter().map(|item| &item.stable_id).collect();
    assert_eq!(
        ids.len(),
        discoveries.len(),
        "distinct paths must never share an item identity: {ids:?}"
    );
}

// ---------------------------------------------------------------------------
// Launch targets (spec 18.3, ADR-0007)
// ---------------------------------------------------------------------------

/// The tag of the platform this build is not, so the same rejection is
/// meaningful whichever desktop runs the suite.
#[cfg(unix)]
const FOREIGN_TAG: &str = "%windows;";
#[cfg(windows)]
const FOREIGN_TAG: &str = "%unix;";

/// A target the other platform could have written, escapes and all.
#[cfg(unix)]
const FOREIGN_TARGET: &str = "%windows;C:\\Program Files\\App%uD800.exe";
#[cfg(windows)]
const FOREIGN_TARGET: &str = "%unix;/usr/bin/app-%FF";

/// Encodes to exactly one spelling and decodes back to the path it started
/// from: an encoder without its inverse is a one-way loss of the payload.
fn round_trip(target: impl Into<OsString>, encoded: &str) {
    let path = PlatformPath::new(target);
    assert_eq!(
        encode_target(&path),
        encoded,
        "{path:?} has exactly one encoding, so its identity never moves"
    );

    let decoded = decode_target(encoded).unwrap_or_else(|error| panic!("{encoded:?} must decode: {error:?}"));
    assert_eq!(
        decoded, path,
        "decoding {encoded:?} must give back the path it was encoded from"
    );
}

#[test]
fn application_items_carry_the_published_encoding_of_the_target() {
    let discoveries = [
        discovered("Firefox", "/usr/bin/firefox"),
        discovered("Discount", "/opt/save 50%/run"),
    ];

    let items = application_items(&plugin(), &discoveries);
    for (item, application) in items.iter().zip(&discoveries) {
        assert_eq!(
            item.target,
            encode_target(&application.target),
            "an item target is what the published encoder produces, never a second encoding"
        );

        let decoded = decode_target(&item.target)
            .unwrap_or_else(|error| panic!("{:?} must decode: {error:?}", item.target));
        assert_eq!(
            decoded, application.target,
            "an item target decodes back to the path discovery found (ADR-0007)"
        );
    }
}

#[test]
fn targets_containing_a_percent_round_trip() {
    round_trip("/opt/save 50%/run", "/opt/save 50%25/run");
    round_trip("%", "%25");
    round_trip("/opt/%%/app", "/opt/%25%25/app");
}

#[test]
fn a_target_spelling_a_percent_escape_is_not_the_byte_it_spells() {
    round_trip("/opt/%25/app", "/opt/%2525/app");

    assert_ne!(
        encode_target(&PlatformPath::new("/opt/%25/app")),
        encode_target(&PlatformPath::new("/opt/%/app")),
        "a path that spells an escape and the path that escape stands for are two paths"
    );
    let decoded = decode_target("/opt/%25/app")
        .unwrap_or_else(|error| panic!("an escaped percent must decode: {error:?}"));
    assert_eq!(
        decoded,
        PlatformPath::new("/opt/%/app"),
        "%25 decodes to the single character it escapes"
    );
}

#[test]
fn a_trailing_percent_round_trips_and_a_bare_one_is_rejected() {
    round_trip("/usr/bin/app%", "/usr/bin/app%25");

    assert_eq!(
        decode_target("/usr/bin/app%"),
        Err(TargetError::MalformedEscape { offset: 12 }),
        "a trailing % introduces no escape, and a truncated target must fail loudly"
    );
}

#[test]
fn a_target_that_spells_a_platform_tag_is_still_a_path() {
    round_trip("%unix;/usr/bin/app", "%25unix;/usr/bin/app");
    round_trip("%windows;/usr/bin/app", "%25windows;/usr/bin/app");
}

#[test]
fn a_foreign_platform_target_is_a_typed_rejection() {
    let error = decode_target(FOREIGN_TARGET)
        .expect_err("a target this build cannot reconstruct must never decode (ADR-0007)");

    assert_eq!(
        error,
        TargetError::ForeignPlatform { tag: FOREIGN_TAG },
        "the rejection names the platform the target came from"
    );
    assert!(
        error.to_string().contains(FOREIGN_TAG),
        "the diagnostic names the foreign tag, got {error}"
    );
}

#[test]
fn a_platform_escape_without_a_tag_is_rejected() {
    assert_eq!(
        decode_target("/usr/bin/app-%FF"),
        Err(TargetError::MissingPlatformTag { offset: 13 }),
        "an untagged byte escape lost the tag that gives it meaning; reading it as text would \
         silently corrupt the path"
    );
    assert_eq!(
        decode_target("/usr/bin/app-%uD800"),
        Err(TargetError::MissingPlatformTag { offset: 13 }),
        "an untagged code-unit escape is rejected the same way, whichever platform decodes it"
    );
}

#[cfg(unix)]
#[test]
fn non_utf8_targets_round_trip_through_a_tagged_encoding() {
    round_trip(raw_target(b"/usr/bin/app-\xFF"), "%unix;/usr/bin/app-%FF");
    round_trip(raw_target(b"/usr/\xFF/app 50%"), "%unix;/usr/%FF/app 50%25");

    let every_byte: Vec<u8> = (0..=u8::MAX).collect();
    let path = PlatformPath::new(raw_target(&every_byte));
    let encoded = encode_target(&path);
    let decoded =
        decode_target(&encoded).unwrap_or_else(|error| panic!("{encoded:?} must decode: {error:?}"));
    assert_eq!(
        decoded, path,
        "every byte a filesystem can name survives the round trip"
    );
}

#[cfg(unix)]
#[test]
fn a_native_non_utf8_target_names_the_platform_that_wrote_it() {
    let encoded = encode_target(&PlatformPath::new(raw_target(b"/usr/bin/app-\xFF")));

    assert!(
        encoded.starts_with("%unix;"),
        "a target whose bytes only this platform can spell says so, got {encoded:?}"
    );
    assert!(
        !encoded.starts_with(FOREIGN_TAG),
        "a Unix-origin and a Windows-origin target are distinguishable, got {encoded:?}"
    );
}

/// An application is searchable by the name of the thing it launches.
///
/// The matcher never searches `target`: it is an execution payload, and folding
/// a whole path in would make every `/usr/bin/...` item answer to `usr`. But the
/// executable's stem is the name users type, and it is routinely unrecoverable
/// from the label — no reading of `Command Prompt` yields `cmd`, because those
/// letters do not begin its words. Carrying the stem as a search term is what
/// keeps such an application reachable.
#[test]
fn application_items_are_searchable_by_their_executable_stem() {
    let cases = [
        ("Command Prompt", "C:\\Windows\\System32\\cmd.exe", "cmd"),
        ("Visual Studio Code", "/usr/bin/code", "code"),
        ("GNU Image Manipulation Program", "/usr/bin/gimp", "gimp"),
    ];
    for (name, target, stem) in cases {
        let items = application_items(&plugin(), &[discovered(name, target)]);
        let item = items.first().expect("one discovery makes one item");
        assert!(
            item.search_terms.iter().any(|term| term == stem),
            "{name:?} should answer to {stem:?}, found {:?}",
            item.search_terms
        );
        assert!(
            item.search_terms.iter().any(|term| term == name),
            "the display name stays searchable, found {:?}",
            item.search_terms
        );
    }
}

/// A platform identifier contributes the part a user would type.
///
/// Each platform hands over a different shape, and the Linux one is a trap: the
/// identifier is the desktop-entry *file name*, so taking the last dotted
/// segment without stripping the extension files every application on the
/// system under the alias `desktop`.
#[test]
fn application_items_are_searchable_by_their_platform_identifier() {
    let cases = [
        // Linux: plain desktop-entry file name.
        ("Firefox", "firefox.desktop", "firefox"),
        // Linux: reverse-DNS desktop-entry file name.
        ("Files", "org.gnome.Nautilus.desktop", "Nautilus"),
        // macOS: bundle identifier, no extension to strip.
        ("Safari", "com.apple.Safari", "Safari"),
    ];
    for (name, platform_id, expected) in cases {
        let mut discovery = discovered(name, "/usr/bin/placeholder");
        discovery.platform_id = Some(platform_id.to_owned());
        let items = application_items(&plugin(), &[discovery]);
        let terms = &items.first().expect("one item").search_terms;
        assert!(
            terms.iter().any(|term| term == expected),
            "{platform_id:?} should contribute {expected:?}, found {terms:?}"
        );
        assert!(
            !terms.iter().any(|term| term == "desktop"),
            "{platform_id:?} must not file the application under \"desktop\", found {terms:?}"
        );
    }
}

/// No application answers to `desktop` merely because it has a desktop entry.
///
/// A shared alias across the whole catalog is worse than no alias: one query
/// would return every installed application.
#[test]
fn desktop_entries_do_not_share_a_catalog_wide_alias() {
    let discoveries = ["firefox", "gimp", "nautilus"].map(|stem| {
        let mut discovery = discovered(stem, format!("/usr/bin/{stem}"));
        discovery.platform_id = Some(format!("{stem}.desktop"));
        discovery
    });
    for item in application_items(&plugin(), &discoveries) {
        assert!(
            !item.search_terms.iter().any(|term| term == "desktop"),
            "{:?} must not answer to \"desktop\", found {:?}",
            item.label,
            item.search_terms
        );
    }
}

/// Search terms carry no duplicates and no empties.
///
/// An application whose stem already equals its label contributes one term, not
/// two identical ones, so the keyword tier does not score the same text twice.
#[test]
fn application_search_terms_are_deduplicated() {
    let items = application_items(&plugin(), &[discovered("gimp", "/usr/bin/gimp")]);
    let terms = &items.first().expect("one item").search_terms;
    assert_eq!(
        terms,
        &vec!["gimp".to_owned()],
        "expected one term, got {terms:?}"
    );

    // A target with no usable stem contributes nothing rather than an empty term.
    let items = application_items(&plugin(), &[discovered("Shell", "/usr/bin/")]);
    let terms = &items.first().expect("one item").search_terms;
    assert!(
        terms.iter().all(|term| !term.is_empty()),
        "no empty search terms, got {terms:?}"
    );
}

/// The stem is reachable through the real matcher, not merely present.
///
/// `cmd` is not an abbreviation of `Command Prompt` under any word split — the
/// `d` begins no word — so before the stem became a search term the application
/// was unreachable by the name everybody types for it.
#[test]
fn executable_stem_makes_the_application_matchable() {
    use crikey_query::{DefaultMatcher, DefaultNormalizer, MatchMethod, Matcher, Normalizer};

    let items = application_items(
        &plugin(),
        &[discovered("Command Prompt", "C:\\Windows\\System32\\cmd.exe")],
    );
    let item = items.first().expect("one discovery makes one item");
    let query = DefaultNormalizer::default().normalize("cmd");
    let outcome = DefaultMatcher::default()
        .match_item(&query, item)
        .expect("the executable stem should make this reachable");
    assert_eq!(
        outcome.method,
        MatchMethod::Keyword,
        "an alias is a keyword hit, not a reading of the label"
    );
    assert!(
        outcome.highlights.is_empty(),
        "a keyword hit is not in the label, so it highlights nothing"
    );
}
