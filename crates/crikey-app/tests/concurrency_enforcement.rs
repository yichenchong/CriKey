//! `[concurrency]` enforcement at the production dispatch seam (spec 13.5).
//!
//! The unit tests in `crikey-plugin-supervisor` prove `ConcurrencyBudget`
//! counts correctly. They cannot prove the launcher ever asks it anything.
//! These tests therefore drive the real [`QueryPipeline`] — the single place
//! every provider (native, modern, legacy) obtains a unit of suggestion work —
//! and assert on what the pipeline hands back, never on the budget in
//! isolation.
//!
//! # What is enforced today
//!
//! Suggestion requests are exercised below at the live `QueryPipeline::tick`
//! seam. Action, background and catalog dispatches use the same shared handle
//! at their provider/runtime seams; their focused lifecycle tests live beside
//! those providers. Each test here stays focused on suggestion admission and
//! retirement.
//!
//! # Why the refusal must be observable
//!
//! An operator seeing a plugin answer nothing cannot distinguish "throttled"
//! from "broken". Every test below asserts the refusal reached
//! `QueryPipeline::health(...).concurrency_refusals`, not merely that the
//! request vanished.

use std::sync::Arc;

use crikey_app::{PipelineConfig, QueryPipeline};
use crikey_core::PluginId;
use crikey_input_scheduler::Millis;
use crikey_plugin_model::Manifest;
use crikey_plugin_supervisor::BudgetKind;

/// Declares room for two simultaneous suggestion requests in the scheduler but
/// only one in `[concurrency]`. The two knobs disagree on purpose: the tighter
/// of the two is the one that must hold.
const CAPPED_MANIFEST: &str = r#"
manifest-version = 1

[plugin]
id = "dev.crikey.capped"
name = "Capped"
version = "1.0.0"
runtime = "native"
entrypoint = "capped"

[query]
debounce-ms = 0
leading-edge = true
trailing-edge = true
max-concurrent-requests = 2

[concurrency]
max-suggestion-requests = 1
"#;

/// Identical scheduling, no `[concurrency]` section. Its suggestion limit is
/// the two it asked for in `[query]`, so it is the control that shows the
/// refusal is targeted at one plugin rather than a global stall.
const SIBLING_MANIFEST: &str = r#"
manifest-version = 1

[plugin]
id = "dev.crikey.sibling"
name = "Sibling"
version = "1.0.0"
runtime = "native"
entrypoint = "sibling"

[query]
debounce-ms = 0
leading-edge = true
trailing-edge = true
max-concurrent-requests = 2
"#;

/// `0` is a declaration, not an omission: this plugin has asked never to be
/// given a suggestion request.
const REFUSING_MANIFEST: &str = r#"
manifest-version = 1

[plugin]
id = "dev.crikey.refusing"
name = "Refusing"
version = "1.0.0"
runtime = "native"
entrypoint = "refusing"

[query]
debounce-ms = 0
leading-edge = true
trailing-edge = true
max-concurrent-requests = 2

[concurrency]
max-suggestion-requests = 0
"#;

/// The query policy ceiling remains one when its declaration is omitted, even
/// if the separate suggestion budget asks for more.
const CONCURRENCY_ONLY_MANIFEST: &str = r#"
manifest-version = 1

[plugin]
id = "dev.crikey.concurrency-only"
name = "Concurrency only"
version = "1.0.0"
runtime = "native"
entrypoint = "concurrency-only"

[query]
debounce-ms = 0
leading-edge = true
trailing-edge = true

[concurrency]
max-suggestion-requests = 8
"#;

const WASM_MANIFEST: &str = r#"
manifest-version = 1

[plugin]
id = "dev.crikey.wasm"
name = "Wasm"
version = "1.0.0"
runtime = "wasm"
entrypoint = "plugin.wasm"
"#;

/// A `c-abi` package. Its entrypoint is a shared library because the
/// executable CriKey supervises is `crikey-cabi-host`, not the plugin
/// (ADR-0015). `concurrency.max-suggestion-requests` is deliberately above one
/// so the honest-gap report can be asserted.
const CABI_MANIFEST: &str = r#"
manifest-version = 1

[plugin]
id = "dev.crikey.cabi"
name = "C ABI"
version = "1.0.0"
runtime = "c-abi"
entrypoint = "bin/libplugin.so"

[permissions]
native-library-loading = true

