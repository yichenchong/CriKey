//! Linux platform backend.
//!
//! XDG desktop entries and base directories, DBus, Freedesktop notifications,
//! Secret Service, portals, X11/Wayland where available (spec 18.6).
//!
//! Compiled only for its target so platform-independent crates can never
//! accidentally depend on it (spec 5.3).
//!
//! Implemented so far: application discovery over XDG desktop entries, and
//! process launching for the entries it finds. The parser stops at what the
//! core actually consumes -- group scoping, `Type`, the visibility keys and
//! `Exec` -- so locale selection, action groups as separately launchable
//! entries and recursive root layouts stay for a later milestone. Launching
//! runs a program directly and stops there: URI opening needs a portal or a
//! session handler this backend does not have, so it -- and everything else
//! -- keeps reporting itself unavailable (spec 18.2).
//!
//! A root is only as trustworthy as whatever last wrote into it, so a
//! candidate is stat checked and read through a cap before it is parsed:
//! discovery must not block on a FIFO, follow a device node, or pull an
//! unbounded file into memory.

#![cfg(target_os = "linux")]

use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::mem;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, PoisonError};

use crikey_core::{CoreError, PlatformPath, Result};
use crikey_platform::{
    ApplicationDiscovery, Capability, CapabilityState, DiscoveredApplication, ProcessLauncher,
};

/// The only group a launchable entry is read from.
const DESKTOP_ENTRY_GROUP: &[u8] = b"Desktop Entry";

/// The `Type` a launcher may run. `Link` and `Directory` entries are not
/// applications no matter what else they declare.
const APPLICATION_TYPE: &[u8] = b"Application";

/// The extension a file needs before the scanner will open it.
const DESKTOP_EXTENSION: &str = "desktop";

/// `Exec` field codes: launcher substitutions, never arguments.
///
/// The deprecated ones (`%d %D %n %N %v %m`) are listed too because the format
/// requires implementations to drop them rather than hand them to the program.
const FIELD_CODES: &[u8] = b"fFuUdDnNickvm";

/// Application discovery over XDG desktop entries (spec 18.6).
///
/// Roots are scanned in the order they were given and the earliest root wins a
/// duplicate desktop id, which is what lets `~/.local/share/applications`
/// override the system copy of an entry.
#[derive(Debug)]
pub struct DesktopEntryScanner {
    roots: Vec<PathBuf>,
}

impl DesktopEntryScanner {
    /// The largest candidate the scanner will read, in bytes.
    ///
    /// Desktop entries are a few kilobytes of text and even the most
    /// translated ones on a full desktop stay far below this, so the cap costs
    /// no real entry anything. It is public because it is observable
    /// behaviour: a file past it is skipped whole, never truncated into a
    /// half parsed application.
    pub const MAX_ENTRY_BYTES: u64 = 256 * 1024;

    /// Records the roots to scan, highest precedence first.
    ///
    /// Construction touches no filesystem: every read happens inside
    /// [`ApplicationDiscovery::discover`], so a scanner can be built before the
    /// directories it names exist.
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }
}

impl ApplicationDiscovery for DesktopEntryScanner {
    /// Scans every root once and returns the applications it can launch.
    ///
    /// This never fails. A root that is missing, is not a directory or cannot
    /// be read is an ordinary state on Linux -- `XDG_DATA_DIRS` routinely names
    /// directories no package ever created -- and one unreadable or malformed
    /// file must not hide every other application on the machine.
    fn discover(&self) -> Result<Vec<DiscoveredApplication>> {
        let mut discovered = Vec::new();
        let mut claimed: HashSet<OsString> = HashSet::new();

        for root in &self.roots {
            let Ok(directory) = fs::read_dir(root) else {
                continue;
            };

            let mut ids: Vec<OsString> = directory
                .flatten()
                .map(|entry| entry.file_name())
                .filter(|id| is_desktop_entry(id))
                .collect();
            // Directory order is filesystem defined; sorting makes a rescan of
            // an unchanged root repeat itself exactly.
            ids.sort_unstable();

            for id in ids {
                if claimed.contains(&id) {
                    continue;
                }
                let Some(contents) = read_entry(&root.join(&id)) else {
                    // Unreadable, or not a plain entry file at all: leave the
                    // id unclaimed so a later root may still supply it.
                    continue;
                };

                if let Some(application) = parse_entry(&contents, &id) {
                    discovered.push(application);
                }
                // The id is spent even when the entry yielded nothing: a user
                // level `Hidden=true` deletes the system entry of the same id
                // instead of falling through to it.
                claimed.insert(id);
            }
        }

        Ok(discovered)
    }
}

