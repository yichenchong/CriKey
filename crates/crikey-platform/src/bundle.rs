//! macOS application-bundle parsing (spec 18.5).
//!
//! The macOS backend crate is gated on `target_os = "macos"` and therefore
//! cannot be exercised anywhere else, so the part of bundle discovery that is
//! pure data transformation -- reading an `Info.plist`, naming a `Foo.app`
//! directory -- lives here instead, where every host can test it. The backend
//! keeps only the filesystem walk and the OS bindings.
//!
//! The parser is hand written against the subset of XML that Apple's plist
//! documents use. A general XML crate would be a dependency in the workspace
//! root pulled in for one file format on one platform, and the subset is small:
//! elements, attributes we ignore, character data, entities, comments,
//! processing instructions, CDATA and the doctype.
//!
//! Two properties matter more than coverage. Discovery walks third-party
//! bundles nobody here controls, so a truncated, unbalanced or outright binary
//! `Info.plist` must skip one application rather than abort a scan or panic.
//! And key lookup is nesting aware: real documents embed `CFBundleDocumentTypes`
//! and `CFBundleURLTypes` arrays whose inner dictionaries reuse the very same
//! key names, so a flat scan for the first `<key>CFBundleName</key>` would
//! index the decoy that happens to appear first.

use std::path::{Component, Path, PathBuf};

/// What a launcher needs out of an `Info.plist`.
///
/// A bundle without a usable display name is not a value of this type at all:
/// [`parse_info_plist`] rejects the document instead, so a nameless entry can
/// never reach the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppBundle {
    pub name: String,
    pub bundle_id: Option<String>,
    pub executable: Option<String>,
    /// The `Resources` file holding the bundle's icon, as the plist spells it:
    /// with or without the `.icns` extension, since Apple's tools accept both.
    /// [`bundle_icon_path`] is what turns it into a file.
    pub icon_file: Option<String>,
}

/// The user-visible name, preferred over [`BUNDLE_NAME_KEY`] wherever both are
/// declared: the short internal name belongs in the filesystem, not in the UI.
const DISPLAY_NAME_KEY: &str = "CFBundleDisplayName";
const BUNDLE_NAME_KEY: &str = "CFBundleName";
const IDENTIFIER_KEY: &str = "CFBundleIdentifier";
const EXECUTABLE_KEY: &str = "CFBundleExecutable";
const ICON_FILE_KEY: &str = "CFBundleIconFile";

/// The suffix that names an application bundle, spelled exactly as Apple
/// spells it. Matching is case sensitive on purpose: HFS+ and APFS are
/// case insensitive by default but preserve case, so a directory called
/// `Safari.APP` is something somebody typed by hand rather than a bundle any
/// Apple tool produced.
const BUNDLE_SUFFIX: &str = ".app";

/// Where a bundle keeps the resources `CFBundleIconFile` names.
const BUNDLE_RESOURCES: &str = "Contents/Resources";

/// The extension `CFBundleIconFile` is allowed to omit.
const ICNS_SUFFIX: &str = ".icns";

