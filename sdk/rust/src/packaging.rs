//! Author-side native package layout checks (spec 23.3).
//!
//! This module deliberately validates a directory only.  Archive construction
//! and installation remain host/package-manager responsibilities.

use crikey_plugin_model::{Manifest, Runtime};
use std::fs;
use std::path::{Path, PathBuf};

use crate::SdkError;

/// Paths required for one native plugin package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageLayout {
    /// Manifest path.
    pub manifest: PathBuf,
    /// Selected native executable path.
    pub entrypoint: PathBuf,
}

/// Validates `crikey.toml` and the platform-specific native entrypoint without
/// loading or executing the plugin (spec 23.3, 24.1).
///
/// Manifest parsing is delegated to the same schema used by the launcher. This
/// keeps author-side validation aligned with the loader for required metadata,
/// runtime policy and platform entrypoint selection.
pub fn validate_layout(dir: &Path, os: &str, arch: &str) -> Result<PackageLayout, SdkError> {
    if !dir.is_dir() {
        return Err(SdkError::Config(format!(
            "plugin directory does not exist: {}",
            dir.display()
        )));
    }
    let manifest = dir.join("crikey.toml");
    let text = fs::read_to_string(&manifest)
        .map_err(|error| SdkError::Config(format!("cannot read {}: {error}", manifest.display())))?;
    let parsed = Manifest::parse(&text)
        .map_err(|error| SdkError::Config(format!("invalid {}: {error}", manifest.display())))?;
    if parsed.plugin.runtime != Runtime::Native {
        return Err(SdkError::Config(format!(
            "plugin runtime must be native, got {:?}",
            parsed.plugin.runtime
        )));
    }
    let relative = parsed
        .entrypoint_for(os, arch)
        .map_err(|error| SdkError::Config(error.to_string()))?;
    let relative_path = Path::new(relative);
    if relative.is_empty() || relative_path.is_absolute() || has_parent_component(relative_path) {
        return Err(SdkError::Config(format!(
            "entrypoint must be a relative path inside the plugin directory: {relative}"
        )));
    }
    let entrypoint = dir.join(relative_path);
    if !entrypoint.is_file() {
        return Err(SdkError::Config(format!(
            "native entrypoint is missing: {}",
            entrypoint.display()
        )));
    }
    let canonical_dir = fs::canonicalize(dir)
        .map_err(|error| SdkError::Config(format!("cannot resolve {}: {error}", dir.display())))?;
    let canonical_entrypoint = fs::canonicalize(&entrypoint).map_err(|error| {
        SdkError::Config(format!(
            "cannot resolve entrypoint {}: {error}",
            entrypoint.display()
        ))
    })?;
    if !canonical_entrypoint.starts_with(&canonical_dir) {
        return Err(SdkError::Config(format!(
            "entrypoint resolves outside the plugin directory: {}",
            entrypoint.display()
        )));
    }
    Ok(PackageLayout { manifest, entrypoint })
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
}
