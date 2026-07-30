//! Plugin manifest and permission model (spec 19, 20).

pub mod manifest;
pub mod permissions;
pub mod scheduling;

pub use manifest::{
    ActivationSection, Manifest, PerformanceSection, PluginSection, PythonSection, QuerySection, Runtime,
    Startup,
};
pub use permissions::{ClipboardPermission, FilesystemAccess, FilesystemScope, Permissions};
pub use scheduling::{
    PolicyProblem, QueryPolicy, SchedulingProfile, MAX_CONCURRENT_REQUESTS, MAX_DEBOUNCE_MS,
    MAX_MINIMUM_QUERY_LENGTH,
};

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("failed to parse crikey.toml: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("unsupported manifest-version {0}")]
    UnsupportedVersion(u32),
    #[error("invalid query policy field {field}: {problem:?}")]
    InvalidQueryPolicy {
        field: &'static str,
        problem: PolicyProblem,
    },
    #[error("no entrypoint for {os}-{arch}")]
    NoEntrypoint { os: String, arch: String },
}
