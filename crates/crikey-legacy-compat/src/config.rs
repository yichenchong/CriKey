//! Keypirinha-style legacy configuration parsing and settings access
//! (spec 14.3 "Package formats": Keypirinha-style configuration files; 14.4
//! "API behavior": settings access; 21.1 "Configuration format"; 21.2
//! "Configuration layers").
//!
//! A faithful *raw* reader for the `.ini` files legacy packages ship. The
//! legacy format has no normative grammar, so wherever it is ambiguous
//! CPython's `configparser` decides: Keypirinha is built on `configparser`, and
//! the input is real files authored against that behaviour.
//!
//! Known gap: Keypirinha enables `configparser.ExtendedInterpolation`, but
//! this raw layer intentionally keeps `${section:key}` and `${env:NAME}`
//! references literal. Resolving them needs explicit policy for missing names,
//! environment access and reference cycles; callers must not assume expansion.
//! Rules this module holds, each defended by `tests/legacy_configuration.rs`:
//!
//! 1. Section and key lookup folds *ASCII* case only. Locale-dependent Unicode
//!    folding would make one package load differently on two machines, so
//!    `Ünicode` and `ünicode` stay two distinct keys. Values are never folded.
//! 2. The first spelling seen is the canonical one, and it is what `sections()`
//!    and `keys()` report, so a diagnostic quotes the author's own text.
//! 3. A line is a comment iff its first non-whitespace character is `#` or `;`.
//!    There are no inline comments: legacy packages keep URL fragments, regular
//!    expressions, colour literals and format strings in settings, and
//!    truncating those at a `#` would silently corrupt working packages.
//!    Quotes are plain value text; `unquote=True` is layered on top by the
//!    Python shim, not here.
//! 4. Leading whitespace before a new key or section is accepted. When a key
//!    is pending, only indentation deeper than that key's indentation continues
//!    its value; an equally indented line starts new syntax.
//! 5. A repeated key takes the last value at its first position; a repeated
//!    header merges into the section it names.
//! 6. Every typed accessor is a required read. A plugin default belongs in the
//!    package's default layer (spec 21.2 step 5), never in a silent fallback
//!    inside the parser, so absence is `Missing` and malformed data is a typed
//!    rejection instead of a coerced `0` or `false`.
//!
//! There is no implicit default section: `[DEFAULT]` is an ordinary section
//! that happens to be spelled `DEFAULT`. Section-defaulting, coercion fallbacks
//! and unquoting belong to the `keypirinha.Settings` view in the Python shim,
//! which reads the ordered section -> key -> string mapping this layer builds.
//!
//! Nothing here reads a clock, a network or the environment, and the retained
//! size is bounded by the input text: each section name, key and value is
//! copied exactly once and nothing else accumulates.

use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Every boolean spelling a legacy `.ini` may use, matched ASCII-case-
/// insensitively.
///
/// The first eight are `configparser.BOOLEAN_STATES`; the remaining spellings
/// mirror the original Keypirinha Settings API. Nothing outside this table
/// coerces: an unlisted spelling is a typed rejection, never a silent `false`
/// (rule 6).
const BOOLEAN_SPELLINGS: [(&str, bool); 16] = [
    ("yes", true),
    ("no", false),
    ("true", true),
    ("false", false),
    ("1", true),
    ("0", false),
    ("on", true),
    ("off", false),
    ("y", true),
    ("n", false),
    ("t", true),
    ("f", false),
    ("enable", true),
    ("enabled", true),
    ("disable", false),
    ("disabled", false),
];

