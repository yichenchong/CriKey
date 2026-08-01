//! macOS platform backend.
//!
//! Application bundles, Launch Services, Spotlight metadata, Keychain,
//! accessibility-based window integration (spec 18.5).
//!
//! Compiled only for its target so platform-independent crates can never
//! accidentally depend on it (spec 5.3).
//!
//! Implemented so far: application discovery over `.app` bundles, and process
//! launching through `/usr/bin/open`. Both stop at what the core actually
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

use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;

use crikey_core::{CoreError, PlatformPath, Result};
use crikey_platform::{
    bundle_display_name, parse_info_plist, ApplicationDiscovery, Capability, CapabilityState,
    DiscoveredApplication, ProcessLauncher,
};

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
/// Roots are scanned in the order they were given and the earliest root wins a
/// duplicate bundle directory name, which is what lets `~/Applications`
/// override a system-wide copy of the same application.
#[derive(Debug)]
pub struct BundleScanner {
    roots: Vec<PathBuf>,
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

    /// Records the roots to scan, highest precedence first.
    ///
    /// Construction touches no filesystem: every read happens inside
    /// [`ApplicationDiscovery::discover`], so a scanner can be built before the
    /// directories it names exist.
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }
}

impl ApplicationDiscovery for BundleScanner {
    /// Scans every root once and returns the bundles it found.
    ///
    /// This never fails. `~/Applications` does not exist on most machines and
    /// a bundle installed by a third party may be unreadable or malformed;
    /// neither is a reason to hide every other application on the machine.
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
    let (name, bundle_id) = match info {
        Some(bundle) => (bundle.name, bundle.bundle_id),
        None => (directory_name, None),
    };

    Some(DiscoveredApplication {
        name,
        // Launch Services opens the bundle, not the executable inside it, so
        // the bundle directory is the launch target.
        target: PlatformPath::new(path.as_os_str().to_owned()),
        arguments: Vec::new(),
        // `CFBundleIconFile` names an `.icns` file nothing in this build can
        // render, and an icon reference no consumer can resolve is worse than
        // none (spec 18.2).
        icon_reference: None,
        platform_id: bundle_id,
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
/// The type check happens before the open because the open is the dangerous
/// part: opening a FIFO blocks until somebody writes to it, and a symlink to a
/// device node hands the scanner a stream that never ends. Metadata
/// deliberately follows symlinks -- an application may legitimately be a link
/// -- so what is inspected is the file that would actually be read.
///
/// The size is capped twice. The stat rejects a file that is already too big,
/// and the read still runs through a reader limited to one byte past the cap,
/// so a file that grows between the two calls is dropped rather than followed.
///
/// A file that is not UTF-8 is a binary property list, which this backend
/// cannot decode; it is reported as absent so the caller falls back to the
/// directory name instead of parsing bytes it cannot read.
fn read_info_plist(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > BundleScanner::MAX_INFO_PLIST_BYTES {
        return None;
    }

    let mut contents = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(BundleScanner::MAX_INFO_PLIST_BYTES + 1)
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
        let mut command = Command::new(OPEN);
        if args.is_empty() {
            command.arg(operand(target));
        } else {
            command.arg("-a").arg(operand(target)).arg("--args").args(args);
        }
        self.spawn(&mut command, &target.display().to_string())
    }

    /// Opens `uri` with whatever Launch Services has registered for its scheme.
    ///
    /// The scheme is required and checked here rather than left to `open`: a
    /// string without one is a path, and `open` would silently open a *file* of
    /// that name -- or, for a leading `-`, read it as one of its own options.
    /// Handing user input to a program that reinterprets it is the class of bug
    /// this check exists to prevent.
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
/// thread limit -- the handle is dropped and that one child stays uncollected
/// until CriKey exits. The launch itself already succeeded, so it is not turned
/// into an error.
fn reap(mut child: Child) {
    let _ = thread::Builder::new()
        .name("crikey-open-reaper".to_owned())
        .spawn(move || {
            let _ = child.wait();
        });
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
}

impl MacOsBackend {
    /// Stable backend identifier surfaced by diagnostics and `crikey version`.
    pub const NAME: &'static str = "macos";

    /// Discovers applications from the standard bundle locations of the
    /// running user.
    pub fn new() -> Self {
        Self::with_application_roots(bundle_roots())
    }

    /// Discovers applications from exactly these roots, highest precedence
    /// first, instead of the standard locations.
    pub fn with_application_roots(roots: Vec<PathBuf>) -> Self {
        Self {
            applications: BundleScanner::new(roots),
            processes: OpenLauncher::new(),
        }
    }

    /// Capability reporting is honest: a capability is claimed only once a
    /// macOS implementation stands behind it (spec 18.2). The unimplemented
    /// arms are listed one by one so that adding a capability to the enum
    /// forces a deliberate answer here instead of inheriting a wildcard.
    pub fn capability(&self, capability: Capability) -> CapabilityState {
        match capability {
            Capability::ApplicationDiscovery | Capability::ProcessLaunch | Capability::UriOpen => {
                CapabilityState::Available
            }
            Capability::FileSearch
            | Capability::Clipboard
            | Capability::GlobalHotkeys
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

    /// The launcher behind [`Capability::ProcessLaunch`] and
    /// [`Capability::UriOpen`].
    pub fn process_launcher(&self) -> &dyn ProcessLauncher {
        &self.processes
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
