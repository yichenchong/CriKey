//! macOS platform backend.
//!
//! Application bundles, Launch Services, Spotlight metadata, Keychain,
//! accessibility-based window integration (spec 18.5).
//!
//! Compiled only for its target so platform-independent crates can never
//! accidentally depend on it (spec 5.3).
//!
//! Implemented so far: application discovery over `.app` bundles, process
//! launching through `/usr/bin/open`, and the general pasteboard
//! ([`clipboard`]). The first two stop at what the core actually
//! consumes. Discovery reads the four `Info.plist` keys a launcher needs and
//! nothing else, so icons, document types and Spotlight metadata stay for a
//! later milestone; launching hands the target to Launch Services rather than
//! executing `Contents/MacOS/<executable>` directly, because a bundle expects
//! to be started with its own environment, activation policy and single
//! instance semantics. Everything else -- Keychain, notifications, the
//! accessibility APIs behind window control -- keeps reporting itself
//! unavailable (spec 18.2).
//!
//! The `Info.plist` parsing itself lives in `crikey-platform`
//! ([`parse_info_plist`], [`bundle_display_name`]): this crate cannot be built,
//! let alone tested, anywhere but macOS, so the pure data transformation is
//! kept where every host can exercise it and only the filesystem walk and the
//! OS binding remain here.

#![cfg(target_os = "macos")]

pub mod clipboard;
pub mod file_search;

pub use clipboard::MacPasteboard;

use crikey_core::{CoreError, PlatformPath, Result};
use crikey_platform::{
    bundle_display_name, bundle_icon_path, parse_info_plist, ApplicationDiscovery, Capability,
    CapabilityState, Clipboard, DiscoveredApplication, FileOpener, FileSearchService, IconLoader,
    IconProvider, PathIconSource, ProcessLauncher, StandardDirectories,
};
use file_search::MacFileSearch;
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread;

/// The suffix a bundle directory carries, as bytes: a directory name is not
/// required to be UTF-8, and the check has to happen before any conversion.
const BUNDLE_SUFFIX: &[u8] = b".app";

/// Where a bundle keeps the property list describing it.
const INFO_PLIST: &str = "Contents/Info.plist";

/// The directory holding a bundle's executable, and therefore the marker that
/// separates an application bundle from the other `.app` directories -- backup
/// copies, stubs, plain folders somebody renamed -- that turn up in the same
/// place.
const EXECUTABLE_DIRECTORY: &str = "Contents/MacOS";

/// Launch Services' command line front end.
///
/// Addressed by absolute path rather than by name so the launcher runs Apple's
/// tool and not whatever a user's `PATH` happens to put first.
const OPEN: &str = "/usr/bin/open";

/// Application discovery over `.app` bundles (spec 18.5).
///
/// Two kinds of source feed one scan. *Roots* are directories whose immediate
/// `.app` children are applications; *named bundles* are individual bundle
/// paths that no install location contains. Roots are scanned in the order they
/// were given and the earliest root wins a duplicate bundle directory name,
/// which is what lets `~/Applications` override a system-wide copy of the same
/// application; named bundles are read last, so a scanned copy of the same
/// bundle name always wins over a hard-coded path.
#[derive(Debug)]
pub struct BundleScanner {
    roots: Vec<PathBuf>,
    bundles: Vec<PathBuf>,
}

impl BundleScanner {
    /// The largest `Info.plist` the scanner will read, in bytes.
    ///
    /// Even a heavily localised property list stays far below this, so the cap
    /// costs no real bundle anything. It is public because it is observable
    /// behaviour: a file past it is skipped whole, and the bundle then falls
    /// back to the name its directory carries rather than appearing half
    /// parsed.
    pub const MAX_INFO_PLIST_BYTES: u64 = 1024 * 1024;

    /// Records the roots to scan, highest precedence first, and no named
    /// bundles.
    ///
    /// Construction touches no filesystem: every read happens inside
    /// [`ApplicationDiscovery::discover`], so a scanner can be built before the
    /// directories it names exist.
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            bundles: Vec::new(),
        }
    }

    /// Records the roots to scan and individual bundle paths to read besides
    /// them.
    ///
    /// Each named bundle is a candidate, not a claim: one that is not there is
    /// silently absent from the result (see `well_known_bundles`).
    pub fn with_bundles(roots: Vec<PathBuf>, bundles: Vec<PathBuf>) -> Self {
        Self { roots, bundles }
    }

    /// The individual bundle paths this scanner reads besides its roots.
    pub fn bundles(&self) -> &[PathBuf] {
        &self.bundles
    }
}

