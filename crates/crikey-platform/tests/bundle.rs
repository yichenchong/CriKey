//! Public-API contract for macOS application-bundle parsing (spec 18.5
//! "Application-bundle discovery"; roadmap M6 "Additional platforms").
//!
//! This logic lives in the cross-platform `crikey-platform` crate on purpose.
//! `crikey-platform-macos` is gated on `target_os = "macos"` and therefore
//! cannot be exercised from the development host at all, so the pure data
//! transformation — reading an `Info.plist` and naming a `Foo.app` directory —
//! is placed where it can be tested, and the macOS crate keeps only the thin
//! OS binding (Launch Services, Spotlight, Accessibility). These tests are the
//! reason that split exists, so they pin the parser exhaustively.
//!
//! The surface these tests require of `crikey-platform`:
//!
//! ```text
//! pub struct AppBundle {
//!     pub name: String,
//!     pub bundle_id: Option<String>,
//!     pub executable: Option<String>,
//! }
//! pub fn parse_info_plist(xml: &str) -> Option<AppBundle>;
//! pub fn bundle_display_name(dir_name: &str) -> Option<&str>;
//! ```

use std::time::{Duration, Instant};

use crikey_platform::{bundle_display_name, parse_info_plist, AppBundle};

/// Wraps `body` in the plist envelope Apple's tooling emits, so the parser is
/// always fed a realistic document rather than a bare fragment.
fn plist(body: &str) -> String {
    format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
            "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" ",
            "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
            "<plist version=\"1.0\">\n<dict>\n{}\n</dict>\n</plist>\n"
        ),
        body
    )
}

fn parsed(body: &str) -> AppBundle {
    parse_info_plist(&plist(body)).expect("a well-formed Info.plist with a name must parse")
}

// ---------------------------------------------------------------------------
// parse_info_plist
// ---------------------------------------------------------------------------

/// A realistic minimal `Info.plist` yields all three fields, and each field
/// carries its own distinct value. Kills the field-swap bug where the
/// identifier is reported as the name (or the executable as the identifier),
/// which a fixture reusing one string everywhere would not detect.
#[test]
fn a_minimal_info_plist_yields_name_identifier_and_executable_as_three_distinct_values() {
    let bundle = parsed(
        "  <key>CFBundleName</key>\n  <string>Transmission</string>\n\
         \x20 <key>CFBundleIdentifier</key>\n  <string>org.m0k.transmission</string>\n\
         \x20 <key>CFBundleExecutable</key>\n  <string>Transmission-bin</string>",
    );

    assert_eq!(bundle.name, "Transmission");
    assert_eq!(bundle.bundle_id.as_deref(), Some("org.m0k.transmission"));
    assert_eq!(bundle.executable.as_deref(), Some("Transmission-bin"));
}

/// `CFBundleDisplayName` is the user-visible name and wins over
/// `CFBundleName` when both are present. Kills the first-key-wins bug that
/// would surface the short internal name in the launcher UI.
#[test]
fn display_name_is_preferred_over_bundle_name_when_both_keys_are_present() {
    let display_first = parsed(
        "  <key>CFBundleDisplayName</key>\n  <string>Visual Studio Code</string>\n\
         \x20 <key>CFBundleName</key>\n  <string>Code</string>",
    );
    assert_eq!(display_first.name, "Visual Studio Code");

    // The preference is by key, not by document position: swapping the order
    // must not flip the answer.
    let name_first = parsed(
        "  <key>CFBundleName</key>\n  <string>Code</string>\n\
         \x20 <key>CFBundleDisplayName</key>\n  <string>Visual Studio Code</string>",
    );
    assert_eq!(name_first.name, "Visual Studio Code");
}

/// Optional keys that are absent are reported as `None`, not as an empty
/// string or as a copy of another field. Kills the bug where a missing
/// identifier is defaulted to `Some("")` and then indexed as a real id.
#[test]
fn a_plist_without_an_identifier_reports_bundle_id_none_rather_than_an_empty_string() {
    let bundle = parsed(
        "  <key>CFBundleName</key>\n  <string>Terminal</string>\n\
         \x20 <key>CFBundleExecutable</key>\n  <string>Terminal</string>",
    );

    assert_eq!(bundle.name, "Terminal");
    assert_eq!(bundle.bundle_id, None);
    assert_eq!(bundle.executable.as_deref(), Some("Terminal"));
}

