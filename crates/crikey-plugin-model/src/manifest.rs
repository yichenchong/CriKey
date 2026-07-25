//! `crikey.toml` (spec 19.1).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::permissions::Permissions;
use crate::ManifestError;

pub const SUPPORTED_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Manifest {
    pub manifest_version: u32,
    pub plugin: PluginSection,
    #[serde(default)]
    pub platform: PlatformSection,
    #[serde(default)]
    pub activation: ActivationSection,
    #[serde(default)]
    pub query: QuerySection,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub performance: PerformanceSection,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let manifest: Manifest = toml::from_str(text)?;
        if manifest.manifest_version != SUPPORTED_MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedVersion(manifest.manifest_version));
        }
        Ok(manifest)
    }

    /// Selects the binary for a platform/architecture pair (spec 19.3). A
    /// package without a compatible entrypoint is unavailable, not loaded.
    pub fn entrypoint_for(&self, os: &str, arch: &str) -> Result<&str, ManifestError> {
        let key = format!("{os}-{arch}");
        self.plugin
            .entrypoint
            .get(&key)
            .or_else(|| self.plugin.entrypoint.get("any"))
            .map(String::as_str)
            .ok_or_else(|| ManifestError::NoEntrypoint {
                os: os.into(),
                arch: arch.into(),
            })
    }
}

/// Runtime that executes the plugin (spec 19.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Runtime {
    LegacyPython,
    Python,
    Native,
    Wasm,
    Builtin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PluginSection {
    pub id: String,
    pub name: String,
    pub version: String,
    pub runtime: Runtime,
    /// Keyed by `<os>-<arch>`, or a plain string for runtime-neutral plugins.
    #[serde(default, deserialize_with = "entrypoint_map")]
    pub entrypoint: BTreeMap<String, String>,
    #[serde(default)]
    pub api: Option<String>,
}

fn entrypoint_map<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Single(String),
        PerTarget(BTreeMap<String, String>),
    }
    Ok(match Raw::deserialize(deserializer)? {
        Raw::Single(value) => BTreeMap::from([("any".to_string(), value)]),
        Raw::PerTarget(map) => map,
    })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PlatformSection {
    #[serde(default)]
    pub os: Vec<String>,
    #[serde(default)]
    pub arch: Vec<String>,
}

/// Relevance gating metadata (spec 8.11). Never applied to `legacy-strict`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ActivationSection {
    #[serde(default)]
    pub minimum_query_length: usize,
    #[serde(default)]
    pub prefixes: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub empty_query: bool,
}

/// Modern query policy (spec 19.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct QuerySection {
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default)]
    pub maximum_wait_ms: Option<u64>,
    #[serde(default = "yes")]
    pub leading_edge: bool,
    #[serde(default = "yes")]
    pub trailing_edge: bool,
    #[serde(default = "one")]
    pub max_concurrent_requests: u32,
    #[serde(default)]
    pub network_backed: bool,
}

impl Default for QuerySection {
    fn default() -> Self {
        Self {
            debounce_ms: default_debounce_ms(),
            maximum_wait_ms: None,
            leading_edge: true,
            trailing_edge: true,
            max_concurrent_requests: 1,
            network_backed: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Startup {
    #[default]
    Lazy,
    Eager,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PerformanceSection {
    #[serde(default)]
    pub startup: Startup,
    #[serde(default = "default_soft_timeout")]
    pub suggest_soft_timeout_ms: u64,
    #[serde(default = "default_hard_timeout")]
    pub suggest_hard_timeout_ms: u64,
    #[serde(default = "default_max_results_query")]
    pub maximum_results_per_query: usize,
    #[serde(default = "default_max_results_batch")]
    pub maximum_results_per_batch: usize,
}

impl Default for PerformanceSection {
    fn default() -> Self {
        Self {
            startup: Startup::Lazy,
            suggest_soft_timeout_ms: default_soft_timeout(),
            suggest_hard_timeout_ms: default_hard_timeout(),
            maximum_results_per_query: default_max_results_query(),
            maximum_results_per_batch: default_max_results_batch(),
        }
    }
}

fn default_debounce_ms() -> u64 {
    50
}
fn yes() -> bool {
    true
}
fn one() -> u32 {
    1
}
fn default_soft_timeout() -> u64 {
    50
}
fn default_hard_timeout() -> u64 {
    500
}
fn default_max_results_query() -> usize {
    250
}
fn default_max_results_batch() -> usize {
    50
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manifest example printed in the specification must parse verbatim.
    const SPEC_EXAMPLE: &str = include_str!("../tests/data/spec-example.crikey.toml");

    #[test]
    fn parses_the_specification_example() {
        let manifest = Manifest::parse(SPEC_EXAMPLE).expect("spec example must parse");
        assert_eq!(manifest.plugin.id, "dev.example.repositories");
        assert_eq!(manifest.plugin.runtime, Runtime::Native);
        assert_eq!(manifest.activation.minimum_query_length, 2);
        assert_eq!(manifest.query.maximum_wait_ms, Some(200));
        assert_eq!(manifest.performance.startup, Startup::Lazy);
        assert_eq!(
            manifest.entrypoint_for("linux", "x86_64").unwrap(),
            "bin/repository-search"
        );
        assert!(manifest.entrypoint_for("freebsd", "x86_64").is_err());
    }
}
