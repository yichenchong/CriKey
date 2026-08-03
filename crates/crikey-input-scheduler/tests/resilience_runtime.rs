//! Resilience contract for the M2 `QueryScheduler` (roadmap M2; spec 7.1, 8.4,
//! 8.8, 8.12, 9.2, 9.3, 12.4, 13.4, 13.5, 13.6, 25.4, 26.4, 31.4-8, 31.14-17,
//! 31.24).
//!
//! Red-first. Nothing here exists yet; the file pins the shape the scheduler
//! must grow. The modern debounce contract lives in `modern_runtime.rs` and the
//! two files deliberately share one API surface — this one owns the pieces that
//! keep the launcher alive under abuse:
//!
//! * `legacy-strict` prompt dispatch, callback serialization and
//!   `should_terminate()`, kept explicitly separate from modern debouncing.
//! * Bounded per-plugin and global request queues with named overflow
//!   behaviour: `ReplaceOldest`, `RejectNewest`, `DropOldest`.
//! * Round-robin dispatch under a global per-tick budget and a per-plugin
//!   budget, so no plugin starves and no plugin monopolizes.
//! * A slow plugin never delaying a fast one, proven under sustained input.
//! * Diagnostics: live depth gauges, high-water marks, drop/cancel/stale
//!   counters that saturate rather than wrap.
//!
//! Every timestamp is a caller-supplied virtual millisecond. No sleeps, no wall
//! clock, no thread scheduling: the tests are exactly reproducible.

use std::collections::HashSet;
use std::mem::discriminant;

use crikey_core::{Generation, PluginId};
use crikey_input_scheduler::{
    ActivationPolicy, BatchAdmission, BatchCompletion, CancelReason, CancelledRequest, CompletionOutcome,
    DebounceDecision, DebouncePolicy, DispatchedRequest, GateReason, LegacyDispatch, Millis, PluginPolicy,
    QueryScheduler, QueryTraceEvent, QueuePolicy, SchedulerConfig, SchedulerDiagnostics, SchedulingProfile,
};

// ---------------------------------------------------------------------------
// Fixtures. Capacities are deliberately tiny so a bound can be breached in a
// handful of keystrokes instead of a stress loop.
// ---------------------------------------------------------------------------

fn plugin(id: &str) -> PluginId {
    PluginId(id.to_owned())
}

/// Generous budgets: nothing here throttles, so a test that trips a bound has
/// tripped the bound it meant to trip.
fn roomy_config() -> SchedulerConfig {
    SchedulerConfig {
        request_queue_capacity: 64,
        result_queue_capacity: 64,
        per_plugin_dispatch_budget: 8,
        dispatch_budget_per_tick: 64,
        trace_capacity: 4096,
    }
}

/// A modern policy that dispatches as promptly as `legacy-strict` does, so a
/// queue or fairness test can use several concurrent slots without dragging
/// debounce timing into the assertion.
fn modern_prompt() -> PluginPolicy {
    PluginPolicy {
        debounce: DebouncePolicy {
            debounce_ms: 0,
            maximum_wait_ms: None,
            leading_edge: true,
            trailing_edge: true,
            minimum_query_length: 0,
        },
        ..PluginPolicy::modern()
    }
}

#[test]
fn zero_direct_limits_are_normalized_to_live_bounded_values() {
    let mut scheduler = QueryScheduler::new(SchedulerConfig {
        request_queue_capacity: 0,
        result_queue_capacity: 0,
        per_plugin_dispatch_budget: 0,
        dispatch_budget_per_tick: 0,
        trace_capacity: 0,
    });
    assert_eq!(
        *scheduler.config(),
        SchedulerConfig {
            request_queue_capacity: 1,
            result_queue_capacity: 1,
            per_plugin_dispatch_budget: 1,
            dispatch_budget_per_tick: 1,
            trace_capacity: 1,
        }
    );

    let modern = plugin("modern.normalized-zeroes");
    let mut direct = modern_prompt();
    direct.max_concurrent_requests = 0;
    direct.queue_policy = QueuePolicy::RejectNewest;
    direct.queue_capacity = 0;
    direct.debounce.leading_edge = false;
    direct.debounce.trailing_edge = false;
    scheduler.register_plugin(modern.clone(), direct);

    let applied = scheduler.plugin_policy(&modern).expect("normalized policy");
    assert_eq!(applied.max_concurrent_requests, 1);
    assert_eq!(applied.queue_capacity, 1);
    assert!(
        applied.debounce.trailing_edge,
        "an edge-less policy must not wedge"
    );

    let first = scheduler.submit_query("one", 0);
    let dispatched = scheduler.tick(0);
    assert_eq!(dispatched.len(), 1, "zero budgets normalize to usable budgets");
    assert_eq!(dispatched[0].generation, first);

    let second = scheduler.submit_query("two", 1);
    assert!(
        scheduler.tick(1).is_empty(),
        "normalized concurrency remains serial"
    );
    assert_eq!(scheduler.pending(&modern), Some(second));
    assert_eq!(scheduler.complete(&modern, first, 2), CompletionOutcome::Stale);
    let resumed = scheduler.tick(2);
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].generation, scheduler.current_generation());
    assert_eq!(scheduler.trace().len(), 1, "the normalized trace remains bounded");
    assert!(scheduler.diagnostics().trace_events_dropped > 0);

    let legacy = plugin("legacy.normalized-strict");
    let mut strict = PluginPolicy::legacy_strict();
    strict.max_concurrent_requests = 0;
    strict.queue_policy = QueuePolicy::DropOldest;
    strict.queue_capacity = 0;
    strict.debounce.leading_edge = false;
    strict.debounce.trailing_edge = false;
    strict.debounce.minimum_query_length = usize::MAX;
    strict.activation.supports_empty_query = false;
    strict.activation.prefixes = vec!["never".to_owned()];

    let mut strict_scheduler = QueryScheduler::new(roomy_config());
    strict_scheduler.register_plugin(legacy.clone(), strict);
    let expected = PluginPolicy::legacy_strict();
    assert_eq!(strict_scheduler.plugin_policy(&legacy), Some(&expected));
    let empty = strict_scheduler.submit_query("", 10);
    assert_eq!(
        strict_scheduler.tick(10)[0].generation,
        empty,
        "legacy-strict remains immediate and ungated after normalization"
    );
}

/// The plugins a batch of dispatches went to, in dispatch order.
fn dispatch_order(dispatched: &[DispatchedRequest]) -> Vec<String> {
    dispatched.iter().map(|d| d.plugin.0.clone()).collect()
}

fn dispatches_for<'a>(dispatched: &'a [DispatchedRequest], plugin: &PluginId) -> Vec<&'a DispatchedRequest> {
    dispatched.iter().filter(|d| &d.plugin == plugin).collect()
}