/// Malformed XML is rejected with `None` and never panics. Discovery walks
/// third-party bundles we do not control, so a truncated or unbalanced
/// `Info.plist` must skip one application, not abort the scan.
#[test]
fn malformed_xml_returns_none_instead_of_panicking() {
    let truncated = "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleName";
    assert!(parse_info_plist(truncated).is_none(), "truncated plist");

    let unbalanced = plist("  <key>CFBundleName</key>\n  <string>Broken</dict>");
    assert!(parse_info_plist(&unbalanced).is_none(), "unbalanced tags");

    let not_xml_at_all = "bplist00\u{0}\u{1}not xml";
    assert!(parse_info_plist(not_xml_at_all).is_none(), "binary plist");
}

/// An XML document has exactly one root element, and for an `Info.plist` that
/// root is `<plist>`. A second top-level element on either side of it is not a
/// well-formed document and must be rejected whole.
///
/// Kills the parser that hunts for a `<plist>` element *anywhere* in the event
/// stream: both documents below carry a complete, valid plist, so a scanner
/// that ignores what surrounds it returns a bundle for input no XML parser
/// would accept. The same body without the intruder is asserted to parse, so
/// the test can only fail on the root rule.
#[test]
fn a_second_top_level_element_beside_the_plist_root_returns_none() {
    let body = "  <key>CFBundleName</key>\n  <string>Rooted</string>";
    let well_formed = plist(body);
    assert_eq!(
        parse_info_plist(&well_formed).map(|bundle| bundle.name),
        Some("Rooted".to_string()),
        "the same document without an intruding element must parse"
    );

    let leading = well_formed.replace("<plist version", "<junk/><plist version");
    assert!(
        parse_info_plist(&leading).is_none(),
        "an element before the plist root"
    );

    let trailing = format!("{well_formed}<extra/>\n");
    assert!(
        parse_info_plist(&trailing).is_none(),
        "an element after the plist root"
    );
}

/// The macOS scanner reads any `Info.plist` up to a megabyte before handing it
/// here, and those bytes come from third-party bundles nobody in this project
/// controls. Entity decoding must therefore be linear in the length of the
/// character data: a run of bare ampersands with no `;` anywhere after them is
/// the worst case for a decoder that re-scans the remainder of the run looking
/// for a delimiter after every `&`.
///
/// Kills that quadratic decoder. At this size it performs on the order of
/// 10^12 byte comparisons and does not finish; the linear one is milliseconds.
/// The wall-clock bound is deliberately enormous -- it is a stall guard, not a
/// performance budget, and exists only so a quadratic regression fails this
/// test by name instead of hanging the suite. Duration is the thing under test
/// here, which is why an otherwise-forbidden timing assertion is appropriate.
#[test]
fn a_megabyte_of_bare_ampersands_decodes_in_linear_time() {
    // Mirrors `BundleScanner::MAX_INFO_PLIST_BYTES` in `crikey-platform-macos`,
    // which is gated on macOS and so cannot be imported here.
    const MAX_INFO_PLIST_BYTES: usize = 1024 * 1024;
    const STALL_GUARD: Duration = Duration::from_secs(10);

    let envelope = plist("  <key>CFBundleName</key>\n  <string></string>").len();
    let ampersands = "&".repeat(MAX_INFO_PLIST_BYTES - envelope);
    let document = plist(&format!(
        "  <key>CFBundleName</key>\n  <string>{ampersands}</string>"
    ));
    assert_eq!(
        document.len(),
        MAX_INFO_PLIST_BYTES,
        "the fixture must sit exactly at the size the scanner admits"
    );

    let started = Instant::now();
    let bundle = parse_info_plist(&document);
    let elapsed = started.elapsed();

    // A bare `&` is not a reference this subset defines, so it survives the
    // decode verbatim rather than being dropped or rejected.
    assert_eq!(
        bundle.map(|bundle| bundle.name),
        Some(ampersands),
        "bare ampersands pass through unchanged"
    );
    assert!(
        elapsed < STALL_GUARD,
        "decoding {MAX_INFO_PLIST_BYTES} bytes of ampersands took {elapsed:?}, \
         which means entity scanning is no longer linear"
    );
}

/// Entity references are expanded, and only the ones this subset defines.
/// Kills both a decoder that drops an unknown reference -- silently mangling a
/// display name -- and one whose bounded look-ahead skips over a valid entity
/// that follows an undecodable one.
#[test]
fn defined_entities_expand_and_undefined_ones_survive_verbatim() {
    let bundle = parsed(
        "  <key>CFBundleName</key>\n\
         \x20 <string>Rock &amp; Roll &#65; &#x42; &nope; &amp;</string>",
    );

    assert_eq!(bundle.name, "Rock & Roll A B &nope; &");
}