impl ApplicationDiscovery for BundleScanner {
    /// Scans every root and every named bundle once, and returns what it found.
    ///
    /// This never fails. `~/Applications` does not exist on most machines, a
    /// named bundle is a path this build believes in rather than one it can
    /// insist on, and a bundle installed by a third party may be unreadable or
    /// malformed; none of those is a reason to hide every other application on
    /// the machine.
    fn discover(&self) -> Result<Vec<DiscoveredApplication>> {
        let mut discovered = Vec::new();
        let mut claimed: HashSet<OsString> = HashSet::new();

        for root in &self.roots {
            let Ok(directory) = fs::read_dir(root) else {
                continue;
            };

            let mut names: Vec<OsString> = directory
                .flatten()
                .map(|entry| entry.file_name())
                .filter(|name| name.as_bytes().ends_with(BUNDLE_SUFFIX))
                .collect();
            // Directory order is filesystem defined; sorting makes a rescan of
            // an unchanged root repeat itself exactly.
            names.sort_unstable();

            for name in names {
                if claimed.contains(&name) {
                    continue;
                }
                if let Some(application) = read_bundle(&root.join(&name), &name) {
                    discovered.push(application);
                    // The name is spent only once it named a real bundle, so a
                    // renamed folder ending in `.app` does not shadow the
                    // application of the same name in a later root.
                    claimed.insert(name);
                }
            }
        }

        // Named bundles last: a copy under a scanned root carries the user's own
        // version of the application and has already claimed the name.
        for bundle in &self.bundles {
            let Some(name) = bundle.file_name() else {
                continue;
            };
            if claimed.contains(name) {
                continue;
            }
            if let Some(application) = read_bundle(bundle, name) {
                discovered.push(application);
                claimed.insert(name.to_owned());
            }
        }

        Ok(discovered)
    }
}

/// Reads one candidate directory into a discovery result.
///
/// `None` means "not a bundle a launcher can show": a name with nothing left
/// once `.app` is stripped, or a directory without the executable directory
/// every application bundle has. A missing, oversized, binary or malformed
/// `Info.plist` is not disqualifying: the application is still there and Launch
/// Services still opens it, so it is reported under the name its directory
/// carries, with no identifier.
fn read_bundle(path: &Path, name: &OsStr) -> Option<DiscoveredApplication> {
    let directory_name = bundle_stem(name)?;
    if !path.join(EXECUTABLE_DIRECTORY).is_dir() {
        return None;
    }

    let info = read_info_plist(&path.join(INFO_PLIST)).and_then(|xml| parse_info_plist(&xml));
    let (name, bundle_id, icon_file) = match info {
        Some(bundle) => (bundle.name, bundle.bundle_id, bundle.icon_file),
        None => (directory_name, None, None),
    };

    Some(DiscoveredApplication {
        name,
        // Launch Services opens the bundle, not the executable inside it, so
        // the bundle directory is the launch target.
        target: PlatformPath::new(path.as_os_str().to_owned()),
        arguments: Vec::new(),
        // The resolved `.icns` file, which [`MacOsBackend::icon_provider`]
        // decodes. A reference is only recorded when the file is really there:
        // `CFBundleIconFile` is author supplied and routinely names a resource a
        // trimmed or relocated bundle no longer ships.
        //
        // The path has to be UTF-8 to be carried, because an icon reference is a
        // `String` (spec 10.1) and a lossy conversion would name a different
        // file. A bundle under a non-UTF-8 path therefore loses its icon and
        // keeps everything else, which is the right way round: the launch target
        // keeps the exact bytes (spec 18.3).
        icon_reference: icon_file
            .as_deref()
            .and_then(|icon_file| bundle_icon_path(path, icon_file))
            .and_then(|icon| icon.to_str().map(str::to_owned)),
        platform_id: bundle_id,
        working_directory: None,
    })
}