/// Timestamp carried by any trace event. Test-local so the assertions do not
/// depend on a convenience accessor the scheduler may or may not grow.
fn event_at(event: &QueryTraceEvent) -> Millis {
    match event {
        QueryTraceEvent::Keystroke { at, .. }
        | QueryTraceEvent::Debounce { at, .. }
        | QueryTraceEvent::LegacyDispatch { at, .. }
        | QueryTraceEvent::Dispatched { at, .. }
        | QueryTraceEvent::RequestDropped { at, .. }
        | QueryTraceEvent::Cancelled { at, .. }
        | QueryTraceEvent::FirstResult { at, .. }
        | QueryTraceEvent::FinalResult { at, .. }
        | QueryTraceEvent::ResultBatch { at, .. }
        | QueryTraceEvent::StaleResultRejected { at, .. }
        | QueryTraceEvent::Ranking { at, .. }
        | QueryTraceEvent::Presentation { at, .. } => *at,
    }
}

/// The plugin a trace event is attributed to. `Keystroke`, `Ranking` and
/// `Presentation` are query-wide and belong to no single plugin.
fn event_plugin(event: &QueryTraceEvent) -> Option<&PluginId> {
    match event {
        QueryTraceEvent::Keystroke { .. }
        | QueryTraceEvent::Ranking { .. }
        | QueryTraceEvent::Presentation { .. } => None,
        QueryTraceEvent::Debounce { plugin, .. }
        | QueryTraceEvent::LegacyDispatch { plugin, .. }
        | QueryTraceEvent::Dispatched { plugin, .. }
        | QueryTraceEvent::RequestDropped { plugin, .. }
        | QueryTraceEvent::Cancelled { plugin, .. }
        | QueryTraceEvent::FirstResult { plugin, .. }
        | QueryTraceEvent::FinalResult { plugin, .. }
        | QueryTraceEvent::ResultBatch { plugin, .. }
        | QueryTraceEvent::StaleResultRejected { plugin, .. } => Some(plugin),
    }
}

fn events_for<'a>(scheduler: &'a QueryScheduler, plugin: &PluginId) -> Vec<&'a QueryTraceEvent> {
    scheduler
        .trace()
        .iter()
        .filter(|event| event_plugin(event) == Some(plugin))
        .collect()
}

/// One representative value per spec 26.4 trace category. Used to prove the
/// trace can express every category the developer tooling promises, by
/// comparing enum discriminants against a real recorded trace.
fn spec_26_4_categories(plugin: &PluginId, generation: Generation) -> Vec<(&'static str, QueryTraceEvent)> {
    vec![
        (
            "keystroke timestamps + query generations",
            QueryTraceEvent::Keystroke {
                at: 0,
                generation,
                query_length: 0,
            },
        ),
        (
            "modern debounce decisions",
            QueryTraceEvent::Debounce {
                at: 0,
                plugin: plugin.clone(),
                generation,
                decision: DebounceDecision::LeadingEdge,
            },
        ),
        (
            "legacy dispatch and replacement decisions",
            QueryTraceEvent::LegacyDispatch {
                at: 0,
                plugin: plugin.clone(),
                generation,
                decision: LegacyDispatch::Idle,
            },
        ),
        (
            "plugin dispatch timestamps",
            QueryTraceEvent::Dispatched {
                at: 0,
                plugin: plugin.clone(),
                generation,
            },
        ),
        (
            "bounded-queue overflow",
            QueryTraceEvent::RequestDropped {
                at: 0,
                plugin: plugin.clone(),
                generation,
                policy: QueuePolicy::RejectNewest,
            },
        ),
        (
            "cancellation timestamps",
            QueryTraceEvent::Cancelled {
                at: 0,
                plugin: plugin.clone(),
                generation,
                reason: CancelReason::Shutdown,
            },
        ),
        (
            "first-result latency",
            QueryTraceEvent::FirstResult {
                at: 0,
                plugin: plugin.clone(),
                generation,
                latency_ms: 0,
            },
        ),
        (
            "final-result latency",
            QueryTraceEvent::FinalResult {
                at: 0,
                plugin: plugin.clone(),
                generation,
                latency_ms: 0,
            },
        ),
        (
            "result-batch sizes",
            QueryTraceEvent::ResultBatch {
                at: 0,
                plugin: plugin.clone(),
                generation,
                items: 0,
                completion: BatchCompletion::Final,
            },
        ),
        (
            "rejected stale responses",
            QueryTraceEvent::StaleResultRejected {
                at: 0,
                plugin: plugin.clone(),
                generation,
            },
        ),
        (
            "ranking updates",
            QueryTraceEvent::Ranking {
                at: 0,
                generation,
                ranked_items: 0,
            },
        ),
        (
            "presentation updates",
            QueryTraceEvent::Presentation {
                at: 0,
                generation,
                visible_items: 0,
            },
        ),
    ]
}

// ---------------------------------------------------------------------------
// legacy-strict prompt dispatch, kept apart from modern debouncing
// (spec 7.1, 8.4, 25.4, 31.14, 31.15)
// ---------------------------------------------------------------------------

#[test]
fn legacy_strict_initial_query_reaches_every_legacy_plugin_at_the_submit_timestamp() {
    let mut scheduler = QueryScheduler::new(roomy_config());
    let calc = plugin("legacy.calc");
    let files = plugin("legacy.files");
    let urls = plugin("legacy.urls");
    for id in [&calc, &files, &urls] {
        scheduler.register_plugin(id.clone(), PluginPolicy::legacy_strict());
    }

    let generation = scheduler.submit_query("term", 1_000);
    let dispatched = scheduler.tick(1_000);

    // Spec 7.1 "Initial query broadcast: all loaded legacy plugins", and 8.4.1
    // "dispatch the query promptly": promptly means this tick, not a later one.
    assert_eq!(dispatched.len(), 3, "every legacy plugin is broadcast to");
    let mut reached: Vec<String> = dispatch_order(&dispatched);
    reached.sort();
    assert_eq!(reached, vec!["legacy.calc", "legacy.files", "legacy.urls"]);
    for request in &dispatched {
        assert_eq!(request.generation, generation);
        assert_eq!(
            request.dispatched_at, 1_000,
            "legacy-strict adds no latency to the submit timestamp"
        );
        assert_eq!(request.query, "term");
    }

    assert_eq!(
        scheduler.next_wakeup(),
        None,
        "nothing is postponed, so no timer is armed"
    );
    let diagnostics = scheduler.diagnostics();
    assert_eq!(diagnostics.queued_requests, 0);
    assert_eq!(diagnostics.in_flight_requests, 3);
    assert_eq!(diagnostics.dispatched_requests, 3);
    assert_eq!(diagnostics.peak_queue_depth, 0);
}

