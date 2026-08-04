//! Plugin manifest and permission model (spec 19, 20).

pub mod configuration;
pub mod manifest;
pub mod permissions;
pub mod scheduling;

pub use configuration::{
    ConfigurationField, ConfigurationKind, ConfigurationSection, FieldViolation, REDACTED, RULE_ALLOWED,
    RULE_DEFAULT, RULE_DUPLICATE_FIELD, RULE_FIELD_NAME, RULE_MAXIMUM, RULE_MAX_LENGTH, RULE_MINIMUM,
    RULE_REQUIRED, RULE_TYPE, RULE_UNKNOWN_FIELD,
};
pub use manifest::{
    ActivationSection, ConcurrencySection, Manifest, PerformanceSection, PluginSection, PythonSection,
    QuerySection, Runtime, Startup,
};
pub use permissions::{ClipboardPermission, FilesystemAccess, FilesystemScope, Permissions};
pub use scheduling::{
    PolicyProblem, QueryPolicy, SchedulingProfile, MAX_CONCURRENT_REQUESTS, MAX_DEBOUNCE_MS,
    MAX_MINIMUM_QUERY_LENGTH,
};

#[derive(Debug)]
pub enum ManifestError {
    Parse(toml::de::Error),
    UnsupportedVersion(u32),
    InvalidQueryPolicy {
        field: &'static str,
        problem: PolicyProblem,
        detail: Option<&'static str>,
    },
    NoEntrypoint {
        os: String,
        arch: String,
    },
    /// The `[configuration]` declaration is unusable (spec 21.3): the manifest
    /// parsed, but a field it declares could never be satisfied.
    InvalidConfiguration(FieldViolation),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(error) => write!(formatter, "failed to parse crikey.toml: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported manifest-version {version}")
            }
            Self::InvalidQueryPolicy {
                detail: Some(detail), ..
            } => write!(formatter, "invalid crikey.toml query policy: {detail}"),
            Self::InvalidQueryPolicy { field, problem, .. } => {
                write!(formatter, "invalid query policy field {field}: {problem:?}")
            }
            Self::NoEntrypoint { os, arch } => write!(formatter, "no entrypoint for {os}-{arch}"),
            Self::InvalidConfiguration(violation) => {
                write!(formatter, "invalid crikey.toml [configuration]: {violation}")
            }
        }
    }
}

impl std::error::Error for ManifestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::InvalidConfiguration(violation) => Some(violation),
            _ => None,
        }
    }
}

impl From<toml::de::Error> for ManifestError {
    fn from(error: toml::de::Error) -> Self {
        Self::Parse(error)
    }
}

impl From<FieldViolation> for ManifestError {
    fn from(violation: FieldViolation) -> Self {
        Self::InvalidConfiguration(violation)
    }
}