[concurrency]
max-suggestion-requests = 4
"#;

fn register(pipeline: &mut QueryPipeline, text: &str) -> PluginId {
    let manifest = Manifest::parse(text).expect("fixture manifest must parse and validate");
    pipeline
        .register_manifest(&manifest)
        .expect("fixture plugins register once")
}

/// Plugins dispatched by `tick` at `now`, in dispatch order.
fn dispatched(pipeline: &mut QueryPipeline, now: Millis) -> Vec<PluginId> {
    pipeline
        .tick(now)
        .dispatches
        .into_iter()
        .map(|dispatch| dispatch.plugin)
        .collect()
}
#[test]
fn omitted_query_maximum_still_caps_an_explicit_suggestion_budget() {
    let manifest = Manifest::parse(CONCURRENCY_ONLY_MANIFEST).expect("fixture manifest parses");
    let plugin = PluginId(manifest.plugin.id.clone());
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    pipeline
        .register_namespaced_manifest(plugin.clone(), &manifest)
        .expect("fixture plugin registers");

    assert_eq!(
        pipeline
            .plugin_budget(&plugin)
            .expect("registered plugin has a budget")
            .limit(BudgetKind::Suggestion),
        1,
        "an omitted query maximum resolves to the conservative one-request ceiling"
    );
}

fn refusals(pipeline: &mut QueryPipeline, plugin: &PluginId) -> u64 {
    pipeline.health(plugin).concurrency_refusals.total()
}

/// The provider and pipeline must observe one occupancy counter rather than
/// independently admitting the same plugin. Holding a provider clone before
/// `tick` forces the query path to refuse and records that refusal.
#[test]
fn provider_and_pipeline_budget_clones_share_suggestion_admission() {
    let manifest = Manifest::parse(CAPPED_MANIFEST).expect("fixture manifest parses");
    let plugin = PluginId(manifest.plugin.id.clone());
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let provider_handle = pipeline
        .register_namespaced_manifest(plugin.clone(), &manifest)
        .expect("fixture plugin registers once");
    let pipeline_handle = pipeline
        .plugin_budget(&plugin)
        .expect("registered plugin has a budget")
        .clone();

    assert!(Arc::ptr_eq(&provider_handle, &pipeline_handle));
    let held = provider_handle
        .try_acquire_owned(BudgetKind::Suggestion)
        .expect("the provider clone admits the first unit");
    assert_eq!(
        pipeline_handle.in_flight(BudgetKind::Suggestion),
        1,
        "the pipeline clone must observe provider occupancy"
    );

    pipeline.keystroke("a", 0);
    assert!(
        dispatched(&mut pipeline, 0).is_empty(),
        "the occupied shared slot must refuse the suggestion"
    );
    assert_eq!(refusals(&mut pipeline, &plugin), 1);
    drop(held);
    assert_eq!(pipeline_handle.in_flight(BudgetKind::Suggestion), 0);
}

/// A failed runtime start must remove the query registration entirely, so a
/// later load of the same plugin id does not inherit stale scheduler or health
/// state.
#[test]
fn unregister_plugin_drops_registration_and_allows_clean_reload() {
    let manifest = Manifest::parse(CAPPED_MANIFEST).expect("fixture manifest parses");
    let plugin = PluginId(manifest.plugin.id.clone());
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let handle = pipeline
        .register_namespaced_manifest(plugin.clone(), &manifest)
        .expect("fixture plugin registers once");
    let held = handle
        .try_acquire_owned(BudgetKind::Suggestion)
        .expect("the provider clone admits a unit");

    assert!(pipeline.unregister_plugin(&plugin));
    assert!(pipeline.plugin_budget(&plugin).is_none());
    assert!(pipeline.plugin_diagnostics(&plugin).is_none());
    assert_eq!(pipeline.diagnostics().in_flight_requests, 0);
    drop(held);
    pipeline.keystroke("after unregister", 0);
    let _ = pipeline.tick(0);

    let replacement = pipeline
        .register_namespaced_manifest(plugin.clone(), &manifest)
        .expect("the removed plugin can be registered cleanly");
    assert!(!Arc::ptr_eq(&handle, &replacement));
    assert_eq!(pipeline.health(&plugin).concurrency_refusals.total(), 0);
}

