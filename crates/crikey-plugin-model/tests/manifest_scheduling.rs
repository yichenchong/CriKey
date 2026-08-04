//! Manifest-to-scheduling-policy semantics (spec 7, 8.3-8.11, 19.4, 25.4).
//!
//! M2 resolves a per-plugin query policy from `crikey.toml` before the
//! scheduler ever sees a keystroke. These tests pin that resolution: what a
//! manifest is allowed to declare, what an *absent* declaration resolves to,
//! and which declarations are rejected outright. Nothing here touches a clock;
//! the timing behaviour of the resolved policy belongs to
//! `crikey-input-scheduler`.
//!
//! Two properties drive most of the design:
//!
//! * An absent field is not the same as an explicitly declared default. A
//!   plugin that writes `debounce-ms = 0` has declared "no debounce" (spec
//!   8.3); a plugin that writes nothing inherits a runtime-derived band from
//!   spec 25.4. The declared values therefore round-trip as `Option`s, and the
//!   effective values come from [`Manifest::query_policy`].
//! * `legacy-strict` never inherits modern host gating (spec 7.1, 8.4, 8.10,
//!   8.11, 25.4). A legacy manifest may still *contain* modern fields, but the
//!   host must drop them and say so rather than silently applying them.

use crikey_plugin_model::{
    Manifest, ManifestError, PolicyProblem, QueryPolicy, Runtime, SchedulingProfile, MAX_CONCURRENT_REQUESTS,
    MAX_DEBOUNCE_MS, MAX_MINIMUM_QUERY_LENGTH,
};

/// The manifest example printed in spec 19.1, byte for byte.
const SPEC_EXAMPLE: &str = include_str!("data/spec-example.crikey.toml");

/// Spec 25.4 recommended bands, used instead of hard-coded constants so that
/// retuning inside a band stays green and leaving one turns red.
const NATIVE_LOCAL_BAND: std::ops::RangeInclusive<u64> = 30..=50;
const PYTHON_LOCAL_BAND: std::ops::RangeInclusive<u64> = 50..=75;
const NETWORK_BACKED_BAND: std::ops::RangeInclusive<u64> = 150..=250;

/// Builds a manifest around `sections`, which supplies everything after
/// `[plugin]`. Keeping the boilerplate in one place means a test body shows
/// only the scheduling inputs it is actually pinning.
fn manifest_text(runtime: &str, sections: &str) -> String {
    format!(
        "manifest-version = 1\n\
         \n\
         [plugin]\n\
         id = \"dev.example.scheduling\"\n\
         name = \"Scheduling Fixture\"\n\
         version = \"1.0.0\"\n\
         runtime = \"{runtime}\"\n\
         entrypoint = \"bin/plugin\"\n\
         {sections}"
    )
}

fn parse(runtime: &str, sections: &str) -> Manifest {
    Manifest::parse(&manifest_text(runtime, sections)).expect("manifest must parse")
}

fn reject(runtime: &str, sections: &str) -> ManifestError {
    match Manifest::parse(&manifest_text(runtime, sections)) {
        Ok(manifest) => panic!("expected rejection, got policy {:?}", manifest.query_policy()),
        Err(error) => error,
    }
}

fn resolved_policy(runtime: &str, sections: &str) -> QueryPolicy {
    parse(runtime, sections).query_policy()
}

