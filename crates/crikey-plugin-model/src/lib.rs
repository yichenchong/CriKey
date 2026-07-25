//! Plugin manifest and permission model (spec 19, 20).

pub mod manifest;
pub mod permissions;

pub use manifest::{
    ActivationSection, Manifest, PerformanceSection, PluginSection, QuerySection, Runtime, Startup,
};
pub use permissions::{ClipboardPermission, FilesystemAccess, FilesystemScope, Permissions};

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("failed to parse crikey.toml: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("unsupported manifest-version {0}")]
    UnsupportedVersion(u32),
    #[error("no entrypoint for {os}-{arch}")]
    NoEntrypoint { os: String, arch: String },
}