/// A failure to read a legacy configuration file or one of its settings.
///
/// `Io` carries the operating system's explanation as a `String` rather than a
/// `std::io::Error` so the whole type stays `Clone + PartialEq + Eq`: a
/// compatibility diagnostic (spec 26.2) retains these and compares them.
///
/// Deliberately not `#[non_exhaustive]`: a report renders each variant's fields
/// by name, and a new variant must break those sites at compile time instead of
/// disappearing into a catch-all arm.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SettingsError {
    /// A line that is neither a comment, a `[section]` header, a key/value
    /// pair, nor an indented continuation of a pending value.
    ///
    /// `line` is 1-based. `content` is the line verbatim, indentation included,
    /// so a malformed indented syntax line can be corrected exactly as written.
    #[error("line {line}: not a comment, a [section] header or a key/value pair: {content}")]
    MalformedLine { line: usize, content: String },

    /// A key/value pair written before the first `[section]` header, which
    /// `configparser` reports as `MissingSectionHeaderError`. `line` is 1-based.
    #[error("line {line}: key `{key}` is written before any [section] header")]
    KeyOutsideSection { line: usize, key: String },

    /// A required setting is absent, named with the caller's spelling of the
    /// section and key because that is the text the caller can correct.
    #[error("[{section}] {key}: required setting is not set")]
    Missing { section: String, key: String },

    /// A value that is not one of the documented boolean spellings.
    #[error(
        "[{section}] {key}: `{value}` is not one of yes/no, true/false, 1/0, on/off, y/n, t/f, enable/enabled, disable/disabled"
    )]
    InvalidBool {
        section: String,
        key: String,
        value: String,
    },

    /// A value that is not a base-zero decimal, hexadecimal, octal or binary
    /// integer, or that does not fit the 64-bit range the accessor asked for.
    /// Never wrapped, never saturated.
    #[error("[{section}] {key}: `{value}` is not an integer in the supported range")]
    InvalidInteger {
        section: String,
        key: String,
        value: String,
    },

    /// A value outside the set the caller offered. `allowed` keeps the order it
    /// was offered in, so the message reads like the documentation does.
    #[error("[{section}] {key}: `{value}` is not one of {}", .allowed.join(", "))]
    InvalidEnum {
        section: String,
        key: String,
        value: String,
        allowed: Vec<String>,
    },

    /// A configuration file that could not be read, naming the exact path that
    /// was attempted and the operating system's explanation.
    #[error("cannot read legacy configuration file `{}`: {message}", .path.display())]
    Io { path: PathBuf, message: String },
}

impl SettingsError {
    /// The 1-based line this error is located at, if it is a line-level one.
    ///
    /// Matched exhaustively rather than through a catch-all arm so that a new
    /// variant has to state whether it carries a location.
    pub fn line(&self) -> Option<usize> {
        match self {
            SettingsError::MalformedLine { line, .. } | SettingsError::KeyOutsideSection { line, .. } => {
                Some(*line)
            }
            SettingsError::Missing { .. }
            | SettingsError::InvalidBool { .. }
            | SettingsError::InvalidInteger { .. }
            | SettingsError::InvalidEnum { .. }
            | SettingsError::Io { .. } => None,
        }
    }
}

/// A parsed legacy configuration: sections in file order, each holding its keys
/// in file order.
///
/// `Vec` rather than a map because order is part of the contract, and a legacy
/// `.ini` holds a few dozen keys: a linear scan over contiguous entries beats
/// hashing every lookup and keeps the author's spelling as the only spelling
/// stored. Retained size is bounded by the parsed text.
#[derive(Debug, Clone, Default)]
pub struct LegacySettings {
    sections: Vec<Section>,
}

impl LegacySettings {
    /// Parses legacy configuration text (spec 21.1).
    ///
    /// A single pass with a 1-based line counter, total on arbitrary input: a
    /// lone `]`, a UTF-8 BOM, CRLF endings and NUL bytes all resolve to a value
    /// or to a located `SettingsError`, never to a panic. A package may ship
    /// anything at all and the loader still has to survive it (spec 14.3).
    pub fn parse(text: &str) -> Result<Self, SettingsError> {
        // Stripped here rather than in `load_file` so that parsing a string and
        // loading the identical bytes cannot disagree. Left in place, the BOM
        // becomes part of the first section's name and every lookup misses.
        let body = text.strip_prefix('\u{feff}').unwrap_or(text);

        let mut parsed = LegacySettings::default();
        let mut section: Option<usize> = None;
        // The entry an indented line would continue, plus the indentation at
        // which its option started. A continuation must be deeper than that
        // baseline, matching ConfigParser's indentation rule. Cleared by a
        // blank line, by a header and by a new pair, but never by a comment:
        // comments are stripped before continuations are joined (rule 3), so
        // one written inside a continuation block drops out without ending
        // the value.
        let mut pending: Option<(usize, usize, usize)> = None;

        for (offset, raw) in body.split('\n').enumerate() {
            let number = offset + 1;
            // `split('\n')` leaves the CR of a CRLF ending attached; a stray CR
            // inside a value would leak into every later comparison.
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            let trimmed = line.trim();

            if trimmed.is_empty() {
                pending = None;
                continue;
            }
            if trimmed.starts_with(['#', ';']) {
                continue;
            }

            if line.starts_with(char::is_whitespace) {
                let indent = line
                    .chars()
                    .take_while(|character| character.is_whitespace())
                    .count();
                if let Some((owner, slot, baseline)) = pending {
                    if indent > baseline {
                        // Continuations are trimmed, so indentation depth never
                        // leaks into a value, and interior newlines survive
                        // verbatim.
                        let value = &mut parsed.sections[owner].entries[slot].value;
                        value.push('\n');
                        value.push_str(trimmed);
                        continue;
                    }
                }
                // ConfigParser permits indentation before a new option or
                // section header. If it is not deeper than a pending option,
                // fall through and parse the trimmed line as ordinary syntax.
            }

            pending = None;

            if trimmed.starts_with('[') {
                // A header owns its whole line: `[main` and `[main] extra` are
                // typos, and accepting either would silently file every key
                // that follows under a section the author never wrote.
                let header = trimmed
                    .strip_prefix('[')
                    .and_then(|inner| inner.strip_suffix(']'))
                    .map(str::trim)
                    .filter(|inner| !inner.is_empty());
                let Some(name) = header else {
                    return Err(malformed(number, line));
                };
                section = Some(parsed.section_slot(name));
                continue;
            }

            // The first delimiter splits, matching `configparser`'s defaults,
            // so a later `=` in `a = b` and the drive colon of `C:\path` stay
            // value text.
            let Some(split) = trimmed.find(['=', ':']) else {
                return Err(malformed(number, line));
            };
            let key = trimmed[..split].trim_end();
            let value = trimmed[split + 1..].trim_start();
            if key.is_empty() {
                return Err(malformed(number, line));
            }
            let Some(owner) = section else {
                return Err(SettingsError::KeyOutsideSection {
                    line: number,
                    key: key.to_owned(),
                });
            };
            let target = &mut parsed.sections[owner];
            let slot = target.slot(key);
            target.entries[slot].value = value.to_owned();
            pending = Some((
                owner,
                slot,
                line.chars()
                    .take_while(|character| character.is_whitespace())
                    .count(),
            ));
        }

        Ok(parsed)
    }