/// Process launching for filesystem targets (spec 18.1).
///
/// A discovered application already arrives split into a program and an
/// argument vector -- that is what [`exec_command`] produces -- so launching
/// is a direct spawn: no shell, no re-quoting, no re-splitting. Keeping the
/// arguments a vector all the way down is the whole point of the split, since
/// an `Exec` line's `"My Documents"` has to reach the program as one argument
/// and not as two.
#[derive(Debug, Default)]
pub struct CommandLauncher {
    /// Handles of children that were spawned and never waited for.
    ///
    /// A process that has exited but whose parent has not collected its status
    /// stays a zombie holding a pid, and a launcher lives for a whole desktop
    /// session: without this the pid table fills with every application the
    /// user ever started. Waiting is out of the question, so the handles are
    /// kept and swept without blocking on the next launch, which bounds the
    /// list by the number of launched applications still running.
    running: Mutex<Vec<Child>>,
}

impl CommandLauncher {
    /// A launcher holding no children yet.
    ///
    /// Construction starts nothing: every process appears inside
    /// [`ProcessLauncher::launch`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `child`, first dropping the handles of children that already
    /// exited.
    ///
    /// `try_wait` reports only the children the kernel already has a status
    /// for and returns immediately for the rest, so the sweep never delays the
    /// launch that triggered it. A handle whose wait errored is dropped too:
    /// there is no status left to collect from it.
    fn keep(&self, child: Child) {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        running.retain_mut(|spawned| matches!(spawned.try_wait(), Ok(None)));
        running.push(child);
    }
}

impl ProcessLauncher for CommandLauncher {
    /// Starts `target` with exactly `args` and returns as soon as the process
    /// exists.
    ///
    /// The target is handed over as its own `OsStr`, so an install path that
    /// is not UTF-8 launches unchanged (spec 18.3), and every argument is
    /// passed individually: spaces, quotes and empty strings inside one
    /// argument reach the program as written.
    ///
    /// Nothing is waited for. A launcher must be usable again the instant the
    /// application it started is on its way, and an application outlives the
    /// launcher that started it.
    ///
    /// Standard streams are detached and the child enters a new process group.
    /// A terminal interrupt sent to CriKey's foreground group therefore does
    /// not kill the application, and the application cannot block writing into
    /// a pipe nobody drains.
    fn launch(&self, target: &PlatformPath, args: &[String]) -> Result<()> {
        let mut command = Command::new(target.as_os_str());
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let child = command.spawn().map_err(|error| {
            // Both halves matter to whoever reads this: which target was
            // tried, and what the kernel said about it.
            CoreError::Invalid(format!("cannot launch {}: {error}", target.display()))
        })?;

        self.keep(child);
        Ok(())
    }

    /// Always fails: this backend cannot open URIs (spec 18.2).
    ///
    /// Opening a URI on Linux means handing it to whatever the session
    /// designates as its handler -- a desktop portal, or the handler lookup a
    /// helper like `xdg-open` performs -- and this crate has neither a portal
    /// client nor a rule for choosing a helper. Picking a command here would
    /// be a guess, and a launcher that quietly runs the wrong program with a
    /// user's URI is worse than one that admits it cannot do it.
    fn open_uri(&self, uri: &str) -> Result<()> {
        Err(CoreError::Invalid(format!(
            "the linux backend cannot open URIs: {uri}"
        )))
    }
}

#[derive(Debug)]
pub struct LinuxBackend {
    applications: DesktopEntryScanner,
    processes: CommandLauncher,
}

impl LinuxBackend {
    /// Stable backend identifier surfaced by diagnostics and `crikey version`.
    pub const NAME: &'static str = "linux";

    /// Discovers applications from the XDG base directories of the running
    /// user.
    pub fn new() -> Self {
        Self::with_application_roots(xdg_application_roots())
    }

    /// Discovers applications from exactly these roots, highest precedence
    /// first, instead of the XDG defaults.
    pub fn with_application_roots(roots: Vec<PathBuf>) -> Self {
        Self {
            applications: DesktopEntryScanner::new(roots),
            processes: CommandLauncher::new(),
        }
    }

