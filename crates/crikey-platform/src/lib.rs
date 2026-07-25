//! Platform service interfaces (spec 18).
//!
//! Platform-independent crates depend on these traits only; concrete desktop
//! APIs live in the per-OS backend crates.

use crikey_core::{PlatformPath, Result};

/// Optional platform capabilities and their availability (spec 18.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    ApplicationDiscovery,
    FileSearch,
    Clipboard,
    GlobalHotkeys,
    ProcessLaunch,
    UriOpen,
    WindowEnumeration,
    WindowActivation,
    Notifications,
    Icons,
    FileWatching,
    SecretStorage,
    ShellIntegration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityState {
    Available,
    Unavailable,
    PermissionGated,
    Partial,
    UnsupportedDesktopEnvironment,
}

#[derive(Debug, Clone)]
pub struct DiscoveredApplication {
    pub name: String,
    pub target: PlatformPath,
    pub arguments: Vec<String>,
    pub icon_reference: Option<String>,
    /// Platform native identifier, e.g. a Windows AppUserModelID or a Linux
    /// desktop-entry id.
    pub platform_id: Option<String>,
}

pub trait ApplicationDiscovery {
    fn discover(&self) -> Result<Vec<DiscoveredApplication>>;
}

pub trait Clipboard {
    fn read_text(&self) -> Result<Option<String>>;
    fn write_text(&self, text: &str) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyBinding {
    pub accelerator: String,
}

pub trait HotkeyService {
    fn register(&mut self, binding: &HotkeyBinding) -> Result<()>;
    fn unregister(&mut self, binding: &HotkeyBinding) -> Result<()>;
}

pub trait ProcessLauncher {
    fn launch(&self, target: &PlatformPath, args: &[String]) -> Result<()>;
    fn open_uri(&self, uri: &str) -> Result<()>;
}

pub trait Notifications {
    fn notify(&self, title: &str, body: &str) -> Result<()>;
}

pub trait SecretStore {
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn set(&self, key: &str, value: &str) -> Result<()>;
}

/// The aggregate a backend crate implements and the app wires in.
pub trait PlatformBackend {
    fn name(&self) -> &'static str;
    fn capability(&self, capability: Capability) -> CapabilityState;
    fn application_discovery(&self) -> &dyn ApplicationDiscovery;
    fn clipboard(&self) -> &dyn Clipboard;
    fn process_launcher(&self) -> &dyn ProcessLauncher;
}