    /// Reads and parses a configuration file.
    ///
    /// An unreadable file is `Io`, never an empty default: silently swapping
    /// defaults in for a user layer that failed to open is how a launcher loses
    /// someone's settings without ever saying so. Text that is not UTF-8 fails
    /// the same way, because guessing an encoding would mangle every non-ASCII
    /// value instead of reporting one problem.
    pub fn load_file(path: &Path) -> Result<Self, SettingsError> {
        let read = fs::read_to_string(path);
        let text = read.map_err(|err| SettingsError::Io {
            path: path.to_path_buf(),
            message: err.to_string(),
        })?;
        LegacySettings::parse(&text)
    }

    /// Folds the `user` layer over the `default` layer (spec 21.2 steps 5 - 6).
    ///
    /// Only the keys `user` actually names are overridden; a key or a whole
    /// section the user layer never mentions survives untouched. The first
    /// layer to introduce a name fixes its position and spelling, the
    /// highest-priority layer that sets it fixes its value. That is what makes
    /// the fold associative, so a precedence chain can be collapsed from either
    /// end and still produce the same settings.
    pub fn layered(default: Self, user: Self) -> Self {
        let mut merged = default;
        for section in user.sections {
            let Some(index) = merged.section_index(&section.name) else {
                merged.sections.push(section);
                continue;
            };
            let target = &mut merged.sections[index];
            for entry in section.entries {
                // Moving the whole entry when the key is new keeps the user's
                // spelling and copies no string; an existing key keeps the
                // lower layer's position and spelling and takes this value.
                match target.entry_index(&entry.key) {
                    Some(slot) => target.entries[slot].value = entry.value,
                    None => target.entries.push(entry),
                }
            }
        }
        merged
    }

    /// Whether no section was declared at all. Empty and comment-only files are
    /// well formed, so a package may ship one (rule 7 of the format notes).
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// Section names in file order, in the author's spelling.
    pub fn sections(&self) -> Vec<&str> {
        let mut names = Vec::with_capacity(self.sections.len());
        for section in &self.sections {
            names.push(section.name.as_str());
        }
        names
    }

    /// Whether `section` exists, folding ASCII case exactly as `get` does.
    pub fn has_section(&self, section: &str) -> bool {
        self.section_index(section).is_some()
    }

    /// The keys of `section` in file order, in the author's spelling. An
    /// unknown section has no keys rather than being an error: enumerating is
    /// how a caller discovers what a package ships.
    pub fn keys(&self, section: &str) -> Vec<&str> {
        let Some(index) = self.section_index(section) else {
            return Vec::new();
        };
        let entries = &self.sections[index].entries;
        let mut keys = Vec::with_capacity(entries.len());
        for entry in entries {
            keys.push(entry.key.as_str());
        }
        keys
    }