#[track_caller]
fn assert_invalid(error: &ManifestError, expected_field: &str, expected: PolicyProblem) {
    match error {
        ManifestError::InvalidQueryPolicy { field, problem, .. } => {
            assert_eq!(
                (*field, *problem),
                (expected_field, expected),
                "wrong rejection for {expected_field}"
            );
        }
        other => panic!("expected InvalidQueryPolicy for {expected_field}, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The specification example
// ---------------------------------------------------------------------------

/// Spec 19.1's example carries every M2 scheduling input except empty-query
/// support; it must survive parsing with each value attributed to the right
/// field, and resolve to exactly what it declared.
#[test]
fn spec_example_resolves_to_the_policy_it_declares() {
    let manifest = Manifest::parse(SPEC_EXAMPLE).expect("spec example must parse");

    assert_eq!(manifest.plugin.runtime, Runtime::Native);
    assert_eq!(manifest.query.debounce_ms, Some(50));
    assert_eq!(manifest.query.maximum_wait_ms, Some(200));
    assert_eq!(manifest.query.leading_edge, Some(true));
    assert_eq!(manifest.query.trailing_edge, Some(true));
    assert_eq!(manifest.query.max_concurrent_requests, Some(1));
    assert_eq!(manifest.query.network_backed, None);
    assert_eq!(manifest.activation.minimum_query_length, Some(2));
    assert_eq!(manifest.activation.prefixes, ["repo", "git"]);
    assert_eq!(manifest.activation.empty_query, None);

    let policy = manifest.query_policy();
    assert_eq!(policy.profile, SchedulingProfile::Modern);
    assert_eq!(policy.debounce_ms, 50);
    assert_eq!(policy.maximum_wait_ms, Some(200));
    assert!(policy.leading_edge);
    assert!(policy.trailing_edge);
    assert_eq!(policy.minimum_query_length, 2);
    assert_eq!(policy.max_concurrent_requests, 1);
    assert!(!policy.network_backed);
    assert!(!policy.empty_query);
    assert!(manifest.ignored_modern_fields().is_empty());
}

/// The example's activation metadata is a gate, not decoration: a query that
/// matches no declared prefix must never reach the plugin (spec 8.11), and one
/// shorter than the declared minimum must not either (spec 8.10).
#[test]
fn spec_example_gates_queries_by_prefix_and_length() {
    let policy = Manifest::parse(SPEC_EXAMPLE)
        .expect("spec example must parse")
        .query_policy();

    assert!(policy.admits("repo crikey"));
    assert!(policy.admits("git log"));
    assert!(!policy.admits("hello"), "no declared prefix matches");
    assert!(!policy.admits("g"), "below the declared minimum length");
    assert!(!policy.admits(""), "empty-query support was never declared");
}

// ---------------------------------------------------------------------------
// Absent versus explicit
// ---------------------------------------------------------------------------

/// A manifest with no `[query]` or `[activation]` table declares nothing. The
/// distinction matters because the resolver's runtime-derived defaults may only
/// be applied where the plugin author was silent.
#[test]
fn a_manifest_without_scheduling_tables_declares_nothing() {
    let manifest = parse("native", "");

    assert_eq!(manifest.query.debounce_ms, None);
    assert_eq!(manifest.query.maximum_wait_ms, None);
    assert_eq!(manifest.query.leading_edge, None);
    assert_eq!(manifest.query.trailing_edge, None);
    assert_eq!(manifest.query.max_concurrent_requests, None);
    assert_eq!(manifest.query.network_backed, None);
    assert_eq!(manifest.activation.minimum_query_length, None);
    assert_eq!(manifest.activation.empty_query, None);
    assert!(manifest.activation.prefixes.is_empty());
    assert!(manifest.activation.keywords.is_empty());
    assert!(manifest.activation.categories.is_empty());
}

/// Spec 8.3 lets a modern plugin declare "no debounce". Written explicitly that
/// is a policy; omitted it is an inherited band. The two must not collapse into
/// the same parsed value, and an undebounced plugin has nothing to postpone, so
/// no maximum wait is synthesised for it.
#[test]
fn explicit_zero_debounce_differs_from_an_absent_debounce() {
    let declared = parse("native", "[query]\ndebounce-ms = 0\n");
    assert_eq!(declared.query.debounce_ms, Some(0));

    let resolved = declared.query_policy();
    assert_eq!(resolved.debounce_ms, 0);
    assert_eq!(
        resolved.maximum_wait_ms, None,
        "a zero debounce cannot postpone anything, so no maximum wait applies"
    );

    let inherited = parse("native", "");
    assert_eq!(inherited.query.debounce_ms, None);
    assert!(
        NATIVE_LOCAL_BAND.contains(&inherited.query_policy().debounce_ms),
        "an absent debounce must inherit the spec 25.4 native band"
    );
}

/// A maximum wait only has meaning when a debounce period can postpone work.
/// Accepting it with an explicit zero debounce would silently discard the
/// declaration at policy resolution.
#[test]
fn a_maximum_wait_requires_a_nonzero_debounce() {
    let error = reject("native", "[query]\ndebounce-ms = 0\nmaximum-wait-ms = 50\n");
    assert_invalid(&error, "query.maximum-wait-ms", PolicyProblem::Contradictory);
    let rendered = error.to_string();
    for expected in [
        "crikey.toml",
        "query.debounce-ms",
        "query.maximum-wait-ms",
        "cannot be combined",
        "dispatches immediately",
    ] {
        assert!(
            rendered.contains(expected),
            "contradiction must explain {expected:?}, got: {rendered}"
        );
    }
}

/// Builtin providers inherit an immediate (zero-debounce) dispatch policy.
/// A maximum wait must still be rejected when debounce is omitted; otherwise
/// the declaration is accepted and then discarded by `query_policy`.
#[test]
fn a_builtin_maximum_wait_without_debounce_is_rejected() {
    let error = reject("builtin", "[query]\nmaximum-wait-ms = 50\n");
    assert_invalid(&error, "query.maximum-wait-ms", PolicyProblem::Contradictory);
    assert!(
        error.to_string().contains("effective debounce period of 0"),
        "the error must explain the builtin default, got: {error}"
    );
}

/// `minimum-query-length = 0` is a deliberate "gate on nothing"; absence leaves
/// the field open for a future host default. Both currently resolve to zero, so
/// only the declared value distinguishes them.
#[test]
fn explicit_zero_minimum_query_length_is_still_a_declaration() {
    let declared = parse("native", "[activation]\nminimum-query-length = 0\n");
    assert_eq!(declared.activation.minimum_query_length, Some(0));
    assert_eq!(declared.query_policy().minimum_query_length, 0);

    let absent = parse("native", "[activation]\nprefixes = [\"x\"]\n");
    assert_eq!(absent.activation.minimum_query_length, None);
    assert_eq!(absent.query_policy().minimum_query_length, 0);
}

/// Spec 8.5 makes both edges default-on. Turning one off is an explicit
/// declaration and must survive resolution; leaving the other alone must not be
/// read as a refusal.
#[test]
fn explicit_false_edge_differs_from_an_absent_edge() {
    let manifest = parse("native", "[query]\ntrailing-edge = false\n");
    assert_eq!(manifest.query.trailing_edge, Some(false));
    assert_eq!(manifest.query.leading_edge, None);

    let policy = manifest.query_policy();
    assert!(!policy.trailing_edge, "an explicit refusal must be honoured");
    assert!(policy.leading_edge, "silence keeps the spec 8.5 default");
}

/// Spec 8.9: modern plugins declare empty-query support explicitly, so silence
/// means unsupported. An explicit declaration outranks the prefix gate, since
/// an empty query cannot carry a prefix.
#[test]
fn empty_query_support_must_be_declared_explicitly() {
    let silent = parse("native", "");
    assert_eq!(silent.activation.empty_query, None);
    assert!(!silent.query_policy().empty_query);
    assert!(!silent.query_policy().admits(""));

    let declared = parse(
        "native",
        "[activation]\nempty-query = true\nprefixes = [\"go\"]\n",
    );
    assert_eq!(declared.activation.empty_query, Some(true));
    let policy = declared.query_policy();
    assert!(policy.empty_query);
    assert!(policy.admits(""), "declared empty-query support gates in");
    assert!(!policy.admits("hello"), "prefix gating still applies");
}

// ---------------------------------------------------------------------------
// Modern defaults derived from the runtime (spec 25.4)
// ---------------------------------------------------------------------------

/// A silent native plugin gets the spec 8.5 default shape: both edges, a
/// bounded maximum wait so continuous typing cannot postpone indefinitely, and
/// one in-flight request.
#[test]
fn silent_native_plugin_inherits_the_default_modern_shape() {
    let policy = resolved_policy("native", "");

    assert_eq!(policy.profile, SchedulingProfile::Modern);
    assert!(NATIVE_LOCAL_BAND.contains(&policy.debounce_ms));
    assert!(policy.leading_edge);
    assert!(policy.trailing_edge);
    assert_eq!(policy.minimum_query_length, 0);
    assert_eq!(policy.max_concurrent_requests, 1);

    let maximum_wait = policy
        .maximum_wait_ms
        .expect("spec 8.5 requires a default maximum wait");
    assert!(
        maximum_wait >= policy.debounce_ms,
        "a maximum wait below the debounce period would never be reachable"
    );
    assert!(
        maximum_wait <= MAX_DEBOUNCE_MS,
        "the default must respect the same bound declarations do"
    );
}

/// Spec 25.4 puts modern Python in a slower band than native code.
#[test]
fn modern_python_inherits_the_python_band() {
    let policy = resolved_policy("python", "");
    assert_eq!(policy.profile, SchedulingProfile::Modern);
    assert!(
        PYTHON_LOCAL_BAND.contains(&policy.debounce_ms),
        "python default {} outside the spec 25.4 band",
        policy.debounce_ms
    );
}

/// Spec 8.2 and 25.4: in-process catalog lookups are local and cheap, so a
/// builtin provider is never debounced.
#[test]
fn builtin_providers_are_never_debounced() {
    let policy = resolved_policy("builtin", "");
    assert_eq!(policy.debounce_ms, 0);
    assert_eq!(policy.maximum_wait_ms, None);
}

/// Spec 8.3/25.4: network-backed plugins default slower. Declaring the network
/// permission without contradicting it is enough to infer the status, which is
/// what keeps a manifest from having to say the same thing twice.
#[test]
fn network_permission_infers_the_network_backed_band() {
    let manifest = parse("native", "[permissions]\nnetwork = true\n");
    assert_eq!(
        manifest.query.network_backed, None,
        "inference must not fabricate a declaration"
    );

    let policy = manifest.query_policy();
    assert!(policy.network_backed);
    assert!(
        NETWORK_BACKED_BAND.contains(&policy.debounce_ms),
        "network default {} outside the spec 25.4 band",
        policy.debounce_ms
    );
    assert!(
        policy.debounce_ms > resolved_policy("native", "").debounce_ms,
        "network-backed must be slower than a local plugin"
    );
}

/// A plugin that holds the network permission but answers from a local cache
/// can opt out. The explicit `false` must beat the inference, which is only
/// observable because absence and `false` stay distinguishable.
#[test]
fn explicit_network_backed_false_overrides_the_permission_inference() {
    let manifest = parse(
        "native",
        "[query]\nnetwork-backed = false\n\n[permissions]\nnetwork = true\n",
    );
    assert_eq!(manifest.query.network_backed, Some(false));

    let policy = manifest.query_policy();
    assert!(!policy.network_backed);
    assert!(
        NATIVE_LOCAL_BAND.contains(&policy.debounce_ms),
        "an opted-out plugin keeps the local band"
    );
}

/// A declared debounce is a declaration, not a hint: it wins over the
/// runtime-derived band in both directions.
#[test]
fn declared_debounce_overrides_the_inherited_band() {
    let policy = resolved_policy(
        "python",
        "[query]\ndebounce-ms = 250\nmaximum-wait-ms = 400\n\n[permissions]\nnetwork = true\n",
    );
    assert_eq!(policy.debounce_ms, 250);
    assert_eq!(policy.maximum_wait_ms, Some(400));
}

// ---------------------------------------------------------------------------
// Legacy profiles (spec 7.1, 7.2, 8.4, 8.10, 8.11)
// ---------------------------------------------------------------------------

/// The `legacy-python` runtime is `legacy-strict` unless the author opts out,
/// and `legacy-strict` is the profile spec 7.1 pins to zero host scheduling.
#[test]
fn legacy_python_defaults_to_legacy_strict_with_no_host_scheduling() {
    let manifest = parse("legacy-python", "");
    assert_eq!(manifest.scheduling_profile(), SchedulingProfile::LegacyStrict);

    let policy = manifest.query_policy();
    assert_eq!(policy.debounce_ms, 0, "spec 8.4: no time debounce");
    assert_eq!(policy.maximum_wait_ms, None);
    assert_eq!(policy.minimum_query_length, 0, "spec 8.10");
    assert_eq!(
        policy.max_concurrent_requests, 1,
        "spec 7.1: callbacks are serial"
    );
    assert!(policy.prefixes.is_empty(), "spec 8.11: no host gating");
    assert!(policy.keywords.is_empty());
    assert!(
        policy.empty_query,
        "spec 8.9: legacy plugins receive empty-query callbacks"
    );
    assert!(
        policy.admits(""),
        "spec 7.1: initial requests broadcast to every loaded legacy plugin"
    );
    assert!(policy.admits("anything at all"));
}

/// A repackaged legacy plugin may carry modern fields by accident or by
/// copy-paste. Spec 7.1 and ADR 0006 forbid applying them, and the manifest
/// must still round-trip the author's text so tooling can explain the drop
/// rather than silently changing behaviour.
#[test]
fn legacy_strict_drops_declared_modern_gating_and_reports_it() {
    let manifest = parse(
        "legacy-python",
        "[activation]\nminimum-query-length = 3\nprefixes = [\"kp\"]\n\n\
         [query]\ndebounce-ms = 120\nmaximum-wait-ms = 400\nmax-concurrent-requests = 4\n",
    );

    assert_eq!(manifest.query.debounce_ms, Some(120));
    assert_eq!(manifest.activation.minimum_query_length, Some(3));

    let policy = manifest.query_policy();
    assert_eq!(policy.profile, SchedulingProfile::LegacyStrict);
    assert_eq!(policy.debounce_ms, 0);
    assert_eq!(policy.maximum_wait_ms, None);
    assert_eq!(policy.minimum_query_length, 0);
    assert_eq!(policy.max_concurrent_requests, 1);
    assert!(policy.prefixes.is_empty());
    assert!(policy.admits("k"), "a one-character query still reaches it");

    let mut ignored = manifest.ignored_modern_fields();
    ignored.sort_unstable();
    assert_eq!(
        ignored,
        [
            "activation.minimum-query-length",
            "activation.prefixes",
            "query.debounce-ms",
            "query.max-concurrent-requests",
            "query.maximum-wait-ms",
        ],
        "every dropped field must be nameable in a diagnostic"
    );
}

/// Spec 7.2: `legacy-optimized` is the opt-in that makes host scheduling legal
/// for a legacy plugin. Once chosen, the declared fields apply and nothing is
/// dropped.
#[test]
fn legacy_optimized_opt_in_honours_declared_scheduling() {
    let manifest = parse(
        "legacy-python",
        "[activation]\nminimum-query-length = 3\nprefixes = [\"kp\"]\n\n\
         [query]\ndebounce-ms = 120\nmaximum-wait-ms = 400\n",
    );
    // Sanity: the same body is neutralised without the opt-in.
    assert_eq!(manifest.query_policy().debounce_ms, 0);

    let opted_in = Manifest::parse(&manifest_text(
        "legacy-python",
        "scheduling-profile = \"legacy-optimized\"\n\n\
         [activation]\nminimum-query-length = 3\nprefixes = [\"kp\"]\n\n\
         [query]\ndebounce-ms = 120\nmaximum-wait-ms = 400\n",
    ))
    .expect("the opt-in profile must parse");

    assert_eq!(opted_in.scheduling_profile(), SchedulingProfile::LegacyOptimized);
    let policy = opted_in.query_policy();
    assert_eq!(policy.debounce_ms, 120);
    assert_eq!(policy.maximum_wait_ms, Some(400));
    assert_eq!(policy.minimum_query_length, 3);
    assert!(!policy.admits("k"), "opted-in gating now applies");
    assert!(policy.admits("kp foo"));
    assert!(opted_in.ignored_modern_fields().is_empty());
}

/// Spec 7.3 scopes `modern` to modern runtimes. A legacy runtime claiming it
/// would smuggle in cancellation tokens and streaming the legacy worker cannot
/// provide, so the contradiction is refused rather than downgraded.
#[test]
fn a_legacy_runtime_cannot_declare_the_modern_profile() {
    let error = reject(
        "legacy-python",
        "scheduling-profile = \"modern\"\n[query]\ndebounce-ms = 50\n",
    );
    assert_invalid(&error, "plugin.scheduling-profile", PolicyProblem::Contradictory);
}

/// The reverse contradiction: obsolete-work replacement is defined against the
/// legacy lifecycle, so a native plugin cannot ask for a legacy profile.
#[test]
fn a_modern_runtime_cannot_declare_a_legacy_profile() {
    let error = reject("native", "scheduling-profile = \"legacy-strict\"\n");
    assert_invalid(&error, "plugin.scheduling-profile", PolicyProblem::Contradictory);
}

// ---------------------------------------------------------------------------
// Contradictory, out-of-range and malformed declarations
// ---------------------------------------------------------------------------

/// Spec 8.6 defines the maximum wait as a bound *on* the debounce period. A
/// value below it would fire before the ordinary period could ever elapse,
/// which is a policy the author cannot have meant.
#[test]
fn a_maximum_wait_below_the_debounce_period_is_contradictory() {
    for body in [
        "[query]\ndebounce-ms = 100\nmaximum-wait-ms = 40\n",
        "[query]\ndebounce-ms = 50\nmaximum-wait-ms = 0\n",
    ] {
        let error = reject("native", body);
        assert_invalid(&error, "query.maximum-wait-ms", PolicyProblem::Contradictory);
    }
}

/// A maximum wait equal to the debounce period is degenerate but coherent:
/// every burst flushes at exactly one period. It must not be swept up by the
/// rejection above.
#[test]
fn a_maximum_wait_equal_to_the_debounce_period_is_accepted() {
    let policy = resolved_policy("native", "[query]\ndebounce-ms = 50\nmaximum-wait-ms = 50\n");
    assert_eq!(policy.debounce_ms, 50);
    assert_eq!(policy.maximum_wait_ms, Some(50));
}

/// With both edges refused there is no moment at which the query could ever be
/// dispatched; the plugin would be permanently mute (spec 8.5).
#[test]
fn refusing_both_edges_is_contradictory() {
    let error = reject("native", "[query]\nleading-edge = false\ntrailing-edge = false\n");
    match &error {
        ManifestError::InvalidQueryPolicy { field, problem, .. } => {
            assert_eq!(*problem, PolicyProblem::Contradictory);
            assert!(
                matches!(*field, "query.leading-edge" | "query.trailing-edge"),
                "unexpected field {field}"
            );
        }
        other => panic!("expected InvalidQueryPolicy, got {other:?}"),
    }
}

/// Trailing-only is legal: the plugin simply waits for a pause (spec 8.5).
#[test]
fn refusing_only_the_leading_edge_is_accepted() {
    let policy = resolved_policy("native", "[query]\nleading-edge = false\n");
    assert!(!policy.leading_edge);
    assert!(policy.trailing_edge);
}

/// Spec 8.9 versus 8.10: a plugin cannot both support empty queries and demand
/// a non-empty one. Left unresolved this would silently make `empty-query` a
/// no-op.
#[test]
fn empty_query_support_with_a_minimum_length_is_contradictory() {
    let error = reject(
        "native",
        "[activation]\nempty-query = true\nminimum-query-length = 2\n",
    );
    assert_invalid(
        &error,
        "activation.minimum-query-length",
        PolicyProblem::Contradictory,
    );
}

/// Spec 20: a plugin without the network permission cannot be network-backed,
/// and inflating its debounce band on the strength of a false claim would slow
/// the user down for nothing.
#[test]
fn network_backed_without_the_network_permission_is_refused() {
    let error = reject(
        "native",
        "[query]\nnetwork-backed = true\n\n[permissions]\nnetwork = false\n",
    );
    assert_invalid(&error, "query.network-backed", PolicyProblem::NotPermitted);
}

/// Spec 12.4: every bound is explicit. A debounce past the ceiling is a typo
/// (`5000` for `500`) or an attempt to stall the pipeline; either way the
/// author, not the host, must fix it.
#[test]
fn a_debounce_beyond_the_ceiling_is_out_of_range() {
    for value in [MAX_DEBOUNCE_MS + 1, u64::MAX] {
        let error = reject("native", &format!("[query]\ndebounce-ms = {value}\n"));
        assert_invalid(&error, "query.debounce-ms", PolicyProblem::OutOfRange);
    }
}

/// The ceiling itself is inside the accepted range; an off-by-one here would
/// make the constant a lie.
#[test]
fn a_debounce_at_the_ceiling_is_accepted() {
    let policy = resolved_policy(
        "native",
        &format!("[query]\ndebounce-ms = {MAX_DEBOUNCE_MS}\nmaximum-wait-ms = {MAX_DEBOUNCE_MS}\n"),
    );
    assert_eq!(policy.debounce_ms, MAX_DEBOUNCE_MS);
}

/// The maximum wait shares the ceiling; otherwise an unbounded wait could
/// postpone a dispatch past any useful horizon (spec 8.6).
#[test]
fn a_maximum_wait_beyond_the_ceiling_is_out_of_range() {
    let error = reject(
        "native",
        &format!(
            "[query]\ndebounce-ms = 50\nmaximum-wait-ms = {}\n",
            MAX_DEBOUNCE_MS + 1
        ),
    );
    assert_invalid(&error, "query.maximum-wait-ms", PolicyProblem::OutOfRange);
}

/// A legacy-strict host ignores modern scheduling semantics, but it must not
/// accept malformed numeric declarations that could hide a typo in a manifest.
#[test]
fn legacy_strict_still_rejects_out_of_range_numeric_declarations() {
    for (body, field) in [
        (
            &format!("[query]\ndebounce-ms = {}\n", MAX_DEBOUNCE_MS + 1),
            "query.debounce-ms",
        ),
        (
            &format!("[query]\nmaximum-wait-ms = {}\n", MAX_DEBOUNCE_MS + 1),
            "query.maximum-wait-ms",
        ),
        (
            &format!(
                "[query]\nmax-concurrent-requests = {}\n",
                MAX_CONCURRENT_REQUESTS + 1
            ),
            "query.max-concurrent-requests",
        ),
        (
            &format!(
                "[activation]\nminimum-query-length = {}\n",
                MAX_MINIMUM_QUERY_LENGTH + 1
            ),
            "activation.minimum-query-length",
        ),
    ] {
        let error = reject("legacy-python", body);
        assert_invalid(&error, field, PolicyProblem::OutOfRange);
    }
}

/// Zero concurrent requests means the plugin can never be asked anything,
/// which is a disabled plugin expressed as a scheduling budget.
#[test]
fn zero_concurrent_requests_is_out_of_range() {
    let error = reject("native", "[query]\nmax-concurrent-requests = 0\n");
    assert_invalid(&error, "query.max-concurrent-requests", PolicyProblem::OutOfRange);
}

/// Spec 8.12 and 12.4: the request budget is what stops one plugin
/// monopolising IPC capacity, so it needs its own ceiling.
#[test]
fn a_request_budget_beyond_the_cap_is_out_of_range() {
    let error = reject(
        "native",
        &format!(
            "[query]\nmax-concurrent-requests = {}\n",
            MAX_CONCURRENT_REQUESTS + 1
        ),
    );
    assert_invalid(&error, "query.max-concurrent-requests", PolicyProblem::OutOfRange);
}

/// Spec 7.3 allows declared concurrency; a value inside the cap resolves as
/// written rather than being clamped to the serial default.
#[test]
fn a_declared_request_budget_inside_the_cap_is_honoured() {
    let policy = resolved_policy("native", "[query]\nmax-concurrent-requests = 4\n");
    assert_eq!(policy.max_concurrent_requests, 4);

    let at_cap = resolved_policy(
        "native",
        &format!("[query]\nmax-concurrent-requests = {MAX_CONCURRENT_REQUESTS}\n"),
    );
    assert_eq!(at_cap.max_concurrent_requests, MAX_CONCURRENT_REQUESTS);
}

/// A minimum length longer than any plausible query gates the plugin out of
/// existence; spec 8.10's declaration is meant to be a filter, not a mute.
#[test]
fn a_minimum_query_length_beyond_the_cap_is_out_of_range() {
    let error = reject(
        "native",
        &format!(
            "[activation]\nminimum-query-length = {}\n",
            MAX_MINIMUM_QUERY_LENGTH + 1
        ),
    );
    assert_invalid(
        &error,
        "activation.minimum-query-length",
        PolicyProblem::OutOfRange,
    );
}

/// Values outside the field's integer type are a deserialisation failure, not a
/// range decision: there is no `u64` to range-check.
#[test]
fn integer_overflow_in_a_scheduling_field_is_a_parse_error() {
    for body in [
        // u64::MAX + 1
        "[query]\ndebounce-ms = 18446744073709551616\n",
        // u32::MAX + 1
        "[query]\nmax-concurrent-requests = 4294967296\n",
    ] {
        let error = reject("native", body);
        assert!(
            matches!(error, ManifestError::Parse(_)),
            "expected a parse error, got {error:?}"
        );
    }
}

/// The overflow normalizer must not turn malformed TOML into a valid value.
/// Decimal integers with a leading zero are invalid TOML even when their
/// numeric value is inside the declared `u64` domain.
#[test]
fn an_oversized_integer_with_a_leading_zero_is_still_a_parse_error() {
    let error = reject("native", "[query]\ndebounce-ms = 09223372036854775808\n");
    assert!(
        matches!(error, ManifestError::Parse(_)),
        "a malformed oversized integer must not be normalized into a value: {error:?}"
    );
}

/// Durations and budgets are unsigned counts. Negative and fractional values
/// must fail at the type level rather than being truncated into something the
/// author never wrote.
#[test]
fn negative_fractional_and_quoted_scheduling_values_are_parse_errors() {
    for body in [
        "[query]\ndebounce-ms = -1\n",
        "[query]\ndebounce-ms = 50.5\n",
        "[query]\ndebounce-ms = \"50\"\n",
        "[query]\nleading-edge = \"true\"\n",
        "[activation]\nminimum-query-length = -2\n",
        "[activation]\nprefixes = \"repo\"\n",
    ] {
        let error = reject("native", body);
        assert!(
            matches!(error, ManifestError::Parse(_)),
            "expected a parse error for {body:?}, got {error:?}"
        );
    }
}

/// A misspelled scheduling key would otherwise resolve to the inherited
/// default and quietly ignore what the author asked for.
#[test]
fn a_misspelled_scheduling_key_is_rejected() {
    for body in [
        "[query]\ndebounce-msec = 50\n",
        "[query]\nmaximum_wait_ms = 200\n",
        "[activation]\nminimum-query-len = 2\n",
    ] {
        let error = reject("native", body);
        assert!(
            matches!(error, ManifestError::Parse(_)),
            "expected a parse error for {body:?}, got {error:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Declared activation gates (spec 8.11)
// ---------------------------------------------------------------------------

/// Prefix gating matches leading text and ignores case; a prefix is how a user
/// addresses a plugin, not a case-sensitive token.
#[test]
fn prefixes_gate_on_leading_text_case_insensitively() {
    let policy = resolved_policy("native", "[activation]\nprefixes = [\"repo\"]\n");

    assert!(policy.admits("repo crikey"));
    assert!(policy.admits("REPO crikey"));
    assert!(policy.admits("repo"));
    assert!(!policy.admits("my repo"), "a prefix must lead the query");
    assert!(!policy.admits("rep"));
}

/// Keyword gating matches a whole leading token, which is what separates
/// `gh issues` from an unrelated query that merely starts with the letters.
#[test]
fn keywords_gate_on_the_first_whole_token() {
    let policy = resolved_policy("native", "[activation]\nkeywords = [\"gh\"]\n");

    assert!(policy.admits("gh issues"));
    assert!(policy.admits("GH issues"));
    assert!(!policy.admits("ghost"), "a keyword is a token, not a prefix");
    assert!(!policy.admits("open gh"));
}

/// Gates are alternatives, not a conjunction: satisfying either declared list
/// admits the query.
#[test]
fn prefixes_and_keywords_are_alternatives() {
    let policy = resolved_policy(
        "native",
        "[activation]\nprefixes = [\"repo\"]\nkeywords = [\"gh\"]\n",
    );

    assert!(policy.admits("repository search"), "prefix match");
    assert!(policy.admits("gh issues"), "keyword match");
    assert!(!policy.admits("unrelated"));
}

/// Spec 8.11 gating is opt-in. A plugin that declares no gate is asked about
/// every query long enough to matter, and categories alone are metadata rather
/// than a query filter.
#[test]
fn a_plugin_without_declared_gates_sees_every_long_enough_query() {
    let policy = resolved_policy(
        "native",
        "[activation]\nminimum-query-length = 2\ncategories = [\"developer\"]\n",
    );

    assert_eq!(policy.categories, ["developer"]);
    assert!(policy.admits("ab"));
    assert!(policy.admits("anything at all"));
    assert!(!policy.admits("a"));
    assert!(!policy.admits(""));
}

// ---------------------------------------------------------------------------
// Manifest shape and permission strictness
// ---------------------------------------------------------------------------

/// A missing required top-level field is a load error that identifies both the
/// manifest dialect's filename and the missing field.
#[test]
fn a_missing_manifest_version_names_the_field_and_manifest_file() {
    let error = Manifest::parse(
        "[plugin]\n\
         id = \"dev.example.missing-version\"\n\
         name = \"Missing Version\"\n\
         version = \"1.0.0\"\n\
         runtime = \"native\"\n\
         entrypoint = \"bin/plugin\"\n",
    )
    .expect_err("manifest-version is required");
    let rendered = error.to_string();
    assert!(
        rendered.contains("crikey.toml"),
        "the error must name the manifest file, got: {rendered}"
    );
    assert!(
        rendered.contains("manifest-version"),
        "the error must name the missing field, got: {rendered}"
    );
}

/// TOML syntax alone does not establish the manifest shape. A scalar
/// `plugin` value must fail deserialization instead of being accepted as an
/// empty/default section.
#[test]
fn a_valid_toml_manifest_with_the_wrong_plugin_shape_is_rejected() {
    let error = Manifest::parse("manifest-version = 1\nplugin = \"not-a-table\"\n")
        .expect_err("a scalar plugin value is not a manifest section");
    assert!(
        matches!(error, ManifestError::Parse(_)),
        "wrong-shape TOML must be a parse error, got {error:?}"
    );
    assert!(
        error.to_string().contains("plugin"),
        "the wrong-shape error must identify plugin, got: {error}"
    );
}

/// TOML rejects a repeated key rather than choosing whichever occurrence was
/// seen last. This matters for manifests because silently choosing one value
/// would make a scheduling policy depend on file ordering.
#[test]
fn a_duplicate_manifest_key_is_rejected() {
    let error = Manifest::parse(&manifest_text(
        "native",
        "[query]\ndebounce-ms = 40\ndebounce-ms = 60\n",
    ))
    .expect_err("duplicate query keys must be rejected");
    assert!(
        matches!(error, ManifestError::Parse(_)),
        "duplicate keys must be parse errors, got {error:?}"
    );
    assert!(
        error.to_string().contains("duplicate"),
        "the duplicate-key error must explain the rejection, got: {error}"
    );
}

/// An empty platform-specific path must not shadow a valid runtime-neutral
/// fallback. Returning an empty path would defer a malformed manifest to a
/// later process-start failure.
#[test]
fn an_empty_entrypoint_falls_back_to_a_nonempty_any_entrypoint() {
    let manifest = Manifest::parse(
        "manifest-version = 1\n\
         \n[plugin]\n\
         id = \"dev.example.entrypoint\"\n\
         name = \"Entrypoint Fixture\"\n\
         version = \"1.0.0\"\n\
         runtime = \"native\"\n\
         entrypoint = { \"linux-x86_64\" = \"\", any = \"bin/plugin\" }\n",
    )
    .expect("manifest must parse");

    assert_eq!(
        manifest
            .entrypoint_for("linux", "x86_64")
            .expect("nonempty any entrypoint is usable"),
        "bin/plugin"
    );
}

/// Permission enums are closed: a value unknown to the loader must not be
/// accepted and then treated as a weaker or stronger known permission.
#[test]
fn an_unknown_permission_value_is_rejected() {
    let error = reject("native", "[permissions]\nclipboard = \"execute\"\n");
    assert!(
        matches!(error, ManifestError::Parse(_)),
        "an unknown permission must be a parse error, got {error:?}"
    );
    assert!(
        error.to_string().contains("execute"),
        "the error must identify the unknown permission value, got: {error}"
    );
}

/// Every omitted permission is restrictive at the declaration layer. Runtime
/// hosts still have to enforce these values before privileged operations.
#[test]
fn omitted_permissions_default_to_restrictive_values() {
    let permissions = parse("native", "").permissions;
    assert!(permissions.filesystem.is_empty());
    assert!(!permissions.network);
    assert!(!permissions.network_listener);
    assert_eq!(
        permissions.clipboard,
        crikey_plugin_model::ClipboardPermission::None
    );
    assert!(!permissions.process);
    assert!(!permissions.window_enumeration);
    assert!(!permissions.window_control);
    assert!(!permissions.notifications);
    assert!(!permissions.secrets);
    assert!(!permissions.environment);
    assert!(!permissions.native_library_loading);
    assert!(!permissions.background_execution);
}