#[test]
fn legacy_strict_ignores_debounce_configuration_that_a_modern_plugin_obeys() {
    let mut scheduler = QueryScheduler::new(roomy_config());
    let legacy = plugin("legacy.strict");
    let modern = plugin("modern.deferred");

    // The same nominally aggressive debounce is attached to both plugins. Only
    // the profile decides whether it means anything (spec 7.1, 8.4, 25.4).
    let debounce = DebouncePolicy {
        debounce_ms: 250,
        maximum_wait_ms: Some(1_000),
        leading_edge: false,
        trailing_edge: true,
        minimum_query_length: 0,
    };
    scheduler.register_plugin(
        legacy.clone(),
        PluginPolicy {
            debounce,
            ..PluginPolicy::legacy_strict()
        },
    );
    scheduler.register_plugin(
        modern.clone(),
        PluginPolicy {
            debounce,
            ..PluginPolicy::modern()
        },
    );

    let generation = scheduler.submit_query("abcd", 1_000);

    let immediate = scheduler.tick(1_000);
    assert_eq!(
        dispatch_order(&immediate),
        vec!["legacy.strict"],
        "legacy-strict is never time debounced; the modern plugin is"
    );
    assert_eq!(immediate[0].generation, generation);

    assert_eq!(
        scheduler.next_wakeup(),
        Some(1_250),
        "only the modern plugin arms a trailing-edge timer"
    );
    assert!(
        scheduler.tick(1_249).is_empty(),
        "the modern trailing edge has not arrived"
    );

    let trailing = scheduler.tick(1_250);
    assert_eq!(dispatch_order(&trailing), vec!["modern.deferred"]);
    assert_eq!(trailing[0].generation, generation);

    // The two profiles are traced through different decision channels, which is
    // what makes "this plugin was not debounced" auditable (spec 26.4).
    let legacy_events = events_for(&scheduler, &legacy);
    assert!(
        legacy_events.iter().any(|event| matches!(
            event,
            QueryTraceEvent::LegacyDispatch {
                decision: LegacyDispatch::Now(g),
                at: 1_000,
                ..
            } if *g == generation
        )),
        "legacy dispatch decision is traced at the keystroke timestamp"
    );
    assert!(
        !legacy_events
            .iter()
            .any(|event| matches!(event, QueryTraceEvent::Debounce { .. })),
        "a legacy-strict plugin never produces a debounce decision"
    );
    assert!(
        events_for(&scheduler, &modern).iter().any(|event| matches!(
            event,
            QueryTraceEvent::Debounce {
                decision: DebounceDecision::Deferred { until: 1_250 },
                ..
            }
        )),
        "the modern plugin's postponement is traced with its deadline"
    );
}

#[test]
fn legacy_strict_ignores_host_gating_that_a_modern_plugin_obeys() {
    let mut scheduler = QueryScheduler::new(roomy_config());
    let legacy = plugin("legacy.ungated");
    let short = plugin("modern.min-length");
    let prefixed = plugin("modern.prefixed");

    // Spec 7.1: legacy-strict has no minimum query length and no prefix
    // relevance gating, even when a manifest asks for them.
    scheduler.register_plugin(
        legacy.clone(),
        PluginPolicy {
            debounce: DebouncePolicy {
                minimum_query_length: 5,
                ..DebouncePolicy::default()
            },
            activation: ActivationPolicy {
                prefixes: vec!["xyz".to_owned()],
                ..ActivationPolicy::default()
            },
            ..PluginPolicy::legacy_strict()
        },
    );
    scheduler.register_plugin(
        short.clone(),
        PluginPolicy {
            debounce: DebouncePolicy {
                debounce_ms: 0,
                minimum_query_length: 5,
                ..DebouncePolicy::default()
            },
            ..modern_prompt()
        },
    );
    scheduler.register_plugin(
        prefixed.clone(),
        PluginPolicy {
            activation: ActivationPolicy {
                prefixes: vec!["xyz".to_owned()],
                ..ActivationPolicy::default()
            },
            ..modern_prompt()
        },
    );

    scheduler.submit_query("a", 2_000);
    let dispatched = scheduler.tick(2_000);

    assert_eq!(
        dispatch_order(&dispatched),
        vec!["legacy.ungated"],
        "host gating never suppresses a legacy-strict dispatch"
    );

    assert!(
        events_for(&scheduler, &short).iter().any(|event| matches!(
            event,
            QueryTraceEvent::Debounce {
                decision: DebounceDecision::Gated(GateReason::MinimumQueryLength),
                ..
            }
        )),
        "the modern plugin is gated by minimum query length"
    );
    assert!(
        events_for(&scheduler, &prefixed).iter().any(|event| matches!(
            event,
            QueryTraceEvent::Debounce {
                decision: DebounceDecision::Gated(GateReason::PrefixMismatch),
                ..
            }
        )),
        "the modern plugin is gated by prefix relevance"
    );

    // A gate is not a queue overflow: nothing was dropped or coalesced.
    let diagnostics = scheduler.diagnostics();
    assert_eq!(diagnostics.dropped_obsolete_requests, 0);
    assert_eq!(diagnostics.rejected_requests(), 0);
    assert_eq!(diagnostics.queued_requests, 0);
}

// ---------------------------------------------------------------------------
// Serial callbacks, newest-pending replacement, should_terminate()
// (spec 8.4, 9.2, 13.4, 31.16, 31.17)
// ---------------------------------------------------------------------------

#[test]
fn legacy_callbacks_are_serial_and_only_the_newest_pending_query_survives() {
    let mut scheduler = QueryScheduler::new(roomy_config());
    let legacy = plugin("legacy.serial");
    scheduler.register_plugin(legacy.clone(), PluginPolicy::legacy_strict());

    let first = scheduler.submit_query("a", 1_000);
    assert_eq!(scheduler.tick(1_000).len(), 1);
    assert_eq!(scheduler.in_flight(&legacy), 1);

    // Three more keystroke states arrive while the callback is still running.
    scheduler.submit_query("ab", 1_010);
    assert!(scheduler.tick(1_010).is_empty(), "callbacks are serialized");
    scheduler.submit_query("abc", 1_020);
    assert!(scheduler.tick(1_020).is_empty());
    let newest = scheduler.submit_query("abcd", 1_030);
    assert!(scheduler.tick(1_030).is_empty());

    assert_eq!(scheduler.in_flight(&legacy), 1, "never two callbacks at once");
    assert_eq!(
        scheduler.queued(&legacy),
        1,
        "older undispatched queries are discarded"
    );
    assert_eq!(scheduler.pending(&legacy), Some(newest));
    assert!(
        scheduler.should_terminate(&legacy),
        "the running callback is obsolete"
    );

    let per_plugin = scheduler
        .plugin_diagnostics(&legacy)
        .expect("a registered plugin has diagnostics");
    assert_eq!(per_plugin.queued_requests, 1);
    assert_eq!(
        per_plugin.peak_queue_depth, 1,
        "newest-wins never grows the queue"
    );
    assert_eq!(per_plugin.coalesced_requests, 2, "abc and ab were superseded");
    assert_eq!(per_plugin.rejected_queue_full, 0, "replacement is not overflow");
    assert!(per_plugin.should_terminate);

    // The obsolete callback finally returns. Its results are not displayable,
    // and the newest pending request goes out next (spec 8.4.5, 8.4.7).
    assert_eq!(
        scheduler.complete(&legacy, first, 1_040),
        CompletionOutcome::Stale
    );
    let resumed = scheduler.tick(1_040);
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].generation, newest);
    assert_eq!(resumed[0].query, "abcd");
    assert!(
        !scheduler.should_terminate(&legacy),
        "the successor is current, so cooperative termination clears"
    );
    assert_eq!(scheduler.queued(&legacy), 0);
    assert_eq!(scheduler.in_flight(&legacy), 1);
}

