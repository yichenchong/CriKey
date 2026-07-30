//! Keypirinha-style legacy configuration parsing and settings access
//! (spec 14.3 "Keypirinha-style configuration files", 14.4 "Settings access",
//! 21.1 "Configuration format", 21.2 "Configuration layers"; roadmap M3
//! "legacy configuration parsing").
//!
//! These tests are written before the implementation. They pin
//! `crikey_legacy_compat::{LegacySettings, SettingsError}` (owned by
//! `src/config.rs`) as the whole of the legacy configuration contract: an
//! INI-like reader for files authored for Keypirinha, plus the layered
//! default/user override model of spec 21.2 steps 5 and 6.
//!
//! Nothing here touches a clock, a network, or a thread. Only the three
//! `load_file` tests touch the filesystem, each inside its own uniquely named
//! temporary directory removed by an RAII guard.
//!
//! # Documented rules this file defends
//!
//! The legacy format has no normative grammar, so every ambiguous case is
//! decided here, once, and defended by a test. Where CPython's `configparser`
//! (what Keypirinha itself is built on) has an established answer, that answer
//! wins, because the input is real Keypirinha `.ini` files written against
//! that behaviour.
//!
//! 1. **Case sensitivity.** Section names and key names are matched
//!    *ASCII*-case-insensitively; values are never case folded. The first
//!    spelling seen in the file is the canonical one and is what `sections()`
//!    and `keys()` report, so diagnostics quote the author's text. Folding is
//!    ASCII-only and deliberately so: locale-dependent Unicode folding would
//!    make a package load differently on different machines, so `Ünicode` and
//!    `ünicode` are two distinct keys.
//! 2. **Comments.** A line is a comment if its first non-whitespace character
//!    is `#` or `;`, indented or not. There are no inline comments: everything
//!    after the delimiter is value text, `#` and `;` included. Legacy plugins
//!    store URL fragments (`https://host/page#anchor`), regular expressions,
//!    colour literals and format strings in settings; truncating those at a
//!    `#` would silently corrupt working packages, which is strictly worse
//!    than requiring a comment to own its line. Quotes are likewise plain
//!    value text - this layer never unquotes (the `keypirinha` shim's
//!    `unquote=True` option is layered on top of `get`).
//! 3. **Delimiters.** `=` and `:` both separate a key from its value and the
//!    *first* occurrence wins, matching `configparser`'s default delimiters.
//!    Key and value are each trimmed of surrounding whitespace.
//! 4. **Continuations.** Leading whitespace is the sole continuation marker.
//!    Any indented line, while a key/value pair is pending, is appended to
//!    that value as a new line - even if it looks like `key = value` or like
//!    `[section]`. Continuation lines are trimmed, so indentation depth never
//!    leaks into a value, and interior newlines are preserved. A blank line
//!    (empty or all whitespace) terminates the pending value; a comment line
//!    inside a continuation block is dropped without terminating it.
//! 5. **Duplicates.** A repeated key inside one section keeps its first
//!    position and first spelling and takes the *last* value. A repeated
//!    section header merges into the existing section instead of truncating
//!    it. Layering follows the same rule: the first layer that introduces a
//!    name fixes its position and spelling, the highest-priority layer that
//!    sets it fixes its value.
//! 6. **Errors are typed and located.** A line that is neither a comment, a
//!    valid `[section]` header, a key/value pair, nor a continuation is a
//!    `SettingsError` carrying the *1-based* line number, reachable uniformly
//!    through `SettingsError::line()`. A typed accessor never coerces bad data
//!    to a default: absence is `Missing`, malformed data is a typed rejection
//!    naming the section and key that carried it.
//! 7. **Empty input is empty, not an error.** A package that ships an empty or
//!    comment-only configuration file is well formed.
//! 8. **There is no implicit default section.** Every key belongs to an
//!    explicit `[section]`; a pair written before the first header is a hard
//!    error, exactly as `configparser` raises `MissingSectionHeaderError`.
//!    `[DEFAULT]` is an ordinary section that happens to be spelled
//!    `DEFAULT`, with no inheritance into other sections. The Rust layer is a
//!    faithful raw reader: the `keypirinha.Settings` view in the Python shim
//!    (`tests/python_api.rs`, whose `DEFAULT_SECTION` is `"DEFAULT"`) is what
//!    adds section-defaulting and coercion on top of the raw
//!    section -> key -> string mapping this layer produces.
//!
//! # Surface under test
//!
//! * `LegacySettings::parse(&str) -> Result<LegacySettings, SettingsError>`
//! * `LegacySettings::load_file(&Path) -> Result<LegacySettings, SettingsError>`
//! * `LegacySettings::layered(default: LegacySettings, user: LegacySettings)
//!   -> LegacySettings`
//! * `sections() -> Vec<&str>`, `has_section(&str) -> bool`,
//!   `keys(&str) -> Vec<&str>`, `is_empty() -> bool`
//! * `get(section, key) -> Option<&str>`
//! * `get_bool`, `get_int`, `get_uint`, `get_multiline`, `get_enum` - all
//!   `Result<_, SettingsError>`, all required (absence is an error, because a
//!   plugin default belongs in the package's default layer, not in a silent
//!   fallback inside the parser).
//! * `SettingsError::{MalformedLine, KeyOutsideSection, Missing, InvalidBool,
//!   InvalidInteger, InvalidEnum, Io}` and `SettingsError::line()`.
//!
//! `SettingsError::Io` carries a `message: String` rather than a
//! `std::io::Error` so the whole error type stays cheap to clone, compare and
//! embed in a compatibility diagnostic (spec 26.2).
//!
//! Derives these tests depend on: `LegacySettings: Debug + Clone` (folding a
//! precedence chain both ways needs the same layer twice) and
//! `SettingsError: Debug + PartialEq` (comparing a whole `Result` against
//! `Ok(..)` is what proves a typed accessor never coerces bad data to a
//! default). `Eq` and `Clone` on the error are free and worth having.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crikey_legacy_compat::{LegacySettings, SettingsError};

