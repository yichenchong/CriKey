//! Application discovery over the Start Menu and the shell's app list
//! (spec 10.2, 18.4).
//!
//! Three sources feed one result. The Start Menu known folders are walked for
//! `.lnk` files, each resolved through the shell link object into the target
//! and arguments a launcher would run; the shell's Applications folder is
//! enumerated for packaged applications, which have no path at all and are
//! named by their AppUserModelID instead; and a short hard-coded table names
//! the components of Windows itself that neither source reports, of which File
//! Explorer is one (see [`WELL_KNOWN_APPLICATIONS`]). All land in one
//! [`ApplicationSet`], which is what makes the result deduplicated and stable.
//!
//! The Win32 half lives in the target-gated submodule. Everything above it --
//! the tree walk, the argument splitter, the deduplication rule -- is ordinary
//! Rust, because those are the parts with rules worth testing.

use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use crikey_core::{PlatformPath, Result};
use crikey_platform::{ApplicationDiscovery, DiscoveredApplication};

#[cfg(target_os = "windows")]
mod win32;

/// One application Windows ships that neither discovery source reports.
///
/// Both sources look for something an installer left behind: a `.lnk` under a
/// Start Menu known folder, or a package registered with the shell. A component
/// of Windows itself may have neither, and File Explorer is exactly that case
/// (see [`WELL_KNOWN_APPLICATIONS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WellKnownApplication {
    /// The label the entry carries. Hard coded, and therefore English: the
    /// localised name lives in the shell's own item for the application, which
    /// is the thing this entry exists because the machine may not have.
    pub name: &'static str,
    /// The executable, named relative to the Windows system root so the entry
    /// follows a system installed on a volume other than `C:`.
    pub executable: &'static str,
}

/// The applications this backend names itself, because Windows names them
/// nowhere the two discovery sources look (spec 18.4).
///
/// # Why File Explorer needs an entry at all
///
/// The Start Menu walk only reports `.lnk` files (`is_shortcut`), and a stock
/// Windows 11 need not have one for File Explorer: Microsoft's own default
/// layout pins it through
/// `%APPDATA%\Microsoft\Windows\Start Menu\Programs\File Explorer.lnk`, a
/// per-user file that is absent on machines whose profile was never given it,
/// and pinning by identifier instead uses the desktop application id
/// `Microsoft.Windows.Explorer`
/// (<https://learn.microsoft.com/en-us/windows/configuration/start/layout>).
/// The packaged-application enumeration cannot cover the gap either: File
/// Explorer is not packaged, so its identifier carries no `!`, and
/// `win32::packaged_application` drops every identifier without one.
///
/// # Why not relax that filter instead
///
/// The `!` test is not a formality. The Applications folder lists *every*
/// desktop program as well as the packaged ones, so dropping the filter would
/// duplicate most of the Start Menu, and admitting `Microsoft.Windows.Explorer`
/// by name would still duplicate File Explorer itself on every machine that does
/// have the shortcut: the two discoveries carry different targets
/// (`shell:AppsFolder\Microsoft.Windows.Explorer` against `explorer.exe`), and
/// [`ApplicationSet`] deduplicates on the target because that is what an item's
/// stable id is derived from. Naming the executable produces the *same* target
/// the shortcut resolves to -- the shortcut stores `%windir%\explorer.exe`,
/// which `win32::resolve` expands -- so the duplicate collapses on its own and
/// the shortcut, which carries the localised name and its own icon, wins.
///
/// # The admission rule
///
/// This list, and nothing else. Each entry names one executable relative to
/// `%SystemRoot%`, and it becomes an application only if that exact file is
/// there: nothing is enumerated, no filter is loosened, and no registry or
/// shell namespace is searched, so no third party can add to the result set
/// through it. An entry whose file is missing -- a future Windows that moves
/// `explorer.exe`, or an off-target build with no system root at all -- yields
/// no item and no error.
pub const WELL_KNOWN_APPLICATIONS: &[WellKnownApplication] = &[WellKnownApplication {
    // `explorer.exe` sits directly in the Windows directory, not in
    // `System32`, and launching it with no arguments opens File Explorer at the
    // shell's configured start location -- the same thing the Start Menu
    // shortcut and the taskbar button do.
    name: "File Explorer",
    executable: "explorer.exe",
}];

