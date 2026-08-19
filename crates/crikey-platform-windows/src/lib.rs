//! Windows platform backend.
//!
//! Start Menu shortcuts, packaged apps, `.lnk` parsing, AppUserModelIDs,
//! global hotkeys (spec 18.4) and shell execution of what discovery found
//! (spec 18.2). Named pipes, window integration and notifications stay for
//! later milestones and keep reporting themselves unavailable; the clipboard is
//! implemented, in `clipboard.rs`.
//!
//! # Why this crate compiles everywhere
//!
//! Every call into Win32 is behind `cfg(target_os = "windows")`, but the crate
//! itself is not. Two things follow from that, and both are deliberate.
//!
//! The first is testability. The parts of a backend that are easiest to get
//! wrong are not the FFI calls: they are the tables and the set arithmetic
//! around them -- which virtual key an accelerator names, which registration id
//! it gets, which of two shortcuts pointing at one executable survives, which
//! command line hands a program back the argument vector it was launched with.
//! None of that needs a Windows kernel to be exercised, so none of it is
//! allowed to hide behind one. [`HotkeyCode`], [`HotkeyRegistrations`],
//! [`StartMenuDiscovery::shortcuts`], [`StartMenuDiscovery::well_known_applications`],
//! [`ApplicationSet`] and [`quote_arguments`] are ordinary Rust on every host,
//! and the test suite runs them on every host.
//!
//! The second is honesty. A backend that cannot reach Win32 must say so rather
//! than answer plausibly: [`WindowsBackend::capability`] reports everything
//! unavailable off target, discovery and hotkey registration return a typed
//! refusal naming the reason, and no code path fabricates an empty application
//! list or a silently dead hotkey. Nothing outside this crate depends on it off
//! target either -- `crikey-app` names it only under `cfg(windows)` -- so the
//! isolation spec 5.3 asks for is enforced by the dependency edge, not by an
//! empty crate.

mod applications;
/// The clipboard, and gated like `win32` below: it is a Win32 binding whole, so
/// off target there is no honest stub to compile -- [`WindowsBackend::clipboard`]
/// hands out nothing there instead.
#[cfg(target_os = "windows")]
mod clipboard;
mod file_search;
mod hotkeys;
mod icons;
mod process;
#[cfg(target_os = "windows")]
mod win32;

use std::{path::PathBuf, sync::OnceLock};

pub use applications::{
    split_arguments, ApplicationSet, Shortcut, StartMenuDiscovery, WellKnownApplication,
    WELL_KNOWN_APPLICATIONS,
};
#[cfg(target_os = "windows")]
pub use clipboard::WindowsClipboard;
#[cfg(not(target_os = "windows"))]
use crikey_core::CoreError;
use crikey_platform::{
    ApplicationDiscovery, Capability, CapabilityState, Clipboard, FileOpener, FileSearchService,
    HotkeyService, IconLoader, IconProvider, ProcessLauncher, StandardDirectories,
};
pub use file_search::{
    system_index_sql, unix_seconds_from_file_time, WindowsFileSearch, SELECT_COLUMNS, WALK_SUBDIRECTORIES,
};
pub use hotkeys::{HotkeyCode, HotkeyRegistration, HotkeyRegistrations, WindowsHotkeys};
pub use icons::ShortcutIconSource;
pub use process::{quote_arguments, ShellLauncher};

/// The Windows backend and the services it stands behind (spec 18.2, 18.4).
#[derive(Debug)]
pub struct WindowsBackend {
    applications: StartMenuDiscovery,
    hotkeys: WindowsHotkeys,
    launcher: ShellLauncher,
    /// Built on first use and cached: resolving the cache directory reads the
    /// environment, which a constructor that touches nothing should not do.
    icons: OnceLock<IconLoader<ShortcutIconSource>>,
    /// Built on first use and cached: the searcher resolves this user's profile
    /// folders from the environment, which a constructor that touches nothing
    /// should not do.
    files: OnceLock<WindowsFileSearch>,
}

