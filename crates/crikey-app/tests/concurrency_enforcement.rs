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
//! Suggestion requests are the only kind with a live dispatch site in this
//! workspace: `QueryPipeline::tick` mints them and all three providers consume
//! them. There is no plugin-owned action dispatch (`SearchService::execute`
//! refuses non-host-mediated actions outright), no background-task scheduler
//! and no modern/native catalog-build call, so the action, background and
//! catalog budgets are resolved and observable but have nothing to gate yet.
//! Each of these tests is about the kind that does exist.
//!
//! # Why the refusal must be observable
//!
//! An operator seeing a plugin answer nothing cannot distinguish "throttled"
//! from "broken". Every test below asserts the refusal reached
//! `QueryPipeline::health(...).concurrency_refusals`, not merely that the
//! request vanished.

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

fn refusals(pipeline: &QueryPipeline, plugin: &PluginId) -> u64 {
    pipeline.health(plugin).concurrency_refusals
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
        refusals(&pipeline, &capped),
        1,
        "the refusal must be observable in per-plugin diagnostics, not a silent drop"
    );
    assert_eq!(
        refusals(&pipeline, &sibling),
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
    assert_eq!(refusals(&pipeline, &capped), 1);

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
        refusals(&pipeline, &capped),
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
            refusals(&pipeline, &refusing),
            u64::try_from(index).expect("small index") + 1,
            "every refused request is counted"
        );
        // The sibling answers each round, so its own budget is never the
        // reason a later round dispatches nothing.
        pipeline.complete(&sibling, generation, now + 1);
    }

    assert_eq!(
        refusals(&pipeline, &sibling),
        0,
        "the neighbour of a zero-budget plugin is never throttled"
    );
}