    /// The verbatim value of a setting, or `None` if the section or the key is
    /// absent. The only optional read: every typed accessor is required.
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        let found = &self.sections[self.section_index(section)?];
        let entry = &found.entries[found.entry_index(key)?];
        Some(entry.value.as_str())
    }

    /// A required boolean in any documented spelling, matched ASCII-case-
    /// insensitively. An undocumented spelling is `InvalidBool`, never `false`.
    pub fn get_bool(&self, section: &str, key: &str) -> Result<bool, SettingsError> {
        let value = self.required(section, key)?;
        for (spelling, truth) in BOOLEAN_SPELLINGS {
            if value.eq_ignore_ascii_case(spelling) {
                return Ok(truth);
            }
        }
        Err(SettingsError::InvalidBool {
            section: section.to_owned(),
            key: key.to_owned(),
            value: value.to_owned(),
        })
    }

    /// A required signed integer using Python's `int(value, base=0)` grammar:
    /// decimal, hexadecimal (`0x`), octal (`0o`) and binary (`0b`), with an
    /// optional sign and surrounding whitespace already removed by the parser.
    /// Out of range is a rejection, so an overflowing value is never wrapped
    /// or saturated into a plausible one.
    pub fn get_int(&self, section: &str, key: &str) -> Result<i64, SettingsError> {
        let value = self.required(section, key)?;
        let Some((negative, magnitude)) = parse_base_zero(value) else {
            return Err(invalid_integer(section, key, value));
        };
        let parsed = if negative {
            if magnitude == 1_u64 << 63 {
                i64::MIN
            } else {
                let Ok(magnitude) = i64::try_from(magnitude) else {
                    return Err(invalid_integer(section, key, value));
                };
                -magnitude
            }
        } else {
            let Ok(magnitude) = i64::try_from(magnitude) else {
                return Err(invalid_integer(section, key, value));
            };
            magnitude
        };
        Ok(parsed)
    }

    /// A required unsigned integer using the same base-zero grammar as
    /// [`Self::get_int`]. A negative value is a rejection rather than a wrapped
    /// `u64`, because wrapping would invent a huge limit.
    pub fn get_uint(&self, section: &str, key: &str) -> Result<u64, SettingsError> {
        let value = self.required(section, key)?;
        let Some((negative, magnitude)) = parse_base_zero(value) else {
            return Err(invalid_integer(section, key, value));
        };
        if negative {
            return Err(invalid_integer(section, key, value));
        }
        Ok(magnitude)
    }

    /// A required multi-line value as trimmed, non-empty entries.
    ///
    /// `paths =` followed by indented entries leaves a leading empty line in
    /// the raw text; the list form drops it. An empty value is an empty list,
    /// which is a different answer from a missing key.
    pub fn get_multiline(&self, section: &str, key: &str) -> Result<Vec<&str>, SettingsError> {
        let value = self.required(section, key)?;
        let entries = value.split('\n').map(str::trim);
        Ok(entries.filter(|entry| !entry.is_empty()).collect())
    }

    /// A required value from `allowed`, matched ASCII-case-insensitively and
    /// returned in the caller's canonical spelling so downstream comparisons
    /// need no further folding. An unknown value is `InvalidEnum` carrying
    /// every accepted value, never a quietly chosen first option.
    pub fn get_enum<'a>(
        &self,
        section: &str,
        key: &str,
        allowed: &[&'a str],
    ) -> Result<&'a str, SettingsError> {
        let value = self.required(section, key)?;
        for &candidate in allowed {
            if value.eq_ignore_ascii_case(candidate) {
                return Ok(candidate);
            }
        }
        Err(SettingsError::InvalidEnum {
            section: section.to_owned(),
            key: key.to_owned(),
            value: value.to_owned(),
            allowed: allowed.iter().copied().map(String::from).collect(),
        })
    }

    /// A required read. The error names the caller's spelling of `section` and
    /// `key`, not the stored canonical one, because the caller's spelling is
    /// the text in the caller's source.
    fn required(&self, section: &str, key: &str) -> Result<&str, SettingsError> {
        match self.get(section, key) {
            Some(value) => Ok(value),
            None => Err(SettingsError::Missing {
                section: section.to_owned(),
                key: key.to_owned(),
            }),
        }
    }

    fn section_index(&self, name: &str) -> Option<usize> {
        self.sections.iter().position(|s| s.has_name(name))
    }

    /// Index of the section named `name`, appending it with the author's
    /// spelling if it is new.
    ///
    /// A repeated header merges into the existing section instead of opening a
    /// second one, because an `.ini` split into two `[main]` blocks is a
    /// working legacy file and truncating the first block would drop settings.
    fn section_slot(&mut self, name: &str) -> usize {
        if let Some(index) = self.section_index(name) {
            return index;
        }
        self.sections.push(Section {
            name: name.to_owned(),
            entries: Vec::new(),
        });
        self.sections.len() - 1
    }
}

