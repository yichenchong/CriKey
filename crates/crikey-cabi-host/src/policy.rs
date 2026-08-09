//! Which shared library this host is allowed to load.
//!
//! The rule is short and deliberately unhelpful to anything clever: the library
//! is the entrypoint the *installed package's own manifest* declares for this
//! platform, it lives inside that package directory, and its bytes still match
//! the digest installation recorded. No path from a query, an argument or an
//! environment variable can name a library; the argument this host takes is a
//! package directory, and the directory decides the rest (ADR-0015).

use std::path::{Component, Path, PathBuf};

use crikey_plugin_model::{Manifest, Runtime};

use crate::library::LoadError;

/// Largest `crikey.toml` this host will read. Manifests are small; a package
/// that ships a hundred-megabyte one is hostile, not unlucky.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// The manifest of an installed package, together with the library that
/// manifest points at.
#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    /// Canonical package directory.
    pub directory: PathBuf,
    /// Canonical path of the library to load.
    pub library: PathBuf,
    /// The package-relative entrypoint name, as written in the manifest.
    pub entrypoint: String,
    /// The parsed manifest, which also carries the deadlines this host applies.
    pub manifest: Manifest,
}

fn policy(library: impl Into<String>, reason: impl Into<String>) -> LoadError {
    LoadError::Policy {
        library: library.into(),
        reason: reason.into(),
    }
}

/// Reads and validates an installed `c-abi` package directory.
///
/// Every refusal names the directory or the library. Nothing here executes
/// plugin code; this all happens before the loader is asked for anything.
pub fn resolve_package(directory: &Path, os: &str, arch: &str) -> Result<ResolvedPackage, LoadError> {
    let shown = directory.display().to_string();
    let metadata = std::fs::symlink_metadata(directory)
        .map_err(|error| policy(&shown, format!("cannot stat package directory: {error}")))?;
    if !metadata.is_dir() {
        return Err(policy(&shown, "package path is not a directory"));
    }
    let directory = directory
        .canonicalize()
        .map_err(|error| policy(&shown, format!("cannot canonicalise package directory: {error}")))?;

    let manifest_path = directory.join("crikey.toml");
    let manifest_len = std::fs::symlink_metadata(&manifest_path)
        .map_err(|error| policy(&shown, format!("cannot stat crikey.toml: {error}")))?;
    if !manifest_len.is_file() {
        return Err(policy(&shown, "crikey.toml is not a regular file"));
    }
    if manifest_len.len() > MAX_MANIFEST_BYTES {
        return Err(policy(
            &shown,
            format!("crikey.toml is larger than {MAX_MANIFEST_BYTES} bytes"),
        ));
    }
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|error| policy(&shown, format!("cannot read crikey.toml: {error}")))?;
    let manifest =
        Manifest::parse(&text).map_err(|error| policy(&shown, format!("invalid crikey.toml: {error}")))?;

    if manifest.plugin.runtime != Runtime::CAbi {
        return Err(policy(
            &shown,
            format!(
                "crikey-cabi-host serves runtime \"c-abi\"; this package declares {:?}",
                manifest.plugin.runtime
            ),
        ));
    }
    if !manifest.permissions.native_library_loading {
        return Err(policy(
            &shown,
            "permissions.native-library-loading must be true for a c-abi package",
        ));
    }

    let entrypoint = manifest
        .entrypoint_for(os, arch)
        .map_err(|error| policy(&shown, format!("no usable entrypoint for {os}-{arch}: {error}")))?
        .to_owned();
    let library = resolve_entrypoint(&directory, &entrypoint)?;

    // The digest installation recorded, re-checked now rather than trusted
    // from whenever the install happened. Refusing here is the difference
    // between "we validated this package once" and "these are the bytes we
    // validated".
    crikey_package_manager::verify_installed_member(&directory, &entrypoint)
        .map_err(|error| policy(library.display().to_string(), error.to_string()))?;

    Ok(ResolvedPackage {
        directory,
        library,
        entrypoint,
        manifest,
    })
}

/// Turns a manifest entrypoint into a path inside `directory`, or refuses it.
///
/// A manifest entrypoint is a package-relative name, never a command line and
/// never an escape hatch: absolute paths, `..`, drive prefixes and symlinks are
/// all refused rather than normalised into something that happens to work.
fn resolve_entrypoint(directory: &Path, entrypoint: &str) -> Result<PathBuf, LoadError> {
    let shown = directory.join(entrypoint).display().to_string();
    if entrypoint.is_empty() {
        return Err(policy(&shown, "manifest entrypoint is empty"));
    }
    if entrypoint.as_bytes().contains(&0) {
        return Err(policy(&shown, "manifest entrypoint contains a NUL byte"));
    }
    let relative = Path::new(entrypoint);
    if relative.is_absolute() {
        return Err(policy(
            &shown,
            "manifest entrypoint must be relative to the package directory",
        ));
    }
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(policy(
                &shown,
                "manifest entrypoint must contain only ordinary path components",
            ));
        }
    }

    let candidate = directory.join(relative);
    let metadata = std::fs::symlink_metadata(&candidate)
        .map_err(|error| policy(&shown, format!("cannot stat library: {error}")))?;
    if metadata.file_type().is_symlink() {
        return Err(policy(&shown, "library is a symbolic link"));
    }
    if !metadata.is_file() {
        return Err(policy(&shown, "library is not a regular file"));
    }
    let library = candidate
        .canonicalize()
        .map_err(|error| policy(&shown, format!("cannot canonicalise library: {error}")))?;
    // Belt and braces after canonicalisation: a component of the *directory*
    // prefix could itself be a link that leads out of the package.
    if !library.starts_with(directory) {
        return Err(policy(
            library.display().to_string(),
            "library resolves outside its package directory",
        ));
    }
    Ok(library)
}