/// The display name a bundle directory carries.
///
/// The UTF-8 case is the shared [`bundle_display_name`] rule, which is where
/// the exact suffix matching is pinned. A directory name that is not UTF-8
/// still names a real application, so only the label degrades to replacement
/// characters; the launch target keeps the exact bytes (spec 18.3).
fn bundle_stem(name: &OsStr) -> Option<String> {
    match name.to_str() {
        Some(name) => bundle_display_name(name).map(str::to_owned),
        None => {
            let stem = name.as_bytes().strip_suffix(BUNDLE_SUFFIX)?;
            (!stem.is_empty()).then(|| String::from_utf8_lossy(stem).into_owned())
        }
    }
}

/// Reads an `Info.plist`, refusing anything that is not an ordinary file of
/// plausible size and encoding.
///
/// The file is opened non-blocking before its metadata is checked. Checking
/// first and opening second is a TOCTOU race: a third party can replace a
/// regular file with a FIFO after the stat, making discovery block forever.
/// Opening first also makes the size check and the bytes read refer to the same
/// file, while following symlinks so a linked application remains supported.
///
/// The read is still capped one byte past the maximum, so a file that grows
/// after opening is dropped rather than followed.
///
/// A file that is not UTF-8 is a binary property list, which this backend
/// cannot decode; it is reported as absent so the caller falls back to the
/// directory name instead of parsing bytes it cannot read.
fn read_info_plist(path: &Path) -> Option<String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > BundleScanner::MAX_INFO_PLIST_BYTES {
        return None;
    }

    let mut contents = Vec::new();
    file.take(BundleScanner::MAX_INFO_PLIST_BYTES + 1)
        .read_to_end(&mut contents)
        .ok()?;
    if contents.len() as u64 > BundleScanner::MAX_INFO_PLIST_BYTES {
        return None;
    }

    String::from_utf8(contents).ok()
}

/// Process launching through Launch Services (spec 18.1).
///
/// A bundle is not started by executing `Contents/MacOS/<executable>`: doing so
/// bypasses the activation policy, the single-instance rule and the environment
/// the application expects, and a second copy of an already running application
/// is exactly the bug that produces. `/usr/bin/open` is the supported command
/// line entry point to Launch Services, so every launch goes through it.
#[derive(Debug, Default)]
pub struct OpenLauncher;

impl OpenLauncher {
    /// A launcher holding nothing.
    ///
    /// Construction starts nothing: every process appears inside
    /// [`ProcessLauncher::launch`].
    pub fn new() -> Self {
        Self
    }

    /// Spawns an already assembled `open` invocation, detached.
    ///
    /// Nothing is waited for. A launcher must be usable again the instant the
    /// application it started is on its way, and blocking on Launch Services
    /// would stall the UI for as long as the window server takes. The cost is
    /// stated rather than hidden: only the spawn itself is reported, so a
    /// request `open` accepts and then fails to satisfy is not observable here.
    ///
    /// Standard streams are detached and the child enters a new process group,
    /// so a terminal interrupt sent to CriKey's foreground group does not kill
    /// the launch and `open` cannot block writing into a pipe nobody drains.
    fn spawn(&self, command: &mut Command, subject: &str) -> Result<()> {
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|error| {
                // Both halves matter to whoever reads this: what was tried, and
                // what the kernel said about it.
                CoreError::Invalid(format!("cannot open {subject}: {error}"))
            })?;

        reap(child);
        Ok(())
    }
}

impl ProcessLauncher for OpenLauncher {
    /// Opens `target` through Launch Services, passing `args` to it.
    ///
    /// The target is handed over as its own `OsStr`, so an install path that is
    /// not UTF-8 launches unchanged (spec 18.3), and every argument is passed
    /// individually: spaces, quotes and empty strings inside one argument reach
    /// the application as written.
    ///
    /// Arguments require `-a`, because `open`'s `--args` applies to the
    /// application named by that option and not to a positional file operand.
    /// Without arguments the target stays positional, which is what lets the
    /// same call open a document as well as a bundle.
    fn launch(&self, target: &PlatformPath, args: &[String]) -> Result<()> {
        self.launch_with_directory(target, args, None)
    }

    /// Opens a target with a caller-selected current directory.
    ///
    /// Launch Services still owns application activation; setting the
    /// `/usr/bin/open` helper's directory preserves the process-launcher
    /// contract for relative targets and helper-side path resolution.
    fn launch_in(
        &self,
        target: &PlatformPath,
        args: &[String],
        working_directory: Option<&PlatformPath>,
    ) -> Result<()> {
        self.launch_with_directory(target, args, working_directory)
    }

