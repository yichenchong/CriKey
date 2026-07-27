//! Manifest scheduling declarations and their resolved runtime policy.

use serde::{Deserialize, Serialize};

use crate::manifest::{Manifest, Runtime};
use crate::ManifestError;

/// Largest accepted debounce or maximum-wait declaration, in milliseconds.
pub const MAX_DEBOUNCE_MS: u64 = 1_000;
/// Largest accepted per-plugin in-flight request budget.
pub const MAX_CONCURRENT_REQUESTS: u32 = 32;
/// Largest accepted activation minimum, measured in Unicode scalar values.
pub const MAX_MINIMUM_QUERY_LENGTH: usize = 1_024;

const NATIVE_DEBOUNCE_MS: u64 = 40;
const PYTHON_DEBOUNCE_MS: u64 = 60;
const NETWORK_DEBOUNCE_MS: u64 = 200;
const DEFAULT_MAXIMUM_WAIT_MULTIPLIER: u64 = 4;

/// Host scheduling semantics selected for a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SchedulingProfile {
    LegacyStrict,
    LegacyOptimized,
    Modern,
}

/// Classification attached to a rejected scheduling declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyProblem {
    Contradictory,
    OutOfRange,
    NotPermitted,
}

/// Fully resolved scheduling and admission policy consumed by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPolicy {
    pub profile: SchedulingProfile,
    pub debounce_ms: u64,
    pub maximum_wait_ms: Option<u64>,
    pub leading_edge: bool,
    pub trailing_edge: bool,
    pub minimum_query_length: usize,
    pub max_concurrent_requests: u32,
    pub network_backed: bool,
    pub empty_query: bool,
    pub prefixes: Vec<String>,
    pub keywords: Vec<String>,
    pub categories: Vec<String>,
}

impl QueryPolicy {
    /// Whether a normalized query passes the manifest's activation gates.
    pub fn admits(&self, query: &str) -> bool {
        if query.is_empty() {
            return self.empty_query;
        }
        if query.chars().count() < self.minimum_query_length {
            return false;
        }
        if self.prefixes.is_empty() && self.keywords.is_empty() {
            return true;
        }

        self.prefixes
            .iter()
            .any(|prefix| starts_with_ignore_ascii_case(query, prefix))
            || query.split_whitespace().next().is_some_and(|first| {
                self.keywords
                    .iter()
                    .any(|keyword| first.eq_ignore_ascii_case(keyword))
            })
    }
}

impl Manifest {
    /// Resolves an omitted profile from the plugin runtime.
    pub fn scheduling_profile(&self) -> SchedulingProfile {
        self.plugin.scheduling_profile.unwrap_or({
            if matches!(self.plugin.runtime, Runtime::LegacyPython) {
                SchedulingProfile::LegacyStrict
            } else {
                SchedulingProfile::Modern
            }
        })
    }

    /// Resolves optional declarations into the effective host policy.
    pub fn query_policy(&self) -> QueryPolicy {
        let profile = self.scheduling_profile();
        if profile == SchedulingProfile::LegacyStrict {
            return QueryPolicy {
                profile,
                debounce_ms: 0,
                maximum_wait_ms: None,
                leading_edge: true,
                trailing_edge: true,
                minimum_query_length: 0,
                max_concurrent_requests: 1,
                network_backed: false,
                empty_query: true,
                prefixes: Vec::new(),
                keywords: Vec::new(),
                categories: self.activation.categories.clone(),
            };
        }

        let network_backed = self.query.network_backed.unwrap_or(self.permissions.network);
        let debounce_ms = self
            .query
            .debounce_ms
            .unwrap_or_else(|| default_debounce_ms(self.plugin.runtime, network_backed));
        let maximum_wait_ms = if debounce_ms == 0 {
            None
        } else {
            self.query
                .maximum_wait_ms
                .or_else(|| default_maximum_wait_ms(debounce_ms))
        };

        QueryPolicy {
            profile,
            debounce_ms,
            maximum_wait_ms,
            leading_edge: self.query.leading_edge.unwrap_or(true),
            trailing_edge: self.query.trailing_edge.unwrap_or(true),
            minimum_query_length: self.activation.minimum_query_length.unwrap_or(0),
            max_concurrent_requests: self.query.max_concurrent_requests.unwrap_or(1),
            network_backed,
            empty_query: self.activation.empty_query.unwrap_or(false),
            prefixes: self.activation.prefixes.clone(),
            keywords: self.activation.keywords.clone(),
            categories: self.activation.categories.clone(),
        }
    }

    /// Names declarations that `legacy-strict` deliberately neutralizes.
    pub fn ignored_modern_fields(&self) -> Vec<&'static str> {
        if self.scheduling_profile() != SchedulingProfile::LegacyStrict {
            return Vec::new();
        }