/// The empty document is `None`. Kills the bug where a zero-byte
/// `Info.plist` produces a nameless `AppBundle` that reaches the index.
#[test]
fn an_empty_document_returns_none() {
    assert!(parse_info_plist("").is_none(), "empty document");
    assert!(parse_info_plist("   \n\t ").is_none(), "whitespace-only document");
}

/// Key order in the dictionary is not significant: the same pairs written in
/// a different order parse to the same bundle. Kills a positional parser that
/// assigns fields by index rather than by key.
#[test]
fn the_same_keys_in_a_different_order_parse_to_the_same_bundle() {
    let forward = parsed(
        "  <key>CFBundleName</key>\n  <string>Numbers</string>\n\
         \x20 <key>CFBundleIdentifier</key>\n  <string>com.apple.iWork.Numbers</string>\n\
         \x20 <key>CFBundleExecutable</key>\n  <string>Numbers-exec</string>",
    );
    let reversed = parsed(
        "  <key>CFBundleExecutable</key>\n  <string>Numbers-exec</string>\n\
         \x20 <key>CFBundleIdentifier</key>\n  <string>com.apple.iWork.Numbers</string>\n\
         \x20 <key>CFBundleName</key>\n  <string>Numbers</string>",
    );

    assert_eq!(forward.name, reversed.name);
    assert_eq!(forward.bundle_id, reversed.bundle_id);
    assert_eq!(forward.executable, reversed.executable);
    // ... and they are the real values, not two identically-empty results.
    assert_eq!(reversed.name, "Numbers");
    assert_eq!(reversed.bundle_id.as_deref(), Some("com.apple.iWork.Numbers"));
    assert_eq!(reversed.executable.as_deref(), Some("Numbers-exec"));
}

/// A non-string value is not silently coerced into a string. `<integer>42</integer>`
/// is not a bundle identifier, and a bundle whose only name value is a
/// non-string has no usable display name at all. Kills the tag-agnostic parser
/// that grabs whatever text follows a `<key>`.
#[test]
fn a_non_string_value_is_not_silently_read_as_a_string() {
    let numeric_identifier = parsed(
        "  <key>CFBundleName</key>\n  <string>Oddball</string>\n\
         \x20 <key>CFBundleIdentifier</key>\n  <integer>42</integer>",
    );
    assert_eq!(numeric_identifier.name, "Oddball");
    assert_eq!(numeric_identifier.bundle_id, None);

    let boolean_executable = parsed(
        "  <key>CFBundleName</key>\n  <string>Oddball</string>\n\
         \x20 <key>CFBundleExecutable</key>\n  <true/>",
    );
    assert_eq!(boolean_executable.executable, None);

    // No string-valued name key at all => no bundle, rather than a bundle
    // named "42".
    let numeric_name = plist("  <key>CFBundleName</key>\n  <integer>42</integer>");
    assert!(
        parse_info_plist(&numeric_name).is_none(),
        "an integer CFBundleName is not a display name"
    );
}

#[test]
fn nested_markup_inside_a_scalar_is_rejected() {
    let malformed = plist(
        "  <key>CFBundleName</key>\n\
         \x20 <string>Visible <em>name</em></string>",
    );
    assert!(
        parse_info_plist(&malformed).is_none(),
        "nested markup must not be flattened into a scalar value"
    );
}

/// Non-ASCII values survive byte-for-byte. Kills a parser that slices on byte
/// offsets or normalises to ASCII, which would corrupt localised bundle names.
#[test]
fn unicode_values_survive_parsing_intact() {
    let bundle = parsed(
        "  <key>CFBundleDisplayName</key>\n  <string>日本語エディタ</string>\n\
         \x20 <key>CFBundleIdentifier</key>\n  <string>jp.example.エディタ</string>",
    );

    assert_eq!(bundle.name, "日本語エディタ");
    assert_eq!(bundle.name.chars().count(), 7);
    assert_eq!(bundle.bundle_id.as_deref(), Some("jp.example.エディタ"));
}