/// Reads the five keys a launcher consumes out of an `Info.plist` document.
///
/// `None` means "nothing launchable here": the document is not well-formed XML,
/// carries no top-level dictionary, or declares no string-valued name. A value
/// is only read when its element actually is a `<string>`; `<integer>42</integer>`
/// is not an identifier and `<true/>` is not an executable name, so a
/// tag-agnostic grab of whatever text follows a `<key>` would fabricate both.
///
/// An empty string is treated as an absent value throughout. `<string></string>`
/// names no executable and identifies no bundle, and reporting `Some("")` would
/// put an entry with an empty identity into the index.
pub fn parse_info_plist(xml: &str) -> Option<AppBundle> {
    let events = tokenize(xml)?;
    // `tokenize` guarantees a single root element, so the document root is
    // event zero: a `<plist>` found anywhere else is a nested element, not
    // this document's root.
    if !matches!(
        events.first(),
        Some(Event::Start {
            name: "plist",
            empty: false
        })
    ) {
        return None;
    }
    const ROOT: usize = 0;
    let dict = direct_child(&events, ROOT, "dict")?;
    let mut after_dict = skip_element(&events, dict);
    while matches!(
        events.get(after_dict),
        Some(Event::Text(text) | Event::Cdata(text)) if text.trim().is_empty()
    ) {
        after_dict += 1;
    }
    if !matches!(events.get(after_dict), Some(Event::End("plist"))) {
        return None;
    }

    let mut display_name = None;
    let mut name = None;
    let mut bundle_id = None;
    let mut executable = None;
    let mut icon_file = None;
    let mut display_name_seen = false;
    let mut name_seen = false;
    let mut bundle_id_seen = false;
    let mut executable_seen = false;
    let mut icon_file_seen = false;

    let mut index = dict + 1;
    while index < events.len() {
        match events[index] {
            Event::End(_) => break,
            Event::Start {
                name: "key",
                empty: false,
            } => {
                let key = element_text(&events, index)?;
                index = skip_element(&events, index);
                // Whitespace between the key and its value is ordinary
                // formatting; any other text makes the dictionary malformed.
                while matches!(
                    events.get(index),
                    Some(Event::Text(text) | Event::Cdata(text)) if text.trim().is_empty()
                ) {
                    index += 1;
                }
                let Some(Event::Start { name: tag, empty }) = events.get(index).copied() else {
                    return None;
                };
                // Only a string element carries a value; every other type is
                // skipped whole, leaving the field absent.
                let value = if tag == "string" {
                    if empty {
                        Some(String::new())
                    } else {
                        element_text(&events, index)
                    }
                } else {
                    None
                };
                let (slot, seen) = match key.as_str() {
                    DISPLAY_NAME_KEY => (&mut display_name, &mut display_name_seen),
                    BUNDLE_NAME_KEY => (&mut name, &mut name_seen),
                    IDENTIFIER_KEY => (&mut bundle_id, &mut bundle_id_seen),
                    EXECUTABLE_KEY => (&mut executable, &mut executable_seen),
                    ICON_FILE_KEY => (&mut icon_file, &mut icon_file_seen),
                    _ => {
                        index = skip_element(&events, index);
                        continue;
                    }
                };
                // A duplicate key is malformed input; the first spelling wins
                // so the result does not depend on how far the parser read.
                // Track that first spelling separately from the optional
                // value: an empty or non-string first value must not let a
                // later duplicate silently replace it.
                if !*seen {
                    *seen = true;
                    *slot = value.filter(|value| !value.is_empty());
                }
                index = skip_element(&events, index);
            }
            Event::Start { .. } => index = skip_element(&events, index),
            Event::Text(_) | Event::Cdata(_) => index += 1,
        }
    }

    Some(AppBundle {
        name: display_name.or(name)?,
        bundle_id,
        executable,
        icon_file,
    })
}

/// The application name carried by a bundle directory's own name.
///
/// `"Safari.app"` is `Safari`, spaces and dots inside the stem included:
/// `"my.app.app"` is the bundle `my.app`. Everything else is `None`, so the
/// plain directories that sit beside bundles in `/Applications` are not
/// indexed as applications. The suffix must be the whole final component --
/// `"my.app.backup"` is a backup copy, not a bundle -- and a bare `".app"` has
/// no stem left to name anything with.
pub fn bundle_display_name(dir_name: &str) -> Option<&str> {
    let stem = dir_name.strip_suffix(BUNDLE_SUFFIX)?;
    (!stem.is_empty()).then_some(stem)
}

/// The icon file a bundle's [`AppBundle::icon_file`] names, if it is there.
///
/// Apple's tools accept the key with or without its extension and both
/// spellings ship in real bundles, so both are tried: the literal name first,
/// because a bundle that really does contain both `AppIcon` and `AppIcon.icns`
/// meant the one it named.
///
/// Only `Contents/Resources` is searched, and only for a single plain component.
/// The key is documented as naming a file there, and a `CFBundleIconFile` of
/// `../../../../etc/shadow` in a bundle any user can unzip into `~/Applications`
/// is exactly the input this refuses: an icon path is a display detail, and it
/// must not become a way to make the launcher read an arbitrary file.
pub fn bundle_icon_path(bundle: &Path, icon_file: &str) -> Option<PathBuf> {
    let mut components = Path::new(icon_file).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return None;
    }
    let resources = bundle.join(BUNDLE_RESOURCES);
    let literal = resources.join(icon_file);
    if literal.is_file() {
        return Some(literal);
    }
    let suffixed = resources.join(format!("{icon_file}{ICNS_SUFFIX}"));
    suffixed.is_file().then_some(suffixed)
}

// ---------------------------------------------------------------------------
// The plist XML subset
// ---------------------------------------------------------------------------

