//! Linux platform backend.
//!
//! XDG desktop entries and base directories, DBus, Freedesktop notifications,
//! Secret Service, portals, X11/Wayland where available (spec 18.6).
//!
//! Compiled only for its target so platform-independent crates can never
//! accidentally depend on it (spec 5.3).
//!
//! Implemented so far: application discovery over XDG desktop entries, and
//! process launching for the entries it finds. The parser covers group
//! scoping, `Type`, visibility keys, `TryExec`, locale selection and `Exec`;
//! action groups are not separately launchable entries and recursive root
//! layouts stay for a later milestone. Launching runs a program directly and
//! stops there: URI opening needs a portal or a session handler this backend
//! does not have, so it -- and everything else without an implementation --
//! keeps reporting itself unavailable (spec 18.2).
//!
//! Global shortcuts and window control are reported against the detected
//! session rather than in the abstract, because on Linux they are optional
//! (spec 18.6) and the reason they are missing differs: a Wayland compositor
//! withholds them, a headless unit has nothing to withhold.
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
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, OnceLock};
use std::thread;

use crikey_core::{CoreError, PlatformPath, Result};
use crikey_platform::{
    ApplicationDiscovery, Capability, CapabilityState, DiscoveredApplication, HotkeyService, ProcessLauncher,
    WindowService,
};

pub mod hotkeys;
pub mod window;