    /// Capability reporting is honest: a capability is claimed only once a
    /// Linux implementation stands behind it (spec 18.2). The unimplemented
    /// arms are listed one by one so that adding a capability to the enum
    /// forces a deliberate answer here instead of inheriting a wildcard.
    pub fn capability(&self, capability: Capability) -> CapabilityState {
        match capability {
            Capability::ApplicationDiscovery | Capability::ProcessLaunch => CapabilityState::Available,
            Capability::FileSearch
            | Capability::Clipboard
            | Capability::GlobalHotkeys
            | Capability::UriOpen
            | Capability::WindowEnumeration
            | Capability::WindowActivation
            | Capability::Notifications
            | Capability::Icons
            | Capability::FileWatching
            | Capability::SecretStorage
            | Capability::ShellIntegration => CapabilityState::Unavailable,
        }
    }

    /// The discovery service behind [`Capability::ApplicationDiscovery`].
    pub fn application_discovery(&self) -> &dyn ApplicationDiscovery {
        &self.applications
    }

    /// The launcher behind [`Capability::ProcessLaunch`].
    pub fn process_launcher(&self) -> &dyn ProcessLauncher {
        &self.processes
    }
}

impl Default for LinuxBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// The XDG `applications` directories in basedir precedence order: the user's
/// data home first, so a user entry overrides the system copy of the same
/// desktop id, then the system data directories in their listed order.
///
/// Relative entries are ignored as the specification requires, and an unset or
/// empty variable falls back to its documented default.
fn xdg_application_roots() -> Vec<PathBuf> {
    const APPLICATIONS: &str = "applications";
    const DEFAULT_DATA_DIRS: [&str; 2] = ["/usr/local/share", "/usr/share"];

    let mut roots = Vec::new();
    if let Some(data_home) = absolute_from_env("XDG_DATA_HOME") {
        roots.push(data_home.join(APPLICATIONS));
    } else if let Some(home) = absolute_from_env("HOME") {
        roots.push(home.join(".local").join("share").join(APPLICATIONS));
    }

    match env::var_os("XDG_DATA_DIRS").filter(|dirs| !dirs.is_empty()) {
        Some(dirs) => roots.extend(
            env::split_paths(&dirs)
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join(APPLICATIONS)),
        ),
        None => roots.extend(DEFAULT_DATA_DIRS.map(|dir| Path::new(dir).join(APPLICATIONS))),
    }

    roots
}

fn absolute_from_env(key: &str) -> Option<PathBuf> {
    let value = PathBuf::from(env::var_os(key)?);
    value.is_absolute().then_some(value)
}

/// `name.desktop` and nothing else: not `name.desktop.bak`, not a bare
/// `desktop`, not the `name.desktop.d` drop-in directories that ship beside
/// real entries.
fn is_desktop_entry(id: &OsStr) -> bool {
    Path::new(id).extension() == Some(OsStr::new(DESKTOP_EXTENSION))
}

/// Reads one candidate entry, refusing anything that is not an ordinary file
/// of plausible size.
///
/// The type check happens before the open because the open is the dangerous
/// part: opening a FIFO blocks until somebody writes to it, and a symlink to a
/// device node hands the scanner a stream that never ends. A directory named
/// `something.desktop` falls to the same check. Metadata deliberately follows
/// symlinks -- distributions do ship entries as links -- so what is inspected
/// is the file that would actually be read.
///
/// The size is capped twice. The stat rejects a file that is already too big,
/// and the read still runs through a reader limited to one byte past the cap,
/// so a file that grows between the two calls is dropped rather than followed.
fn read_entry(path: &Path) -> Option<Vec<u8>> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > DesktopEntryScanner::MAX_ENTRY_BYTES {
        return None;
    }

    // One byte past the cap, so an oversized file is detected by the read
    // itself instead of being trusted to match the size the stat reported.
    let limit = DesktopEntryScanner::MAX_ENTRY_BYTES.saturating_add(1);
    let mut contents = Vec::with_capacity(usize::try_from(metadata.len()).ok()?);
    let mut reader = fs::File::open(path).ok()?.take(limit);
    reader.read_to_end(&mut contents).ok()?;

    // The extra byte is spent only by a file that outgrew the cap.
    (reader.limit() > 0).then_some(contents)
}