/// The blocking finding: a declared limit must bind live dispatch.
///
/// Both plugins are given a second query while their first is still running.
/// The scheduler is willing to dispatch both — its own limit is two — so the
/// only thing that can hold the capped plugin to one is the `[concurrency]`
/// budget being consulted at the seam.
#[test]
fn a_plugin_at_its_declared_suggestion_limit_is_refused_while_its_sibling_is_not() {
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let capped = register(&mut pipeline, CAPPED_MANIFEST);
    let sibling = register(&mut pipeline, SIBLING_MANIFEST);

    pipeline.keystroke("a", 0);
    let first = dispatched(&mut pipeline, 0);
    assert!(
        first.contains(&capped) && first.contains(&sibling),
        "both plugins must receive their first request: {first:?}"
    );

    // Neither plugin is completed: both units of work are still in flight when
    // the next query arrives.
    pipeline.keystroke("ab", 10);
    let second = dispatched(&mut pipeline, 10);

    assert!(
        !second.contains(&capped),
        "a plugin at max-suggestion-requests = 1 must not receive a second concurrent request: {second:?}"
    );
    assert!(
        second.contains(&sibling),
        "the sibling declared room for two and must be unaffected: {second:?}"
    );

    assert_eq!(
        refusals(&mut pipeline, &capped),
        1,
        "the refusal must be observable in per-plugin diagnostics, not a silent drop"
    );
    assert_eq!(
        refusals(&mut pipeline, &sibling),
        0,
        "an admitted request is not a refusal"
    );

    let capped_budget = pipeline
        .plugin_budget(&capped)
        .expect("registered plugins own a budget");
    assert_eq!(capped_budget.limit(BudgetKind::Suggestion), 1);
    assert_eq!(
        capped_budget.in_flight(BudgetKind::Suggestion),
        1,
        "the refused request must not have consumed a slot"
    );
    assert_eq!(capped_budget.refusals(BudgetKind::Suggestion), 1);

    let sibling_budget = pipeline
        .plugin_budget(&sibling)
        .expect("registered plugins own a budget");
    assert_eq!(sibling_budget.limit(BudgetKind::Suggestion), 2);
    assert_eq!(sibling_budget.in_flight(BudgetKind::Suggestion), 2);
    assert_eq!(sibling_budget.refusals(BudgetKind::Suggestion), 0);
}

/// A budget that never gave slots back would turn a one-shot throttle into a
/// permanent outage, which is indistinguishable from the bug this fixes.
#[test]
fn retiring_the_running_request_returns_the_slot_to_the_plugin() {
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let capped = register(&mut pipeline, CAPPED_MANIFEST);

    let first_generation = pipeline.keystroke("a", 0);
    assert_eq!(dispatched(&mut pipeline, 0), vec![capped.clone()]);

    pipeline.keystroke("ab", 10);
    assert!(
        dispatched(&mut pipeline, 10).is_empty(),
        "the plugin is at its limit while the first request runs"
    );
    assert_eq!(refusals(&mut pipeline, &capped), 1);

    pipeline.complete(&capped, first_generation, 20);
    assert_eq!(
        pipeline
            .plugin_budget(&capped)
            .expect("registered plugins own a budget")
            .in_flight(BudgetKind::Suggestion),
        0,
        "completion must release the admitted slot"
    );

    pipeline.keystroke("abc", 30);
    assert_eq!(
        dispatched(&mut pipeline, 30),
        vec![capped.clone()],
        "the freed slot must admit the next request"
    );
    assert_eq!(
        refusals(&mut pipeline, &capped),
        1,
        "releasing a slot must not erase the refusal history"
    );
}

/// `Some(0)` was the value the audit named specifically: it parsed, it was
/// stored, and production ignored it.
#[test]
fn a_declared_limit_of_zero_admits_no_suggestion_request_at_all() {
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let refusing = register(&mut pipeline, REFUSING_MANIFEST);
    let sibling = register(&mut pipeline, SIBLING_MANIFEST);

    for (index, now) in [0u64, 10, 20].into_iter().enumerate() {
        let generation = pipeline.keystroke(&"a".repeat(index + 1), now);
        let plugins = dispatched(&mut pipeline, now);
        assert!(
            !plugins.contains(&refusing),
            "max-suggestion-requests = 0 must never be dispatched: {plugins:?}"
        );
        assert!(
            plugins.contains(&sibling),
            "a plugin declaring zero must not stall its neighbours: {plugins:?}"
        );
        assert_eq!(
            refusals(&mut pipeline, &refusing),
            u64::try_from(index).expect("small index") + 1,
            "every refused request is counted"
        );
        pipeline.complete(&sibling, generation, now + 1);
    }

    assert_eq!(
        refusals(&mut pipeline, &sibling),
        0,
        "the neighbour of a zero-budget plugin is never throttled"
    );
}