    fn open_uri(&self, uri: &str) -> Result<()> {
        if !has_scheme(uri) {
            return Err(CoreError::Invalid(format!(
                "not a URI, so it names no scheme to open it with: {uri}"
            )));
        }

        let mut command = Command::new(OPEN);
        command.arg(uri);
        self.spawn(&mut command, uri)
    }
}

impl OpenLauncher {
    fn launch_with_directory(
        &self,
        target: &PlatformPath,
        args: &[String],
        working_directory: Option<&PlatformPath>,
    ) -> Result<()> {
        if target.as_os_str().is_empty() {
            return Err(CoreError::Invalid(
                "the macOS backend cannot launch an empty target".to_owned(),
            ));
        }
        let mut command = Command::new(OPEN);
        if args.is_empty() {
            command.arg(operand(target));
        } else {
            command.arg("-a").arg(operand(target)).arg("--args").args(args);
        }
        if let Some(directory) = working_directory {
            if directory.as_os_str().is_empty() {
                return Err(CoreError::Invalid(
                    "the macOS backend cannot use an empty working directory".to_owned(),
                ));
            }
            command.current_dir(directory.as_os_str());
        }
        self.spawn(&mut command, &target.display().to_string())
    }
}

/// Opening a file or folder through Launch Services (spec 18.2).
///
/// The same `/usr/bin/open` the launcher uses, because on macOS these really
/// are one mechanism: a positional operand is opened with the application
/// Launch Services has associated with it, whether that operand is a document,
/// a folder or a bundle.
///
/// The path travels as one argv entry. No shell is involved, which is not a
/// stylistic preference: a file legitimately named `report;rm -rf ~.txt` or
/// `$(reboot).pdf` is a file the user is entitled to open, and any construction
/// that built a command *string* would run it instead. `Command` execs `open`
/// directly, so those bytes can only ever arrive as an argument.
impl FileOpener for OpenLauncher {
    fn open_path(&self, path: &PlatformPath) -> Result<()> {
        if path.as_os_str().is_empty() {
            return Err(CoreError::Invalid(
                "the macOS backend cannot open an empty path".to_owned(),
            ));
        }
        let mut command = Command::new(OPEN);
        command.arg(operand(path));
        self.spawn(&mut command, &path.display().to_string())
    }

    /// Selects the item in the Finder, through `open -R`.
    ///
    /// A true reveal, unlike the other two backends: `-R` is Launch Services'
    /// documented "reveal in Finder" and it selects the item rather than merely
    /// opening the directory around it.
    fn reveal_path(&self, path: &PlatformPath) -> Result<()> {
        if path.as_os_str().is_empty() {
            return Err(CoreError::Invalid(
                "the macOS backend cannot reveal an empty path".to_owned(),
            ));
        }
        let mut command = Command::new(OPEN);
        command.arg("-R").arg(operand(path));
        self.spawn(&mut command, &path.display().to_string())
    }
}

/// Collects `child`'s exit status on a thread of its own.
///
/// `open` hands the request to Launch Services and exits within milliseconds,
/// but a child nobody waits for stays a zombie holding a pid, and a launcher
/// lives for a whole desktop session. Neither retaining nor dropping the
/// [`Child`] reaps it on Unix, and sweeping stored handles on the *next* launch
/// leaves the common case -- one launch, then hours of idleness -- with a
/// zombie for the rest of the session. So each launch gets one short-lived
/// thread that performs the blocking wait the UI thread must never perform; it
/// ends the moment `open` does.
///
/// If the thread cannot be created -- the process is out of memory or at its
/// thread limit -- the child is waited for synchronously as a last resort.
/// `open` normally exits within milliseconds, and this fallback closes the
/// process handle rather than leaving a zombie behind. The launch itself
/// already succeeded, so a reaper-thread failure is not turned into an error.
fn reap(child: Child) {
    let child = Arc::new(Mutex::new(Some(child)));
    let reaper_child = Arc::clone(&child);
    if thread::Builder::new()
        .name("crikey-open-reaper".to_owned())
        .spawn(move || {
            let child = reaper_child.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(mut child) = child {
                let _ = child.wait();
            }
        })
        .is_err()
    {
        let child = child.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(mut child) = child {
            let _ = child.wait();
        }
    }
}

