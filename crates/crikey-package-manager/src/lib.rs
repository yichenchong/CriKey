//! Plugin package management (spec 23).
//!
//! This crate carries two related but distinct responsibilities:
//!
//! * installing, verifying, upgrading and rolling back plugin packages
//!   ([`PluginInstaller`], [`InstallSource`]), and
//! * the *modern* managed-environment machinery (spec 15.3, 15.4, 23.2, 23.4):
//!   content-addressed [`EnvironmentId`]s, offline [`PackageIndex`] resolution
//!   into a byte-stable [`Lockfile`], and an [`EnvironmentStore`] that
//!   materialises a plugin's dependency closure into an isolated site dir.

use std::path::{Path, PathBuf};

mod environment;
mod native;

pub use native::{
    build_package, inspect_package, install_native, rollback_native, verify_package, NativeInstall,
    NativePackageReport,
};

mod fetch;
mod import_path;
mod index;
mod installer;
mod launcher_lock;
mod lockfile;
mod resolve;

pub use environment::{EnvironmentId, EnvironmentInputs, EnvironmentStore, MaterializedEnvironment};
pub use fetch::{HttpFetcher, PackageFetcher};
pub use import_path::ImportPath;
pub use index::PackageIndex;
pub use installer::{InstalledPlugin, PluginInstaller};
pub use launcher_lock::LauncherLock;
pub use lockfile::{LockedPackage, Lockfile};
pub use resolve::resolve;

/// Where a package to install comes from (spec 23.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallSource {
    /// An unpacked plugin source tree.
    Directory(PathBuf),
    /// A packaged plugin: a native `crikey` package or a modern source archive.
    Archive(PathBuf),
    /// An `http`/`https` URL naming an archive.
    Url(String),
    /// An existing Keypirinha package file.
    LegacyPackage(PathBuf),
}

impl InstallSource {
    /// Classifies what a user typed on a command line.
    ///
    /// One place decides what `crikey plugin install <thing>` means, so the
    /// command line and any other caller cannot disagree about whether a path
    /// ending in `.keypirinha-package` is a legacy package. The distinction
    /// between a directory and a file is taken from the filesystem rather than
    /// from the spelling, because a plugin source tree is not required to have
    /// a trailing separator.
    pub fn detect(value: &str) -> Result<Self, PackageError> {
        if value.starts_with("https://") || value.starts_with("http://") {
            return Ok(Self::Url(value.to_owned()));
        }
        let path = Path::new(value);
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            PackageError::SourceUnavailable(format!("{value} could not be read: {error}"))
        })?;
        if metadata.is_dir() {
            return Ok(Self::Directory(path.to_path_buf()));
        }
        if !metadata.is_file() {
            return Err(PackageError::SourceUnavailable(format!(
                "{value} is neither a directory nor a package file"
            )));
        }
        if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("keypirinha-package"))
        {
            return Ok(Self::LegacyPackage(path.to_path_buf()));
        }
        Ok(Self::Archive(path.to_path_buf()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    #[error("hash verification failed for {0}")]
    HashMismatch(String),
    #[error("dependency resolution failed: {0}")]
    Resolution(String),
    /// The package does not declare this target at all (spec 19.3): the
    /// `[platform]` lists exclude it, so nothing about it was ever built.
    #[error("no binary for this platform/architecture")]
    IncompatiblePlatform,
    /// The package *declares* this target but ships no entrypoint for it. This
    /// is a different defect from [`PackageError::IncompatiblePlatform`] — the
    /// build is expected to exist and is simply absent — so the message names
    /// the `<os>-<arch>` key an operator has to go look for.
    #[error("package declares {os}-{arch} but ships no entrypoint for {os}-{arch}")]
    MissingEntrypoint { os: String, arch: String },
    #[error("requires-python {required} not satisfied by {found}")]
    UnsatisfiedRequiresPython { required: String, found: String },
    #[error("malformed native package archive: {0}")]
    MalformedArchive(String),
    #[error("invalid native package manifest: {0}")]
    Manifest(String),
    #[error("native package installation failed: {0}")]
    Install(String),
    /// A launcher holds the exclusive lock, so replacing installed files would
    /// be replacing them underneath running plugins (spec 23.3). The pid is
    /// diagnostic text; the lock, not the pid, is what makes this safe.
    #[error("crikey is running{}; quit the launcher before changing plugins", match pid {
        Some(pid) => format!(" (pid {pid})"),
        None => String::new(),
    })]
    LauncherRunning { pid: Option<u32> },
    #[error("invalid import path: {0}")]
    InvalidImportPath(String),
    #[error("package source unavailable: {0}")]
    SourceUnavailable(String),
    #[error("malformed package index: {0}")]
    MalformedIndex(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
