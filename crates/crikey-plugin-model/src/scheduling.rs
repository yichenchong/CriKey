//! Manifest scheduling declarations and their resolved runtime policy.

use serde::{Deserialize, Serialize};

use crate::manifest::{Manifest, Runtime};
use crate::permissions::{ClipboardPermission, FilesystemScope};
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

/// One manifest declaration the host accepts but cannot act on, and why.
///
/// `reason` is a stable kebab-case token rather than prose: it is printed by
/// `crikey plugin doctor`, whose output is parsed by operators and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnhonouredDeclaration {
    pub field: &'static str,
    pub reason: &'static str,
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

    /// Declarations this build parses and validates but cannot act on for the
    /// declared runtime.
    ///
    /// Distinct from [`Self::ignored_modern_fields`], which names declarations
    /// a profile deliberately neutralizes: those are policy, these are honest
    /// gaps. A declaration listed here is accepted by the manifest and grants
    /// nothing at runtime, which is precisely the shape of defect an audit
    /// looks for — a capability advertised with no production consumer — so it
    /// is reported rather than left for an author to infer from behaviour.
    ///
    /// Most of the permission entries below share one cause. A permission can
    /// only be enforced where the HOST performs the privileged operation for
    /// the plugin and can decline; spec 20.2 defers operating-system
    /// confinement, so where the plugin's own process does the work — a
    /// clipboard write, a notification, a window activation, a keyring read, a
    /// `dlopen` — the declaration buys the author nothing at all. Saying so is
    /// the whole point: a permission list that reads like a sandbox and
    /// confines nothing is worse than no permission list.
    pub fn unhonoured_declarations(&self) -> Vec<UnhonouredDeclaration> {
        let mut unhonoured = Vec::new();
        // The host reads exactly one filesystem region on a plugin's behalf:
        // the plugin's own package, for icons and other package resources.
        // Every other scope names files the plugin opens itself.
        if self
            .permissions
            .filesystem
            .iter()
            .any(|entry| !matches!(entry.scope, FilesystemScope::Package | FilesystemScope::None))
        {
            unhonoured.push(UnhonouredDeclaration {
                field: "permissions.filesystem",
                reason: "host-mediates-no-filesystem-access-outside-the-plugin-package",
            });
        }
        if self.permissions.clipboard != ClipboardPermission::None {
            unhonoured.push(UnhonouredDeclaration {
                field: "permissions.clipboard",
                reason: "no-host-mediated-clipboard-api-for-plugins",
            });
        }
        if self.permissions.window_enumeration {
            unhonoured.push(UnhonouredDeclaration {
                field: "permissions.window-enumeration",
                reason: "no-host-mediated-window-api-for-plugins",
            });
        }
        if self.permissions.window_control {
            unhonoured.push(UnhonouredDeclaration {
                field: "permissions.window-control",
                reason: "no-host-mediated-window-api-for-plugins",
            });
        }
        if self.permissions.notifications {
            unhonoured.push(UnhonouredDeclaration {
                field: "permissions.notifications",
                reason: "no-host-mediated-notification-api-for-plugins",
            });
        }
        if self.permissions.secrets {
            unhonoured.push(UnhonouredDeclaration {
                field: "permissions.secrets",
                reason: "no-host-mediated-secret-store-api-for-plugins",
            });
        }
        // The environment grant decides whether the host hands a child the
        // ambient environment or the stripped one, so it means something only
        // where the host spawns that child: the modern Python worker, native
        // worker and dedicated C-ABI host. A builtin, WASM or legacy worker
        // does not take this grant.
        if self.permissions.environment
            && !matches!(
                self.plugin.runtime,
                Runtime::Python | Runtime::Native | Runtime::CAbi
            )
        {
            unhonoured.push(UnhonouredDeclaration {
                field: "permissions.environment",
                reason: "host-does-not-mediate-child-environment-for-runtime",
            });
        }
        // The dedicated C-ABI host gates loading on this declaration. Other
        // runtimes either cannot use a native library through a host seam or
        // load their own dependencies before the host can mediate them.
        if self.permissions.native_library_loading && self.plugin.runtime != Runtime::CAbi {
            unhonoured.push(UnhonouredDeclaration {
                field: "permissions.native-library-loading",
                reason: "out-of-process-plugin-loads-its-own-libraries",
            });
        }
        // Only the modern Python worker registers background tasks with the
        // host and gates them on admission (spec 15.8). No other runtime has
        // an equivalent API, and spec 20.2 defers permission ENFORCEMENT
        // entirely, so on those runtimes the declaration is inert: it neither
        // grants the plugin anything nor stops it spawning its own threads.
        if self.permissions.background_execution && self.plugin.runtime != Runtime::Python {
            unhonoured.push(UnhonouredDeclaration {
                field: "permissions.background-execution",
                reason: "no-background-task-api-for-runtime",
            });
        }
        // `crikey-cabi-host` calls one restricted C entry point at a time on
        // one library handle: the ABI has no reentrancy contract to lean on
        // and no way to interrupt a call already inside plugin code, so the
        // host serialises. A `c-abi` plugin asking for more than one
        // concurrent suggestion therefore gets exactly one (ADR-0015).
        if self.plugin.runtime == Runtime::CAbi
            && self
                .concurrency
                .max_suggestion_requests
                .is_some_and(|value| value > 1)
        {
            unhonoured.push(UnhonouredDeclaration {
                field: "concurrency.max-suggestion-requests",
                reason: "c-abi-calls-are-serialised",
            });
        }
        unhonoured
    }

    /// Refuses permission declarations nothing in this host can grant.
    ///
    /// Reporting is the honest fallback for a declaration that is merely
    /// inert. `network-listener` is not merely inert: no build of this host
    /// offers a plugin an inbound socket and no configuration turns one on, so
    /// an author who writes it has described a plugin this launcher cannot
    /// run. That is the same case as `query.network-backed` without
    /// `permissions.network`, and it gets the same answer — a loud rejection
    /// at parse time, rather than a doctor note about a plugin already
    /// serving queries under a confinement it does not have.
    pub(crate) fn validate_permissions(&self) -> Result<(), ManifestError> {
        if self.permissions.network_listener {
            return Err(invalid_with_detail(
                "permissions.network-listener",
                PolicyProblem::NotPermitted,
                "permissions.network-listener cannot be granted: this host offers plugins no inbound socket",
            ));
        }
        Ok(())
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

        // Numeric declarations are validated even when legacy-strict ignores
        // their modern scheduling semantics. Otherwise a typo can be accepted
        // and hidden behind the compatibility profile.
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

        // A maximum wait only has meaning when the effective debounce period
        // can postpone dispatch. For runtimes such as `builtin`, the omitted
        // debounce is itself a zero-valued default; checking only
        // `query.debounce_ms == Some(0)` would accept a maximum wait and then
        // silently discard it in `query_policy`.
        let effective_network_backed = self.query.network_backed.unwrap_or(self.permissions.network);
        let effective_debounce_ms = self
            .query
            .debounce_ms
            .unwrap_or_else(|| default_debounce_ms(self.plugin.runtime, effective_network_backed));
        if effective_debounce_ms == 0 && self.query.maximum_wait_ms.is_some() {
            return Err(invalid_with_detail(
                "query.maximum-wait-ms",
                PolicyProblem::Contradictory,
                "query.maximum-wait-ms cannot be combined with query.debounce-ms = 0 or an effective debounce period of 0 because zero debounce dispatches immediately",
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
        Runtime::Native | Runtime::Wasm | Runtime::CAbi => NATIVE_DEBOUNCE_MS,
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
    invalid_with_detail(field, problem, None)
}

fn invalid_with_detail(
    field: &'static str,
    problem: PolicyProblem,
    detail: impl Into<Option<&'static str>>,
) -> ManifestError {
    ManifestError::InvalidQueryPolicy {
        field,
        problem,
        detail: detail.into(),
    }
}
