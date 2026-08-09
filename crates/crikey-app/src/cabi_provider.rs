//! Turning an installed `c-abi` package into a supervised launch.
//!
//! There is no separate provider for restricted C-ABI plugins, and there must
//! not be one. A `c-abi` package is served by `crikey-cabi-host`, which is an
//! ordinary supervised native plugin executable, so [`crate::native_provider`]
//! owns it end to end: same worker, same supervisor, same circuit breaker, same
//! teardown. All this module contributes is *which* executable to start and
//! *what* to hand it (ADR-0015).
//!
//! Nothing here loads a shared library, and this crate deliberately does not
//! depend on `crikey-cabi-host`: the launcher has no reason to link code that
//! can `dlopen` (spec 2.3; acceptance criterion 30). The checks below are
//! therefore the cheap ones that let a broken package be reported as
//! unavailable at load time instead of as a worker that starts and dies. The
//! authoritative refusals — ABI version, required symbols, entrypoint
//! containment, member digest — belong to the host process and are reported
//! through the worker's exit and its captured diagnostics.

use std::path::{Path, PathBuf};

use crikey_plugin_model::Manifest;

/// Overrides where the host executable is found. Set by tests and by an
/// operator running an uninstalled build; unset in every normal installation.
pub const ENV_HOST_OVERRIDE: &str = "CRIKEY_CABI_HOST";

/// File name of the host executable, without a platform suffix.
pub const HOST_BINARY: &str = "crikey-cabi-host";

fn host_file_name() -> String {
    format!("{HOST_BINARY}{}", std::env::consts::EXE_SUFFIX)
}

/// Locates the `crikey-cabi-host` executable.
///
/// First hit wins: [`ENV_HOST_OVERRIDE`], then beside the running executable
/// (the installed layout), then one directory above that (which is where a
/// Cargo test binary in `target/<profile>/deps` finds its siblings). The search
/// path is deliberately not consulted: the host that loads third-party code is
/// part of this installation, not whatever is first on `PATH`.
///
/// Returns `None` rather than a guess. A caller that cannot find the host must
/// report the plugin unavailable, because there is nothing that could run it.
pub fn host_executable() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os(ENV_HOST_OVERRIDE) {
        let path = PathBuf::from(configured);
        return path.is_file().then_some(path);
    }
    let executable = std::env::current_exe().ok()?;
    let name = host_file_name();
    let directory = executable.parent()?;
    let beside = directory.join(&name);
    if beside.is_file() {
        return Some(beside);
    }
    let above = directory.parent()?.join(&name);
    above.is_file().then_some(above)
}

/// The executable and arguments that serve `directory`'s `c-abi` package.
///
/// The host takes the package directory and nothing else. It reads the
/// manifest itself and derives the library from it, so no path from this
/// process — let alone from a query — can name what gets loaded.
///
/// `Err` carries a reason fit for a [`crate::native_provider::NativeUnavailable`]
/// diagnostic.
pub fn launch_recipe(
    manifest: &Manifest,
    directory: &Path,
    os: &str,
    arch: &str,
) -> Result<(PathBuf, Vec<String>), String> {
    let entrypoint = manifest
        .entrypoint_for(os, arch)
        .map_err(|error| format!("no usable c-abi entrypoint: {error}"))?;
    let library = directory.join(entrypoint);
    if !library.is_file() {
        return Err(format!("c-abi entrypoint is not a file: {}", library.display()));
    }
    let host = host_executable().ok_or_else(|| {
        format!(
            "{} is not installed beside the launcher, so no host can load this c-abi plugin \
             (set {ENV_HOST_OVERRIDE} to point at one)",
            host_file_name()
        )
    })?;
    let directory = directory
        .to_str()
        .ok_or_else(|| {
            format!(
                "c-abi package directory is not valid UTF-8: {}",
                directory.display()
            )
        })?
        .to_owned();
    Ok((host, vec![directory]))
}