        let mut ignored = Vec::with_capacity(10);
        if self.activation.minimum_query_length.is_some() {
            ignored.push("activation.minimum-query-length");
        }
        if !self.activation.prefixes.is_empty() {
            ignored.push("activation.prefixes");
        }
        if !self.activation.keywords.is_empty() {
            ignored.push("activation.keywords");
        }
        if self.activation.empty_query.is_some() {
            ignored.push("activation.empty-query");
        }
        if self.query.debounce_ms.is_some() {
            ignored.push("query.debounce-ms");
        }
        if self.query.maximum_wait_ms.is_some() {
            ignored.push("query.maximum-wait-ms");
        }
        if self.query.leading_edge.is_some() {
            ignored.push("query.leading-edge");
        }
        if self.query.trailing_edge.is_some() {
            ignored.push("query.trailing-edge");
        }
        if self.query.max_concurrent_requests.is_some() {
            ignored.push("query.max-concurrent-requests");
        }
        if self.query.network_backed.is_some() {
            ignored.push("query.network-backed");
        }
        ignored
    }

    pub(crate) fn validate_query_policy(&self) -> Result<(), ManifestError> {
        let profile = self.scheduling_profile();
        let legacy_runtime = matches!(self.plugin.runtime, Runtime::LegacyPython);
        if legacy_runtime == matches!(profile, SchedulingProfile::Modern) {
            return Err(invalid("plugin.scheduling-profile", PolicyProblem::Contradictory));
        }
        if !legacy_runtime && profile != SchedulingProfile::Modern {
            return Err(invalid("plugin.scheduling-profile", PolicyProblem::Contradictory));
        }

        // Modern declarations are retained for diagnostics but have no semantic
        // force under the strict compatibility profile.
        if profile == SchedulingProfile::LegacyStrict {
            return Ok(());
        }

        if self
            .query
            .debounce_ms
            .is_some_and(|value| value > MAX_DEBOUNCE_MS)
        {
            return Err(invalid("query.debounce-ms", PolicyProblem::OutOfRange));
        }
        if self
            .query
            .maximum_wait_ms
            .is_some_and(|value| value > MAX_DEBOUNCE_MS)
        {
            return Err(invalid("query.maximum-wait-ms", PolicyProblem::OutOfRange));
        }
        if self
            .query
            .max_concurrent_requests
            .is_some_and(|value| value == 0 || value > MAX_CONCURRENT_REQUESTS)
        {
            return Err(invalid(
                "query.max-concurrent-requests",
                PolicyProblem::OutOfRange,
            ));
        }
        if self
            .activation
            .minimum_query_length
            .is_some_and(|value| value > MAX_MINIMUM_QUERY_LENGTH)
        {
            return Err(invalid(
                "activation.minimum-query-length",
                PolicyProblem::OutOfRange,
            ));
        }
        if self.query.network_backed == Some(true) && !self.permissions.network {
            return Err(invalid("query.network-backed", PolicyProblem::NotPermitted));
        }

        let policy = self.query_policy();
        if policy
            .maximum_wait_ms
            .is_some_and(|maximum_wait| maximum_wait < policy.debounce_ms)
        {
            return Err(invalid("query.maximum-wait-ms", PolicyProblem::Contradictory));
        }
        if !policy.leading_edge && !policy.trailing_edge {
            return Err(invalid("query.leading-edge", PolicyProblem::Contradictory));
        }
        if policy.empty_query && policy.minimum_query_length > 0 {
            return Err(invalid(
                "activation.minimum-query-length",
                PolicyProblem::Contradictory,
            ));
        }

        Ok(())
    }
}

fn default_debounce_ms(runtime: Runtime, network_backed: bool) -> u64 {
    match runtime {
        Runtime::Builtin => 0,
        _ if network_backed => NETWORK_DEBOUNCE_MS,
        Runtime::Python | Runtime::LegacyPython => PYTHON_DEBOUNCE_MS,
        Runtime::Native | Runtime::Wasm => NATIVE_DEBOUNCE_MS,
    }
}

fn default_maximum_wait_ms(debounce_ms: u64) -> Option<u64> {
    (debounce_ms != 0).then(|| {
        debounce_ms
            .saturating_mul(DEFAULT_MAXIMUM_WAIT_MULTIPLIER)
            .min(MAX_DEBOUNCE_MS)
            .max(debounce_ms)
    })
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
        .as_bytes()
        .get(..prefix.len())
        .is_some_and(|leading| leading.eq_ignore_ascii_case(prefix.as_bytes()))
}

fn invalid(field: &'static str, problem: PolicyProblem) -> ManifestError {
    ManifestError::InvalidQueryPolicy { field, problem }
}