// ---------------------------------------------------------------------------
// Fixtures
//
// Configuration text is built from an explicit slice of lines so that leading
// whitespace is visible in the source and the 1-based line number asserted by
// an error test is the index of the literal, plus one.
// ---------------------------------------------------------------------------

fn joined(lines: &[&str]) -> String {
    lines.join("\n")
}

#[track_caller]
fn settings(lines: &[&str]) -> LegacySettings {
    LegacySettings::parse(&joined(lines))
        .unwrap_or_else(|err| panic!("well-formed legacy configuration must parse, got {err:?}"))
}

#[track_caller]
fn parse_failure(lines: &[&str]) -> SettingsError {
    match LegacySettings::parse(&joined(lines)) {
        Err(err) => err,
        Ok(parsed) => panic!("malformed legacy configuration must be rejected, got {parsed:?}"),
    }
}

/// Every key of a section paired with its value, in file order. Going through
/// both `keys()` and `get()` also pins that the two agree.
fn pairs<'a>(settings: &'a LegacySettings, section: &str) -> Vec<(&'a str, &'a str)> {
    settings
        .keys(section)
        .into_iter()
        .map(|key| {
            let value = settings
                .get(section, key)
                .unwrap_or_else(|| panic!("keys() listed {key:?} in [{section}] but get() returned None"));
            (key, value)
        })
        .collect()
}

/// The two scheduling profiles a legacy package may name (spec 14.9: the
/// `legacy-optimized` override is the only documented way to opt out of
/// `legacy-strict`). Used as the allowed set for the enum accessor.
const SCHEDULING_PROFILES: &[&str] = &["legacy-strict", "legacy-optimized"];

#[track_caller]
fn expect_malformed(err: &SettingsError, line: usize, fragment: &str) {
    match err {
        SettingsError::MalformedLine { line: at, content } => {
            assert_eq!(
                *at, line,
                "a malformed line must report its own 1-based line number"
            );
            assert!(
                content.contains(fragment),
                "a malformed line must quote the offending text, wanted {fragment:?} in {content:?}"
            );
        }
        other => panic!("expected SettingsError::MalformedLine, got {other:?}"),
    }
    assert_eq!(
        err.line(),
        Some(line),
        "line() must expose the location of every line-level error"
    );
}

#[track_caller]
fn expect_missing(err: &SettingsError, section: &str, key: &str) {
    match err {
        SettingsError::Missing {
            section: got_section,
            key: got_key,
        } => {
            assert_eq!(
                (got_section.as_str(), got_key.as_str()),
                (section, key),
                "a missing setting must name the section and key the caller asked for"
            );
        }
        other => panic!("expected SettingsError::Missing, got {other:?}"),
    }
    assert_eq!(err.line(), None, "a missing setting is not a line-level error");
}