pub use hotkeys::{x11_binding, X11HotkeyService, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_SUPER};
pub use window::X11WindowService;

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
    desktop_names: Vec<String>,
    locale_candidates: Vec<String>,
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
    /// [`ApplicationDiscovery::discover`], so a scanner can be built before
    /// the directories it names exist. The desktop and locale environment is
    /// captured here so one rescan cannot observe a half-updated environment.
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self::with_environment(roots, current_desktop_names(), locale_candidates())
    }

    /// Records roots and an explicit desktop-name list.
    ///
    /// This is useful to embedders that already know which desktop session is
    /// active and to tests that must not depend on the process environment.
    pub fn with_desktop_names(roots: Vec<PathBuf>, desktop_names: Vec<String>) -> Self {
        Self::with_environment(roots, desktop_names, locale_candidates())
    }

    /// Records roots and explicit desktop and locale context.
    ///
    /// The values use the same spellings as `XDG_CURRENT_DESKTOP` and the
    /// locale environment variables. This constructor makes discovery
    /// deterministic for callers that already have session metadata.
    pub fn with_environment(
        roots: Vec<PathBuf>,
        desktop_names: Vec<String>,
        locale_candidates: Vec<String>,
    ) -> Self {
        let mut expanded_locales = Vec::new();
        for locale in locale_candidates {
            locale_fallbacks(&locale, &mut expanded_locales);
        }
        Self {
            roots,
            desktop_names,
            locale_candidates: expanded_locales,
        }
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
                let path = root.join(&id);
                let Some(contents) = read_entry(&path) else {
                    // Unreadable, or not a plain entry file at all: leave the
                    // id unclaimed so a later root may still supply it.
                    continue;
                };

                if let Some(application) =
                    parse_entry(&contents, &id, &self.desktop_names, &self.locale_candidates)
                {
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
pub struct CommandLauncher;

impl CommandLauncher {
    /// A launcher holding no children yet.
    ///
    /// Construction starts nothing: every process appears inside
    /// [`ProcessLauncher::launch`].
    pub fn new() -> Self {
        Self
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
    /// The caller does not wait for the child. A launcher must be usable again
    /// the instant the application it started is on its way, and an application
    /// outlives the launcher that started it; a private waiter still reaps the
    /// child when it exits.
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
        let mut child = command.spawn().map_err(|error| {
            // Both halves matter to whoever reads this: which target was
            // tried, and what the kernel said about it.
            CoreError::Invalid(format!("cannot launch {}: {error}", target.display()))
        })?;

        // Keep the child owned by a waiter until it exits. Dropping a Child
        // without waiting would leave an exited process as a zombie while the
        // launcher remains alive; waiting in this detached thread reaps it
        // without delaying the caller or tying the child to the launcher
        // object's lifetime.
        let _ = thread::spawn(move || {
            let _ = child.wait();
        });
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

/// How the running session presents itself, which decides what window control
/// and global shortcuts can honestly be claimed (spec 18.2, 18.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopEnvironment {
    /// An X11 server, where a client may grab keys and inspect other windows.
    X11,
    /// A Wayland compositor, which withholds both from ordinary clients.
    Wayland,
    /// No display server at all: a unit, a container or an SSH login.
    Headless,
}

/// Names the session from the environment `get` describes.
///
/// The getter is injected rather than read from the process environment so
/// that detection is a pure function: callers -- and tests -- decide what the
/// session looks like, and nothing here can be perturbed by the ambient
/// session a build happens to run in.
///
/// Order matters. A live socket outranks `XDG_SESSION_TYPE`, which is
/// inherited across `su`, multiplexers and user units and routinely names a
/// session that is no longer there. Between the two sockets Wayland wins:
/// XWayland sets `DISPLAY` as a compatibility shim, so a set `DISPLAY` under a
/// Wayland compositor would otherwise promise key grabs the compositor never
/// delivers. An empty value is unset -- `DISPLAY=` is exactly what a stripped
/// systemd unit hands a service -- so presence of the key alone proves
/// nothing.
pub fn detect_desktop_environment(get: impl Fn(&str) -> Option<String>) -> DesktopEnvironment {
    let present = |key: &str| get(key).filter(|value| !value.is_empty());

    if present("WAYLAND_DISPLAY").is_some() {
        return DesktopEnvironment::Wayland;
    }
    if present("DISPLAY").is_some() {
        return DesktopEnvironment::X11;
    }
    match present("XDG_SESSION_TYPE").as_deref() {
        Some("wayland") => DesktopEnvironment::Wayland,
        Some("x11") => DesktopEnvironment::X11,
        // `tty` and friends positively state there is no display server, and
        // an unrecognised value is not evidence that one exists.
        _ => DesktopEnvironment::Headless,
    }
}

#[derive(Debug)]
pub struct LinuxBackend {
    applications: DesktopEntryScanner,
    processes: CommandLauncher,
    desktop: DesktopEnvironment,
    /// Connected on first use and cached: a connection is a side effect, and
    /// the constructors are pure functions over their arguments.
    window: OnceLock<Option<window::X11WindowService>>,
    /// Connected on first use, like the window service, but held directly
    /// rather than behind a `OnceLock`: [`HotkeyService`] registration takes
    /// `&mut self`, which a shared cell cannot hand out.
    hotkeys: Option<hotkeys::X11HotkeyService>,
    /// The X server time of the last user action, written by the hotkey
    /// reader and read by the window service so that an activation carries the
    /// timestamp EWMH asks for. Shared here because it is the one thing the two
    /// X connections have to agree about.
    user_time: Arc<AtomicU32>,
}

impl LinuxBackend {
    /// Stable backend identifier surfaced by diagnostics and `crikey version`.
    pub const NAME: &'static str = "linux";

    /// Discovers applications from the XDG base directories of the running
    /// user.
    ///
    /// The session is detected from the real process environment here, once,
    /// at construction: a backend built for the running user should report the
    /// session that user is actually in, and re-reading the environment on
    /// every capability query would let a mid-run mutation change the answer a
    /// plugin was already given.
    pub fn new() -> Self {
        Self::with_application_roots(xdg_application_roots())
    }

    /// Discovers applications from exactly these roots, highest precedence
    /// first, instead of the XDG defaults. The session is still detected from
    /// the process environment.
    pub fn with_application_roots(roots: Vec<PathBuf>) -> Self {
        Self::build(
            DesktopEntryScanner::new(roots),
            detect_desktop_environment(|key| env::var(key).ok()),
        )
    }

    /// Reports for `desktop` rather than for the session this process is in,
    /// with the XDG application roots of the running user.
    pub fn with_desktop_environment(desktop: DesktopEnvironment) -> Self {
        Self::build(DesktopEntryScanner::new(xdg_application_roots()), desktop)
    }

    fn build(applications: DesktopEntryScanner, desktop: DesktopEnvironment) -> Self {
        Self {
            applications,
            processes: CommandLauncher::new(),
            desktop,
            window: OnceLock::new(),
            hotkeys: None,
            user_time: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Capability reporting is honest: a capability is claimed only once a
    /// Linux implementation stands behind it *and* the session can carry it
    /// (spec 18.2). The unimplemented arms are listed one by one so that adding
    /// a capability to the enum forces a deliberate answer here instead of
    /// inheriting a wildcard.
    ///
    /// Window control and global shortcuts are the session-dependent three
    /// (spec 18.6). Under Wayland they report
    /// [`CapabilityState::UnsupportedDesktopEnvironment`] rather than
    /// [`CapabilityState::Unavailable`], because the two say different things
    /// to a plugin author: the first is "this session does not offer it", which
    /// is a fact about the compositor and not a CriKey defect to report or a
    /// permission prompt away.
    ///
    /// The two window capabilities are [`CapabilityState::Partial`] under X11
    /// rather than `Available`, and the difference is not hedging. This function
    /// is a pure function of the detected session, deliberately: it must not
    /// open a display to answer. But window control additionally needs an EWMH
    /// *window manager*, which is a separate program that may not be running --
    /// on a bare X server [`Self::window_service`] hands out nothing. "The
    /// session type supports it, subject to a runtime gate" is exactly what
    /// `Partial` says, and it is the strongest claim this function can back.
    /// Global shortcuts stay `Available`: `GrabKey` is core protocol, so an X11
    /// display with no window manager still delivers them.
    pub fn capability(&self, capability: Capability) -> CapabilityState {
        match capability {
            Capability::ApplicationDiscovery | Capability::ProcessLaunch => CapabilityState::Available,
            Capability::GlobalHotkeys => match self.desktop {
                DesktopEnvironment::X11 => CapabilityState::Available,
                DesktopEnvironment::Wayland => CapabilityState::UnsupportedDesktopEnvironment,
                DesktopEnvironment::Headless => CapabilityState::Unavailable,
            },
            Capability::WindowEnumeration | Capability::WindowActivation => match self.desktop {
                DesktopEnvironment::X11 => CapabilityState::Partial,
                DesktopEnvironment::Wayland => CapabilityState::UnsupportedDesktopEnvironment,
                DesktopEnvironment::Headless => CapabilityState::Unavailable,
            },
            Capability::FileSearch
            | Capability::Clipboard
            | Capability::UriOpen
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

    /// The service behind [`Capability::WindowEnumeration`] and
    /// [`Capability::WindowActivation`], or `None` when this session cannot
    /// carry it.
    ///
    /// `None` for a non-X11 session without touching a display, and `None` in
    /// an X11 session that fails the EWMH handshake: that per-server gate is
    /// why [`Self::capability`] answers [`CapabilityState::Partial`] under X11
    /// rather than `Available`. Connecting once and caching keeps a repeated
    /// call from reconnecting; the cell is a `OnceLock` rather than a
    /// `LazyLock` because the connection must not happen until a caller asks
    /// for it, and a constructor stays free of that side effect.
    pub fn window_service(&self) -> Option<&dyn WindowService> {
        if self.desktop != DesktopEnvironment::X11 {
            return None;
        }
        let user_time = Arc::clone(&self.user_time);
        self.window
            .get_or_init(move || window::X11WindowService::connect_sharing(None, user_time).ok())
            .as_ref()
            .map(|service| service as &dyn WindowService)
    }

    /// The service behind [`Capability::GlobalHotkeys`], connecting on first
    /// use.
    ///
    /// `&mut` all the way down because [`HotkeyService::register`] is: a grab is
    /// exclusive server state, and two callers taking one concurrently is not a
    /// thing this backend should make expressible.
    ///
    /// # Errors
    ///
    /// [`CoreError::Invalid`] naming the session when it is not X11 -- a
    /// compositor or a headless unit has no `GrabKey` to offer -- and the
    /// connection's own refusal when the display will not carry a service. The
    /// failure is never softened into a service that swallows registrations.
    pub fn hotkeys(&mut self) -> Result<&mut dyn HotkeyService> {
        if self.desktop != DesktopEnvironment::X11 {
            return Err(CoreError::Invalid(format!(
                "global hotkeys need an X11 session; this one is {:?}, which offers no GrabKey",
                self.desktop
            )));
        }
        if self.hotkeys.is_none() {
            self.hotkeys = Some(hotkeys::X11HotkeyService::connect_sharing(
                None,
                Arc::clone(&self.user_time),
            )?);
        }
        let service = self
            .hotkeys
            .as_mut()
            .expect("the hotkey service was just connected");
        Ok(service)
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
/// no runnable `Exec`, a failed `TryExec`, an entry hidden by the current
/// desktop, or an author set `NoDisplay`/`Hidden`. Malformed lines are skipped
/// rather than aborting the parse, because one junk line in a vendor file must
/// not delete a working application. The desktop-entry specification requires
/// the complete file to be UTF-8, so a non-UTF-8 candidate is ignored instead
/// of being converted into a different application name.
fn parse_entry(
    contents: &[u8],
    id: &OsStr,
    desktop_names: &[String],
    locale_candidates: &[String],
) -> Option<DiscoveredApplication> {
    std::str::from_utf8(contents).ok()?;

    let mut inside = false;
    let mut kind = None;
    let mut name = None;
    let mut localized_names = Vec::new();
    let mut exec = None;
    let mut try_exec = None;
    let mut icon = None;
    let mut no_display = None;
    let mut hidden = None;
    let mut only_show_in = None;
    let mut not_show_in = None;

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
        // makes the choice deterministic. Localized Name keys are collected
        // separately so the active locale can override the base Name.
        match key {
            b"Type" => keep_first(&mut kind, value),
            b"Name" => keep_first(&mut name, value),
            key if localized_name_locale(key).is_some() => {
                let locale = localized_name_locale(key)?;
                if !localized_names.iter().any(|(known, _)| *known == locale) {
                    localized_names.push((locale, value));
                }
            }
            b"Exec" => keep_first(&mut exec, value),
            b"TryExec" => keep_first(&mut try_exec, value),
            b"Icon" => keep_first(&mut icon, value),
            b"NoDisplay" => keep_first(&mut no_display, value),
            b"Hidden" => keep_first(&mut hidden, value),
            b"OnlyShowIn" => keep_first(&mut only_show_in, value),
            b"NotShowIn" => keep_first(&mut not_show_in, value),
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
    if !desktop_visibility(only_show_in, not_show_in, desktop_names) {
        return None;
    }
    if let Some(try_exec) = try_exec {
        if !try_exec_is_available(try_exec) {
            return None;
        }
    }

    let name = select_name(name, &localized_names, locale_candidates)?;
    if name.is_empty() {
        return None;
    }
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

/// The desktop names advertised by the current session.
fn current_desktop_names() -> Vec<String> {
    env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .into_iter()
        .flat_map(|value| value.split(':').map(str::to_owned).collect::<Vec<_>>())
        .filter(|name| !name.is_empty())
        .collect()
}

/// Locale spellings to try, in the same order as the desktop environment.
fn locale_candidates() -> Vec<String> {
    let mut candidates = Vec::new();
    for key in ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"] {
        let Some(value) = env::var(key).ok().filter(|value| !value.is_empty()) else {
            continue;
        };
        let values = if key == "LANGUAGE" {
            value.split(':').collect::<Vec<_>>()
        } else {
            vec![value.as_str()]
        };
        for value in values {
            locale_fallbacks(value, &mut candidates);
        }
        if !candidates.is_empty() {
            break;
        }
    }
    candidates
}

fn locale_fallbacks(value: &str, candidates: &mut Vec<String>) {
    let mut add = |candidate: &str| {
        if !candidate.is_empty() && !candidates.iter().any(|known| known == candidate) {
            candidates.push(candidate.to_owned());
        }
    };
    add(value);
    let without_modifier = value.split_once('@').map_or(value, |(base, _)| base);
    add(without_modifier);
    let without_codeset = without_modifier
        .split_once('.')
        .map_or(without_modifier, |(base, _)| base);
    add(without_codeset);
    if let Some((language, _country)) = without_codeset.split_once('_') {
        add(language);
    }
}

fn localized_name_locale(key: &[u8]) -> Option<&[u8]> {
    key.strip_prefix(b"Name[")?
        .strip_suffix(b"]")
        .filter(|locale| !locale.is_empty())
}

fn select_name<'a>(
    base: Option<&'a [u8]>,
    localized: &[(&'a [u8], &'a [u8])],
    candidates: &[String],
) -> Option<&'a [u8]> {
    for candidate in candidates {
        if let Some((_, value)) = localized
            .iter()
            .find(|(locale, _)| *locale == candidate.as_bytes())
            .copied()
        {
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    base
}

fn desktop_visibility(
    only_show_in: Option<&[u8]>,
    not_show_in: Option<&[u8]>,
    desktop_names: &[String],
) -> bool {
    let matches = |value: &[u8]| {
        value
            .split(|byte| *byte == b';')
            .map(trim)
            .filter(|entry| !entry.is_empty())
            .any(|entry| desktop_names.iter().any(|desktop| entry == desktop.as_bytes()))
    };

    if let Some(only_show_in) = only_show_in {
        if !matches(only_show_in) {
            return false;
        }
    }
    !not_show_in.is_some_and(matches)
}

fn try_exec_is_available(value: &[u8]) -> bool {
    let command = PathBuf::from(OsString::from(text(value)));
    if command.as_os_str().is_empty() {
        return false;
    }
    if command.is_absolute() {
        return is_executable_file(&command);
    }

    env::var_os("PATH")
        .into_iter()
        .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .any(|directory| is_executable_file(&directory.join(&command)))
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
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
