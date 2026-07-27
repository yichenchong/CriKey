//! Application discovery over the Start Menu and the shell's app list
//! (spec 10.2, 18.4).
//!
//! Two sources feed one result. The Start Menu known folders are walked for
//! `.lnk` files, each resolved through the shell link object into the target
//! and arguments a launcher would run; the shell's Applications folder is
//! enumerated for packaged applications, which have no path at all and are
//! named by their AppUserModelID instead. Both land in one [`ApplicationSet`],
//! which is what makes the result deduplicated and stable.
//!
//! The Win32 half lives in the target-gated submodule. Everything above it --
//! the tree walk, the argument splitter, the deduplication rule -- is ordinary
//! Rust, because those are the parts with rules worth testing.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use crikey_core::{PlatformPath, Result};
use crikey_platform::{ApplicationDiscovery, DiscoveredApplication};

#[cfg(target_os = "windows")]
mod win32;

/// The extension a Start Menu entry needs before it is offered to the shell
/// link object. Compared case insensitively: the filesystem is.
const SHORTCUT_EXTENSION: &str = "lnk";

/// One `.lnk` the Start Menu walk found.
///
/// The shortcut is not resolved yet: this is the file, plus the name a user
/// would recognise it by. Resolution needs COM and happens later, so the walk
/// stays testable and cheap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcut {
    /// Absolute path of the `.lnk` file.
    pub path: PathBuf,
    /// Display name: the file name without its extension.
    ///
    /// Windows shows the shortcut's own name in the Start Menu, not the
    /// target's, so a shortcut called `Visual Studio Code` keeps that name
    /// even though it points at `Code.exe`.
    pub name: String,
}

/// Start Menu and packaged application discovery (spec 18.4).
///
/// Roots are scanned in the order they were given, so a per-user shortcut
/// shadows the machine-wide copy of the same program.
#[derive(Debug)]
pub struct StartMenuDiscovery {
    roots: Vec<PathBuf>,
    packaged: bool,
}

impl StartMenuDiscovery {
    /// How deep below a root the walk will go.
    ///
    /// Start Menu trees are two or three levels of program folders. The cap
    /// exists for the pathological case -- a directory junction pointing at an
    /// ancestor -- and is far above anything a real menu reaches.
    pub const MAX_DEPTH: usize = 8;

    /// How many shortcuts one scan will collect.
    ///
    /// A bound on work, not a policy: a machine with more Start Menu entries
    /// than this has something wrong with it, and a launcher must not spend an
    /// unbounded startup resolving them.
    pub const MAX_SHORTCUTS: usize = 20_000;

    /// Discovers this user's Start Menu, the machine's, and the packaged
    /// applications the shell publishes.
    ///
    /// Off target the root list is empty, because there are no known folders to
    /// resolve; [`ApplicationDiscovery::discover`] refuses outright rather than
    /// reporting that empty list as a result.
    pub fn new() -> Self {
        Self {
            roots: start_menu_roots(),
            packaged: true,
        }
    }

    /// Discovers exactly these roots, highest precedence first, and packaged
    /// applications only if asked.
    ///
    /// Construction touches no filesystem and no COM: every read happens inside
    /// [`ApplicationDiscovery::discover`] or [`Self::shortcuts`], so a scanner
    /// can be built before the directories it names exist.
    pub fn with_roots(roots: Vec<PathBuf>, packaged: bool) -> Self {
        Self { roots, packaged }
    }

    /// The roots this scanner walks, highest precedence first.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Whether packaged applications are enumerated alongside the shortcuts.
    pub fn packaged(&self) -> bool {
        self.packaged
    }

    /// Every `.lnk` under the roots, in a deterministic order.
    ///
    /// Roots are visited in order and each directory is read in sorted name
    /// order, so an unchanged Start Menu produces an identical list on every
    /// scan and the precedence rule the deduplicator applies is reproducible.
    ///
    /// A missing or unreadable root is skipped rather than reported: a machine
    /// with no per-user Start Menu is ordinary, and one unreadable program
    /// folder must not hide every other application. Directory entries are
    /// inspected without following links, so a junction back up the tree is
    /// simply not descended into.
    pub fn shortcuts(&self) -> Vec<Shortcut> {
        let mut found = Vec::new();
        for root in &self.roots {
            Self::walk(root, 0, &mut found);
        }
        found
    }

    fn walk(directory: &Path, depth: usize, found: &mut Vec<Shortcut>) {
        if depth > Self::MAX_DEPTH || found.len() >= Self::MAX_SHORTCUTS {
            return;
        }

        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        let mut names: Vec<OsString> = entries.flatten().map(|entry| entry.file_name()).collect();
        // Directory order is filesystem defined; sorting makes a rescan of an
        // unchanged tree repeat itself exactly.
        names.sort_unstable();

        for name in names {
            let path = directory.join(&name);
            // `symlink_metadata`, so a reparse point is never descended into
            // and a `.lnk` that is really a link to a device is never opened.
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };

            if metadata.is_dir() {
                Self::walk(&path, depth + 1, found);
            } else if metadata.is_file() && is_shortcut(&name) {
                if found.len() >= Self::MAX_SHORTCUTS {
                    return;
                }
                found.push(Shortcut {
                    name: shortcut_name(&name),
                    path,
                });
            }
        }
    }
}

impl Default for StartMenuDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

