//! Windows platform backend.
//!
//! Start Menu shortcuts, packaged apps, `.lnk` parsing, AppUserModelIDs,
//! global hotkeys (spec 18.4) and shell execution of what discovery found
//! (spec 18.2). Named pipes, window integration, clipboard and notifications
//! stay for later milestones and keep reporting themselves unavailable.
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
//! [`StartMenuDiscovery::shortcuts`], [`ApplicationSet`] and
//! [`quote_arguments`] are ordinary Rust on every host, and the test suite runs
//! them on every host.
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
mod hotkeys;
mod process;
#[cfg(target_os = "windows")]
mod win32;

use std::path::PathBuf;

use crikey_platform::{ApplicationDiscovery, Capability, CapabilityState, HotkeyService, ProcessLauncher};

pub use applications::{split_arguments, ApplicationSet, Shortcut, StartMenuDiscovery};
pub use hotkeys::{HotkeyCode, HotkeyRegistration, HotkeyRegistrations, WindowsHotkeys};
pub use process::{quote_arguments, ShellLauncher};

/// The Windows backend and the services it stands behind (spec 18.2, 18.4).
#[derive(Debug)]
pub struct WindowsBackend {
    applications: StartMenuDiscovery,
    hotkeys: WindowsHotkeys,
    launcher: ShellLauncher,
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
        }
    }

    /// Capability reporting is honest: a capability is claimed only once a
    /// Windows implementation stands behind it *and* this build can reach it
    /// (spec 18.2). The unimplemented arms are listed one by one so that adding
    /// a capability to the enum forces a deliberate answer here instead of
    /// inheriting a wildcard.
    pub fn capability(&self, capability: Capability) -> CapabilityState {
        match capability {
            Capability::ApplicationDiscovery
            | Capability::GlobalHotkeys
            | Capability::ProcessLaunch
            | Capability::UriOpen => {
                if cfg!(target_os = "windows") {
                    CapabilityState::Available
                } else {
                    // Not `UnsupportedDesktopEnvironment`: that describes a
                    // Windows session missing a feature, not a build that has
                    // no Windows underneath it at all.
                    CapabilityState::Unavailable
                }
            }
            Capability::FileSearch
            | Capability::Clipboard
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
        &self.launcher
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
fn off_target(action: &str) -> crikey_core::CoreError {
    use crikey_core::CoreError;

    CoreError::Invalid(format!(
        "the Windows backend cannot {action}: this build does not target Windows"
    ))
}