#[test]
fn wasm_registration_is_supported_and_leaves_pipeline_state() {
    let manifest = Manifest::parse(WASM_MANIFEST).expect("wasm fixture must be well-formed");
    let plugin = PluginId(manifest.plugin.id.clone());
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());

    pipeline
        .register_manifest(&manifest)
        .expect("wasm is served by the supervised wasm host");
    assert!(pipeline.plugin_budget(&plugin).is_some());
    assert!(pipeline.plugin_diagnostics(&plugin).is_some());
    assert_eq!(pipeline.diagnostics().in_flight_requests, 0);
}

#[test]
fn cabi_registration_succeeds_and_leaves_usable_pipeline_state() {
    let manifest = Manifest::parse(CABI_MANIFEST).expect("c-abi fixture must be well-formed");
    let plugin = PluginId(manifest.plugin.id.clone());
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());

    let registered = pipeline
        .register_manifest(&manifest)
        .expect("this build hosts c-abi plugins out of process and must accept the manifest");
    assert_eq!(registered, plugin);
    assert!(
        pipeline.plugin_budget(&plugin).is_some(),
        "an accepted plugin owns a concurrency budget"
    );
    assert!(
        pipeline.plugin_diagnostics(&plugin).is_some(),
        "an accepted plugin is observable"
    );

    // The host serialises every call into one library, so a manifest asking
    // for four concurrent suggestions gets one and is told so by name rather
    // than silently granted something it did not ask for (README invariant 7).
    let unhonoured = manifest.unhonoured_declarations();
    assert!(
        unhonoured.iter().any(|declaration| {
            declaration.field == "concurrency.max-suggestion-requests"
                && declaration.reason == "c-abi-calls-are-serialised"
        }),
        "the serialised-call gap must be reported, not hidden: {unhonoured:?}"
    );
}

/// A four-slot manifest that grants exactly one unit of every §13.5 kind, so
/// occupying a slot and asking for a second one is a refusal of that kind and
/// of nothing else.
const EVERY_KIND_MANIFEST: &str = r#"
manifest-version = 1

[plugin]
id = "dev.crikey.every-kind"
name = "Every kind"
version = "1.0.0"
runtime = "python"
entrypoint = "plugin.py"

[query]
max-concurrent-requests = 1

[concurrency]
max-suggestion-requests = 1
max-action-requests = 1
max-background-tasks = 1
max-catalog-tasks = 1
"#;

/// Occupies one slot of `kind` on the shared budget, asks for a second, and
/// returns the plugin's health after the refusal.
///
/// The handle this reaches for is the exact `Arc` every production dispatch
/// site holds: `ModernProvider`/`NativeProvider` keep it in their loaded-plugin
/// record and clone it into their action endpoints and catalog builders,
/// `LegacyProvider::action_budgets` clones the same one, and
/// `WorkerOptions::with_shared_budget` hands it to the Python worker that
/// admits background tasks. Refusing on it is therefore what those sites do,
/// not an imitation of it.
fn refuse_one(kind: BudgetKind) -> (QueryPipeline, PluginId) {
    let manifest = Manifest::parse(EVERY_KIND_MANIFEST).expect("fixture manifest parses");
    let plugin = PluginId(manifest.plugin.id.clone());
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let budget = pipeline
        .register_namespaced_manifest(plugin.clone(), &manifest)
        .expect("fixture plugin registers once");

    let held = budget
        .try_acquire_owned(kind)
        .expect("the single declared slot admits the first unit");
    assert!(
        budget.try_acquire_owned(kind).is_none(),
        "a second unit must be refused while the first holds the only slot"
    );
    drop(held);
    (pipeline, plugin)
}

/// The refusal an operator can act on is the one attributed to a kind. All
/// four are asserted the same way so no kind can quietly stop reporting.
#[test]
fn a_refused_suggestion_reaches_per_plugin_health_as_a_suggestion_refusal() {
    let (mut pipeline, plugin) = refuse_one(BudgetKind::Suggestion);
    let refusals = pipeline.health(&plugin).concurrency_refusals;

    assert_eq!(refusals.suggestion, 1, "the suggestion refusal must be reported");
    assert_eq!(refusals.total(), 1, "no other kind may be credited with it");
    assert_eq!(
        (refusals.action, refusals.background, refusals.catalog),
        (0, 0, 0)
    );
}