/// One piece of a well-formed document.
///
/// `Cdata` is kept apart from `Text` because the two decode differently: a
/// character-data run expands entities, a CDATA section is literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event<'a> {
    /// `empty` marks the self-closing `<true/>` form, which opens and closes in
    /// one event and therefore never appears on the element stack.
    Start {
        name: &'a str,
        empty: bool,
    },
    End(&'a str),
    Text(&'a str),
    Cdata(&'a str),
}

/// Scans a document into events, rejecting anything that is not well formed.
///
/// Balance is checked here, once, so every walk below can assume that each
/// non-empty `Start` has its matching `End` at the same depth. `None` covers a
/// truncated tag, an end tag naming another element than the open one, an
/// unterminated comment or doctype, and character data outside the root
/// element -- which is what a binary plist looks like when it is handed to a
/// text parser.
///
/// A document also has exactly one root element. Without that check a stray
/// `<junk/>` before the `<plist>`, or an `<extra/>` after it, is not
/// well-formed XML yet still hands the walk below a usable event stream, so
/// the parser would accept documents it documents itself as rejecting. Events
/// therefore always begin with the root's `Start`, which is what lets callers
/// identify the root by index rather than by searching for it.
fn tokenize(xml: &str) -> Option<Vec<Event<'_>>> {
    let bytes = xml.as_bytes();
    let mut events = Vec::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut roots = 0usize;
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] != b'<' {
            let start = index;
            while index < bytes.len() && bytes[index] != b'<' {
                index += 1;
            }
            let text = &xml[start..index];
            if stack.is_empty() {
                // Indentation around the root is formatting; anything else is
                // not an XML document at all.
                if !text.trim().is_empty() {
                    return None;
                }
            } else {
                events.push(Event::Text(text));
            }
            continue;
        }

        let rest = &xml[index..];
        if let Some(body) = rest.strip_prefix("<!--") {
            index += 4 + body.find("-->")? + 3;
        } else if let Some(body) = rest.strip_prefix("<![CDATA[") {
            if stack.is_empty() {
                return None;
            }
            let end = body.find("]]>")?;
            events.push(Event::Cdata(&body[..end]));
            index += 9 + end + 3;
        } else if rest.starts_with("<!") {
            index = skip_declaration(bytes, index)?;
        } else if let Some(body) = rest.strip_prefix("<?") {
            index += 2 + body.find("?>")? + 2;
        } else if rest.starts_with("</") {
            let (name, next) = read_name(xml, index + 2)?;
            let next = skip_whitespace(bytes, next);
            if bytes.get(next) != Some(&b'>') {
                return None;
            }
            if stack.pop() != Some(name) {
                return None;
            }
            events.push(Event::End(name));
            index = next + 1;
        } else {
            let (name, next) = read_name(xml, index + 1)?;
            let (empty, next) = skip_attributes(bytes, next)?;
            if stack.is_empty() {
                roots += 1;
                if roots > 1 {
                    return None;
                }
            }
            if !empty {
                stack.push(name);
            }
            events.push(Event::Start { name, empty });
            index = next;
        }
    }

    stack.is_empty().then_some(events)
}

/// Reads an element name, which for this subset is the ASCII name characters
/// XML allows without needing a full name-character table.
fn read_name(xml: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = xml.as_bytes();
    let mut end = start;
    while end < bytes.len() && is_name_byte(bytes[end]) {
        end += 1;
    }
    (end > start).then(|| (&xml[start..end], end))
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.')
}

fn skip_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    index
}

/// Steps over a start tag's attributes to just past its `>`, reporting whether
/// the tag closed itself.
///
/// Attribute values are not read: plist elements carry only `version`, and a
/// `>` inside a quoted value must not be mistaken for the end of the tag.
fn skip_attributes(bytes: &[u8], mut index: usize) -> Option<(bool, usize)> {
    let mut quote = None;
    loop {
        let byte = *bytes.get(index)?;
        match quote {
            Some(open) => {
                if byte == open {
                    quote = None;
                }
                index += 1;
            }
            None => match byte {
                b'"' | b'\'' => {
                    quote = Some(byte);
                    index += 1;
                }
                b'>' => return Some((false, index + 1)),
                b'/' => {
                    if bytes.get(index + 1) != Some(&b'>') {
                        return None;
                    }
                    return Some((true, index + 2));
                }
                // An unescaped `<` inside a tag means the previous tag was
                // never terminated.
                b'<' => return None,
                _ => index += 1,
            },
        }
    }
}