#[test]
fn superseding_an_in_flight_legacy_query_raises_should_terminate_at_the_keystroke() {
    let mut scheduler = QueryScheduler::new(roomy_config());
    let legacy = plugin("legacy.terminate");
    scheduler.register_plugin(legacy.clone(), PluginPolicy::legacy_strict());

    let running = scheduler.submit_query("q", 5_000);
    scheduler.tick(5_000);
    assert!(
        !scheduler.should_terminate(&legacy),
        "current work is not asked to terminate"
    );

    // Spec 8.4.2: the flag flips when the query changes, not when the host
    // later gets around to a tick.
    scheduler.submit_query("qq", 5_007);
    assert!(scheduler.should_terminate(&legacy));

    let cancellations: Vec<&QueryTraceEvent> = scheduler
        .trace()
        .iter()
        .filter(|event| matches!(event, QueryTraceEvent::Cancelled { .. }))
        .collect();
    assert_eq!(cancellations.len(), 1, "supersession is a cancellation event");
    assert!(matches!(
        cancellations[0],
        QueryTraceEvent::Cancelled {
            at: 5_007,
            generation: g,
            reason: CancelReason::QueryChanged,
            ..
        } if *g == running
    ));
    assert_eq!(scheduler.diagnostics().cancelled_requests, 1);
}

#[test]
fn legacy_strict_serialization_cannot_be_widened_by_reconfiguration() {
    let mut scheduler = QueryScheduler::new(roomy_config());
    let legacy = plugin("legacy.pinned");
    scheduler.register_plugin(legacy.clone(), PluginPolicy::legacy_strict());

    // Spec 13.4 is unconditional: no two lifecycle callbacks may run
    // concurrently against one legacy plugin instance, whatever a manifest or
    // a later reconfiguration asks for.
    scheduler.set_policy(
        &legacy,
        PluginPolicy {
            profile: SchedulingProfile::LegacyStrict,
            max_concurrent_requests: 4,
            ..PluginPolicy::legacy_strict()
        },
        900,
    );

    scheduler.submit_query("one", 1_000);
    assert_eq!(scheduler.tick(1_000).len(), 1);
    scheduler.submit_query("two", 1_010);
    assert!(
        scheduler.tick(1_010).is_empty(),
        "a second callback must not start"
    );
    assert_eq!(scheduler.in_flight(&legacy), 1);
    assert_eq!(scheduler.queued(&legacy), 1);
}

#[test]
fn cancelling_a_plugin_invalidates_in_flight_and_queued_work_with_a_named_reason() {
    let mut scheduler = QueryScheduler::new(roomy_config());
    let legacy = plugin("legacy.cancelled");
    scheduler.register_plugin(legacy.clone(), PluginPolicy::legacy_strict());

    let running = scheduler.submit_query("a", 1_000);
    scheduler.tick(1_000);
    let queued = scheduler.submit_query("ab", 1_010);
    scheduler.tick(1_010);
    assert_eq!(scheduler.queued(&legacy), 1);

    // Spec 9.3: disabling a plugin cancels its work.
    let invalidated = scheduler.cancel_plugin(&legacy, CancelReason::Disabled, 1_100);
    assert_eq!(
        invalidated,
        vec![running, queued],
        "in-flight work is reported before queued work"
    );

    assert_eq!(scheduler.queued(&legacy), 0, "queued work is discarded");
    assert!(scheduler.should_terminate(&legacy));
    assert_eq!(
        scheduler.in_flight(&legacy),
        1,
        "a cancelled callback still owns its slot until it returns"
    );
    assert!(
        scheduler.tick(1_100).is_empty(),
        "the cancelled pending request must not be dispatched"
    );

    assert_eq!(
        scheduler.complete(&legacy, running, 1_150),
        CompletionOutcome::Stale
    );
    assert_eq!(scheduler.in_flight(&legacy), 0);
    assert!(scheduler.tick(1_150).is_empty());

    let per_plugin = scheduler
        .plugin_diagnostics(&legacy)
        .expect("a registered plugin has diagnostics");
    assert_eq!(per_plugin.cancelled_requests, 2);
    assert_eq!(scheduler.diagnostics().cancelled_requests, 2);

    // The same facts reach the host as a signalable list, so it can propagate
    // cooperative cancellation to the worker without scraping the trace.
    let drained = scheduler.drain_cancellations();
    assert_eq!(
        drained,
        vec![
            CancelledRequest {
                plugin: legacy.clone(),
                generation: running,
                reason: CancelReason::QueryChanged,
                cancelled_at: 1_010,
            },
            CancelledRequest {
                plugin: legacy.clone(),
                generation: queued,
                reason: CancelReason::Disabled,
                cancelled_at: 1_100,
            },
        ],
        "each generation is notified once at its first invalidation"
    );
    assert!(
        scheduler.drain_cancellations().is_empty(),
        "draining consumes the queue exactly once"
    );

    let reasons: Vec<&QueryTraceEvent> = scheduler
        .trace()
        .iter()
        .filter(|event| matches!(event, QueryTraceEvent::Cancelled { .. }))
        .collect();
    assert_eq!(
        reasons.len(),
        2,
        "one trace event accompanies each counter increment"
    );
    assert!(reasons.iter().any(|event| matches!(
        event,
        QueryTraceEvent::Cancelled {
            generation,
            reason: CancelReason::QueryChanged,
            at: 1_010,
            ..
        } if *generation == running
    )));
    assert!(reasons.iter().any(|event| matches!(
        event,
        QueryTraceEvent::Cancelled {
            generation,
            reason: CancelReason::Disabled,
            at: 1_100,
            ..
        } if *generation == queued
    )));
}

#[test]
fn results_from_an_obsolete_generation_are_rejected_and_counted() {
    let mut scheduler = QueryScheduler::new(roomy_config());
    let legacy = plugin("legacy.stale");
    scheduler.register_plugin(legacy.clone(), PluginPolicy::legacy_strict());

    let stale = scheduler.submit_query("a", 1_000);
    scheduler.tick(1_000);
    let current = scheduler.submit_query("ab", 1_010);

    assert_eq!(
        scheduler.record_result_batch(&legacy, stale, 12, BatchCompletion::Final, 1_020),
        BatchAdmission::StaleRejected,
        "a batch for a superseded generation never becomes visible state"
    );
    assert_eq!(scheduler.diagnostics().rejected_stale_results, 1);
    assert_eq!(
        scheduler
            .plugin_diagnostics(&legacy)
            .expect("diagnostics")
            .rejected_stale_results,
        1
    );
    assert!(scheduler.trace().iter().any(|event| matches!(
        event,
        QueryTraceEvent::StaleResultRejected { at: 1_020, generation: g, .. } if *g == stale
    )));

    // The current generation is accepted through the same door.
    scheduler.complete(&legacy, stale, 1_025);
    let resumed = scheduler.tick(1_025);
    assert_eq!(resumed[0].generation, current);
    assert_eq!(
        scheduler.record_result_batch(&legacy, current, 4, BatchCompletion::Partial, 1_030),
        BatchAdmission::Accepted
    );
    assert_eq!(
        scheduler.diagnostics().rejected_stale_results,
        1,
        "accepting a current batch does not touch the stale counter"
    );
}

// ---------------------------------------------------------------------------
// Bounded queues and named overflow outcomes (spec 8.8, 12.4, 31.4, 31.24)
// ---------------------------------------------------------------------------