/// One `[section]` and its keys, both in file order.
#[derive(Debug, Clone)]
struct Section {
    /// The author's spelling, fixed by the first header that named it.
    name: String,
    entries: Vec<Entry>,
}

impl Section {
    fn has_name(&self, name: &str) -> bool {
        same_name(&self.name, name)
    }

    fn entry_index(&self, key: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.has_key(key))
    }

    /// Index of `key`, appending an empty entry with the author's spelling if
    /// it is new.
    ///
    /// A repeated key keeps its first position and spelling and takes the last
    /// value (rule 5), so this never opens a second slot for one name.
    fn slot(&mut self, key: &str) -> usize {
        if let Some(index) = self.entry_index(key) {
            return index;
        }
        self.entries.push(Entry {
            key: key.to_owned(),
            value: String::new(),
        });
        self.entries.len() - 1
    }
}

/// Parses the lexical grammar of Python's `int(text, base=0)` for values that
/// fit a `u64` magnitude. The caller applies the requested signedness and
/// range, so overflow is rejected instead of silently clamped.
fn parse_base_zero(value: &str) -> Option<(bool, u64)> {
    let (negative, unsigned) = match value.strip_prefix(['+', '-']) {
        Some(rest) => (value.starts_with('-'), rest),
        None => (false, value),
    };
    if unsigned.is_empty() {
        return None;
    }

    let (radix, mut digits, prefixed) = if let Some(rest) = unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
    {
        (16, rest, true)
    } else if let Some(rest) = unsigned
        .strip_prefix("0o")
        .or_else(|| unsigned.strip_prefix("0O"))
    {
        (8, rest, true)
    } else if let Some(rest) = unsigned
        .strip_prefix("0b")
        .or_else(|| unsigned.strip_prefix("0B"))
    {
        (2, rest, true)
    } else {
        (10, unsigned, false)
    };

    // Python permits one underscore immediately after a radix prefix, but no
    // other leading underscore.
    if prefixed && digits.starts_with('_') {
        digits = &digits[1..];
    }
    if digits.is_empty() {
        return None;
    }

    let mut previous_was_digit = false;
    let mut saw_digit = false;
    let mut has_underscore = false;
    for character in digits.chars() {
        if character == '_' {
            previous_was_digit.then_some(())?;
            previous_was_digit = false;
            has_underscore = true;
            continue;
        }
        character.to_digit(radix)?;
        previous_was_digit = true;
        saw_digit = true;
    }
    if !saw_digit || !previous_was_digit {
        return None;
    }

    let normalized = if has_underscore {
        Cow::Owned(digits.replace('_', ""))
    } else {
        Cow::Borrowed(digits)
    };

    // Base-zero decimal accepts zero-only spellings (`00`, `0_0`) but rejects
    // a non-zero decimal with a redundant leading zero (`010`).
    if radix == 10 && normalized.starts_with('0') && normalized.chars().any(|character| character != '0') {
        return None;
    }

    let magnitude = u64::from_str_radix(&normalized, radix).ok()?;
    Some((negative, magnitude))
}
/// One key and its value.
#[derive(Debug, Clone)]
struct Entry {
    /// The author's spelling, fixed by the first line that named it.
    key: String,
    /// Value text, trimmed at both ends, interior newlines preserved.
    value: String,
}

impl Entry {
    fn has_key(&self, key: &str) -> bool {
        same_name(&self.key, key)
    }
}

/// ASCII-only case folding for a section or key name (rule 1).
///
/// `eq_ignore_ascii_case` leaves every non-ASCII byte exactly as it is, which
/// is the point: Unicode folding is locale- and version-dependent, and a
/// package that loaded on one machine has to load identically on the next.
fn same_name(stored: &str, wanted: &str) -> bool {
    stored.eq_ignore_ascii_case(wanted)
}

/// Both integer accessors reject identically and quote the caller's spelling.
fn invalid_integer(section: &str, key: &str, value: &str) -> SettingsError {
    SettingsError::InvalidInteger {
        section: section.to_owned(),
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

/// A located rejection quoting the line as written, indentation included.
fn malformed(line: usize, content: &str) -> SettingsError {
    SettingsError::MalformedLine {
        line,
        content: content.to_owned(),
    }
}
