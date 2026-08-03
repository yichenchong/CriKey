//! The plugin import path (spec 15.4).
//!
//! A modern plugin's `sys.path` is assembled by CriKey, never inherited, in this
//! exact order: plugin source (so a plugin can shadow), its packaged modules,
//! its managed dependency environment, then the CriKey SDK. The system-wide
//! site-packages is NEVER on it — excluded by construction — which together with
//! the worker's `-S` flag makes a plugin's imports reproducible.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::environment::MaterializedEnvironment;
use crate::PackageError;
/// The assembled import path, in spec order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPath {
    pub entries: Vec<PathBuf>,
}

impl ImportPath {
    /// Lay out the entries as exactly
    /// `[plugin_source, packaged.., env.site_dir, sdk]`.
    pub fn assemble(
        plugin_source: &Path,
        packaged: &[PathBuf],
        env: &MaterializedEnvironment,
        sdk: &Path,
    ) -> ImportPath {
        let mut entries = Vec::with_capacity(packaged.len() + 3);
        entries.push(plugin_source.to_path_buf());
        entries.extend(packaged.iter().cloned());
        entries.push(env.site_dir.clone());
        entries.push(sdk.to_path_buf());
        ImportPath { entries }
    }

    /// Join the entries with the OS path-list separator (never a global site).
    ///
    /// A path-list value has no portable escaping for a separator embedded in
    /// one component, so reject that component instead of panicking or silently
    /// changing the path seen by Python.
    pub fn to_pythonpath(&self) -> Result<OsString, PackageError> {
        let separator = if cfg!(windows) { ';' } else { ':' };
        for entry in &self.entries {
            if entry.to_string_lossy().contains(separator) {
                return Err(PackageError::InvalidImportPath(format!(
                    "path component {:?} contains the path-list separator `{separator}`",
                    entry
                )));
            }
        }
        std::env::join_paths(&self.entries).map_err(|error| {
            PackageError::InvalidImportPath(format!("could not encode import path: {error}"))
        })
    }
}