/// Reads the `[Desktop Entry]` group of one file into a discovery result.
///
/// `None` means "nothing a launcher can show or run": another `Type`, no name,
/// no runnable `Exec`, or an author set `NoDisplay`/`Hidden`. Malformed lines
/// are skipped rather than aborting the parse, because one junk line in a
/// vendor file must not delete a working application. Parsing is byte oriented
/// so a file that is not valid UTF-8 degrades to replacement characters in the
/// display strings instead of being rejected, while the target keeps its exact
/// bytes (spec 18.3).
fn parse_entry(contents: &[u8], id: &OsStr) -> Option<DiscoveredApplication> {
    let mut inside = false;
    let mut kind = None;
    let mut name = None;
    let mut exec = None;
    let mut icon = None;
    let mut no_display = None;
    let mut hidden = None;

    for line in contents.split(|byte| *byte == b'\n') {
        let line = trim(line);
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }

        if let Some(group) = group_name(line) {
            if inside {
                // Action groups repeat `Name`, `Exec` and `Icon`; reading on
                // would launch the action instead of the application.
                break;
            }
            inside = group == DESKTOP_ENTRY_GROUP;
            continue;
        }

        // Keys ahead of `[Desktop Entry]`, or in any other group, belong to
        // somebody else and must neither disqualify nor rewrite this entry.
        if !inside {
            continue;
        }

        let Some((key, value)) = split_key_value(line) else {
            continue;
        };

        // Duplicate keys are invalid in the format; keeping the first sighting
        // makes the choice deterministic. Locale variants such as `Name[xx]`
        // are a different key and are deliberately out of contract.
        match key {
            b"Type" => keep_first(&mut kind, value),
            b"Name" => keep_first(&mut name, value),
            b"Exec" => keep_first(&mut exec, value),
            b"Icon" => keep_first(&mut icon, value),
            b"NoDisplay" => keep_first(&mut no_display, value),
            b"Hidden" => keep_first(&mut hidden, value),
            _ => {}
        }
    }

    if kind? != APPLICATION_TYPE {
        return None;
    }
    // Absent means visible, so the `NoDisplay=false` an author writes on
    // purpose keeps the entry.
    if is_true(no_display) || is_true(hidden) {
        return None;
    }

    let name = name.filter(|name| !name.is_empty())?;
    let (target, arguments) = exec_command(exec?)?;

    Some(DiscoveredApplication {
        name: text(name),
        target,
        arguments,
        icon_reference: icon.filter(|icon| !icon.is_empty()).map(text),
        // The desktop id is the file name: the identity the rest of the
        // desktop (`gtk-launch`, `.desktop` references) uses for this entry.
        platform_id: Some(id.to_string_lossy().into_owned()),
    })
}

/// Splits an `Exec` value into the program and the argument vector
/// `ProcessLauncher::launch` takes.
///
/// `None` when nothing runnable is left, which is what an empty or field-code
/// only `Exec` amounts to.
fn exec_command(value: &[u8]) -> Option<(PlatformPath, Vec<String>)> {
    let mut tokens = split_exec(value).into_iter();
    let program = tokens.next().filter(|program| !program.is_empty())?;

    Some((
        PlatformPath::new(OsString::from_vec(program)),
        tokens.map(into_text).collect(),
    ))
}

/// Tokenizes an `Exec` value.
///
/// Double quotes group a token and inside them a backslash escapes the next
/// byte: the two rules real entries rely on for paths containing spaces. Field
/// codes are launcher substitutions rather than arguments, so they are removed
/// and `%%` collapses to a single percent -- inside quotes exactly as outside,
/// because the format expands field codes before quoting is considered, so the
/// `"%f"` an author quoted is still a substitution and not a filename. A token
/// that was nothing but field codes disappears instead of becoming an empty
/// argument. An unterminated quote yields the rest of the value rather than
/// discarding the entry.
fn split_exec(value: &[u8]) -> Vec<Vec<u8>> {
    let mut tokens = Vec::new();
    let mut token = Vec::new();
    // Tracked separately from `token`, so `""` stays an explicit empty
    // argument while a stripped `%f` leaves no argument at all.
    let mut started = false;
    let mut stripped = false;
    let mut quoted = false;
    let mut index = 0;

    while let Some(&byte) = value.get(index) {
        index = index.saturating_add(1);

        // Ahead of the quoting rules on purpose: the launcher expands a field
        // code wherever it appears. A backslash escaped percent never arrives
        // here because the quoted arm below consumes it, which keeps `\%` the
        // literal percent the author asked for.
        if byte == b'%' {
            match value.get(index) {
                Some(b'%') => {
                    token.push(b'%');
                    started = true;
                    index = index.saturating_add(1);
                }
                Some(code) if FIELD_CODES.contains(code) => {
                    stripped = true;
                    index = index.saturating_add(1);
                }
                // A stray percent is not a substitution; keep it.
                _ => {
                    token.push(b'%');
                    started = true;
                }
            }
            continue;
        }

        if quoted {
            match byte {
                b'"' => quoted = false,
                b'\\' => match value.get(index) {
                    Some(&escaped) => {
                        token.push(escaped);
                        index = index.saturating_add(1);
                    }
                    // A trailing backslash escapes nothing; keep it literal.
                    None => token.push(b'\\'),
                },
                _ => token.push(byte),
            }
            continue;
        }

        match byte {
            b'"' => {
                quoted = true;
                started = true;
            }
            _ if byte.is_ascii_whitespace() => {
                if started {
                    push_token(&mut tokens, mem::take(&mut token), stripped);
                    started = false;
                }
                // Reset even without a token, so a bare `%f` cannot carry its
                // removal into whatever argument comes next.
                stripped = false;
            }
            _ => {
                token.push(byte);
                started = true;
            }
        }
    }

    if started {
        push_token(&mut tokens, token, stripped);
    }

    tokens
}