#[test]
fn replace_oldest_holds_queue_depth_at_one_however_fast_input_arrives() {
    let mut scheduler = QueryScheduler::new(SchedulerConfig {
        request_queue_capacity: 4,
        ..roomy_config()
    });
    let legacy = plugin("legacy.typeahead");
    scheduler.register_plugin(legacy.clone(), PluginPolicy::legacy_strict());

    let running = scheduler.submit_query("a", 1_000);
    scheduler.tick(1_000);

    // 200 keystrokes land on a plugin that never answers. Under newest-wins the
    // undispatched set is a single slot, so memory cannot grow (spec 31.4).
    let mut newest: Option<Generation> = None;
    for step in 1..=200u64 {
        let now = 1_000 + step;
        newest = Some(scheduler.submit_query(&format!("a{step}"), now));
        assert!(scheduler.tick(now).is_empty(), "the plugin is still busy");
        assert_eq!(scheduler.queued(&legacy), 1, "at most one pending request");
        assert!(scheduler.diagnostics().queued_requests <= 1);
    }

    let diagnostics = scheduler.diagnostics();
    assert_eq!(diagnostics.peak_queue_depth, 1);
    assert_eq!(diagnostics.coalesced_requests, 199, "199 supersessions");
    assert_eq!(
        diagnostics.dropped_obsolete_requests, 0,
        "newest-wins replacement is coalescing, not dropping"
    );
    assert_eq!(diagnostics.rejected_requests(), 0, "a bound was never breached");
    assert_eq!(diagnostics.dispatched_requests, 1);

    // Only the newest survivor is dispatched; the 199 intermediate states are gone.
    scheduler.complete(&legacy, running, 1_300);
    let resumed = scheduler.tick(1_300);
    assert_eq!(resumed.len(), 1);
    assert_eq!(Some(resumed[0].generation), newest);
    assert_eq!(resumed[0].generation, scheduler.current_generation());
    assert_eq!(resumed[0].query, "a200");
}

#[test]
fn reject_newest_reclaims_obsolete_entries_before_admission_or_dispatch() {
    let mut scheduler = QueryScheduler::new(roomy_config());
    let queued_plugin = plugin("modern.reject-newest");
    scheduler.register_plugin(
        queued_plugin.clone(),
        PluginPolicy {
            queue_policy: QueuePolicy::RejectNewest,
            queue_capacity: 2,
            max_concurrent_requests: 1,
            ..modern_prompt()
        },
    );

    let running = scheduler.submit_query("q1", 1_000);
    assert_eq!(scheduler.tick(1_000).len(), 1);

    let second = scheduler.submit_query("q2", 1_010);
    assert!(scheduler.tick(1_010).is_empty());
    assert_eq!(scheduler.pending(&queued_plugin), Some(second));

    let third = scheduler.submit_query("q3", 1_020);
    assert!(scheduler.tick(1_020).is_empty());
    assert_eq!(
        scheduler.pending(&queued_plugin),
        Some(third),
        "the obsolete second generation is reclaimed before admission"
    );

    let current = scheduler.submit_query("q4", 1_030);
    assert!(scheduler.tick(1_030).is_empty());
    assert_eq!(scheduler.queued(&queued_plugin), 1);
    assert_eq!(scheduler.pending(&queued_plugin), Some(current));

    let diagnostics = scheduler.diagnostics();
    assert_eq!(diagnostics.dropped_obsolete_requests, 2);
    assert_eq!(diagnostics.rejected_plugin_queue_full, 0);
    assert_eq!(diagnostics.rejected_global_queue_full, 0);
    assert_eq!(diagnostics.peak_queue_depth, 1);
    let reclaimed: Vec<Generation> = scheduler
        .trace()
        .iter()
        .filter_map(|event| match event {
            QueryTraceEvent::RequestDropped {
                generation,
                policy: QueuePolicy::RejectNewest,
                ..
            } => Some(*generation),
            _ => None,
        })
        .collect();
    assert_eq!(reclaimed, vec![second, third]);

    assert_eq!(
        scheduler.complete(&queued_plugin, running, 1_040),
        CompletionOutcome::Stale
    );
    let next = scheduler.tick(1_040);
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].generation, current);
    assert_eq!(next[0].generation, scheduler.current_generation());
}

#[test]
fn drop_oldest_reclaims_every_obsolete_entry_before_dispatch() {
    let mut scheduler = QueryScheduler::new(roomy_config());
    let queued_plugin = plugin("modern.drop-oldest");
    scheduler.register_plugin(
        queued_plugin.clone(),
        PluginPolicy {
            queue_policy: QueuePolicy::DropOldest,
            queue_capacity: 2,
            max_concurrent_requests: 1,
            ..modern_prompt()
        },
    );

    let running = scheduler.submit_query("q1", 1_000);
    scheduler.tick(1_000);
    let second = scheduler.submit_query("q2", 1_010);
    scheduler.tick(1_010);
    let third = scheduler.submit_query("q3", 1_020);
    scheduler.tick(1_020);
    let current = scheduler.submit_query("q4", 1_030);
    scheduler.tick(1_030);

    assert_eq!(scheduler.queued(&queued_plugin), 1);
    assert_eq!(scheduler.pending(&queued_plugin), Some(current));
    let diagnostics = scheduler.diagnostics();
    assert_eq!(
        diagnostics.dropped_obsolete_requests, 2,
        "each dead generation leaves before the next admission"
    );
    assert_eq!(diagnostics.rejected_requests(), 0);
    assert_eq!(diagnostics.peak_queue_depth, 1);
    let reclaimed: Vec<Generation> = scheduler
        .trace()
        .iter()
        .filter_map(|event| match event {
            QueryTraceEvent::RequestDropped {
                generation,
                policy: QueuePolicy::DropOldest,
                ..
            } => Some(*generation),
            _ => None,
        })
        .collect();
    assert_eq!(reclaimed, vec![second, third]);

    assert_eq!(
        scheduler.complete(&queued_plugin, running, 1_040),
        CompletionOutcome::Stale
    );
    let dispatched = scheduler.tick(1_040);
    assert_eq!(dispatched.len(), 1);
    assert_eq!(dispatched[0].generation, current);
    assert_eq!(dispatched[0].generation, scheduler.current_generation());
}