/// Nested dictionaries and arrays do not capture top-level key lookup. Real
/// `Info.plist` files embed `CFBundleDocumentTypes` and `CFBundleURLTypes`
/// arrays of dicts that reuse the very same key names; a flat scanner would
/// return the decoy values that appear *first* in the document.
#[test]
fn a_nested_dict_before_the_real_key_does_not_shadow_the_top_level_value() {
    let bundle = parsed(
        "  <key>CFBundleDocumentTypes</key>\n\
         \x20 <array>\n\
         \x20   <dict>\n\
         \x20     <key>CFBundleName</key>\n     <string>Decoy Document Type</string>\n\
         \x20     <key>CFBundleIdentifier</key>\n     <string>decoy.document.type</string>\n\
         \x20     <key>CFBundleExecutable</key>\n     <string>decoy-exec</string>\n\
         \x20   </dict>\n\
         \x20 </array>\n\
         \x20 <key>CFBundleURLTypes</key>\n\
         \x20 <dict>\n\
         \x20   <key>CFBundleDisplayName</key>\n   <string>Decoy URL Type</string>\n\
         \x20 </dict>\n\
         \x20 <key>CFBundleName</key>\n  <string>Real Application</string>\n\
         \x20 <key>CFBundleIdentifier</key>\n  <string>com.example.real</string>\n\
         \x20 <key>CFBundleExecutable</key>\n  <string>real-exec</string>",
    );

    assert_eq!(bundle.name, "Real Application");
    assert_eq!(bundle.bundle_id.as_deref(), Some("com.example.real"));
    assert_eq!(bundle.executable.as_deref(), Some("real-exec"));
}

#[test]
fn a_dictionary_decoy_or_duplicate_key_is_not_accepted_as_bundle_data() {
    let body = "  <key>CFBundleName</key>\n  <string>Real</string>";
    let well_formed = plist(body);

    let leading_decoy = well_formed.replace("<plist version=\"1.0\">", "<plist version=\"1.0\"><array/>");
    assert!(
        parse_info_plist(&leading_decoy).is_none(),
        "a dictionary after another plist child is not the payload"
    );

    let duplicate = plist(
        "  <key>CFBundleName</key>\n  <integer>42</integer>\n\
         \x20 <key>CFBundleName</key>\n  <string>Decoy</string>",
    );
    assert!(
        parse_info_plist(&duplicate).is_none(),
        "a later duplicate must not replace a malformed first value"
    );
}

// ---------------------------------------------------------------------------
// bundle_display_name
// ---------------------------------------------------------------------------

/// The `.app` suffix is stripped and the remaining name — spaces and all — is
/// returned unchanged. Kills a whitespace-trimming or first-word-only
/// implementation.
#[test]
fn a_bundle_directory_name_yields_the_name_without_the_app_suffix() {
    assert_eq!(bundle_display_name("Safari.app"), Some("Safari"));
    assert_eq!(
        bundle_display_name("Visual Studio Code.app"),
        Some("Visual Studio Code")
    );
}

/// A directory that is not a bundle yields `None`, so plain directories
/// encountered while walking `/Applications` are not indexed as applications.
#[test]
fn a_directory_without_the_app_suffix_is_not_a_bundle() {
    assert_eq!(bundle_display_name("NotABundle"), None);
    assert_eq!(bundle_display_name(""), None);
    assert_eq!(bundle_display_name("Utilities"), None);
}

/// `.app` on its own has an empty stem and is therefore not a bundle name.
/// Kills the `strip_suffix` implementation that happily returns `Some("")` and
/// puts a nameless entry in the launcher.
#[test]
fn a_bare_dot_app_is_not_a_bundle_name() {
    assert_eq!(bundle_display_name(".app"), None);
}

/// Suffix matching is exact and anchored at the end of the string. A naive
/// `split('.').next()` or `find(".app")` implementation would return `"Thing"`
/// and `"my"` here; both must be rejected or preserved exactly as spelled.
#[test]
fn suffix_matching_is_exact_and_anchored_at_the_end() {
    assert_eq!(bundle_display_name("Thing.Apple"), None);
    assert_eq!(bundle_display_name("my.app.backup"), None);
    assert_eq!(bundle_display_name("app"), None);
    assert_eq!(bundle_display_name(".application"), None);

    // A dotted stem is legitimate as long as the *final* component is `.app`.
    assert_eq!(bundle_display_name("my.app.app"), Some("my.app"));
}

/// Case sensitivity is a deliberate choice, pinned here rather than left to
/// accident: only the exact lowercase `.app` suffix names a bundle, matching
/// how Apple's own bundles and `Info.plist` keys are spelled. A directory
/// called `Safari.APP` is not recognised.
#[test]
fn the_app_suffix_is_matched_case_sensitively() {
    assert_eq!(bundle_display_name("Safari.APP"), None);
    assert_eq!(bundle_display_name("Safari.App"), None);
    assert_eq!(bundle_display_name("Safari.app"), Some("Safari"));
}
