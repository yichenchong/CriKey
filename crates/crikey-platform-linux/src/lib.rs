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
//! scoping, `Type`, visibility keys, `TryExec`, locale selection, `Path` and
//! `Exec`; `Terminal=true` entries are omitted because this backend has no
//! terminal-emulator policy, while `DBusActivatable=true` entries use their
//! `Exec` fallback. Action groups are not separately launchable entries and
//! recursive root layouts stay for a later milestone. Launching runs a
//! program directly and stops there: URI opening needs a portal or a session
//! handler this backend does not have, so it -- and everything else without
//! an implementation -- keeps reporting itself unavailable (spec 18.2).
//!
//! File search is implemented too, in [`file_search`]: a deadline-bounded
//! breadth-first walk of the user's roots, with `plocate` in front of it when
//! the session has that binary installed. Linux guarantees no index, so the
//! walk is the floor and the index is an optimisation -- and because an index
//! is only as fresh as the last `updatedb`, having one makes the reported
//! capability `Partial` rather than `Available` (spec 18.1, 18.2).
//!
//! Global shortcuts and window control are reported against the detected
//! session rather than in the abstract, because on Linux they are optional
//! (spec 18.6) and the reason they are missing differs: a Wayland compositor
//! withholds them, a headless unit has nothing to withhold. Wayland gets its
//! shortcuts back through the `GlobalShortcuts` desktop portal (ADR-0011),
//! which is a separate service and is therefore probed rather than assumed;
//! window control there stays unavailable, because no Wayland protocol lets an
//! ordinary client enumerate another client's windows.
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
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, OnceLock};
use std::thread;

use crikey_core::{CoreError, PlatformPath, Result};
use crikey_platform::{
    ApplicationDiscovery, Capability, CapabilityState, DiscoveredApplication, FileSearchService,
    HotkeyService, IconLoader, IconProvider, ProcessLauncher, StandardDirectories, WindowService,
};

pub mod icons;
pub use icons::XdgIconSource;

pub mod file_search;
pub use file_search::FilesystemSearch;

pub mod hotkeys;
pub mod wayland;
pub mod window;