#[test]
fn obsolete_entries_are_reclaimed_before_the_global_request_bound() {
    let mut scheduler = QueryScheduler::new(SchedulerConfig {
        request_queue_capacity: 3,
        ..roomy_config()
    });
    let plugins = [
        plugin("p.one"),
        plugin("p.two"),
        plugin("p.three"),
        plugin("p.four"),
    ];
    let activation_queries = ["one", "two", "three", "four"];
    for (id, query) in plugins.iter().zip(activation_queries) {
        scheduler.register_plugin(
            id.clone(),
            PluginPolicy {
                activation: ActivationPolicy {
                    prefixes: vec![query.to_owned(), "all".to_owned()],
                    ..ActivationPolicy::default()
                },
                queue_policy: QueuePolicy::RejectNewest,
                queue_capacity: 4,
                max_concurrent_requests: 1,
                ..modern_prompt()
            },
        );
    }

    // Put one callback from each plugin in flight without ever requiring more
    // than one queue slot: each setup query activates exactly one plugin.
    let mut running = Vec::new();
    for (offset, query) in activation_queries.into_iter().enumerate() {
        let now = 900 + offset as u64;
        let generation = scheduler.submit_query(query, now);
        let dispatched = scheduler.tick(now);
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].plugin, plugins[offset]);
        running.push(generation);
    }

    let obsolete = scheduler.submit_query("all old", 1_010);
    scheduler.tick(1_010);
    assert_eq!(scheduler.diagnostics().queued_requests, 3);

    let current = scheduler.submit_query("all current", 1_020);
    scheduler.tick(1_020);
    let diagnostics = scheduler.diagnostics();
    assert_eq!(diagnostics.queued_requests, 3);
    assert!(diagnostics.queued_requests <= scheduler.config().request_queue_capacity);
    assert_eq!(
        diagnostics.dropped_obsolete_requests, 3,
        "all resident obsolete entries are reclaimed before applying the bound"
    );
    assert_eq!(
        diagnostics.rejected_global_queue_full, 2,
        "only the fourth current arrival at each full generation is refused"
    );
    assert_eq!(diagnostics.rejected_plugin_queue_full, 0);

    for id in &plugins[..3] {
        assert_eq!(scheduler.pending(id), Some(current));
        assert_eq!(
            scheduler
                .plugin_diagnostics(id)
                .expect("diagnostics")
                .dropped_obsolete_requests,
            1
        );
        assert!(scheduler.trace().iter().any(|event| matches!(
            event,
            QueryTraceEvent::RequestDropped {
                generation,
                plugin,
                policy: QueuePolicy::RejectNewest,
                ..
            } if *generation == obsolete && plugin == id
        )));
    }
    assert_eq!(scheduler.queued(&plugins[3]), 0);
    assert_eq!(
        scheduler
            .plugin_diagnostics(&plugins[3])
            .expect("diagnostics")
            .rejected_queue_full,
        2
    );

    for (id, generation) in plugins.iter().zip(running) {
        assert_eq!(
            scheduler.complete(id, generation, 1_030),
            CompletionOutcome::Stale
        );
    }
    let dispatched = scheduler.tick(1_030);
    assert_eq!(dispatched.len(), 3);
    assert!(
        dispatched
            .iter()
            .all(|request| request.generation == scheduler.current_generation()),
        "the bound may refuse current work but must never dispatch obsolete work"
    );
}

// ---------------------------------------------------------------------------
// Fairness: round robin and per-plugin dispatch budgets (spec 8.12, 13.5)
// ---------------------------------------------------------------------------

#[test]
fn a_budget_limited_tick_serves_plugins_round_robin_so_none_starves() {
    let mut scheduler = QueryScheduler::new(SchedulerConfig {
        dispatch_budget_per_tick: 1,
        per_plugin_dispatch_budget: 1,
        ..roomy_config()
    });
    let plugins = [plugin("rr.a"), plugin("rr.b"), plugin("rr.c")];
    for id in &plugins {
        scheduler.register_plugin(
            id.clone(),
            PluginPolicy {
                max_concurrent_requests: 8,
                ..modern_prompt()
            },
        );
    }

    // Nine keystrokes, one dispatch slot each. Registration-order bias would
    // give rr.a nine dispatches and rr.c none.
    let mut served: Vec<String> = Vec::new();
    for step in 0..9u64 {
        let now = 1_000 + step * 10;
        scheduler.submit_query(&format!("q{step}"), now);
        let dispatched = scheduler.tick(now);
        assert_eq!(
            dispatched.len(),
            1,
            "the global per-tick budget caps this tick at one dispatch"
        );
        served.push(dispatched[0].plugin.0.clone());
    }

    for id in &plugins {
        let count = served.iter().filter(|name| *name == &id.0).count();
        assert_eq!(count, 3, "{} received an equal share, got {served:?}", id.0);
    }
    let first_round: HashSet<&String> = served[0..3].iter().collect();
    assert_eq!(
        first_round.len(),
        3,
        "the very first round already touches every plugin: {served:?}"
    );

    // Nine keystrokes minus nine dispatches leaves the two plugins that were
    // not served on the final tick still holding their newest-wins request.
    let backlog: usize = plugins
        .iter()
        .map(|id| {
            let queued = scheduler.queued(id);
            assert!(queued <= 1, "{} exceeded its newest-wins slot", id.0);
            queued
        })
        .sum();
    assert_eq!(backlog, 2);
    assert_eq!(scheduler.diagnostics().queued_requests, 2);
    for id in &plugins {
        assert_eq!(
            scheduler
                .plugin_diagnostics(id)
                .expect("diagnostics")
                .dispatched_requests,
            3
        );
    }
}

#[test]
fn budgeted_dispatch_reclaims_obsolete_backlog_before_serving() {
    let mut scheduler = QueryScheduler::new(SchedulerConfig {
        dispatch_budget_per_tick: 16,
        per_plugin_dispatch_budget: 2,
        ..roomy_config()
    });
    let chatty = plugin("budget.chatty");
    let calm = plugin("budget.calm");
    for id in [&chatty, &calm] {
        scheduler.register_plugin(
            id.clone(),
            PluginPolicy {
                queue_policy: QueuePolicy::RejectNewest,
                queue_capacity: 8,
                max_concurrent_requests: 8,
                ..modern_prompt()
            },
        );
    }

    let mut current = Generation::ZERO;
    for step in 0..6u64 {
        current = scheduler.submit_query(&format!("q{step}"), 1_000 + step);
    }

    assert_eq!(scheduler.queued(&chatty), 1);
    assert_eq!(scheduler.queued(&calm), 1);
    assert_eq!(scheduler.diagnostics().dropped_obsolete_requests, 10);

    let dispatched = scheduler.tick(1_010);
    assert_eq!(dispatched.len(), 2);
    assert_eq!(dispatches_for(&dispatched, &chatty).len(), 1);
    assert_eq!(dispatches_for(&dispatched, &calm).len(), 1);
    assert!(
        dispatched.iter().all(|request| request.generation == current),
        "dispatch budgets may limit current work but never preserve dead backlog"
    );
    assert_eq!(scheduler.diagnostics().queued_requests, 0);
    assert_eq!(
        scheduler
            .plugin_diagnostics(&chatty)
            .expect("diagnostics")
            .peak_queue_depth,
        1
    );
}

// ---------------------------------------------------------------------------
// Sustained rapid input: bounded depth, fast progress, slow work in flight
// (spec 6.5, 13.6, 31.4, 31.8, 31.24)
// ---------------------------------------------------------------------------