#[test]
fn a_refused_action_reaches_per_plugin_health_as_an_action_refusal() {
    let (mut pipeline, plugin) = refuse_one(BudgetKind::Action);
    let refusals = pipeline.health(&plugin).concurrency_refusals;

    assert_eq!(refusals.action, 1, "the action refusal must be reported");
    assert_eq!(refusals.total(), 1, "no other kind may be credited with it");
    assert_eq!(
        (refusals.suggestion, refusals.background, refusals.catalog),
        (0, 0, 0)
    );
}

#[test]
fn a_refused_background_task_reaches_per_plugin_health_as_a_background_refusal() {
    let (mut pipeline, plugin) = refuse_one(BudgetKind::Background);
    let refusals = pipeline.health(&plugin).concurrency_refusals;

    assert_eq!(refusals.background, 1, "the background refusal must be reported");
    assert_eq!(refusals.total(), 1, "no other kind may be credited with it");
    assert_eq!(
        (refusals.suggestion, refusals.action, refusals.catalog),
        (0, 0, 0)
    );
}

#[test]
fn a_refused_catalog_build_reaches_per_plugin_health_as_a_catalog_refusal() {
    let (mut pipeline, plugin) = refuse_one(BudgetKind::Catalog);
    let refusals = pipeline.health(&plugin).concurrency_refusals;

    assert_eq!(refusals.catalog, 1, "the catalog refusal must be reported");
    assert_eq!(refusals.total(), 1, "no other kind may be credited with it");
    assert_eq!(
        (refusals.suggestion, refusals.action, refusals.background),
        (0, 0, 0)
    );
}

/// A refusal raised on another thread must still reach health: reconciliation
/// happens when health is read, not only when the pipeline happens to tick.
/// Without that, an action refused on the UI thread would stay invisible until
/// the next keystroke, which is exactly when the operator stops looking.
#[test]
fn a_refusal_raised_off_the_pipeline_thread_is_reported_without_a_tick() {
    let manifest = Manifest::parse(EVERY_KIND_MANIFEST).expect("fixture manifest parses");
    let plugin = PluginId(manifest.plugin.id.clone());
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let budget = pipeline
        .register_namespaced_manifest(plugin.clone(), &manifest)
        .expect("fixture plugin registers once");

    let held = budget
        .try_acquire_owned(BudgetKind::Action)
        .expect("the declared slot admits the first unit");
    let elsewhere = Arc::clone(&budget);
    std::thread::spawn(move || {
        assert!(elsewhere.try_acquire_owned(BudgetKind::Action).is_none());
    })
    .join()
    .expect("the refusing thread completes");
    drop(held);

    // No `keystroke`, no `tick`, no `present`: reading health is the only call.
    assert_eq!(pipeline.health(&plugin).concurrency_refusals.action, 1);
}

/// Every registered plugin must appear in the report, including one that has
/// never been throttled: an operator-facing listing that silently omits a
/// healthy plugin cannot be used to confirm a plugin loaded at all.
#[test]
fn the_health_report_covers_every_registered_plugin_with_its_own_refusals() {
    let mut pipeline = QueryPipeline::new(PipelineConfig::default());
    let throttled = register(&mut pipeline, EVERY_KIND_MANIFEST);
    let healthy = register(&mut pipeline, SIBLING_MANIFEST);

    let budget = pipeline
        .plugin_budget(&throttled)
        .expect("registered plugin has a budget")
        .clone();
    let held = budget
        .try_acquire_owned(BudgetKind::Catalog)
        .expect("the declared slot admits the first unit");
    assert!(budget.try_acquire_owned(BudgetKind::Catalog).is_none());
    drop(held);

    let report = pipeline.plugin_health_report();
    let reported = report
        .iter()
        .map(|(plugin, health)| (plugin.clone(), health.concurrency_refusals))
        .collect::<std::collections::BTreeMap<_, _>>();

    assert_eq!(
        reported
            .get(&throttled)
            .expect("the throttled plugin is reported")
            .catalog,
        1
    );
    assert_eq!(
        reported
            .get(&healthy)
            .expect("an unthrottled plugin is still reported")
            .total(),
        0
    );
}