impl WindowsBackend {
    /// Stable backend identifier surfaced by diagnostics and `crikey version`.
    pub const NAME: &'static str = "windows";

    /// Builds the backend over this user's Start Menu and the machine's.
    ///
    /// Construction touches neither the filesystem nor COM nor the message
    /// queue: the known folders are resolved here, but scanning happens inside
    /// [`ApplicationDiscovery::discover`] and the hotkey message thread starts
    /// on the first successful registration. A backend that is built and never
    /// used therefore costs one allocation and no thread.
    pub fn new() -> Self {
        Self {
            applications: StartMenuDiscovery::new(),
            hotkeys: WindowsHotkeys::new(),
            launcher: ShellLauncher::new(),
            icons: OnceLock::new(),
            files: OnceLock::new(),
        }
    }

    /// Builds the backend over exactly these Start Menu roots, highest
    /// precedence first, and optionally over the packaged applications the
    /// shell publishes.
    pub fn with_application_roots(roots: Vec<PathBuf>, packaged: bool) -> Self {
        Self {
            applications: StartMenuDiscovery::with_roots(roots, packaged),
            hotkeys: WindowsHotkeys::new(),
            launcher: ShellLauncher::new(),
            icons: OnceLock::new(),
            files: OnceLock::new(),
        }
    }