impl ApplicationDiscovery for StartMenuDiscovery {
    /// Resolves every shortcut and packaged application into one deduplicated
    /// list.
    ///
    /// Failure is reserved for the cases that make discovery meaningless:
    /// COM refusing to start, or the shell refusing to open its own
    /// Applications folder. A shortcut that will not resolve is dropped, since
    /// one broken `.lnk` must not delete every other application on the
    /// machine.
    fn discover(&self) -> Result<Vec<DiscoveredApplication>> {
        #[cfg(target_os = "windows")]
        {
            win32::discover(self)
        }
        #[cfg(not(target_os = "windows"))]
        {
            Err(crate::off_target("discover applications"))
        }
    }
}

/// The Start Menu known folders, per-user first.
#[cfg(target_os = "windows")]
fn start_menu_roots() -> Vec<PathBuf> {
    win32::start_menu_roots()
}

#[cfg(not(target_os = "windows"))]
fn start_menu_roots() -> Vec<PathBuf> {
    Vec::new()
}

/// `name.lnk` and nothing else: not `name.lnk.bak`, not a bare `lnk`.
fn is_shortcut(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case(SHORTCUT_EXTENSION))
}

/// The display name of a shortcut file: its stem, rendered for humans.
///
/// Lossy on purpose. A name is shown, never used as identity -- that is the
/// target's job -- so a shortcut whose file name is not valid UTF-16 still
/// appears in the launcher instead of vanishing from it.
fn shortcut_name(name: &OsStr) -> String {
    Path::new(name)
        .file_stem()
        .unwrap_or(name)
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------------------
// Deduplication
// ---------------------------------------------------------------------------

/// The discovered applications of one scan, with duplicates collapsed.
///
/// Identity is the launch target and only the launch target, because that is
/// what `crikey_platform::application_items` derives an item's stable id from:
/// two discoveries sharing a target would become one catalog item anyway, and
/// the second would silently overwrite the first. Collapsing them here makes
/// the choice explicit and deterministic -- the first insertion wins, and
/// insertion follows root precedence.
///
/// Comparison is case insensitive because Windows paths are: `C:\Windows\`
/// and `c:\windows\` name one directory, and a launcher that listed both would
/// be listing the same program twice. It is also lossy, which a dedup key can
/// afford and the retained [`PlatformPath`] cannot: the key is thrown away, the
/// target is kept exactly as the shell reported it (spec 18.3, ADR-0007).
#[derive(Debug, Default)]
pub struct ApplicationSet {
    applications: Vec<DiscoveredApplication>,
    claimed: HashSet<String>,
}

impl ApplicationSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one discovery, reporting whether it was kept.
    ///
    /// An application whose target is empty is refused: it names nothing to
    /// launch, and letting it claim the empty key would suppress the next one.
    pub fn insert(&mut self, application: DiscoveredApplication) -> bool {
        let key = target_key(&application.target);
        if key.is_empty() || !self.claimed.insert(key) {
            return false;
        }

        self.applications.push(application);
        true
    }

    pub fn len(&self) -> usize {
        self.applications.len()
    }

    pub fn is_empty(&self) -> bool {
        self.applications.is_empty()
    }

    /// The kept applications, in insertion order.
    pub fn into_applications(self) -> Vec<DiscoveredApplication> {
        self.applications
    }
}

/// The case-folded rendering two targets are considered the same by.
fn target_key(target: &PlatformPath) -> String {
    target.as_os_str().to_string_lossy().to_lowercase()
}

// ---------------------------------------------------------------------------
// Shortcut arguments
// ---------------------------------------------------------------------------

/// Splits a shortcut's argument string into the argument vector
/// `ProcessLauncher::launch` takes.
///
/// A `.lnk` stores its arguments as one command-line string, but a launcher
/// must hand a program a vector: re-splitting on spaces later would break every
/// path with a space in it. The rules are the ones `CommandLineToArgvW`
/// documents, minus its special case for the program name, which a shortcut's
/// argument string does not contain:
///
/// * unquoted whitespace separates arguments;
/// * a double quote toggles quoting, and whitespace inside quotes is ordinary;
/// * `""` inside a quoted run is one literal quote and keeps the run open;
/// * a backslash is literal unless it precedes a quote, where each pair becomes
///   one backslash and an odd one left over escapes the quote instead of
///   toggling.
///
/// An empty argument the author wrote as `""` survives as an empty string,
/// because a program that is handed a positional empty argument asked for one.
pub fn split_arguments(arguments: &str) -> Vec<String> {
    let mut split = Vec::new();
    let mut argument = String::new();
    // Distinguishes "no argument here" from "an argument that is empty", which
    // is the whole difference between `a  b` and `a "" b`.
    let mut started = false;
    let mut quoted = false;
    let mut backslashes = 0usize;
    let mut characters = arguments.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            // Held back: what a run of backslashes means depends on whether a
            // quote follows it.
            '\\' => {
                backslashes += 1;
                started = true;
            }
            '"' => {
                argument.extend(std::iter::repeat_n('\\', backslashes / 2));
                if backslashes % 2 == 1 {
                    argument.push('"');
                } else if quoted && characters.peek() == Some(&'"') {
                    characters.next();
                    argument.push('"');
                } else {
                    quoted = !quoted;
                }
                backslashes = 0;
                started = true;
            }
            ' ' | '\t' if !quoted => {
                argument.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                if started {
                    split.push(std::mem::take(&mut argument));
                    started = false;
                }
            }
            character => {
                argument.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                argument.push(character);
                started = true;
            }
        }
    }

    argument.extend(std::iter::repeat_n('\\', backslashes));
    if started {
        split.push(argument);
    }
    split
}
