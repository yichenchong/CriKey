//! Modern CPython worker host (spec 15).
//!
//! Python never runs on the UI thread and never inside the UI process. Workers
//! speak the same versioned protocol as native plugins.

use crikey_core::PluginId;

/// Which interpreter a worker runs (spec 14.11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeProfile {
    /// Interpreter kept for legacy compatibility.
    LegacyCompatibility,
    /// Interpreter bundled with this CriKey build.
    Bundled,
    /// Externally managed interpreter selected by manifest or user override.
    External(std::path::PathBuf),
}

/// A content-addressed dependency environment. Plugins with identical locked
/// environments may share one worker (spec 15.3, 15.6).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EnvironmentId(pub String);

/// Import path assembly (spec 15.4). System-wide site-packages is excluded.
#[derive(Debug, Clone, Default)]
pub struct ImportPath {
    pub plugin_source: Vec<std::path::PathBuf>,
    pub packaged_modules: Vec<std::path::PathBuf>,
    pub managed_dependencies: Vec<std::path::PathBuf>,
    pub sdk: Option<std::path::PathBuf>,
}

pub trait PythonHost {
    fn start_worker(
        &mut self,
        plugin: &PluginId,
        runtime: RuntimeProfile,
        env: EnvironmentId,
    ) -> crikey_core::Result<()>;
    fn stop_worker(&mut self, plugin: &PluginId) -> crikey_core::Result<()>;
}