/// Ends a token, dropping the ones field-code removal emptied.
///
/// `""` is an empty argument the author asked for and survives. `"%f"` is a
/// substitution with nothing to substitute, and an empty string is not a
/// filename, so handing one to the program would be worse than handing it
/// nothing.
fn push_token(tokens: &mut Vec<Vec<u8>>, token: Vec<u8>, stripped: bool) {
    if !stripped || !token.is_empty() {
        tokens.push(token);
    }
}

/// Splits `Key=Value`, tolerating the separator-less and empty-key lines that
/// turn up in hand edited files.
fn split_key_value(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let separator = line.iter().position(|byte| *byte == b'=')?;
    let (key, value) = line.split_at(separator);
    let key = trim(key);

    (!key.is_empty()).then(|| (key, trim(value.get(1..).unwrap_or_default())))
}

/// The name inside a `[Group Header]` line.
fn group_name(line: &[u8]) -> Option<&[u8]> {
    line.strip_prefix(b"[")?.strip_suffix(b"]")
}

fn keep_first<'a>(slot: &mut Option<&'a [u8]>, value: &'a [u8]) {
    if slot.is_none() {
        *slot = Some(value);
    }
}

/// Only an explicit boolean `true` flips a visibility key.
fn is_true(value: Option<&[u8]>) -> bool {
    value.is_some_and(|value| value.eq_ignore_ascii_case(b"true"))
}

fn trim(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();

    while start < end && bytes[start].is_ascii_whitespace() {
        start = start.saturating_add(1);
    }
    // Also drops the `\r` of a CRLF file, which is otherwise part of the value.
    while end > start && bytes[end.saturating_sub(1)].is_ascii_whitespace() {
        end = end.saturating_sub(1);
    }

    &bytes[start..end]
}

/// Decodes one string value for display.
///
/// The format writes its string escapes literally in the file, so `\s` (a
/// space), `\n`, `\t`, `\r` and `\\` have to be decoded here or a name like
/// `Sound\sand\sVideo` reaches the menu with its backslashes intact. An escape
/// the format does not define stays exactly as written, backslash included,
/// because entries in the wild put literal Windows paths in display strings.
/// `Exec` deliberately does not come through here: [`split_exec`] already
/// gives the backslash its own meaning inside quotes.
fn text(bytes: &[u8]) -> String {
    let Some(escape) = bytes.iter().position(|byte| *byte == b'\\') else {
        // Nothing to decode, which is nearly every value of nearly every file.
        return String::from_utf8_lossy(bytes).into_owned();
    };

    let mut decoded = Vec::with_capacity(bytes.len());
    decoded.extend_from_slice(&bytes[..escape]);
    let mut index = escape;

    while let Some(&byte) = bytes.get(index) {
        index = index.saturating_add(1);
        if byte != b'\\' {
            decoded.push(byte);
            continue;
        }

        let Some(&escaped) = bytes.get(index) else {
            // A trailing backslash escapes nothing; keep it literal.
            decoded.push(b'\\');
            break;
        };
        index = index.saturating_add(1);

        match escaped {
            b's' => decoded.push(b' '),
            b'n' => decoded.push(b'\n'),
            b't' => decoded.push(b'\t'),
            b'r' => decoded.push(b'\r'),
            b'\\' => decoded.push(b'\\'),
            other => {
                decoded.push(b'\\');
                decoded.push(other);
            }
        }
    }

    into_text(decoded)
}

/// Keeps the buffer when it is already UTF-8, so the ordinary case moves the
/// bytes instead of copying them.
fn into_text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}
