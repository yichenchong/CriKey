//! Plugin package management (spec 23).
//!
//! This crate carries two related but distinct responsibilities:
//!
//! * the legacy install/verify surface ([`InstallSource`], [`PackageManager`]),
//!   and
//! * the *modern* managed-environment machinery (spec 15.3, 15.4, 23.2, 23.4):
//!   content-addressed [`EnvironmentId`]s, offline [`PackageIndex`] resolution
//!   into a byte-stable [`Lockfile`], and an [`EnvironmentStore`] that
//!   materialises a plugin's dependency closure into an isolated site dir.

use crikey_core::PluginId;

mod environment;
mod native;

pub use native::{
    build_package, inspect_package, install_native, rollback_native, verify_package, NativeInstall,
    NativePackageReport,
};

mod import_path;
mod index;
mod lockfile;
mod resolve;

pub use environment::{EnvironmentId, EnvironmentInputs, EnvironmentStore, MaterializedEnvironment};
pub use import_path::ImportPath;
pub use index::PackageIndex;
pub use lockfile::{LockedPackage, Lockfile};
pub use resolve::resolve;

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
    #[error("dependency resolution failed: {0}")]
    Resolution(String),
    #[error("no binary for this platform/architecture")]
    IncompatiblePlatform,
    #[error("requires-python {required} not satisfied by {found}")]
    UnsatisfiedRequiresPython { required: String, found: String },
    #[error("malformed native package archive: {0}")]
    MalformedArchive(String),
    #[error("invalid native package manifest: {0}")]
    Manifest(String),
    #[error("native package installation failed: {0}")]
    Install(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub trait PackageManager {
    fn install(&mut self, source: InstallSource) -> Result<InstallOutcome, PackageError>;
    fn remove(&mut self, plugin: &PluginId) -> Result<(), PackageError>;
    fn verify(&self, plugin: &PluginId) -> Result<(), PackageError>;
}