/// A path spelled so `open` reads it as an operand and not as an option.
///
/// Only a relative path can begin with `-`, so prefixing `./` names the same
/// file and can never collide with an absolute one.
fn operand(target: &PlatformPath) -> OsString {
    let bytes = target.as_os_str().as_bytes();
    if bytes.first() != Some(&b'-') {
        return target.as_os_str().to_owned();
    }

    let mut prefixed = Vec::with_capacity(bytes.len() + 2);
    prefixed.extend_from_slice(b"./");
    prefixed.extend_from_slice(bytes);
    OsString::from_vec(prefixed)
}

/// Whether `uri` opens with an RFC 3986 scheme: a letter, then letters, digits,
/// `+`, `-` or `.`, then a colon.
fn has_scheme(uri: &str) -> bool {
    let Some((scheme, _)) = uri.split_once(':') else {
        return false;
    };
    scheme.starts_with(|first: char| first.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.'))
}

#[derive(Debug)]
pub struct MacOsBackend {
    applications: BundleScanner,
    processes: OpenLauncher,
    /// Built on first use and cached: resolving the cache directory reads the
    /// environment, which a constructor that touches nothing should not do.
    icons: OnceLock<IconLoader<PathIconSource>>,
    /// Spotlight, with a bounded walk of the home directory behind it. Its
    /// own roots resolve on first use for the same reason the icons do.
    files: MacFileSearch,
    /// Whether the pasteboard server hands this process a general pasteboard,
    /// probed on first ask and then cached.
    ///
    /// A probe rather than an assumption because `generalPasteboard` returns
    /// null for a process without access to it, and cached because the answer
    /// decides what [`Capability::Clipboard`] reports, which must not change
    /// between two calls in one session. Acquiring the handle neither reads the
    /// user's pasteboard nor writes to it, so this costs the session nothing.
    pasteboard: OnceLock<bool>,
}

impl MacOsBackend {
    /// Stable backend identifier surfaced by diagnostics and `crikey version`.
    pub const NAME: &'static str = "macos";

    /// Discovers applications from the standard bundle locations of the
    /// running user, plus the bundles macOS keeps outside them.
    pub fn new() -> Self {
        Self::with_application_sources(bundle_roots(), well_known_bundles())
    }

    /// Discovers applications from exactly these roots, highest precedence
    /// first, instead of the standard locations, and names no bundle of its own.
    pub fn with_application_roots(roots: Vec<PathBuf>) -> Self {
        Self::with_application_sources(roots, Vec::new())
    }

    /// Discovers applications from exactly these roots and these individual
    /// bundle paths, instead of the standard locations.
    pub fn with_application_sources(roots: Vec<PathBuf>, bundles: Vec<PathBuf>) -> Self {
        Self {
            applications: BundleScanner::with_bundles(roots, bundles),
            processes: OpenLauncher::new(),
            icons: OnceLock::new(),
            files: MacFileSearch::new(),
            pasteboard: OnceLock::new(),
        }
    }

    /// Capability reporting is honest: a capability is claimed only once a
    /// macOS implementation stands behind it (spec 18.2). The unimplemented
    /// arms are listed one by one so that adding a capability to the enum
    /// forces a deliberate answer here instead of inheriting a wildcard.
    pub fn capability(&self, capability: Capability) -> CapabilityState {
        match capability {
            // `/usr/bin/open` is always present on macOS and resolves handlers,
            // schemes and reveals alike, so all four are one claim here. That
            // `FileOpen` and `UriOpen` are separate variants is a fact about
            // Linux, not about this backend.
            Capability::ApplicationDiscovery
            | Capability::ProcessLaunch
            | Capability::UriOpen
            | Capability::FileOpen => CapabilityState::Available,
            // A bundle's own `.icns` resolves and decodes. Everything else macOS
            // calls an icon does not: a document icon composed by Launch
            // Services, a folder or volume icon, the generic icon for a bundle
            // that ships none, and the badge overlays `NSWorkspace` applies are
            // all `NSImage` compositions rather than files, and none of them is
            // implemented. `Partial` is that split.
            Capability::Icons => CapabilityState::Partial,
            // Spotlight answers first and cannot report what it was not
            // allowed to see, so the honest ceiling is `Partial`; the service
            // itself owns that reasoning.
            Capability::FileSearch => self.files.capability_state(),
            // The pasteboard is a store the window server keeps, so there is
            // nothing session-dependent to hedge about and nothing to stay
            // resident for -- but the process really can have no pasteboard at
            // all, so the claim follows the probe rather than the platform.
            Capability::Clipboard => {
                if *self
                    .pasteboard
                    .get_or_init(|| MacPasteboard::for_session().is_some())
                {
                    CapabilityState::Available
                } else {
                    CapabilityState::Unavailable
                }
            }
            Capability::GlobalHotkeys
            | Capability::WindowEnumeration
            | Capability::WindowActivation
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

    /// The launcher behind [`Capability::ProcessLaunch`] and
    /// [`Capability::UriOpen`].
    pub fn process_launcher(&self) -> &dyn ProcessLauncher {
        &self.processes
    }

    /// The service behind [`Capability::Clipboard`], or `None` when this
    /// process has no general pasteboard.
    ///
    /// Owned rather than borrowed, matching the other two backends, whose Linux
    /// half genuinely needs it: an X11 selection lives only as long as the
    /// client that owns it, so the caller has to hold the clipboard. On macOS
    /// the pasteboard server holds the value instead, so a caller may drop this
    /// as soon as the copy returns -- the uniform signature costs nothing and
    /// keeps the platform difference out of the composition root.
    pub fn clipboard(&self) -> Option<Box<dyn Clipboard>> {
        MacPasteboard::for_session().map(|pasteboard| Box::new(pasteboard) as Box<dyn Clipboard>)
    }

    /// The opener behind [`Capability::FileOpen`].
    ///
    /// The same object as [`Self::process_launcher`], because on macOS it is
    /// the same `/usr/bin/open`. Two accessors rather than one so that a caller
    /// asks for the authority it needs: handing a user's document to Launch
    /// Services is not the same decision as running a program.
    ///
    /// Always `Some`, for the same reason [`Self::file_search`] is: `Option` is
    /// what the three backends' accessors have in common, and on Linux the
    /// helper really can be missing.
    pub fn file_opener(&self) -> Option<&dyn FileOpener> {
        Some(&self.processes)
    }

    /// The provider behind [`Capability::Icons`], built on first use.
    ///
    /// The reference discovery recorded is already an absolute path to an
    /// `.icns` file inside the bundle, so resolution is the shared
    /// [`PathIconSource`]: there is no theme search and no shell call to make.
    pub fn icon_provider(&self) -> &dyn IconProvider {
        self.icons.get_or_init(|| {
            match StandardDirectories::for_process() {
                Ok(directories) => IconLoader::caching(PathIconSource, Self::NAME, &directories),
                // A disposable cache with nowhere to live costs a decode per
                // lookup, not an icon.
                Err(_) => IconLoader::new(PathIconSource),
            }
        })
    }

    /// The service behind [`Capability::FileSearch`].
    ///
    /// Always present on macOS: even a machine with Spotlight switched off can
    /// have its home directory walked, and a walk that finds nothing is a
    /// truthful answer where withholding the service would not be.
    pub fn file_search(&self) -> Option<&dyn FileSearchService> {
        Some(&self.files)
    }
}

impl Default for MacOsBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// The directories macOS installs applications into, highest precedence first.
///
/// The user's own `~/Applications` comes first so a per-user copy overrides the
/// system-wide one of the same name. `Utilities` is listed separately because
/// the scan deliberately does not recurse -- a bundle *is* a directory, so
/// recursion would walk into every application's own resources -- and Terminal,
/// Activity Monitor and Disk Utility live nowhere else.
fn bundle_roots() -> Vec<PathBuf> {
    let mut roots = Vec::with_capacity(5);
    if let Some(home) = env::var_os("HOME").filter(|home| !home.is_empty()) {
        roots.push(Path::new(&home).join("Applications"));
    }
    roots.push(PathBuf::from("/Applications"));
    roots.push(PathBuf::from("/Applications/Utilities"));
    roots.push(PathBuf::from("/System/Applications"));
    roots.push(PathBuf::from("/System/Applications/Utilities"));
    roots
}

/// The individual application bundles macOS ships outside every install
/// location, highest precedence first.
///
/// Finder is the file manager of the platform and the one application a user is
/// certain to look for by name, but it is not installed: it lives in
/// `/System/Library/CoreServices`, which [`bundle_roots`] deliberately does not
/// list, so before this it was the one obvious application the launcher could
/// not find.
///
/// Two ways to reach it were weighed.
///
/// Adding `/System/Library/CoreServices` as a scan root was rejected as noise.
/// That directory is not an application folder; it is where macOS keeps the
/// agents that *are* the desktop. A stock system holds `Dock.app`,
/// `SystemUIServer.app`, `loginwindow.app`, `Spotlight.app`,
/// `NotificationCenter.app`, `ControlCenter.app`, `WiFiAgent.app`, `mrt.app`
/// (the malware removal tool) and a long tail of similar bundles there. Almost
/// none of them has a user interface, several restart or reconfigure the running
/// session when opened, and a launcher that offered dozens of them alongside
/// Finder would have made the result list worse in order to fix one entry. The
/// bundles in that tree a user might genuinely want -- Archive Utility,
/// Directory Utility, Screen Sharing, Wireless Diagnostics -- are one level
/// further down in `CoreServices/Applications`, which the non-recursive scan
/// would not have reached anyway.
///
/// Naming the bundle was chosen instead: one entry, one result, nothing else
/// indexed. It costs one `stat` per scan.
///
/// The path is a candidate, never an assumption. It is read by the same
/// [`read_bundle`] as any scanned directory, so a system that does not keep
/// Finder there -- a future macOS, a trimmed image -- contributes no item and
/// reports no error, and the name and bundle identifier always come from the
/// bundle's own `Info.plist` rather than from this list.
fn well_known_bundles() -> Vec<PathBuf> {
    vec![PathBuf::from("/System/Library/CoreServices/Finder.app")]
}

#[cfg(test)]
mod tests {
    //! Contract for the bundles that are discovered without being installed
    //! (spec 10.2, 18.5).
    //!
    //! Finder is the case that motivated named bundles: it is the platform's
    //! file manager, and it is not in any directory [`bundle_roots`] scans.
    //!
    //! Every case builds its own bundle in a unique temp directory, so what is
    //! pinned here is the scanner's rule rather than the contents of the host's
    //! `/System/Library/CoreServices`.

    use super::*;
    use crikey_core::{ExecutionPolicy, PluginId};
    use crikey_platform::{application_items, APPLICATION_LAUNCH_ACTION_ID};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique scratch directory that deletes itself when the test ends.
    ///
    /// Uniqueness comes from the process id plus a monotonic counter, never from
    /// a clock, so parallel test threads and repeated runs cannot collide.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!("crikey-macos-bundles-{}-{unique}", std::process::id()));
            fs::create_dir_all(&path).expect("the scratch directory must be creatable");
            Self { path }
        }

        fn join(&self, relative: &str) -> PathBuf {
            self.path.join(relative)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Writes a bundle that reads back as the application it says it is.
    fn write_bundle(path: &Path, name: &str, identifier: &str) {
        fs::create_dir_all(path.join(EXECUTABLE_DIRECTORY)).expect("bundle layout must be creatable");
        fs::write(
            path.join(INFO_PLIST),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <plist version=\"1.0\"><dict>\
                 <key>CFBundleName</key><string>{name}</string>\
                 <key>CFBundleIdentifier</key><string>{identifier}</string>\
                 </dict></plist>"
            ),
        )
        .expect("the property list must be writable");
    }

    fn discover(scanner: &BundleScanner) -> Vec<DiscoveredApplication> {
        scanner.discover().expect("discovery never fails")
    }

    /// The launcher ships the path Finder actually has, and it is not a root.
    ///
    /// This is the regression this list exists for: before it, no source in the
    /// backend named `/System/Library/CoreServices`, so Finder was unreachable.
    #[test]
    fn the_standard_sources_name_finder_outside_every_root() {
        let finder = PathBuf::from("/System/Library/CoreServices/Finder.app");
        assert!(
            well_known_bundles().contains(&finder),
            "Finder must be named explicitly, found {:?}",
            well_known_bundles()
        );
        assert!(
            !bundle_roots().iter().any(|root| finder.starts_with(root)),
            "no scanned root contains Finder, so naming it is the only way in"
        );
    }

    /// A named bundle is discovered exactly like a scanned one.
    #[test]
    fn a_named_bundle_is_discovered_without_being_under_a_root() {
        let scratch = Scratch::new();
        let finder = scratch.join("CoreServices/Finder.app");
        write_bundle(&finder, "Finder", "com.apple.finder");

        let discovered = discover(&BundleScanner::with_bundles(Vec::new(), vec![finder.clone()]));

        assert_eq!(discovered.len(), 1, "one named bundle is one application");
        assert_eq!(discovered[0].name, "Finder");
        assert_eq!(discovered[0].platform_id.as_deref(), Some("com.apple.finder"));
        assert_eq!(
            discovered[0].target,
            PlatformPath::new(finder.as_os_str().to_owned()),
            "Launch Services opens the bundle, so the bundle is the target"
        );
    }

    /// A user typing `finder` reaches it, and the entry launches like any other
    /// application.
    #[test]
    fn a_named_bundle_becomes_a_launchable_item_with_its_own_aliases() {
        let scratch = Scratch::new();
        let finder = scratch.join("Finder.app");
        write_bundle(&finder, "Finder", "com.apple.finder");

        let discovered = discover(&BundleScanner::with_bundles(Vec::new(), vec![finder]));
        let plugin = PluginId("builtin.crikey.applications".to_owned());
        let items = application_items(&plugin, &discovered);
        let item = items.first().expect("one discovery makes one item");

        assert_eq!(item.label, "Finder");
        for alias in ["Finder", "finder"] {
            assert!(
                item.search_terms.iter().any(|term| term == alias),
                "Finder must answer to {alias:?}, found {:?}",
                item.search_terms
            );
        }
        let action = item.actions.first().expect("an application launches");
        assert_eq!(action.action_id.0, APPLICATION_LAUNCH_ACTION_ID);
        assert_eq!(action.execution_policy, ExecutionPolicy::HostMediated);
    }

    /// A named bundle that is not there is absent, not an error.
    ///
    /// The whole point of naming a path is that the path may one day be wrong;
    /// a future macOS that moves Finder must cost the user one entry, not every
    /// application on the machine.
    #[test]
    fn a_named_bundle_that_is_not_there_yields_no_item_and_no_error() {
        let scratch = Scratch::new();
        let scanner = BundleScanner::with_bundles(
            Vec::new(),
            vec![
                scratch.join("gone/Finder.app"),
                PathBuf::from("/"),
                PathBuf::new(),
            ],
        );

        assert!(discover(&scanner).is_empty());
    }

    /// A copy under a scanned root wins: that is the user's own installation.
    #[test]
    fn a_scanned_copy_shadows_the_named_bundle_of_the_same_name() {
        let scratch = Scratch::new();
        let installed = scratch.join("Applications/Finder.app");
        write_bundle(&installed, "Installed Finder", "com.example.finder");
        let system = scratch.join("CoreServices/Finder.app");
        write_bundle(&system, "Finder", "com.apple.finder");

        let discovered = discover(&BundleScanner::with_bundles(
            vec![scratch.join("Applications")],
            vec![system],
        ));

        assert_eq!(discovered.len(), 1, "one bundle name is one application");
        assert_eq!(discovered[0].name, "Installed Finder");
    }

    /// Naming a bundle indexes that bundle and nothing beside it.
    ///
    /// This is the difference from the rejected alternative of scanning
    /// `/System/Library/CoreServices`: the agents that share Finder's directory
    /// stay out of the catalog.
    #[test]
    fn naming_a_bundle_does_not_index_its_neighbours() {
        let scratch = Scratch::new();
        let core_services = scratch.join("CoreServices");
        write_bundle(&core_services.join("Finder.app"), "Finder", "com.apple.finder");
        write_bundle(&core_services.join("Dock.app"), "Dock", "com.apple.dock");

        let discovered = discover(&BundleScanner::with_bundles(
            Vec::new(),
            vec![core_services.join("Finder.app")],
        ));

        assert_eq!(
            discovered
                .iter()
                .map(|found| found.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Finder"]
        );
    }

    /// The scanner reports the bundles it was given.
    #[test]
    fn a_scanner_reports_the_bundles_it_was_given() {
        let bundles = vec![PathBuf::from("/System/Library/CoreServices/Finder.app")];
        assert_eq!(
            BundleScanner::with_bundles(Vec::new(), bundles.clone()).bundles(),
            bundles.as_slice()
        );
        assert!(
            BundleScanner::new(vec![PathBuf::from("/Applications")])
                .bundles()
                .is_empty(),
            "a roots-only scanner names no bundle of its own"
        );
    }
}
