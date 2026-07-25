//! Plugin package management (spec 23).

use crikey_core::PluginId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallSource {
    Directory(std::path::PathBuf),
    Archive(std::path::PathBuf),
    Url(String),
    /// An existing Keypirinha package file.
    LegacyPackage(std::path::PathBuf),
}

/// Installations are atomic: a failed update leaves the previous working
/// version in place (spec 23.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed,
    Upgraded,
    RolledBack,
    Unavailable,
}

#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    #[error("hash verification failed for {0}")]
    HashMismatch(String),
    #[error("no binary for this platform/architecture")]
    IncompatiblePlatform,
    #[error("dependency resolution failed: {0}")]
    Resolution(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub trait PackageManager {
    fn install(&mut self, source: InstallSource) -> Result<InstallOutcome, PackageError>;
    fn remove(&mut self, plugin: &PluginId) -> Result<(), PackageError>;
    fn verify(&self, plugin: &PluginId) -> Result<(), PackageError>;
}