pub use hotkeys::{x11_binding, X11HotkeyService, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_SUPER};
pub use wayland::WaylandHotkeyService;
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
        self.launch_in(target, args, None)
    }

    /// Starts a target with an optional desktop-entry working directory.
    ///
    /// A stale `Path=` is ignored rather than turning a launch into an error:
    /// this matches other freedesktop launchers and lets an application whose
    /// working directory was removed still start with the launcher's directory.
    fn launch_in(
        &self,
        target: &PlatformPath,
        args: &[String],
        working_directory: Option<&PlatformPath>,
    ) -> Result<()> {
        let directory = working_directory
            .filter(|directory| fs::metadata(directory.as_path()).is_ok_and(|metadata| metadata.is_dir()));
        let spawn = |directory: Option<&PlatformPath>| {
            let mut command = Command::new(target.as_os_str());
            if let Some(directory) = directory {
                command.current_dir(directory.as_path());
            }
            command
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
        };
        let mut child = match spawn(directory) {
            Err(error) if directory.is_some() && error.kind() == std::io::ErrorKind::NotFound => {
                // The directory can disappear after the metadata check. Retry
                // without it so a stale desktop entry still launches.
                spawn(None)
            }
            result => result,
        }
        .map_err(|error| {
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
    /// A Wayland compositor, where window control is withheld from ordinary
    /// clients but global shortcuts may be granted through the portal.
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
    /// The Wayland half of the same thing, held for the same reason. Two
    /// fields rather than one boxed trait object because the two services
    /// share no state and a `Box<dyn HotkeyService>` would cost this struct
    /// its derived `Debug`.
    portal_hotkeys: Option<wayland::WaylandHotkeyService>,
    /// Whether the `GlobalShortcuts` portal answers, probed on first ask and
    /// then cached. Under Wayland the honest answer to
    /// [`Capability::GlobalHotkeys`] cannot be derived from the session label
    /// alone -- the portal is a separate service that may not be installed --
    /// so this is the one place capability reporting reaches outside the
    /// process, and it reaches exactly once.
    portal: OnceLock<bool>,
    /// The X server time of the last user action, written by the hotkey
    /// reader and read by the window service so that an activation carries the
    /// timestamp EWMH asks for. Shared here because it is the one thing the two
    /// X connections have to agree about.
    user_time: Arc<AtomicU32>,
    /// Built on first use and cached, like the window service: flattening the
    /// icon theme chain stats every directory of every installed theme, which is
    /// startup work nothing should pay for before an icon is asked for, and the
    /// answer does not change for the lifetime of the process.
    icons: OnceLock<IconLoader<icons::XdgIconSource>>,
    /// Built on first use and cached, for the same reason as the icon loader:
    /// deciding whether `plocate` is installed and whether `$HOME` is a
    /// readable directory is filesystem work, and a constructor is a pure
    /// function of its arguments. Cached because the answer decides what
    /// [`Capability::FileSearch`] reports, and reporting must not change
    /// between two calls in one session.
    files: OnceLock<FilesystemSearch>,
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

    /// Reports for `desktop` with the portal probe already answered, instead
    /// of asking the running session bus.
    ///
    /// The seam the capability tests need: under Wayland the truthful answer
    /// for global shortcuts depends on a portal, and a test that consulted the
    /// build host's bus would pass or fail on whether that host runs a desktop.
    /// Only the *reporting* is injected -- [`Self::hotkeys`] still connects for
    /// real and still refuses by name when nothing answers.
    pub fn with_desktop_environment_and_portal(desktop: DesktopEnvironment, portal: bool) -> Self {
        let backend = Self::with_desktop_environment(desktop);
        let _ = backend.portal.set(portal);
        backend
    }

    /// Searches files through `files` instead of through the roots and the
    /// index of the running session.
    ///
    /// The same kind of seam as the portal injection above, and needed for the
    /// same reason: what [`Capability::FileSearch`] may claim depends on
    /// whether `$HOME` is readable and whether `plocate` is installed, so a
    /// reporting test that used the session service would pass or fail on the
    /// build host's package list.
    pub fn with_file_search(self, files: FilesystemSearch) -> Self {
        let _ = self.files.set(files);
        self
    }

    fn build(applications: DesktopEntryScanner, desktop: DesktopEnvironment) -> Self {
        Self {
            applications,
            processes: CommandLauncher::new(),
            desktop,
            window: OnceLock::new(),
            hotkeys: None,
            portal_hotkeys: None,
            portal: OnceLock::new(),
            user_time: Arc::new(AtomicU32::new(0)),
            icons: OnceLock::new(),
            files: OnceLock::new(),
        }
    }

    /// Capability reporting is honest: a capability is claimed only once a
    /// Linux implementation stands behind it *and* the session can carry it
    /// (spec 18.2). The unimplemented arms are listed one by one so that adding
    /// a capability to the enum forces a deliberate answer here instead of
    /// inheriting a wildcard.
    ///
    /// Window control and global shortcuts are the session-dependent three
    /// (spec 18.6). Under Wayland window control reports
    /// [`CapabilityState::UnsupportedDesktopEnvironment`] rather than
    /// [`CapabilityState::Unavailable`], because the two say different things
    /// to a plugin author: the first is "this session does not offer it", which
    /// is a fact about the compositor and not a CriKey defect to report or a
    /// permission prompt away.
    ///
    /// Global shortcuts under Wayland are the one answer this function cannot
    /// derive from the session label, and the one place it reaches outside the
    /// process. The compositor withholds key grabs, but the
    /// `GlobalShortcuts` portal grants them back (ADR-0011) -- and the portal
    /// is a separate service that may not be installed. So the portal is
    /// probed, once, and `Available` means it answered while `Unavailable`
    /// means nothing did. Reporting `UnsupportedDesktopEnvironment` for a
    /// session that does offer shortcuts would send a plugin author looking for
    /// a compositor limitation that is not there.
    ///
    /// The two window capabilities are [`CapabilityState::Partial`] under X11
    /// rather than `Available`, and the difference is not hedging. That answer
    /// stays a pure function of the detected session: it must not open a
    /// display. But window control additionally needs an EWMH *window
    /// manager*, which is a separate program that may not be running -- on a
    /// bare X server [`Self::window_service`] hands out nothing. "The session
    /// type supports it, subject to a runtime gate" is exactly what `Partial`
    /// says, and it is the strongest claim that can be backed without
    /// connecting. Global shortcuts stay `Available` under X11: `GrabKey` is
    /// core protocol, so an X11 display with no window manager still delivers
    /// them.
    pub fn capability(&self, capability: Capability) -> CapabilityState {
        match capability {
            Capability::ApplicationDiscovery | Capability::ProcessLaunch => CapabilityState::Available,
            Capability::GlobalHotkeys => match self.desktop {
                DesktopEnvironment::X11 => CapabilityState::Available,
                DesktopEnvironment::Wayland => {
                    if self.portal_answers() {
                        CapabilityState::Available
                    } else {
                        CapabilityState::Unavailable
                    }
                }
                DesktopEnvironment::Headless => CapabilityState::Unavailable,
            },
            Capability::WindowEnumeration | Capability::WindowActivation => match self.desktop {
                DesktopEnvironment::X11 => CapabilityState::Partial,
                DesktopEnvironment::Wayland => CapabilityState::UnsupportedDesktopEnvironment,
                DesktopEnvironment::Headless => CapabilityState::Unavailable,
            },
            // Themed names and absolute paths resolve, PNG and SVG decode, and
            // the result is cached -- but `.svgz` and `.xpm` theme assets are
            // not decoded and scaled (HiDPI) theme directories are skipped, so
            // there are real icon files on a real system that this finds nothing
            // usable for. `Partial` is what that is; `Available` would be a
            // claim the `.xpm`-only icons in `/usr/share/pixmaps` disprove.
            Capability::Icons => CapabilityState::Partial,
            // File search is the one capability whose answer comes from the
            // filesystem rather than from the session: a walk needs no display
            // and no daemon, only a readable root. With one it is `Available`,
            // and with none there is nothing to search and nothing to claim.
            // An installed `plocate` *lowers* the claim to `Partial` on
            // purpose: that answer comes from an index rebuilt on a timer, so a
            // file saved since the last `updatedb` is missing from it, and
            // `Partial` is what "real results that do not cover everything"
            // is called (spec 18.2).
            Capability::FileSearch => match self.file_search() {
                None => CapabilityState::Unavailable,
                Some(_) if self.files().uses_index() => CapabilityState::Partial,
                Some(_) => CapabilityState::Available,
            },
            Capability::Clipboard
            | Capability::UriOpen
            | Capability::Notifications
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

    /// The provider behind [`Capability::Icons`], built on first use.
    ///
    /// Always a provider, never an `Option`: a session with no installed themes
    /// resolves nothing and says so per reference, which is the same answer an
    /// item whose plugin named no icon gets. There is no session-level gate here
    /// of the kind window control has, so there is nothing for an `Option` to
    /// express.
    pub fn icon_provider(&self) -> &dyn IconProvider {
        self.icons.get_or_init(|| {
            let source = icons::XdgIconSource::for_session();
            match StandardDirectories::for_process() {
                Ok(directories) => IconLoader::caching(source, Self::NAME, &directories),
                // No resolvable cache directory means decoding on every lookup,
                // which is slower and completely correct. Refusing to draw icons
                // because a *disposable* cache has nowhere to live would not be.
                Err(_) => IconLoader::new(source),
            }
        })
    }

    /// The service behind [`Capability::FileSearch`], or `None` when this
    /// session has nothing for it to search.
    ///
    /// `None` is not a hypothetical: a systemd unit or a container with no
    /// `$HOME` gives the walk no root, and a service with no root can only
    /// return empty answers forever. Handing one out anyway would teach the
    /// user that file search is broken; declining to, and reporting
    /// [`CapabilityState::Unavailable`] for the same reason, tells them why.
    pub fn file_search(&self) -> Option<&dyn FileSearchService> {
        let files = self.files();
        if files.roots().is_empty() {
            return None;
        }

        Some(files)
    }

    /// The file search service as its concrete type, built on first use.
    ///
    /// Separate from [`Self::file_search`] because capability reporting needs
    /// [`FilesystemSearch::uses_index`], which is not part of the trait: the
    /// trait is what a caller searches through, and how fresh the answer can be
    /// is a fact about this backend's session.
    fn files(&self) -> &FilesystemSearch {
        self.files.get_or_init(FilesystemSearch::for_session)
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

    /// Whether the `GlobalShortcuts` portal answers, asked once.
    ///
    /// Cached because a capability query is cheap by contract and a bus round
    /// trip is not, and because an answer that changed between two queries
    /// would let one plugin be told the launcher has hotkeys while the next is
    /// told it does not.
    fn portal_answers(&self) -> bool {
        *self.portal.get_or_init(wayland::portal_is_available)
    }

    /// The service behind [`Capability::GlobalHotkeys`], connecting on first
    /// use.
    ///
    /// X11 grabs keys itself; Wayland asks the portal for them (ADR-0011).
    /// Both are real bindings and both arrive through the same callback
    /// contract, so a caller never has to know which session it is in.
    ///
    /// `&mut` all the way down because [`HotkeyService::register`] is: a grab is
    /// exclusive server state, and two callers taking one concurrently is not a
    /// thing this backend should make expressible.
    ///
    /// # Errors
    ///
    /// [`CoreError::Invalid`] naming the session when it can offer nothing --
    /// a headless unit has neither a display to grab against nor a desktop
    /// portal to ask -- and the display's or the portal's own refusal
    /// otherwise. The failure is never softened into a service that swallows
    /// registrations.
    pub fn hotkeys(&mut self) -> Result<&mut dyn HotkeyService> {
        match self.desktop {
            DesktopEnvironment::X11 => {
                if self.hotkeys.is_none() {
                    self.hotkeys = Some(hotkeys::X11HotkeyService::connect_sharing(
                        None,
                        Arc::clone(&self.user_time),
                    )?);
                }
                let Some(service) = self.hotkeys.as_mut() else {
                    return Err(CoreError::Invalid(
                        "the X11 hotkey service disappeared before it could be returned".to_owned(),
                    ));
                };
                Ok(service)
            }
            DesktopEnvironment::Wayland => {
                if self.portal_hotkeys.is_none() {
                    self.portal_hotkeys = Some(wayland::WaylandHotkeyService::connect()?);
                }
                let Some(service) = self.portal_hotkeys.as_mut() else {
                    return Err(CoreError::Invalid(
                        "the Wayland hotkey service disappeared before it could be returned".to_owned(),
                    ));
                };
                Ok(service)
            }
            DesktopEnvironment::Headless => Err(CoreError::Invalid(format!(
                "global hotkeys need an X11 display or a desktop portal; this one is {:?}, which \
                 offers neither",
                self.desktop
            ))),
        }
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
/// Opening a path can race a directory scan: a candidate that was a regular
/// file can become a FIFO or device before the open. `O_NONBLOCK` makes that
/// open safe, and metadata taken from the opened descriptor checks the object
/// that will actually be read. Metadata deliberately follows symlinks --
/// distributions do ship entries as links -- while the descriptor check still
/// rejects links to devices and other non-files.
///
/// The size is capped twice. The stat rejects a file that is already too big,
/// and the read still runs through a reader limited to one byte past the cap,
/// so a file that grows between the two calls is dropped rather than followed.
fn read_entry(path: &Path) -> Option<Vec<u8>> {
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > DesktopEntryScanner::MAX_ENTRY_BYTES {
        return None;
    }

    // One byte past the cap, so an oversized file is detected by the read
    // itself instead of being trusted to match the size the stat reported.
    let limit = DesktopEntryScanner::MAX_ENTRY_BYTES.saturating_add(1);
    let mut contents = Vec::with_capacity(usize::try_from(metadata.len()).ok()?);
    let mut reader = file.take(limit);
    reader.read_to_end(&mut contents).ok()?;

    // The extra byte is spent only by a file that outgrew the cap.
    (reader.limit() > 0).then_some(contents)
}

/// `None` means "nothing a launcher can show or run": another `Type`, no name,
/// no runnable `Exec`, a failed `TryExec`, an entry hidden by the current
/// desktop, an entry requiring a terminal that this backend cannot provide, or
/// an author set `NoDisplay`/`Hidden`. Malformed lines are skipped rather than
/// aborting the parse, because one junk line in a vendor file must not delete a
/// working application. The desktop-entry specification requires the complete
/// file to be UTF-8, so a non-UTF-8 candidate is ignored instead of being
/// converted into a different application name.
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
    let mut terminal = None;
    let mut working_directory = None;
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
            b"Path" => keep_first(&mut working_directory, value),
            b"Terminal" => keep_first(&mut terminal, value),
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
    // There is no portable terminal-emulator policy in this backend. Hiding
    // terminal entries is safer than presenting an item that will silently
    // lose its terminal when launched with detached standard streams.
    if is_true(terminal) {
        return None;
    }
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
    let working_directory = working_directory
        .filter(|path| !path.is_empty())
        .map(|path| PlatformPath::new(text(path)));

    Some(DiscoveredApplication {
        name: text(name),
        target,
        arguments,
        icon_reference: icon.filter(|icon| !icon.is_empty()).map(text),
        working_directory,
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
    let mut tokens = split_exec(value)?.into_iter();
    let program = tokens.next().filter(|program| !program.is_empty())?;

    Some((
        PlatformPath::new(OsString::from_vec(program)),
        tokens.map(into_text).collect(),
    ))
}

/// Decodes the general string escapes before the `Exec` quoting rules run.
///
/// Desktop-entry values use `\\s`, `\\n`, `\\t`, `\\r` and `\\\\` escapes
/// before the command-specific quoting pass. In particular, a literal
/// backslash inside a quoted argument needs four source backslashes: the
/// generic pass reduces those to two and the quoting pass reduces them to one.
fn unescape_exec_value(value: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut index = 0;
    while let Some(&byte) = value.get(index) {
        index = index.saturating_add(1);
        if byte != b'\\' {
            decoded.push(byte);
            continue;
        }

        let Some(&escaped) = value.get(index) else {
            decoded.push(b'\\');
            break;
        };
        let replacement = match escaped {
            b's' => Some(b' '),
            b'n' => Some(b'\n'),
            b't' => Some(b'\t'),
            b'r' => Some(b'\r'),
            b'\\' => Some(b'\\'),
            _ => None,
        };
        if let Some(replacement) = replacement {
            decoded.push(replacement);
            index = index.saturating_add(1);
        } else {
            decoded.push(b'\\');
        }
    }
    decoded
}

/// Tokenizes an `Exec` value.
///
/// Desktop-entry string escapes are decoded before command quoting. Double
/// quotes group a token, and inside them a backslash escapes only the four
/// characters the specification names. Field codes are launcher substitutions
/// rather than arguments, so they are removed and `%%` collapses to a single
/// percent. An unknown or unescaped field code makes the command invalid.
fn split_exec(value: &[u8]) -> Option<Vec<Vec<u8>>> {
    let value = unescape_exec_value(value);
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

        // Field-code expansion happens after the generic string escapes but
        // before the resulting argument is handed to the executable. A
        // backslash-escaped percent is not valid Exec syntax and is rejected
        // rather than silently handing a placeholder to the child.
        if byte == b'%' {
            match value.get(index) {
                Some(b'%') => {
                    token.push(b'%');
                    started = true;
                    index = index.saturating_add(1);
                }
                Some(code) if FIELD_CODES.contains(code) => {
                    if matches!(code, b'F' | b'U' | b'i') && !token.is_empty() {
                        return None;
                    }
                    stripped = true;
                    index = index.saturating_add(1);
                }
                _ => return None,
            }
            continue;
        }

        if quoted {
            match byte {
                b'"' => quoted = false,
                b'\\' => match value.get(index) {
                    Some(&escaped) if matches!(escaped, b'"' | b'`' | b'$' | b'\\') => {
                        token.push(escaped);
                        index = index.saturating_add(1);
                    }
                    Some(_) => token.push(b'\\'),
                    None => return None,
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
            _ if matches!(
                byte,
                b'\''
                    | b'\\'
                    | b'`'
                    | b'$'
                    | b'>'
                    | b'<'
                    | b'~'
                    | b'|'
                    | b'&'
                    | b';'
                    | b'*'
                    | b'?'
                    | b'#'
                    | b'('
                    | b')'
            ) =>
            {
                return None
            }
            _ => {
                token.push(byte);
                started = true;
            }
        }
    }

    if quoted {
        return None;
    }
    if started {
        push_token(&mut tokens, token, stripped);
    }

    Some(tokens)
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
