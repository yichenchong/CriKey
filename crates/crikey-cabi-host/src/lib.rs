//! Out-of-process host for restricted C-ABI plugin libraries (ADR-0015).
//!
//! Spec §2.2 asks for "restricted in-process native plugins". Spec §2.3 forbids
//! ABI compatibility with arbitrary Rust dynamic libraries, and acceptance
//! criterion 30 forbids the main process loading arbitrary third-party native
//! libraries. Both hold at once in exactly one arrangement, and this crate is
//! it: the shared library is loaded by *this* executable, which CriKey starts
//! and supervises like any other native plugin and talks to over the existing
//! native protocol. The library is in-process for `crikey-cabi-host` and
//! out-of-process for CriKey.
//!
//! What that buys, precisely: a plugin fault, hang or leak destroys one host
//! process and the supervisor restarts it, while every sibling plugin — in its
//! own host process — keeps serving. What it does not buy: a sandbox. A C
//! plugin has the full authority of this process. The refusals in [`policy`]
//! decide *which* library runs, never what it may do once it is running.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub mod abi;
pub mod library;
pub mod plugin;
pub mod policy;
pub mod watchdog;

pub use library::{DynamicLibrary, LoadError, PluginAbi, SymbolSource};
pub use plugin::{CabiPlugin, HostOptions, ABORT_GRACE, MAX_ITEMS, MAX_STRING_BYTES};
pub use policy::{resolve_package, ResolvedPackage};

/// Usage was wrong; nothing was loaded.
pub const EXIT_USAGE: u8 = 2;
/// A library was refused. Nothing third-party ran, or ran only as far as the
/// platform loader's own initialisers.
pub const EXIT_REFUSED: u8 = 3;
/// The library loaded and the protocol session then failed.
pub const EXIT_SERVE: u8 = 4;

/// Why the host stopped.
#[derive(Debug, thiserror::Error)]
pub enum HostFailure {
    #[error("usage: crikey-cabi-host <installed-package-directory>")]
    Usage,
    #[error(transparent)]
    Refused(#[from] LoadError),
    #[error("protocol session failed: {0}")]
    Serve(crikey_plugin_sdk::SdkError),
}

impl HostFailure {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage => EXIT_USAGE,
            Self::Refused(_) => EXIT_REFUSED,
            Self::Serve(_) => EXIT_SERVE,
        }
    }
}

/// Derives the host's runtime budgets from the package manifest.
///
/// The manifest's declared timeouts are the contract; [`ABORT_GRACE`] is added
/// on top only by the watchdog, so a plugin that respects its own deadline
/// never meets it.
pub fn host_options(package: &ResolvedPackage) -> HostOptions {
    let performance = &package.manifest.performance;
    let hard = Duration::from_millis(performance.suggest_hard_timeout_ms);
    HostOptions {
        plugin_id: package.manifest.plugin.id.clone(),
        package_dir: package.directory.clone(),
        suggest_soft: Duration::from_millis(performance.suggest_soft_timeout_ms).min(hard),
        suggest_hard: hard,
        action_hard: hard,
    }
}

/// Loads the library an already-resolved package declares.
///
/// Split from [`resolve_package`] so the caller keeps the manifest it
/// validated: the identity advertised in the handshake and the library that
/// was loaded then provably come from one reading of one file.
pub fn load_resolved_package(package: &ResolvedPackage) -> Result<CabiPlugin, LoadError> {
    let source = DynamicLibrary::open(&package.library)?;
    // SAFETY: `policy::resolve_package` proved the library is the entrypoint
    // the installed manifest declares, inside the package directory, with the
    // bytes installation recorded. That it genuinely implements this ABI is
    // the plugin author's claim, checked as far as the version symbol allows
    // and contained by this process boundary.
    #[allow(unsafe_code)]
    unsafe {
        CabiPlugin::load(Box::new(source), host_options(package))
    }
}

/// Resolves an installed package and loads the library it declares.
///
/// Every refusal happens here, before the protocol session starts, so a
/// refused package is a named exit rather than a plugin that connects and then
/// answers nothing.
pub fn load_installed_package(directory: &Path, os: &str, arch: &str) -> Result<CabiPlugin, LoadError> {
    load_resolved_package(&resolve_package(directory, os, arch)?)
}

/// Entry point shared by the binary and by tests that drive it directly.
pub fn run<I>(arguments: I) -> Result<(), HostFailure>
where
    I: IntoIterator<Item = OsString>,
{
    let mut arguments = arguments.into_iter();
    let directory = PathBuf::from(arguments.next().ok_or(HostFailure::Usage)?);
    if arguments.next().is_some() {
        return Err(HostFailure::Usage);
    }

    let package = resolve_package(&directory, std::env::consts::OS, std::env::consts::ARCH)?;
    let id = package.manifest.plugin.id.clone();
    let version = package.manifest.plugin.version.clone();
    let mut plugin = load_resolved_package(&package)?;
    let config = crikey_plugin_sdk::ServeConfig::from_env(&id, &version).map_err(HostFailure::Serve)?;
    crikey_plugin_sdk::serve(&mut plugin, config).map_err(HostFailure::Serve)
}