    /// Capability reporting is honest: a capability is claimed only once a
    /// Windows implementation stands behind it *and* this build can reach it
    /// (spec 18.2). The unimplemented arms are listed one by one so that adding
    /// a capability to the enum forces a deliberate answer here instead of
    /// inheriting a wildcard.
    pub fn capability(&self, capability: Capability) -> CapabilityState {
        match capability {
            // One `ShellExecuteExW` dispatch answers all of these: the shell
            // resolves an executable, a packaged moniker, a scheme handler and
            // a document association through the same call. That `FileOpen`
            // and `UriOpen` are separate variants is a fact about Linux, not
            // about this backend.
            Capability::ApplicationDiscovery
            | Capability::GlobalHotkeys
            | Capability::ProcessLaunch
            | Capability::UriOpen
            | Capability::FileOpen => {
                if cfg!(target_os = "windows") {
                    CapabilityState::Available
                } else {
                    // Not `UnsupportedDesktopEnvironment`: that describes a
                    // Windows session missing a feature, not a build that has
                    // no Windows underneath it at all.
                    CapabilityState::Unavailable
                }
            }
            // A shortcut whose icon location names a real `.ico` or `.png`
            // resolves; one naming a PE resource (`shell32.dll,-16801`) or a
            // packaged application (`shell:AppsFolder\...`) does not, because
            // neither is a file and neither extractor is implemented. `Partial`
            // is exactly that, and it stays `Partial` on target rather than
            // becoming `Available` once a Windows kernel is present: the missing
            // half is missing code, not a missing platform.
            Capability::Icons => {
                if cfg!(target_os = "windows") {
                    CapabilityState::Partial
                } else {
                    CapabilityState::Unavailable
                }
            }
            // Windows Search answers from the `SystemIndex` catalog, and the
            // catalog holds the locations indexing is configured for -- in the
            // Classic mode a clean install uses, that is Documents, Pictures,
            // Music and the Desktop, not the drive; Enhanced mode indexes the
            // whole PC and is off by default. The directory walk that covers
            // what the catalog misses is narrower still. Real results from a
            // subset is exactly `Partial`, and it stays `Partial` on target: the
            // missing part is the user's indexing configuration, not missing
            // code.
            Capability::FileSearch => {
                if cfg!(target_os = "windows") {
                    CapabilityState::Partial
                } else {
                    CapabilityState::Unavailable
                }
            }
            // Windows keeps clipboard contents itself, so there is no session
            // gate of the kind Linux has and no partial answer to give: an
            // interactive session has a clipboard and a build with no Win32
            // underneath it has nothing at all (see [`clipboard`]).
            Capability::Clipboard => {
                if cfg!(target_os = "windows") {
                    CapabilityState::Available
                } else {
                    CapabilityState::Unavailable
                }
            }
            Capability::WindowEnumeration
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
        &self.launcher
    }

    /// The service behind [`Capability::Clipboard`], or `None` on a build with
    /// no Win32 underneath it.
    ///
    /// Owned rather than borrowed, matching the other two backends, whose Linux
    /// half genuinely needs it: an X11 selection lives only as long as the
    /// client that owns it, so the caller has to hold the clipboard. Windows
    /// keeps the value itself, so a caller may drop this as soon as the copy
    /// returns -- the uniform signature costs nothing and keeps the platform
    /// difference out of the composition root.
    pub fn clipboard(&self) -> Option<Box<dyn Clipboard>> {
        #[cfg(target_os = "windows")]
        {
            WindowsClipboard::for_session().map(|clipboard| Box::new(clipboard) as Box<dyn Clipboard>)
        }
        // Not a stub standing in for a clipboard: there is no Win32 here, so
        // there is nothing to hand out and nothing that could succeed.
        #[cfg(not(target_os = "windows"))]
        {
            None
        }
    }

    /// The opener behind [`Capability::FileOpen`].
    ///
    /// The same object as [`Self::process_launcher`], because on Windows it is
    /// the same `ShellExecuteExW`. Two accessors rather than one so that a
    /// caller asks for the authority it needs: handing a user's document to the
    /// shell is not the same decision as running a program.
    ///
    /// `None` off Windows, matching [`Self::capability`] and
    /// [`Self::file_search`]: the backend hands out a service only for the
    /// session it is actually running in, and there is no shell to dispatch to
    /// on a build with no Windows underneath it.
    pub fn file_opener(&self) -> Option<&dyn FileOpener> {
        cfg!(target_os = "windows").then_some(&self.launcher as &dyn FileOpener)
    }

    /// The provider behind [`Capability::Icons`], built on first use.
    ///
    /// Answers per reference rather than per session: a shortcut naming a real
    /// image file gets pixels, one naming a PE resource or a packaged
    /// application gets `None`. That split is what
    /// [`CapabilityState::Partial`] reports.
    pub fn icon_provider(&self) -> &dyn IconProvider {
        self.icons.get_or_init(|| {
            let source = ShortcutIconSource::new();
            match StandardDirectories::for_process() {
                Ok(directories) => IconLoader::caching(source, Self::NAME, &directories),
                // A disposable cache with nowhere to live costs a decode per
                // lookup, not an icon.
                Err(_) => IconLoader::new(source),
            }
        })
    }

    /// The service behind [`Capability::FileSearch`], built on first use.
    ///
    /// `None` off Windows, and not because the walk could not run there: the
    /// backend hands out a service only for the session it is actually running
    /// in, and a build with no Windows Search catalog and no Windows profile to
    /// walk has no file search to offer. On target the service answers from the
    /// `SystemIndex` catalog and from this user's profile folders, which is what
    /// [`CapabilityState::Partial`] reports.
    pub fn file_search(&self) -> Option<&dyn FileSearchService> {
        if !cfg!(target_os = "windows") {
            return None;
        }
        Some(self.files.get_or_init(WindowsFileSearch::new))
    }

    /// The hotkey service behind [`Capability::GlobalHotkeys`].
    ///
    /// Registration mutates the backend because it owns the Win32 registration
    /// ids and, on Windows, the message thread they live on.
    pub fn hotkeys(&mut self) -> &mut dyn HotkeyService {
        &mut self.hotkeys
    }
}

impl Default for WindowsBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// The refusal every Win32-backed service returns on a build that is not
/// Windows.
///
/// `action` completes "the Windows backend cannot ...", so it reads as a verb
/// phrase: `"discover applications"`, `"register a global hotkey"`.
#[cfg(not(target_os = "windows"))]
fn off_target(action: &str) -> CoreError {
    CoreError::Invalid(format!(
        "the Windows backend cannot {action}: this build does not target Windows"
    ))
}