/// A uniquely named temporary directory removed when the guard is dropped.
#[derive(Debug)]
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> TempDir {
        static NONCE: AtomicU32 = AtomicU32::new(0);
        let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0);
        let path = env::temp_dir().join(format!(
            "crikey-legacy-config-{label}-{pid}-{nonce}-{stamp}",
            pid = process::id()
        ));
        fs::create_dir_all(&path)
            .unwrap_or_else(|err| panic!("temporary directory {path:?} must be creatable: {err}"));
        TempDir { path }
    }

    fn write(&self, name: &str, lines: &[&str]) -> PathBuf {
        let file = self.path.join(name);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|err| panic!("fixture directory {parent:?} must exist: {err}"));
        }
        fs::write(&file, joined(lines))
            .unwrap_or_else(|err| panic!("fixture file {file:?} must be writable: {err}"));
        file
    }

    fn absent(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Structure: order, spelling and lookup
// ---------------------------------------------------------------------------

#[test]
fn sections_and_keys_are_reported_in_file_order_with_their_original_spelling() {
    let parsed = settings(&[
        "[Main]",
        "Show_Icons = yes",
        "history_limit = 25",
        "",
        "[Search Paths]",
        "Extra = /opt/tools",
    ]);

    assert_eq!(
        parsed.sections(),
        vec!["Main", "Search Paths"],
        "sections must be enumerated in file order with the author's spelling"
    );
    assert_eq!(
        pairs(&parsed, "Main"),
        vec![("Show_Icons", "yes"), ("history_limit", "25")],
        "keys must be enumerated in file order with the author's spelling"
    );
    assert_eq!(
        pairs(&parsed, "Search Paths"),
        vec![("Extra", "/opt/tools")],
        "a section name may contain spaces and is trimmed, not split"
    );
}

#[test]
fn section_and_key_lookup_is_ascii_case_insensitive_while_values_stay_verbatim() {
    let parsed = settings(&["[Main]", "Show_Icons = Yes", "Ünicode = data"]);

    assert_eq!(
        parsed.get("main", "show_icons"),
        Some("Yes"),
        "a lower-cased lookup must find a mixed-case section and key"
    );
    assert_eq!(
        parsed.get("MAIN", "SHOW_ICONS"),
        Some("Yes"),
        "an upper-cased lookup must find the same setting"
    );
    assert!(
        parsed.has_section("mAiN"),
        "has_section must fold case the same way get() does"
    );
    assert_eq!(
        parsed.get("main", "show_icons"),
        Some("Yes"),
        "the value must be returned verbatim and never case folded"
    );
    assert_eq!(
        parsed.sections(),
        vec!["Main"],
        "case-insensitive lookup must not rewrite the stored section spelling"
    );
    assert_eq!(
        parsed.keys("MAIN"),
        vec!["Show_Icons", "Ünicode"],
        "case-insensitive lookup must not rewrite the stored key spelling"
    );

    assert_eq!(
        parsed.get("main", "Ünicode"),
        Some("data"),
        "a non-ASCII key must still be found by its exact spelling"
    );
    assert_eq!(
        parsed.get("main", "ünicode"),
        None,
        "case folding is ASCII-only, so a non-ASCII key must not fold"
    );

    assert_eq!(
        parsed.get_bool("MAIN", "show_icons"),
        Ok(true),
        "typed accessors must use the same case-insensitive lookup as get()"
    );
}

#[test]
fn keys_of_an_unknown_section_are_empty_and_has_section_reports_it_honestly() {
    let parsed = settings(&["[main]", "a = 1"]);

    assert!(
        !parsed.has_section("advanced"),
        "has_section must report an absent section as absent"
    );
    assert!(
        parsed.keys("advanced").is_empty(),
        "keys() of an unknown section must be empty rather than panic"
    );
    assert_eq!(
        parsed.get("advanced", "a"),
        None,
        "a key in an unknown section must be absent, not inherited from another section"
    );
}

#[test]
fn a_section_named_default_is_ordinary_and_never_leaks_into_other_sections() {
    let parsed = settings(&["[DEFAULT]", "shared = fallback", "[main]", "own = 1"]);

    assert_eq!(
        parsed.sections(),
        vec!["DEFAULT", "main"],
        "[DEFAULT] is an ordinary section and must be enumerated like any other"
    );
    assert_eq!(
        parsed.get("default", "shared"),
        Some("fallback"),
        "[DEFAULT] is looked up case-insensitively like any other section"
    );
    assert_eq!(
        parsed.get("main", "shared"),
        None,
        "this layer performs no default-section inheritance: that view semantic \
         belongs to the keypirinha.Settings shim, not to the raw reader"
    );
    assert_eq!(
        pairs(&parsed, "main"),
        vec![("own", "1")],
        "a section must expose exactly its own keys"
    );
}

// ---------------------------------------------------------------------------
// Comments and value text
// ---------------------------------------------------------------------------

#[test]
fn hash_and_semicolon_comment_lines_are_ignored_wherever_they_appear() {
    let parsed = settings(&[
        "# a leading hash comment",
        "; a leading semicolon comment",
        "[main]",
        "    # an indented comment is still a comment",
        "a = 1",
        ";b = 2",
        "#c = 3",
    ]);

    assert_eq!(
        parsed.sections(),
        vec!["main"],
        "comment lines must not create sections"
    );
    assert_eq!(
        pairs(&parsed, "main"),
        vec![("a", "1")],
        "a commented-out key must not be parsed as a key"
    );
    assert_eq!(
        parsed.get("main", "b"),
        None,
        "a semicolon-commented key must be absent"
    );
    assert_eq!(
        parsed.get("main", "c"),
        None,
        "a hash-commented key must be absent"
    );
}

#[test]
fn a_hash_or_semicolon_inside_a_value_is_data_and_never_starts_a_comment() {
    let parsed = settings(&[
        "[main]",
        "url = https://example.test/page#fragment",
        "quoted = \"value # not a comment\"",
        "semicolons = a;b;c",
        "equation = a = b",
        "colour = #ff8800",
    ]);

    assert_eq!(
        parsed.get("main", "url"),
        Some("https://example.test/page#fragment"),
        "a URL fragment must survive: there are no inline comments"
    );
    assert_eq!(
        parsed.get("main", "quoted"),
        Some("\"value # not a comment\""),
        "quotes are plain value text and a quoted hash is not a comment"
    );
    assert_eq!(
        parsed.get("main", "semicolons"),
        Some("a;b;c"),
        "a semicolon inside a value must not start a comment"
    );
    assert_eq!(
        parsed.get("main", "equation"),
        Some("a = b"),
        "only the first delimiter splits, so a later '=' is value text"
    );
    assert_eq!(
        parsed.get("main", "colour"),
        Some("#ff8800"),
        "a value may begin with '#' once the delimiter has been seen"
    );
}

#[test]
fn either_delimiter_is_accepted_and_the_first_one_on_the_line_wins() {
    let parsed = settings(&[
        "[main]",
        "theme: dark",
        r"path = C:\Users\test",
        "mixed: a = b",
        "  spaced_out   =   trimmed value   ",
        "tight=packed",
    ]);

    assert_eq!(
        parsed.get("main", "theme"),
        Some("dark"),
        "':' must be accepted as a key/value delimiter"
    );
    assert_eq!(
        parsed.get("main", "path"),
        Some(r"C:\Users\test"),
        "a colon inside a value must not split the line once '=' came first"
    );
    assert_eq!(
        parsed.get("main", "tight"),
        Some("packed"),
        "a delimiter needs no surrounding whitespace"
    );
    assert_eq!(
        parsed.get("main", "spaced_out"),
        None,
        "an indented line is a continuation, never a new key, whichever delimiter it uses"
    );
    assert_eq!(
        parsed.get("main", "mixed"),
        Some("a = b\nspaced_out   =   trimmed value"),
        "the first delimiter splits the line, and the indented line that follows is \
         appended to that value with its own outer whitespace trimmed"
    );
}

// ---------------------------------------------------------------------------
// Continuations
// ---------------------------------------------------------------------------

#[test]
fn indented_continuation_lines_join_into_one_value_with_interior_newlines() {
    let parsed = settings(&[
        "[main]",
        "description = first",
        "    second",
        "\tthird",
        "next = plain",
    ]);

    assert_eq!(
        parsed.get("main", "description"),
        Some("first\nsecond\nthird"),
        "continuation lines join with interior newlines and lose their indentation"
    );
    assert_eq!(
        pairs(&parsed, "main"),
        vec![("description", "first\nsecond\nthird"), ("next", "plain")],
        "a continuation must not create extra keys and must not swallow the next key"
    );
}

#[test]
fn an_indented_line_is_continuation_text_even_when_it_looks_like_syntax() {
    let parsed = settings(&[
        "[main]",
        "block = start",
        "    nested = value",
        "    [not a section]",
        "outside = 1",
    ]);

    assert_eq!(
        parsed.get("main", "block"),
        Some("start\nnested = value\n[not a section]"),
        "indentation is the sole continuation marker, so key- and header-shaped lines are text"
    );
    assert_eq!(
        parsed.sections(),
        vec!["main"],
        "an indented header must not open a section"
    );
    assert_eq!(
        parsed.get("main", "nested"),
        None,
        "an indented key-shaped line must not become a key"
    );
    assert_eq!(
        parsed.get("main", "outside"),
        Some("1"),
        "an unindented key after a continuation block is a normal key"
    );
}

#[test]
fn a_comment_line_inside_a_continuation_block_is_dropped_without_breaking_the_value() {
    let parsed = settings(&[
        "[main]",
        "block = one",
        "    # explanation of the next entry",
        "    two",
        "; unindented comment inside the block",
        "    three",
    ]);

    assert_eq!(
        parsed.get("main", "block"),
        Some("one\ntwo\nthree"),
        "comment lines are removed before continuations are joined"
    );
}

#[test]
fn a_blank_line_terminates_a_multi_line_value() {
    let continued = settings(&["[main]", "block = one", "  two"]);
    assert_eq!(
        continued.get("main", "block"),
        Some("one\ntwo"),
        "an indented line right after a value continues it"
    );

    let interrupted = parse_failure(&["[main]", "block = one", "", "  two"]);
    expect_malformed(&interrupted, 4, "two");
}

// ---------------------------------------------------------------------------
// Duplicates
// ---------------------------------------------------------------------------

#[test]
fn a_duplicate_key_in_one_section_keeps_its_first_position_and_takes_the_last_value() {
    let parsed = settings(&["[main]", "a = 1", "b = 2", "a = 3", "A = 4"]);

    assert_eq!(
        pairs(&parsed, "main"),
        vec![("a", "4"), ("b", "2")],
        "a duplicate key keeps its first position and spelling and takes the last value"
    );
}

#[test]
fn a_repeated_section_header_merges_into_the_existing_section() {
    let parsed = settings(&["[main]", "a = 1", "[other]", "b = 2", "[Main]", "c = 3", "a = 9"]);

    assert_eq!(
        parsed.sections(),
        vec!["main", "other"],
        "a repeated section header must not create a second section"
    );
    assert_eq!(
        pairs(&parsed, "main"),
        vec![("a", "9"), ("c", "3")],
        "a repeated section merges its keys in rather than truncating the first block"
    );
    assert_eq!(
        pairs(&parsed, "other"),
        vec![("b", "2")],
        "merging a repeated section must not disturb the sections between the two blocks"
    );
}

// ---------------------------------------------------------------------------
// Layering (spec 21.2 steps 5 and 6)
// ---------------------------------------------------------------------------

#[test]
fn the_user_layer_overrides_only_the_keys_it_names() {
    let default = settings(&["[main]", "show_icons = yes", "history_limit = 25", "theme = dark"]);
    let user = settings(&["[main]", "history_limit = 100"]);

    let merged = LegacySettings::layered(default, user);

    assert_eq!(
        pairs(&merged, "main"),
        vec![("show_icons", "yes"), ("history_limit", "100"), ("theme", "dark"),],
        "the user layer replaces only the keys it names and never reorders the defaults"
    );
}

#[test]
fn a_section_present_only_in_the_default_layer_survives_layering_whole() {
    let default = settings(&[
        "[main]",
        "show_icons = yes",
        "[advanced]",
        "cache_size = 64",
        "trace = no",
    ]);
    let user = settings(&["[main]", "show_icons = no"]);

    let merged = LegacySettings::layered(default, user);

    assert_eq!(
        merged.sections(),
        vec!["main", "advanced"],
        "a section the user layer never mentions must survive layering"
    );
    assert_eq!(
        pairs(&merged, "advanced"),
        vec![("cache_size", "64"), ("trace", "no")],
        "an untouched section must keep every key and value from the default layer"
    );
    assert_eq!(
        merged.get("main", "show_icons"),
        Some("no"),
        "the user layer still wins for the keys it does name"
    );
}

#[test]
fn user_only_sections_and_keys_are_appended_after_the_defaults() {
    let default = settings(&["[Main]", "show_icons = yes"]);
    let user = settings(&["[MAIN]", "Show_Icons = no", "new_key = 1", "[User Only]", "x = 2"]);

    let merged = LegacySettings::layered(default, user);

    assert_eq!(
        merged.sections(),
        vec!["Main", "User Only"],
        "the default layer fixes the spelling and position of a shared section"
    );
    assert_eq!(
        pairs(&merged, "main"),
        vec![("show_icons", "no"), ("new_key", "1")],
        "an overridden key keeps the default spelling and position; a user-only key is appended"
    );
    assert_eq!(
        pairs(&merged, "user only"),
        vec![("x", "2")],
        "a section only the user layer defines must be present in full"
    );
}

#[test]
fn layering_is_associative_so_precedence_can_be_folded_in_either_direction() {
    let builtin = settings(&["[main]", "a = builtin", "shared = builtin"]);
    let package = settings(&["[main]", "shared = package", "[package]", "b = package"]);
    let user = settings(&["[main]", "shared = user", "[user]", "c = user"]);

    let left = LegacySettings::layered(
        LegacySettings::layered(builtin.clone(), package.clone()),
        user.clone(),
    );
    let right = LegacySettings::layered(builtin, LegacySettings::layered(package, user));

    assert_eq!(
        left.sections(),
        right.sections(),
        "folding the precedence chain left or right must produce the same section order"
    );
    for section in left.sections() {
        assert_eq!(
            pairs(&left, section),
            pairs(&right, section),
            "folding the precedence chain left or right must produce the same [{section}]"
        );
    }
    assert_eq!(
        left.get("main", "shared"),
        Some("user"),
        "the highest-priority layer must win regardless of fold direction"
    );
    assert_eq!(
        left.get("main", "a"),
        Some("builtin"),
        "a key only the lowest layer defines must survive the whole chain"
    );
}

// ---------------------------------------------------------------------------
// Typed accessors: booleans
// ---------------------------------------------------------------------------

#[test]
fn get_bool_accepts_the_documented_spellings_case_insensitively() {
    let parsed = settings(&[
        "[flags]",
        "a = yes",
        "b = no",
        "c = true",
        "d = false",
        "e = 1",
        "f = 0",
        "g = on",
        "h = off",
        "i = YES",
        "j = False",
        "k = On",
    ]);

    for (key, expected) in [
        ("a", true),
        ("b", false),
        ("c", true),
        ("d", false),
        ("e", true),
        ("f", false),
        ("g", true),
        ("h", false),
        ("i", true),
        ("j", false),
        ("k", true),
    ] {
        assert_eq!(
            parsed.get_bool("flags", key),
            Ok(expected),
            "yes/no, true/false, 1/0 and on/off are the documented boolean spellings, \
             matched case-insensitively (key {key:?})"
        );
    }
}

#[test]
fn get_bool_rejects_an_undocumented_spelling_and_names_the_section_and_key() {
    let parsed = settings(&[
        "[flags]",
        "maybe = y",
        "wordy = enabled",
        "numeric = 2",
        "blank =",
    ]);

    for key in ["maybe", "wordy", "numeric", "blank"] {
        let err = parsed
            .get_bool("flags", key)
            .expect_err("an undocumented boolean spelling must be rejected, never coerced");
        let expected_value = parsed.get("flags", key).expect("fixture key must exist");
        match &err {
            SettingsError::InvalidBool {
                section,
                key: got_key,
                value,
            } => {
                assert_eq!(
                    (section.as_str(), got_key.as_str(), value.as_str()),
                    ("flags", key, expected_value),
                    "an invalid boolean must name its section, key and offending value"
                );
            }
            other => panic!("expected SettingsError::InvalidBool, got {other:?}"),
        }
        let rendered = err.to_string();
        assert!(
            rendered.contains("flags") && rendered.contains(key),
            "the rendered boolean error must name the section and key, got {rendered:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Typed accessors: integers
// ---------------------------------------------------------------------------

#[test]
fn get_int_parses_signed_decimal_integers_at_the_edges_of_the_range() {
    let parsed = settings(&[
        "[limits]",
        "zero = 0",
        "positive = +7",
        "negative = -3",
        "max = 9223372036854775807",
        "min = -9223372036854775808",
        "padded =   42  ",
    ]);

    assert_eq!(parsed.get_int("limits", "zero"), Ok(0), "0 must parse");
    assert_eq!(
        parsed.get_int("limits", "positive"),
        Ok(7),
        "an explicit '+' sign must be accepted"
    );
    assert_eq!(
        parsed.get_int("limits", "negative"),
        Ok(-3),
        "a negative signed integer must be accepted"
    );
    assert_eq!(
        parsed.get_int("limits", "max"),
        Ok(i64::MAX),
        "the largest representable value must parse"
    );
    assert_eq!(
        parsed.get_int("limits", "min"),
        Ok(i64::MIN),
        "the smallest representable value must parse"
    );
    assert_eq!(
        parsed.get_int("limits", "padded"),
        Ok(42),
        "surrounding whitespace is trimmed by the parser, not rejected by the accessor"
    );
}

#[test]
fn get_int_rejects_a_non_numeric_value_with_a_typed_error_instead_of_zero() {
    let parsed = settings(&[
        "[limits]",
        "word = abc",
        "hex = 0x10",
        "fractional = 1.5",
        "suffixed = 12 items",
        "blank =",
    ]);

    for key in ["word", "hex", "fractional", "suffixed", "blank"] {
        let result = parsed.get_int("limits", key);
        assert_ne!(
            result,
            Ok(0),
            "a non-numeric value must never be silently coerced to zero (key {key:?})"
        );
        let err = result.expect_err("a non-numeric value must be a typed error");
        let expected_value = parsed.get("limits", key).expect("fixture key must exist");
        match &err {
            SettingsError::InvalidInteger {
                section,
                key: got_key,
                value,
            } => {
                assert_eq!(
                    (section.as_str(), got_key.as_str(), value.as_str()),
                    ("limits", key, expected_value),
                    "an invalid integer must name its section, key and offending value"
                );
            }
            other => panic!("expected SettingsError::InvalidInteger, got {other:?}"),
        }
    }
}

#[test]
fn get_int_rejects_a_value_that_overflows_the_signed_range() {
    let parsed = settings(&[
        "[limits]",
        "too_big = 9223372036854775808",
        "too_small = -9223372036854775809",
        "absurd = 100000000000000000000000000000",
    ]);

    for key in ["too_big", "too_small", "absurd"] {
        let err = parsed
            .get_int("limits", key)
            .expect_err("an out-of-range integer must be rejected, never wrapped or saturated");
        let rendered = err.to_string();
        let raw = parsed.get("limits", key).expect("fixture key must exist");
        assert!(
            matches!(err, SettingsError::InvalidInteger { .. }),
            "an overflowing integer must be an InvalidInteger, got {err:?}"
        );
        assert!(
            rendered.contains(raw),
            "the rendered error must quote the offending digits, got {rendered:?}"
        );
    }
}

#[test]
fn get_uint_rejects_a_negative_value_while_get_int_still_accepts_it() {
    let parsed = settings(&[
        "[limits]",
        "negative = -1",
        "zero = 0",
        "large = 18446744073709551615",
    ]);

    assert_eq!(
        parsed.get_int("limits", "negative"),
        Ok(-1),
        "the same text is a valid signed integer, so the rejection below is about the type asked for"
    );
    let err = parsed
        .get_uint("limits", "negative")
        .expect_err("a negative value must be rejected by an unsigned accessor, never wrapped");
    assert!(
        matches!(err, SettingsError::InvalidInteger { .. }),
        "a negative unsigned value must be an InvalidInteger, got {err:?}"
    );
    assert_eq!(
        parsed.get_uint("limits", "zero"),
        Ok(0),
        "zero is a valid unsigned value"
    );
    assert_eq!(
        parsed.get_uint("limits", "large"),
        Ok(u64::MAX),
        "the largest representable unsigned value must parse"
    );
}

// ---------------------------------------------------------------------------
// Typed accessors: lists and enums
// ---------------------------------------------------------------------------

#[test]
fn get_multiline_splits_a_continued_value_into_trimmed_non_empty_entries() {
    let parsed = settings(&[
        "[main]",
        "paths =",
        "    /opt/one",
        "    /opt/two",
        "single = /opt/solo",
        "blank =",
    ]);

    assert_eq!(
        parsed.get("main", "paths"),
        Some("\n/opt/one\n/opt/two"),
        "a value that starts on the next line keeps its leading empty line in the raw text"
    );
    assert_eq!(
        parsed.get_multiline("main", "paths"),
        Ok(vec!["/opt/one", "/opt/two"]),
        "the list form drops empty entries, so the leading empty line disappears"
    );
    assert_eq!(
        parsed.get_multiline("main", "single"),
        Ok(vec!["/opt/solo"]),
        "a single-line value is a one-entry list"
    );
    assert_eq!(
        parsed.get_multiline("main", "blank"),
        Ok(Vec::new()),
        "an empty value is an empty list, which is distinct from a missing key"
    );
}

#[test]
fn get_enum_matches_case_insensitively_and_returns_the_canonical_spelling() {
    let parsed = settings(&[
        "[scheduling]",
        "profile = Legacy-Strict",
        "override = legacy-optimized",
    ]);

    assert_eq!(
        parsed.get_enum("scheduling", "profile", SCHEDULING_PROFILES),
        Ok("legacy-strict"),
        "an enum value must match case-insensitively and come back in its canonical spelling"
    );
    assert_eq!(
        parsed.get_enum("scheduling", "override", SCHEDULING_PROFILES),
        Ok("legacy-optimized"),
        "an exactly spelled enum value must round-trip"
    );
}

#[test]
fn get_enum_rejects_an_unknown_value_and_its_error_names_the_accepted_values() {
    let parsed = settings(&["[scheduling]", "profile = turbo"]);

    let err = parsed
        .get_enum("scheduling", "profile", SCHEDULING_PROFILES)
        .expect_err("an unknown enum value must be rejected, never defaulted");

    match &err {
        SettingsError::InvalidEnum {
            section,
            key,
            value,
            allowed,
        } => {
            assert_eq!(
                (section.as_str(), key.as_str(), value.as_str()),
                ("scheduling", "profile", "turbo"),
                "an invalid enum must name its section, key and offending value"
            );
            assert_eq!(
                allowed.iter().map(String::as_str).collect::<Vec<_>>(),
                SCHEDULING_PROFILES.to_vec(),
                "an invalid enum must carry the accepted values in the order they were offered"
            );
        }
        other => panic!("expected SettingsError::InvalidEnum, got {other:?}"),
    }

    let rendered = err.to_string();
    for accepted in SCHEDULING_PROFILES {
        assert!(
            rendered.contains(accepted),
            "the rendered enum error must list {accepted:?}, got {rendered:?}"
        );
    }
    assert!(
        rendered.contains("turbo"),
        "the rendered enum error must quote the offending value, got {rendered:?}"
    );
}

// ---------------------------------------------------------------------------
// Absence
// ---------------------------------------------------------------------------

#[test]
fn a_missing_key_is_none_from_get_and_a_typed_missing_error_from_typed_accessors() {
    let parsed = settings(&["[main]", "present = 1"]);

    assert_eq!(
        parsed.get("main", "absent"),
        None,
        "an absent key must be None, which is how an optional setting is read"
    );
    assert_eq!(
        parsed.get("nowhere", "absent"),
        None,
        "an absent section must also be None rather than an error from get()"
    );

    expect_missing(
        &parsed
            .get_bool("main", "absent")
            .expect_err("a required boolean must fail when the key is absent"),
        "main",
        "absent",
    );
    expect_missing(
        &parsed
            .get_int("main", "absent")
            .expect_err("a required integer must fail when the key is absent"),
        "main",
        "absent",
    );
    expect_missing(
        &parsed
            .get_uint("main", "absent")
            .expect_err("a required unsigned integer must fail when the key is absent"),
        "main",
        "absent",
    );
    expect_missing(
        &parsed
            .get_multiline("main", "absent")
            .expect_err("a required list must fail when the key is absent"),
        "main",
        "absent",
    );
    expect_missing(
        &parsed
            .get_enum("main", "absent", SCHEDULING_PROFILES)
            .expect_err("a required enum must fail when the key is absent"),
        "main",
        "absent",
    );
    expect_missing(
        &parsed
            .get_int("nowhere", "present")
            .expect_err("a key in an absent section must fail as missing"),
        "nowhere",
        "present",
    );
    expect_missing(
        &parsed
            .get_int("MAIN", "Absent")
            .expect_err("a required integer must fail when the key is absent"),
        "MAIN",
        "Absent",
    );
}

// ---------------------------------------------------------------------------
// Malformed input
// ---------------------------------------------------------------------------

#[test]
fn a_line_that_is_neither_comment_header_nor_pair_is_rejected_with_its_line_number() {
    let err = parse_failure(&["# comment", "[main]", "a = 1", "enable_fancy", "b = 2"]);
    expect_malformed(&err, 4, "enable_fancy");
}

#[test]
fn a_key_value_pair_before_any_section_header_is_rejected_with_its_line_number() {
    let err = parse_failure(&["# comment", "orphan = 1", "[main]", "a = 2"]);

    match &err {
        SettingsError::KeyOutsideSection { line, key } => {
            assert_eq!(
                *line, 2,
                "a key outside a section must report its own 1-based line number"
            );
            assert_eq!(
                key, "orphan",
                "a key outside a section must name the offending key"
            );
        }
        other => panic!("expected SettingsError::KeyOutsideSection, got {other:?}"),
    }
    assert_eq!(
        err.line(),
        Some(2),
        "line() must expose the location of a key written outside any section"
    );
}

#[test]
fn an_invalid_section_header_is_rejected_with_its_line_number() {
    expect_malformed(&parse_failure(&["[main", "a = 1"]), 1, "[main");
    expect_malformed(&parse_failure(&["[]", "a = 1"]), 1, "[]");
    expect_malformed(&parse_failure(&["[main]", "trailing]"]), 2, "trailing]");
    expect_malformed(&parse_failure(&["[main] extra", "a = 1"]), 1, "[main] extra");
}

#[test]
fn an_indented_line_with_no_pending_value_is_rejected_with_its_line_number() {
    expect_malformed(&parse_failure(&["[main]", "    orphan = 1"]), 2, "orphan");
    expect_malformed(&parse_failure(&["    orphan", "[main]"]), 1, "orphan");
}

#[test]
fn a_key_value_line_with_an_empty_key_is_rejected_with_its_line_number() {
    expect_malformed(&parse_failure(&["[main]", "a = 1", "= 2"]), 3, "= 2");
    expect_malformed(&parse_failure(&["[main]", ": 2"]), 2, ": 2");
}

// ---------------------------------------------------------------------------
// Degenerate but valid input
// ---------------------------------------------------------------------------

#[test]
fn empty_input_parses_to_an_empty_settings_object_rather_than_an_error() {
    for (label, text) in [
        ("empty", ""),
        ("newline only", "\n"),
        ("whitespace only", "   \n\t\n"),
        ("comments only", "# nothing here\n; nor here\n"),
    ] {
        let parsed = LegacySettings::parse(text)
            .unwrap_or_else(|err| panic!("{label} input must parse to empty settings, got {err:?}"));
        assert!(
            parsed.is_empty(),
            "{label} input must produce an empty settings object"
        );
        assert!(
            parsed.sections().is_empty(),
            "{label} input must produce no sections"
        );
        assert_eq!(
            parsed.get("main", "anything"),
            None,
            "{label} input must not answer any lookup"
        );
    }

    let populated = settings(&["[main]", "a = 1"]);
    assert!(
        !populated.is_empty(),
        "settings with a section must not report themselves as empty"
    );
}

#[test]
fn a_windows_authored_file_with_a_bom_and_crlf_endings_parses_like_its_unix_twin() {
    let windows =
        LegacySettings::parse("\u{feff}[Main]\r\nshow_icons = yes\r\ndescription = first\r\n    second\r\n")
            .expect("a BOM-prefixed CRLF file authored on Windows must parse");
    let unix = settings(&["[Main]", "show_icons = yes", "description = first", "    second"]);

    assert_eq!(
        windows.sections(),
        unix.sections(),
        "a UTF-8 BOM must be stripped and must not become part of the first section name"
    );
    assert_eq!(
        pairs(&windows, "Main"),
        pairs(&unix, "Main"),
        "CRLF line endings must be normalised, leaving no stray carriage return in any value"
    );
    assert_eq!(
        windows.get("main", "description"),
        Some("first\nsecond"),
        "a continued CRLF value must join with plain newlines"
    );
}

// ---------------------------------------------------------------------------
// Errors as diagnostics
// ---------------------------------------------------------------------------

#[test]
fn settings_errors_are_std_errors_and_only_line_level_failures_carry_a_line() {
    let line_level = parse_failure(&["[main]", "not a pair"]);
    let value_level = settings(&["[main]", "flag = maybe"])
        .get_bool("main", "flag")
        .expect_err("an undocumented boolean spelling must be rejected");

    let as_std: &dyn std::error::Error = &line_level;
    assert!(
        as_std.source().is_none(),
        "a configuration error is a leaf error and must not hide a source"
    );
    assert_eq!(
        as_std.to_string(),
        line_level.to_string(),
        "SettingsError must implement std::error::Error with its own Display"
    );

    assert_eq!(
        line_level.line(),
        Some(2),
        "a line-level error must expose its 1-based line number"
    );
    assert_eq!(
        value_level.line(),
        None,
        "a value-level error has no line to report and must say so rather than guess"
    );
}

// ---------------------------------------------------------------------------
// Loading a package configuration from disk
// ---------------------------------------------------------------------------

#[test]
fn load_file_reads_a_package_default_and_layers_a_user_file_over_it() {
    let temp = TempDir::new("layered");
    let default_path = temp.write(
        "package/Example.ini",
        &[
            "[main]",
            "show_icons = yes",
            "history_limit = 25",
            "[advanced]",
            "trace = no",
        ],
    );
    let user_path = temp.write("user/Example.ini", &["[main]", "history_limit = 100"]);

    let default = LegacySettings::load_file(&default_path)
        .expect("a well-formed package default configuration must load");
    let user = LegacySettings::load_file(&user_path).expect("a well-formed user configuration must load");
    let merged = LegacySettings::layered(default, user);

    assert_eq!(
        pairs(&merged, "main"),
        vec![("show_icons", "yes"), ("history_limit", "100")],
        "loading from disk must produce the same layering as parsing from memory"
    );
    assert_eq!(
        pairs(&merged, "advanced"),
        vec![("trace", "no")],
        "a section only the package default defines must survive loading and layering"
    );
}

#[test]
fn load_file_reports_a_missing_file_as_a_typed_io_error_naming_the_path() {
    let temp = TempDir::new("missing");
    let path = temp.absent("Nothing.ini");

    let err = LegacySettings::load_file(&path)
        .expect_err("a missing configuration file must be a typed error, not an empty default");

    match &err {
        SettingsError::Io {
            path: reported,
            message,
        } => {
            assert_eq!(
                reported, &path,
                "an I/O error must name the exact path that could not be read"
            );
            assert!(
                !message.is_empty(),
                "an I/O error must carry the operating system's explanation"
            );
        }
        other => panic!("expected SettingsError::Io, got {other:?}"),
    }
    assert_eq!(
        err.line(),
        None,
        "a file that could not be opened has no offending line"
    );
    assert!(
        err.to_string().contains("Nothing.ini"),
        "the rendered I/O error must name the file, got {rendered:?}",
        rendered = err.to_string()
    );
}

#[test]
fn load_file_propagates_a_parse_error_with_its_line_number() {
    let temp = TempDir::new("malformed");
    let path = temp.write(
        "package/Broken.ini",
        &["[main]", "a = 1", "this line is broken", "b = 2"],
    );

    let err = LegacySettings::load_file(&path).expect_err("a malformed configuration file must fail to load");

    expect_malformed(&err, 3, "this line is broken");
}