impl WellKnownApplication {
    /// The application this entry names, if the system really has it.
    ///
    /// No icon is recorded. An executable's default icon lives in its resources,
    /// and extracting one is not implemented (see [`crate::icons`]); reporting
    /// the `.exe` as an icon reference would hand the decoder a PE image, so the
    /// entry says nothing rather than something wrong -- the same rule
    /// `win32::resolve` applies to a shortcut that declares no icon.
    ///
    /// No AppUserModelID is recorded either: like a Start Menu shortcut, this
    /// entry is identified by the executable it points at, and it is launched by
    /// running that executable rather than by activating an identifier.
    fn resolve(&self, system_root: &Path) -> Option<DiscoveredApplication> {
        let executable = system_root.join(self.executable);
        if !executable.is_file() {
            return None;
        }

        Some(DiscoveredApplication {
            name: self.name.to_owned(),
            target: PlatformPath::new(executable.into_os_string()),
            arguments: Vec::new(),
            icon_reference: None,
            platform_id: None,
            working_directory: None,
        })
    }
}

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

/// Start Menu, packaged and well-known application discovery (spec 18.4).
///
/// Roots are scanned in the order they were given, so a per-user shortcut
/// shadows the machine-wide copy of the same program. The system root is where
/// [`WELL_KNOWN_APPLICATIONS`] are looked for; a scanner without one reports
/// none of them.
#[derive(Debug)]
pub struct StartMenuDiscovery {
    roots: Vec<PathBuf>,
    packaged: bool,
    system_root: Option<PathBuf>,
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
            system_root: system_root(),
        }
    }

    /// Discovers exactly these roots, highest precedence first, and packaged
    /// applications only if asked. No well-known application is named: a
    /// scanner told exactly where to look reports exactly what is there, and
    /// [`Self::with_system_root`] adds them back.
    ///
    /// Construction touches no filesystem and no COM: every read happens inside
    /// [`ApplicationDiscovery::discover`], [`Self::shortcuts`] or
    /// [`Self::well_known_applications`], so a scanner can be built before the
    /// directories it names exist.
    pub fn with_roots(roots: Vec<PathBuf>, packaged: bool) -> Self {
        Self {
            roots,
            packaged,
            system_root: None,
        }
    }

    /// Looks for [`WELL_KNOWN_APPLICATIONS`] below this directory instead of
    /// below the running system's root.
    pub fn with_system_root(mut self, system_root: Option<PathBuf>) -> Self {
        self.system_root = system_root;
        self
    }

    /// Where [`WELL_KNOWN_APPLICATIONS`] are looked for, when anywhere.
    pub fn system_root(&self) -> Option<&Path> {
        self.system_root.as_deref()
    }

    /// The well-known applications this system really has.
    ///
    /// Empty when no system root is known, which is every build that is not
    /// running on Windows: an entry is only ever reported once its executable
    /// has been seen.
    pub fn well_known_applications(&self) -> Vec<DiscoveredApplication> {
        let Some(system_root) = self.system_root.as_deref() else {
            return Vec::new();
        };
        WELL_KNOWN_APPLICATIONS
            .iter()
            .filter_map(|known| known.resolve(system_root))
            .collect()
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
    /// Resolves every shortcut, well-known and packaged application into one
    /// deduplicated list.
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

/// The Windows directory of the running system, when there is one.
///
/// `SystemRoot` is set by Windows itself for every process, so this is the
/// volume-independent way to reach `explorer.exe` without hard-coding `C:`. Off
/// target the variable is unset and [`WELL_KNOWN_APPLICATIONS`] contribute
/// nothing, which matches [`start_menu_roots`] reporting no known folders.
fn system_root() -> Option<PathBuf> {
    env::var_os("SystemRoot")
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
}

/// `name.lnk` and nothing else: not `.lnk`, `name.lnk.bak`, or a bare `lnk`.
fn is_shortcut(name: &OsStr) -> bool {
    const SUFFIX: &[u8] = b".lnk";
    let bytes = name.as_encoded_bytes();
    bytes.len() > SUFFIX.len() && bytes[bytes.len() - SUFFIX.len()..].eq_ignore_ascii_case(SUFFIX)
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
/// be listing the same program twice. The key keeps the target's native units
/// losslessly before folding case, so two paths that differ only by an invalid
/// UTF-16 code unit are not accidentally collapsed by replacement characters.
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

/// The lossless, case-folded rendering two Windows targets are compared by.
fn target_key(target: &PlatformPath) -> String {
    crikey_platform::encode_target(target).to_lowercase()
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