#[test]
fn a_slow_plugin_never_delays_a_fast_plugin_under_sustained_typing() {
    let mut scheduler = QueryScheduler::new(SchedulerConfig {
        request_queue_capacity: 8,
        ..roomy_config()
    });
    let slow = plugin("legacy.network");
    let fast = plugin("legacy.calc");
    scheduler.register_plugin(slow.clone(), PluginPolicy::legacy_strict());
    scheduler.register_plugin(fast.clone(), PluginPolicy::legacy_strict());

    const KEYSTROKES: u64 = 40;
    let mut fast_dispatches = 0usize;
    let mut peak_depth_seen = 0usize;
    let stuck = scheduler.submit_query("k0", 1_000);
    {
        let opening = scheduler.tick(1_000);
        assert_eq!(opening.len(), 2, "both plugins start on the first keystroke");
    }
    // The slow plugin never calls back. The fast one answers within the same
    // virtual millisecond it was dispatched.
    scheduler.complete(&fast, stuck, 1_000);
    fast_dispatches += 1;

    for step in 1..KEYSTROKES {
        let now = 1_000 + step * 8;
        let generation = scheduler.submit_query(&format!("k{step}"), now);
        let dispatched = scheduler.tick(now);

        let to_fast = dispatches_for(&dispatched, &fast);
        assert_eq!(
            to_fast.len(),
            1,
            "keystroke {step} must reach the fast plugin while the slow one hangs"
        );
        assert_eq!(to_fast[0].generation, generation);
        assert_eq!(
            to_fast[0].dispatched_at, now,
            "the fast plugin waits on nothing, not even one millisecond"
        );
        assert!(
            dispatches_for(&dispatched, &slow).is_empty(),
            "the slow plugin is still serialized behind its first callback"
        );
        scheduler.complete(&fast, generation, now);
        fast_dispatches += 1;

        peak_depth_seen = peak_depth_seen.max(scheduler.diagnostics().queued_requests);
        assert!(
            scheduler.diagnostics().queued_requests <= 1,
            "only the slow plugin's single newest-wins slot is ever occupied"
        );
    }

    let fast_diagnostics = scheduler.plugin_diagnostics(&fast).expect("diagnostics");
    assert_eq!(fast_diagnostics.dispatched_requests, fast_dispatches as u64);
    assert_eq!(fast_diagnostics.dispatched_requests, KEYSTROKES);
    assert_eq!(fast_diagnostics.queued_requests, 0);
    assert_eq!(fast_diagnostics.in_flight_requests, 0);
    assert_eq!(
        fast_diagnostics.last_dispatched_at,
        Some(1_000 + (KEYSTROKES - 1) * 8)
    );
    assert!(!fast_diagnostics.should_terminate);

    let slow_diagnostics = scheduler.plugin_diagnostics(&slow).expect("diagnostics");
    assert_eq!(
        slow_diagnostics.dispatched_requests, 1,
        "the slow plugin is still on its first request"
    );
    assert_eq!(slow_diagnostics.in_flight_requests, 1);
    assert_eq!(slow_diagnostics.queued_requests, 1);
    assert_eq!(slow_diagnostics.peak_queue_depth, 1, "bounded, not growing");
    assert!(
        slow_diagnostics.should_terminate,
        "its work has been obsolete for 39 keystrokes"
    );
    assert_eq!(peak_depth_seen, 1);

    // The hung plugin finally answers for a generation nobody is looking at.
    let final_now = 1_000 + KEYSTROKES * 8;
    assert_eq!(
        scheduler.record_result_batch(&slow, stuck, 500, BatchCompletion::Final, final_now),
        BatchAdmission::StaleRejected,
        "40 keystrokes of stale work cannot reach visible state"
    );
    assert_eq!(
        scheduler.complete(&slow, stuck, final_now),
        CompletionOutcome::Stale
    );
    let resumed = scheduler.tick(final_now);
    assert_eq!(dispatch_order(&resumed), vec!["legacy.network"]);
    assert_eq!(
        resumed[0].generation,
        scheduler.current_generation(),
        "the slow plugin resumes on the newest query, skipping 38 dead ones"
    );
    assert_eq!(scheduler.diagnostics().rejected_stale_results, 1);
}

#[test]
fn sustained_rapid_input_bounds_every_queue_the_trace_and_the_accounting() {
    let mut scheduler = QueryScheduler::new(SchedulerConfig {
        request_queue_capacity: 8,
        trace_capacity: 32,
        ..roomy_config()
    });
    let plugins = [
        plugin("stress.a"),
        plugin("stress.b"),
        plugin("stress.c"),
        plugin("stress.d"),
        plugin("stress.e"),
    ];
    for id in &plugins {
        scheduler.register_plugin(id.clone(), PluginPolicy::legacy_strict());
    }

    const KEYSTROKES: u64 = 200;
    let mut previous = SchedulerDiagnostics::default();
    let mut peak_depth_seen = 0usize;

    for step in 0..KEYSTROKES {
        let now = 1_000 + step * 3;
        scheduler.submit_query(&format!("s{step}"), now);
        scheduler.tick(now);

        let diagnostics = scheduler.diagnostics();
        peak_depth_seen = peak_depth_seen.max(diagnostics.queued_requests);
        assert!(
            diagnostics.queued_requests <= scheduler.config().request_queue_capacity,
            "step {step} exceeded the configured request bound"
        );
        assert!(
            diagnostics.queued_requests <= plugins.len(),
            "newest-wins allows one undispatched request per plugin at most"
        );
        // Counters only ever move forward, however many events a step produces.
        assert!(diagnostics.dispatched_requests >= previous.dispatched_requests);
        assert!(diagnostics.coalesced_requests >= previous.coalesced_requests);
        assert!(diagnostics.cancelled_requests >= previous.cancelled_requests);
        assert!(diagnostics.discarded_requests() >= previous.discarded_requests());
        assert!(diagnostics.trace_events_dropped >= previous.trace_events_dropped);
        previous = diagnostics;
    }

    let diagnostics = scheduler.diagnostics();
    assert_eq!(peak_depth_seen, plugins.len());
    assert_eq!(diagnostics.peak_queue_depth, plugins.len());
    assert_eq!(diagnostics.in_flight_requests, plugins.len());
    assert_eq!(diagnostics.dispatched_requests, plugins.len() as u64);

    // Conservation: every offered request was dispatched, discarded, or is
    // still queued. Exactly once. Nothing leaks and nothing is double counted.
    let offered = KEYSTROKES * plugins.len() as u64;
    assert_eq!(
        diagnostics.dispatched_requests
            + diagnostics.discarded_requests()
            + diagnostics.queued_requests as u64,
        offered,
        "request accounting must balance"
    );

    // The developer trace is a bounded ring: it sheds oldest first and says so.
    let trace = scheduler.trace();
    assert_eq!(trace.len(), 32, "the trace never grows past its capacity");
    assert!(
        diagnostics.trace_events_dropped > 0,
        "shed events are counted, not silently lost"
    );
    let timestamps: Vec<Millis> = trace.iter().map(event_at).collect();
    assert!(
        timestamps.windows(2).all(|pair| pair[0] <= pair[1]),
        "the retained trace is chronological: {timestamps:?}"
    );
    let last_keystroke_at = 1_000 + (KEYSTROKES - 1) * 3;
    assert_eq!(
        timestamps.last().copied(),
        Some(last_keystroke_at),
        "the newest events are the ones retained"
    );
    assert!(
        timestamps[0] > 1_000,
        "the oldest events were shed, not the newest"
    );
}

// ---------------------------------------------------------------------------
// Diagnostics arithmetic (spec 26.4; roadmap M2 "bounded queue diagnostics")
// ---------------------------------------------------------------------------

