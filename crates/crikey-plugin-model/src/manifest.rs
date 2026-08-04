//! `crikey.toml` (spec 19.1).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::permissions::Permissions;
use crate::scheduling::SchedulingProfile;
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
    pub concurrency: ConcurrencySection,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub performance: PerformanceSection,
    #[serde(default)]
    pub python: PythonSection,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let manifest: Manifest = match toml::from_str(text) {
            Ok(manifest) => manifest,
            Err(error) => {
                let Some((normalized, oversized)) = normalize_oversized_query_integers(text) else {
                    return Err(error.into());
                };
                let mut manifest: Manifest = toml::from_str(&normalized)?;
                if let Some(value) = oversized.debounce_ms {
                    manifest.query.debounce_ms = Some(value);
                }
                if let Some(value) = oversized.maximum_wait_ms {
                    manifest.query.maximum_wait_ms = Some(value);
                }
                manifest
            }
        };
        if manifest.manifest_version != SUPPORTED_MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedVersion(manifest.manifest_version));
        }
        manifest.validate_query_policy()?;
        Ok(manifest)
    }

    /// Selects the binary for a platform/architecture pair (spec 19.3). A
    /// package without a compatible entrypoint is unavailable, not loaded.
    pub fn entrypoint_for(&self, os: &str, arch: &str) -> Result<&str, ManifestError> {
        let key = format!("{os}-{arch}");
        self.plugin
            .entrypoint
            .get(&key)
            .filter(|entrypoint| !entrypoint.trim().is_empty())
            .or_else(|| {
                self.plugin
                    .entrypoint
                    .get("any")
                    .filter(|entrypoint| !entrypoint.trim().is_empty())
            })
            .map(String::as_str)
            .ok_or_else(|| ManifestError::NoEntrypoint {
                os: os.into(),
                arch: arch.into(),
            })
    }
}

#[derive(Default)]
struct OversizedQueryIntegers {
    debounce_ms: Option<u64>,
    maximum_wait_ms: Option<u64>,
}

#[derive(Clone, Copy)]
enum QueryIntegerField {
    Debounce,
    MaximumWait,
}

/// TOML represents integers as `i64`, while the manifest declaration is a
/// `u64`. Preserve the full declared domain so `u64` values can be validated
/// as out of range; values beyond `u64` remain ordinary parse failures.
fn normalize_oversized_query_integers(text: &str) -> Option<(String, OversizedQueryIntegers)> {
    let mut in_query = false;
    let mut offset = 0;
    let mut replacements = Vec::new();
    let mut oversized = OversizedQueryIntegers::default();

    for line in text.split_inclusive('\n') {
        let statement = line
            .trim()
            .split_once('#')
            .map_or_else(|| line.trim(), |(before, _)| before.trim());
        if statement.starts_with('[') {
            in_query = statement == "[query]";
            offset += line.len();
            continue;
        }
        if !in_query {
            offset += line.len();
            continue;
        }

        let Some(equals) = line.find('=') else {
            offset += line.len();
            continue;
        };
        let field = match line[..equals].trim() {
            "debounce-ms" => QueryIntegerField::Debounce,
            "maximum-wait-ms" => QueryIntegerField::MaximumWait,
            _ => {
                offset += line.len();
                continue;
            }
        };
        let right = &line[equals + 1..];
        let before_comment = right.split_once('#').map_or(right, |(value, _)| value);
        let value_text = before_comment.trim();
        let Some(value) = oversized_decimal(value_text) else {
            offset += line.len();
            continue;
        };

        let leading_whitespace = before_comment.len() - before_comment.trim_start().len();
        let start = offset + equals + 1 + leading_whitespace;
        replacements.push((start, start + value_text.len()));
        match field {
            QueryIntegerField::Debounce => oversized.debounce_ms = Some(value),
            QueryIntegerField::MaximumWait => oversized.maximum_wait_ms = Some(value),
        }
        offset += line.len();
    }

    if replacements.is_empty() {
        return None;
    }

    let mut normalized = String::with_capacity(text.len());
    let mut copied_through = 0;
    for (start, end) in replacements {
        normalized.push_str(&text[copied_through..start]);
        normalized.push('0');
        copied_through = end;
    }
    normalized.push_str(&text[copied_through..]);
    Some((normalized, oversized))
}

fn oversized_decimal(text: &str) -> Option<u64> {
    let digits = text.strip_prefix('+').unwrap_or(text);
    if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
        return None;
    }

    let bytes = digits.as_bytes();
    let mut value = 0_u64;
    for (index, byte) in bytes.iter().copied().enumerate() {
        match byte {
            b'0'..=b'9' => {
                value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
            }
            b'_' if index > 0
                && index + 1 < bytes.len()
                && bytes[index - 1].is_ascii_digit()
                && bytes[index + 1].is_ascii_digit() => {}
            _ => return None,
        }
    }
    (value > i64::MAX as u64).then_some(value)
}

/// Runtime declared by the manifest. Execution support is decided by the
/// corresponding host; parsing a recognized value does not make it runnable.
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
    #[serde(default)]
    pub scheduling_profile: Option<SchedulingProfile>,
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

/// Modern-Python managed-environment inputs (spec 15.2, 19; M4 contract §6).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PythonSection {
    #[serde(default, rename = "requires-python")]
    pub requires_python: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
}

/// Relevance gating metadata (spec 8.11). Never applied to `legacy-strict`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ActivationSection {
    #[serde(default)]
    pub minimum_query_length: Option<usize>,
    #[serde(default)]
    pub prefixes: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub empty_query: Option<bool>,
}

/// Modern query policy (spec 19.4).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct QuerySection {
    #[serde(default)]
    pub debounce_ms: Option<u64>,
    #[serde(default)]
    pub maximum_wait_ms: Option<u64>,
    #[serde(default)]
    pub leading_edge: Option<bool>,
    #[serde(default)]
    pub trailing_edge: Option<bool>,
    #[serde(default)]
    pub max_concurrent_requests: Option<u32>,
    #[serde(default)]
    pub network_backed: Option<bool>,
}

/// Declared per-plugin concurrency budgets (spec 13.5).
///
/// Each budget is optional so the declaration layer can distinguish "the
/// author said nothing" (`None`) from "the author switched this surface off"
/// (`Some(0)`). Collapsing the two would either mute a plugin or uncap it;
/// resolving `None` to an effective limit is the enforcement layer's job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ConcurrencySection {
    #[serde(default)]
    pub max_suggestion_requests: Option<u32>,
    #[serde(default)]
    pub max_action_requests: Option<u32>,
    #[serde(default)]
    pub max_background_tasks: Option<u32>,
    #[serde(default)]
    pub max_catalog_tasks: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Startup {
    #[default]
    Lazy,
    Eager,
}

/// Performance preferences declared by a plugin. A host must consume a field
/// before presenting it as an enforced limit; parsing alone does not apply it.
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
        assert_eq!(manifest.activation.minimum_query_length, Some(2));
        assert_eq!(manifest.query.maximum_wait_ms, Some(200));
        assert_eq!(manifest.performance.startup, Startup::Lazy);
        assert_eq!(
            manifest.entrypoint_for("linux", "x86_64").unwrap(),
            "bin/repository-search"
        );
        assert!(manifest.entrypoint_for("freebsd", "x86_64").is_err());
    }
}