/// Steps over a `<!...>` declaration, which for a plist is the doctype.
///
/// Quotes are honoured because the doctype's public and system identifiers are
/// quoted URLs, and the internal subset in brackets may itself contain `>`.
fn skip_declaration(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start + 2;
    let mut quote = None;
    let mut in_subset = false;
    loop {
        let byte = *bytes.get(index)?;
        index += 1;
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None => match byte {
                b'"' | b'\'' => quote = Some(byte),
                b'[' => in_subset = true,
                b']' => in_subset = false,
                b'>' if !in_subset => return Some(index),
                _ => {}
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Walking the event stream
// ---------------------------------------------------------------------------

/// The index just past the element starting at `start`.
///
/// This is what makes key lookup nesting aware: a `<dict>` or `<array>` value
/// is stepped over whole, so the keys inside it are never mistaken for
/// top-level ones.
fn skip_element(events: &[Event<'_>], start: usize) -> usize {
    match events.get(start) {
        Some(Event::Start { empty: false, .. }) => {}
        Some(_) => return start + 1,
        None => return start,
    }

    let mut depth = 0usize;
    for (offset, event) in events[start..].iter().enumerate() {
        match event {
            Event::Start { empty: false, .. } => depth += 1,
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    return start + offset + 1;
                }
            }
            _ => {}
        }
    }
    // Unreachable for a tokenized document: `tokenize` rejects unbalanced input.
    events.len()
}

/// The first direct child of `parent`, if it has the requested name.
///
/// A plist root has exactly one payload element.  Skipping only formatting
/// text here prevents a decoy dictionary after another element from becoming
/// the document's payload.
fn direct_child(events: &[Event<'_>], parent: usize, name: &str) -> Option<usize> {
    let mut index = parent + 1;
    while index < events.len() {
        match events[index] {
            Event::End(_) => return None,
            Event::Text(text) | Event::Cdata(text) if text.trim().is_empty() => index += 1,
            Event::Start { name: tag, .. } => return (tag == name).then_some(index),
            Event::Text(_) | Event::Cdata(_) => return None,
        }
    }
    None
}

/// The character data of the element starting at `start`, entities expanded.
///
/// A plist scalar cannot contain another element. Rejecting nested markup here
/// prevents malformed `<string>` or `<key>` values from being silently flattened
/// into a different, valid-looking string.
fn element_text(events: &[Event<'_>], start: usize) -> Option<String> {
    let end = skip_element(events, start);
    let mut text = String::new();
    for event in &events[start + 1..end - 1] {
        match event {
            Event::Text(run) => push_decoded(&mut text, run),
            Event::Cdata(run) => text.push_str(run),
            Event::Start { .. } | Event::End(_) => return None,
        }
    }
    Some(text)
}

/// The longest entity body this subset can decode, counted in bytes between
/// the `&` and the `;`. `#x0010FFFF` is ten, the longest named reference is
/// four, so twelve is slack. Anything longer is not a reference this parser
/// defines and would be copied through verbatim regardless -- the bound only
/// removes the incentive to keep looking for a delimiter.
const MAX_ENTITY_BODY_BYTES: usize = 12;

/// Appends character data with its entity references expanded.
///
/// A reference this subset does not define is kept exactly as written rather
/// than dropped: a display name is shown to a user, and losing characters out
/// of it is worse than showing an escape the document author wrote. A bare
/// `&` is therefore *not* rejected even though XML forbids it. Discovery walks
/// third-party bundles, and refusing to index an application because somebody
/// typed `Rock & Roll` into a display name trades a cosmetic authoring bug for
/// a missing entry.
///
/// Being lenient makes the scan strategy load bearing. Searching the rest of
/// the run for a `;` after every `&` is quadratic in a run of bare ampersands,
/// and the macOS scanner accepts an `Info.plist` up to a megabyte, so that is
/// reachable from an untrusted bundle. Each `&` instead looks ahead at most
/// [`MAX_ENTITY_BODY_BYTES`], and the cursor only ever moves forward, so the
/// whole run is one linear pass.
fn push_decoded(text: &mut String, run: &str) {
    let bytes = run.as_bytes();
    // `copied` trails the cursor: unexpanded stretches are pushed in one slice
    // when an expansion finally interrupts them, or at the end.
    let mut copied = 0;
    let mut cursor = 0;

    while let Some(offset) = bytes[cursor..].iter().position(|&byte| byte == b'&') {
        let amp = cursor + offset;
        let body = amp + 1;
        let limit = (body + MAX_ENTITY_BODY_BYTES).min(bytes.len());
        let terminator = bytes[body..limit]
            .iter()
            .position(|&byte| byte == b';')
            .map(|length| body + length);
        // `&`, the entity body and `;` are all ASCII, so every index used to
        // slice `run` here lands on a character boundary.
        match terminator.and_then(|end| Some((decode_entity(&run[body..end])?, end))) {
            Some((character, end)) => {
                text.push_str(&run[copied..amp]);
                text.push(character);
                copied = end + 1;
                cursor = copied;
            }
            // Not a reference we decode. Leave it in place and resume just
            // after the `&`, so a valid entity nested inside the undecodable
            // run is still expanded and no byte is examined twice.
            None => cursor = body,
        }
    }

    text.push_str(&run[copied..]);
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let digits = entity.strip_prefix('#')?;
            let code = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}