#[test]
fn diagnostic_totals_saturate_instead_of_wrapping_or_panicking() {
    // A long-lived session can plausibly retire a counter to its ceiling. A
    // naive `a + b` panics in debug and wraps to a smaller number in release;
    // either turns a saturated diagnostic into a lie.
    let coalesce_saturated = SchedulerDiagnostics {
        coalesced_requests: u64::MAX,
        dropped_obsolete_requests: 1,
        rejected_plugin_queue_full: 1,
        rejected_global_queue_full: 1,
        ..SchedulerDiagnostics::default()
    };
    assert_eq!(coalesce_saturated.rejected_requests(), 2);
    assert_eq!(coalesce_saturated.discarded_requests(), u64::MAX);

    let both_rejections_saturated = SchedulerDiagnostics {
        rejected_plugin_queue_full: u64::MAX,
        rejected_global_queue_full: 7,
        ..SchedulerDiagnostics::default()
    };
    assert_eq!(both_rejections_saturated.rejected_requests(), u64::MAX);
    assert_eq!(both_rejections_saturated.discarded_requests(), u64::MAX);

    let quiet = SchedulerDiagnostics {
        coalesced_requests: 3,
        dropped_obsolete_requests: 5,
        rejected_plugin_queue_full: 7,
        rejected_global_queue_full: 11,
        ..SchedulerDiagnostics::default()
    };
    assert_eq!(quiet.rejected_requests(), 18);
    assert_eq!(quiet.discarded_requests(), 26);
    assert_eq!(
        SchedulerDiagnostics::default().discarded_requests(),
        0,
        "a fresh scheduler has discarded nothing"
    );
}

#[test]
fn the_query_trace_records_every_spec_26_4_category() {
    let mut scheduler = QueryScheduler::new(roomy_config());
    let legacy = plugin("trace.legacy");
    let modern = plugin("trace.modern");
    let bounded = plugin("trace.bounded");

    scheduler.register_plugin(legacy.clone(), PluginPolicy::legacy_strict());
    scheduler.register_plugin(
        modern.clone(),
        PluginPolicy {
            debounce: DebouncePolicy {
                debounce_ms: 20,
                maximum_wait_ms: None,
                leading_edge: false,
                trailing_edge: true,
                minimum_query_length: 0,
            },
            ..PluginPolicy::modern()
        },
    );
    scheduler.register_plugin(
        bounded.clone(),
        PluginPolicy {
            queue_policy: QueuePolicy::RejectNewest,
            queue_capacity: 1,
            max_concurrent_requests: 1,
            ..modern_prompt()
        },
    );

    let first = scheduler.submit_query("alpha", 1_000);
    scheduler.tick(1_000);

    // Result lifecycle for the legacy plugin: first batch, final batch,
    // presentation, completion.
    assert_eq!(
        scheduler.record_result_batch(&legacy, first, 3, BatchCompletion::Partial, 1_005),
        BatchAdmission::Accepted
    );
    assert_eq!(
        scheduler.record_result_batch(&legacy, first, 2, BatchCompletion::Final, 1_008),
        BatchAdmission::Accepted
    );
    scheduler.record_ranking(first, 5, 1_009);
    scheduler.record_presentation(first, 5, 1_009);
    assert_eq!(
        scheduler.complete(&legacy, first, 1_010),
        CompletionOutcome::Accepted
    );

    // Overflow on the bounded plugin: capacity 1 fills, then refuses.
    scheduler.submit_query("alphb", 1_020);
    scheduler.tick(1_020);
    scheduler.submit_query("alphc", 1_030);
    scheduler.tick(1_030);

    // The modern plugin's trailing edge fires on its own timer.
    let wakeup = scheduler
        .next_wakeup()
        .expect("the modern plugin armed a trailing-edge timer");
    scheduler.tick(wakeup);

    // A stale batch from the plugin still stuck on the first generation.
    assert_eq!(
        scheduler.record_result_batch(&bounded, first, 4, BatchCompletion::Final, wakeup + 5),
        BatchAdmission::StaleRejected
    );

    scheduler.cancel_plugin(&modern, CancelReason::Shutdown, wakeup + 10);

    let recorded: HashSet<_> = scheduler.trace().iter().map(discriminant).collect();
    for (category, sample) in spec_26_4_categories(&legacy, first) {
        assert!(
            recorded.contains(&discriminant(&sample)),
            "spec 26.4 requires the query trace to record {category}"
        );
    }
    assert_eq!(
        recorded.len(),
        12,
        "the trace exposes exactly the twelve spec 26.4 categories"
    );

    let timestamps: Vec<Millis> = scheduler.trace().iter().map(event_at).collect();
    assert!(
        timestamps.windows(2).all(|pair| pair[0] <= pair[1]),
        "trace events are appended in virtual-time order: {timestamps:?}"
    );
    assert_eq!(scheduler.diagnostics().trace_events_dropped, 0);
}

#[test]
fn cancellation_notifications_drop_oldest_at_the_bound() {
    let legacy = plugin("legacy.cancel-queue");
    let mut exact = QueryScheduler::new(SchedulerConfig {
        request_queue_capacity: 2,
        ..roomy_config()
    });
    exact.register_plugin(legacy.clone(), PluginPolicy::legacy_strict());

    let first = exact.submit_query("one", 0);
    assert_eq!(exact.tick(0).len(), 1);
    let second = exact.submit_query("two", 1);
    assert!(exact.tick(1).is_empty());
    assert_eq!(exact.complete(&legacy, first, 2), CompletionOutcome::Stale);
    assert_eq!(exact.tick(2).len(), 1);
    let third = exact.submit_query("three", 3);
    assert!(exact.tick(3).is_empty());

    assert_eq!(exact.pending(&legacy), Some(third));
    let retained_at_capacity = exact.drain_cancellations();
    assert_eq!(
        retained_at_capacity
            .iter()
            .map(|cancellation| cancellation.generation)
            .collect::<Vec<_>>(),
        vec![first, second],
        "filling exactly to capacity must retain every cancellation"
    );
    assert_eq!(exact.diagnostics().dropped_cancellation_notifications, 0);

    let mut overflowing = QueryScheduler::new(SchedulerConfig {
        request_queue_capacity: 2,
        ..roomy_config()
    });
    overflowing.register_plugin(legacy.clone(), PluginPolicy::legacy_strict());
    let first = overflowing.submit_query("one", 0);
    assert_eq!(overflowing.tick(0).len(), 1);
    let second = overflowing.submit_query("two", 1);
    assert!(overflowing.tick(1).is_empty());
    assert_eq!(overflowing.complete(&legacy, first, 2), CompletionOutcome::Stale);
    assert_eq!(overflowing.tick(2).len(), 1);
    let third = overflowing.submit_query("three", 3);
    assert!(overflowing.tick(3).is_empty());
    assert_eq!(overflowing.complete(&legacy, second, 4), CompletionOutcome::Stale);
    assert_eq!(overflowing.tick(4).len(), 1);
    let fourth = overflowing.submit_query("four", 5);
    assert!(overflowing.tick(5).is_empty());

    let retained_after_overflow = overflowing.drain_cancellations();
    assert_eq!(
        retained_after_overflow
            .iter()
            .map(|cancellation| cancellation.generation)
            .collect::<Vec<_>>(),
        vec![second, third],
        "overflow keeps the newest notifications and evicts the oldest"
    );
    assert_eq!(overflowing.diagnostics().dropped_cancellation_notifications, 1);
    assert!(!overflowing.diagnostics().counters_saturated());
    assert_eq!(overflowing.pending(&legacy), Some(fourth));
}
