//! Live modern-scheduler contract for M2 (spec 7.3, 8.1-8.12, 9.1-9.4, 11.4,
//! 13.3, 13.5, 19.4, 25.2-25.5, 26.4; roadmap M2; acceptance 31.4-31.8).
//!
//! These tests drive `crikey_input_scheduler::QueryScheduler` — the live
//! scheduler that owns query generations, per-plugin policies, in-flight
//! requests, cancellation and the developer query trace. They are deliberately
//! *not* unit tests of the pure `Debouncer`: a scheduler that merely wrapped
//! `Debouncer` would pass none of the concurrency, cancellation, invalidation
//! or coalescing-under-load assertions below, because those depend on state the
//! debouncer does not model (which generations are in flight, which were
//! cancelled, and which plugin is currently relevant).
//!
//! Conventions used throughout:
//!
//! * Time is virtual. Every timestamp is an explicit `Millis` argument; no test
//!   sleeps, reads a wall clock, or spawns a thread.
//! * `submit_query` has no dispatch side effect. It mints the generation for a
//!   new query state and records the keystroke; `tick(now)` applies intake and
//!   then fires every deadline at or before `now`. Tests therefore call
//!   `tick(t)` at the same virtual millisecond as `submit_query(.., t)` when
//!   they assert on decisions taken "at" `t`.
//! * `DispatchedRequest::query` carries the query text exactly as submitted.
//!   Gating (minimum length, prefixes, keywords, empty-query support) is
//!   evaluated against the normalized form: whitespace-trimmed and case folded.
//! * A cancelled request keeps occupying its concurrency slot until the plugin
//!   reports completion, because cancellation of a *live* plugin is
//!   cooperative (spec 9.1, 9.4). Only teardown — `disable_plugin` and
//!   `shutdown` — abandons the slot outright.

use crikey_core::{Generation, PluginId};
use crikey_input_scheduler::{
    ActivationPolicy, CancelReason, CompletionOutcome, DebounceDecision, DebouncePolicy, DispatchedRequest,
    GateReason, Millis, PluginPolicy, QueryScheduler, QueryTraceEvent, SchedulerConfig, SchedulingProfile,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn plugin(id: &str) -> PluginId {
    PluginId(id.to_string())
}

/// A scheduler with generous queue capacity: bounded-queue overflow policy is
/// covered by the resilience suite, and must not perturb scheduling assertions.
fn scheduler() -> QueryScheduler {
    QueryScheduler::new(SchedulerConfig {
        trace_capacity: 8192,
        ..SchedulerConfig::default()
    })
}

/// A `modern`-profile policy with no activation gating and enough concurrency
/// that the per-plugin request limit never interferes. Tests that exercise the
/// limit set `max_concurrent_requests` explicitly.
fn modern(
    debounce_ms: Millis,
    maximum_wait_ms: Option<Millis>,
    leading_edge: bool,
    trailing_edge: bool,
) -> PluginPolicy {
    PluginPolicy {
        profile: SchedulingProfile::Modern,
        debounce: DebouncePolicy {
            debounce_ms,
            maximum_wait_ms,
            leading_edge,
            trailing_edge,
            minimum_query_length: 0,
        },
        activation: ActivationPolicy::default(),
        max_concurrent_requests: 4,
        // The queue overflow policy stays at the crate default; this suite
        // never fills a queue.
        ..PluginPolicy::modern()
    }
}

fn serial(mut policy: PluginPolicy, limit: usize) -> PluginPolicy {
    policy.max_concurrent_requests = limit;
    policy
}

fn gated(mut policy: PluginPolicy, minimum_query_length: usize) -> PluginPolicy {
    policy.debounce.minimum_query_length = minimum_query_length;
    policy
}

// ---------------------------------------------------------------------------
// Projections
//
// Dispatch results are compared as plain tuples so a failure prints the whole
// observed schedule instead of a struct-by-struct diff. At most one request per
// plugin can be dispatched in a single tick (only the newest undispatched query
// is retained), so sorting by plugin id inside a tick is lossless and keeps
// multi-plugin assertions independent of the fair-queuing order, which the
// resilience suite owns.
// ---------------------------------------------------------------------------

fn shape(requests: &[DispatchedRequest]) -> Vec<(&str, u64, &str, Millis)> {
    let mut rows: Vec<(&str, u64, &str, Millis)> = requests
        .iter()
        .map(|request| {
            (
                request.plugin.0.as_str(),
                request.generation.get(),
                request.query.as_str(),
                request.dispatched_at,
            )
        })
        .collect();
    rows.sort_by(|left, right| left.0.cmp(right.0));
    rows
}

fn keystrokes(scheduler: &QueryScheduler) -> Vec<(Millis, u64, usize)> {
    scheduler
        .trace()
        .iter()
        .filter_map(|event| match event {
            QueryTraceEvent::Keystroke {
                at,
                generation,
                query_length,
            } => Some((*at, generation.get(), *query_length)),
            _ => None,
        })
        .collect()
}

fn decisions(scheduler: &QueryScheduler, of: &PluginId) -> Vec<(Millis, u64, DebounceDecision)> {
    scheduler
        .trace()
        .iter()
        .filter_map(|event| match event {
            QueryTraceEvent::Debounce {
                at,
                plugin,
                generation,
                decision,
            } if plugin == of => Some((*at, generation.get(), *decision)),
            _ => None,
        })
        .collect()
}

fn gates(scheduler: &QueryScheduler, of: &PluginId) -> Vec<(Millis, u64, GateReason)> {
    decisions(scheduler, of)
        .into_iter()
        .filter_map(|(at, generation, decision)| match decision {
            DebounceDecision::Gated(reason) => Some((at, generation, reason)),
            _ => None,
        })
        .collect()
}

fn dispatch_marks(scheduler: &QueryScheduler, of: &PluginId) -> Vec<(Millis, u64)> {
    scheduler
        .trace()
        .iter()
        .filter_map(|event| match event {
            QueryTraceEvent::Dispatched {
                at,
                plugin,
                generation,
            } if plugin == of => Some((*at, generation.get())),
            _ => None,
        })
        .collect()
}

fn cancellations(scheduler: &QueryScheduler, of: &PluginId) -> Vec<(Millis, u64, CancelReason)> {
    scheduler
        .trace()
        .iter()
        .filter_map(|event| match event {
            QueryTraceEvent::Cancelled {
                at,
                plugin,
                generation,
                reason,
            } if plugin == of => Some((*at, generation.get(), *reason)),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Generations
// ---------------------------------------------------------------------------

#[test]
fn every_submission_mints_a_strictly_increasing_generation() {
    // Spec 8.1: every query state receives a monotonically increasing
    // generation — including states that no plugin will ever be dispatched for.
    let repositories = plugin("dev.crikey.repositories");
    let mut scheduler = scheduler();
    scheduler.register_plugin(repositories, gated(modern(50, Some(200), true, true), 3));

    let script = [(0, "f"), (5, "fi"), (9, ""), (14, "   "), (20, "fir")];
    let mut previous = Generation::ZERO;
    for (at, text) in script {
        let generation = scheduler.submit_query(text, at);
        assert!(
            generation > previous,
            "generation for {text:?} at {at} ms must exceed the previous generation"
        );
        assert_eq!(
            scheduler.current_generation(),
            generation,
            "the scheduler's current generation must be the newest submission"
        );
        previous = generation;
        scheduler.tick(at);
    }

    // Keystroke timestamps and generations are part of the query trace
    // (spec 26.4). The recorded length is the normalized length that gating
    // reads, so padding does not inflate it.
    assert_eq!(
        keystrokes(&scheduler),
        vec![(0, 1, 1), (5, 2, 2), (9, 3, 0), (14, 4, 0), (20, 5, 3)],
        "every submission must appear in the trace, gated or not"
    );
}

// ---------------------------------------------------------------------------
// Leading edge and relevance transitions
// ---------------------------------------------------------------------------

#[test]
fn a_newly_relevant_plugin_dispatches_on_the_leading_edge() {
    // Spec 8.5: default modern behavior is immediate execution when a plugin
    // becomes newly relevant.
    let notes = plugin("dev.crikey.notes");
    let mut scheduler = scheduler();
    scheduler.register_plugin(notes.clone(), modern(50, Some(200), true, true));

    let first = scheduler.submit_query("re", 0);
    let dispatched = scheduler.tick(0);

    assert_eq!(
        shape(&dispatched),
        vec![("dev.crikey.notes", first.get(), "re", 0)],
        "the first relevant query must reach the plugin at its own timestamp"
    );
    assert_eq!(
        scheduler.in_flight(&notes),
        1,
        "the leading-edge request is now live"
    );
    assert_eq!(
        scheduler.pending(&notes),
        None,
        "a dispatched query must not stay queued as undispatched work"
    );
    assert_eq!(
        decisions(&scheduler, &notes),
        vec![(0, first.get(), DebounceDecision::LeadingEdge)],
        "the trace must attribute the dispatch to the leading edge"
    );
    assert_eq!(
        scheduler.next_wakeup(),
        None,
        "nothing is deferred, so the scheduler needs no timer"
    );
}

#[test]
fn the_leading_edge_fires_again_only_after_relevance_was_lost() {
    // Spec 8.5 + 8.11: "newly relevant" is a transition, not "the first query
    // ever". A plugin that drops out of relevance and comes back leads again;
    // one that stayed relevant is debounced.
    let repositories = plugin("dev.crikey.repositories");
    let mut scheduler = scheduler();
    scheduler.register_plugin(
        repositories.clone(),
        PluginPolicy {
            activation: ActivationPolicy {
                prefixes: vec!["repo".to_string()],
                ..ActivationPolicy::default()
            },
            ..modern(50, Some(200), true, true)
        },
    );

    let first = scheduler.submit_query("repo a", 0);
    assert_eq!(
        shape(&scheduler.tick(0)),
        vec![("dev.crikey.repositories", first.get(), "repo a", 0)]
    );
    assert_eq!(
        scheduler.complete(&repositories, first, 5),
        CompletionOutcome::Accepted
    );

    // Relevance lost: no dispatch, and the debounce state resets.
    scheduler.submit_query("xyz", 10);
    assert!(
        scheduler.tick(10).is_empty(),
        "an irrelevant query must not reach the plugin"
    );

    // Relevance regained at 20 ms: this is a leading edge, not a 70 ms trailing
    // deadline.
    let third = scheduler.submit_query("repo b", 20);
    assert_eq!(
        shape(&scheduler.tick(20)),
        vec![("dev.crikey.repositories", third.get(), "repo b", 20)],
        "regaining relevance must dispatch immediately"
    );
    assert_eq!(
        scheduler.complete(&repositories, third, 25),
        CompletionOutcome::Accepted
    );

    // Still relevant: the next keystroke is debounced, proving the leading edge
    // is not simply re-armed by every query.
    scheduler.submit_query("repo bc", 30);
    assert!(
        scheduler.tick(30).is_empty(),
        "a continued query must be debounced"
    );
    assert_eq!(
        scheduler.next_wakeup(),
        Some(80),
        "the trailing deadline is the keystroke plus the debounce interval"
    );
}

// ---------------------------------------------------------------------------
// Trailing edge, leading+trailing, maximum wait, no debounce
// ---------------------------------------------------------------------------

#[test]
fn a_trailing_only_policy_never_leads_and_waits_for_the_quiet_period() {
    // Spec 8.5: trailing-edge execution is selectable on its own.
    let weather = plugin("dev.crikey.weather");
    let mut scheduler = scheduler();
    scheduler.register_plugin(weather.clone(), modern(40, None, false, true));

    scheduler.submit_query("a", 0);
    assert!(
        scheduler.tick(0).is_empty(),
        "a trailing-only plugin must not dispatch on the first keystroke"
    );
    assert_eq!(scheduler.next_wakeup(), Some(40));

    let second = scheduler.submit_query("ab", 10);
    assert!(scheduler.tick(10).is_empty());
    assert_eq!(
        scheduler.next_wakeup(),
        Some(50),
        "each keystroke restarts the quiet period"
    );

    // Boundary: the deadline millisecond is inclusive, the one before it is not.
    assert!(
        scheduler.tick(49).is_empty(),
        "one millisecond before the deadline nothing may be dispatched"
    );
    assert_eq!(
        shape(&scheduler.tick(50)),
        vec![("dev.crikey.weather", second.get(), "ab", 50)],
        "the deadline millisecond itself must dispatch the newest query"
    );
    assert_eq!(
        dispatch_marks(&scheduler, &weather),
        vec![(50, second.get())],
        "exactly one dispatch may result from the burst"
    );
    assert_eq!(scheduler.next_wakeup(), None);
}

#[test]
fn leading_and_trailing_dispatch_both_edges_of_one_burst() {
    // Spec 8.5: leading *and* trailing execution dispatches the first and the
    // final query of a burst, and nothing in between.
    let files = plugin("dev.crikey.files");
    let mut scheduler = scheduler();
    scheduler.register_plugin(files.clone(), modern(50, Some(200), true, true));

    let first = scheduler.submit_query("a", 0);
    assert_eq!(
        shape(&scheduler.tick(0)),
        vec![("dev.crikey.files", first.get(), "a", 0)]
    );
    assert_eq!(scheduler.complete(&files, first, 5), CompletionOutcome::Accepted);

    let second = scheduler.submit_query("ab", 10);
    assert!(scheduler.tick(10).is_empty());
    let third = scheduler.submit_query("abc", 20);
    assert!(scheduler.tick(20).is_empty());

    assert!(scheduler.tick(69).is_empty(), "the quiet period ends at 70 ms");
    assert_eq!(
        shape(&scheduler.tick(70)),
        vec![("dev.crikey.files", third.get(), "abc", 70)],
        "the trailing edge must carry the newest query of the burst"
    );
    assert_eq!(
        dispatch_marks(&scheduler, &files),
        vec![(0, first.get()), (70, third.get())],
        "only the first and last query of the burst may be dispatched"
    );
    assert!(
        !dispatch_marks(&scheduler, &files)
            .iter()
            .any(|(_, generation)| *generation == second.get()),
        "the intermediate generation must never be dispatched"
    );
}

#[test]
fn leading_and_trailing_maximum_wait_includes_the_leading_edge() {
    let search = plugin("dev.crikey.leading-maximum");
    let mut scheduler = scheduler();
    scheduler.register_plugin(search.clone(), modern(50, Some(120), true, true));

    let first = scheduler.submit_query("x", 0);
    assert_eq!(shape(&scheduler.tick(0)).len(), 1);
    let mut latest_query = String::from("x");
    for at in (10..=100).step_by(10) {
        latest_query.push('x');
        scheduler.submit_query(&latest_query, at);
        assert!(scheduler.tick(at).is_empty());
    }
    assert_eq!(scheduler.complete(&search, first, 110), CompletionOutcome::Stale);
    assert!(scheduler.tick(119).is_empty());
    let latest = scheduler.current_generation();
    assert_eq!(
        shape(&scheduler.tick(120)),
        vec![(
            "dev.crikey.leading-maximum",
            latest.get(),
            latest_query.as_str(),
            120
        )]
    );
    assert_eq!(
        decisions(&scheduler, &search).last().copied(),
        Some((120, latest.get(), DebounceDecision::MaximumWait))
    );
}

#[test]
fn the_maximum_wait_forces_dispatch_under_sustained_typing() {
    // Spec 8.6 + 25.1: continuous input must not postpone a plugin
    // indefinitely, and the burst window restarts after the forced dispatch.
    let search = plugin("dev.crikey.websearch");
    let mut scheduler = scheduler();
    scheduler.register_plugin(search.clone(), modern(50, Some(120), false, true));

    let mut typed = String::new();
    let mut latest = Generation::ZERO;
    for step in 0..12u64 {
        let at = step * 10;
        typed.push('x');
        latest = scheduler.submit_query(&typed, at);
        assert!(
            scheduler.tick(at).is_empty(),
            "typing never paused for the 50 ms debounce, so nothing may dispatch at {at} ms"
        );
        assert_eq!(
            scheduler.next_wakeup(),
            Some((at + 50).min(120)),
            "the wake-up is the earlier of the quiet period and the maximum wait"
        );
    }

    assert!(
        scheduler.tick(119).is_empty(),
        "the maximum wait has not elapsed one millisecond early"
    );
    let forced = scheduler.tick(120);
    assert_eq!(
        shape(&forced),
        vec![("dev.crikey.websearch", latest.get(), typed.as_str(), 120)],
        "the maximum wait must dispatch the latest query, not the oldest"
    );
    assert_eq!(
        decisions(&scheduler, &search).last().copied(),
        Some((120, latest.get(), DebounceDecision::MaximumWait)),
        "the trace must attribute the dispatch to the maximum wait"
    );

    // The burst window restarts: the next keystroke gets a full quiet period
    // rather than being force-dispatched immediately.
    typed.push('y');
    scheduler.submit_query(&typed, 130);
    assert!(scheduler.tick(130).is_empty());
    assert_eq!(
        scheduler.next_wakeup(),
        Some(180),
        "a forced dispatch must reset the maximum-wait window"
    );
}

#[test]
fn a_zero_debounce_policy_dispatches_every_query_change_at_its_own_timestamp() {
    // Spec 8.2 + 25.4: the core catalog and cached plugins declare 0 ms and
    // must not be delayed by a scheduler tick boundary.
    let catalog = plugin("dev.crikey.catalog");
    let mut scheduler = scheduler();
    scheduler.register_plugin(catalog.clone(), modern(0, None, true, true));

    let mut expected = Vec::new();
    let mut typed = String::new();
    for at in 0..3u64 {
        typed.push('a');
        let generation = scheduler.submit_query(&typed, at);
        let dispatched = scheduler.tick(at);
        assert_eq!(dispatched.len(), 1, "a 0 ms policy must dispatch at {at} ms");
        assert_eq!(dispatched[0].generation, generation);
        assert_eq!(dispatched[0].dispatched_at, at);
        assert_eq!(dispatched[0].query, typed);
        expected.push((at, generation.get()));
        assert_eq!(
            scheduler.complete(&catalog, generation, at),
            CompletionOutcome::Accepted
        );
    }

    assert_eq!(dispatch_marks(&scheduler, &catalog), expected);
    assert_eq!(
        scheduler.diagnostics().coalesced_requests,
        0,
        "nothing can be coalesced when every query is dispatched immediately"
    );
    assert_eq!(scheduler.next_wakeup(), None);
}

#[test]
fn an_elapsed_deadline_dispatches_once_on_the_next_tick_it_sees() {
    // A scheduler woken late (a busy frame, a coarse timer) must still dispatch
    // exactly once and must stamp the request with the tick it actually ran on,
    // not with a deadline that has already passed.
    let history = plugin("dev.crikey.history");
    let mut scheduler = scheduler();
    scheduler.register_plugin(history.clone(), modern(50, None, false, true));

    let generation = scheduler.submit_query("late", 200);
    assert!(scheduler.tick(200).is_empty());
    assert_eq!(scheduler.next_wakeup(), Some(250));

    assert_eq!(
        shape(&scheduler.tick(10_000)),
        vec![("dev.crikey.history", generation.get(), "late", 10_000)],
        "a late tick must dispatch and record the real dispatch timestamp"
    );
    assert!(
        scheduler.tick(10_001).is_empty(),
        "an elapsed deadline must not dispatch twice"
    );
    assert_eq!(scheduler.next_wakeup(), None);
}

// ---------------------------------------------------------------------------
// Coalescing
// ---------------------------------------------------------------------------

#[test]
fn only_the_newest_undispatched_query_survives_coalescing() {
    // Spec 8.8 + 31.5: older undispatched queries are replaced, never queued.
    let translate = plugin("dev.crikey.translate");
    let mut scheduler = scheduler();
    scheduler.register_plugin(translate.clone(), modern(50, Some(400), false, true));

    let mut generations = Vec::new();
    let mut typed = String::new();
    for at in [0, 5, 10, 15] {
        typed.push('a');
        generations.push(scheduler.submit_query(&typed, at));
        assert!(scheduler.tick(at).is_empty());
        assert_eq!(
            scheduler.pending(&translate),
            generations.last().copied(),
            "the pending request must always be the newest query"
        );
    }

    let newest = *generations.last().expect("four queries were submitted");
    assert_eq!(
        shape(&scheduler.tick(65)),
        vec![("dev.crikey.translate", newest.get(), "aaaa", 65)],
        "the quiet period must dispatch the newest query exactly once"
    );
    assert_eq!(
        dispatch_marks(&scheduler, &translate),
        vec![(65, newest.get())],
        "no superseded generation may reach the plugin"
    );
    assert_eq!(
        decisions(&scheduler, &translate),
        vec![
            (0, generations[0].get(), DebounceDecision::Deferred { until: 50 }),
            (
                5,
                generations[1].get(),
                DebounceDecision::Coalesced {
                    superseded: generations[0]
                }
            ),
            (
                10,
                generations[2].get(),
                DebounceDecision::Coalesced {
                    superseded: generations[1]
                }
            ),
            (
                15,
                generations[3].get(),
                DebounceDecision::Coalesced {
                    superseded: generations[2]
                }
            ),
            (65, newest.get(), DebounceDecision::TrailingEdge),
        ],
        "each supersession must be traceable to the generation it replaced"
    );
    assert_eq!(scheduler.diagnostics().coalesced_requests, 3);
    assert_eq!(scheduler.pending(&translate), None);
}

// ---------------------------------------------------------------------------
// Host gating: minimum length, empty query, prefixes and keywords
// ---------------------------------------------------------------------------

#[test]
fn a_minimum_normalized_query_length_gates_dispatch() {
    // Spec 8.10 + 19.4: the gate reads the normalized query, so padding does
    // not satisfy it.
    let emoji = plugin("dev.crikey.emoji");
    let mut scheduler = scheduler();
    scheduler.register_plugin(emoji.clone(), gated(modern(50, Some(200), true, true), 3));

    let short = scheduler.submit_query("ab", 0);
    assert!(scheduler.tick(0).is_empty(), "two characters are below the gate");
    assert_eq!(scheduler.pending(&emoji), None, "a gated query must not queue");
    assert_eq!(
        scheduler.next_wakeup(),
        None,
        "a gated query must not arm a timer"
    );

    let padded = scheduler.submit_query("  ab  ", 10);
    assert!(
        scheduler.tick(10).is_empty(),
        "whitespace padding must not satisfy a minimum length"
    );

    let long = scheduler.submit_query("abc", 20);
    assert_eq!(
        shape(&scheduler.tick(20)),
        vec![("dev.crikey.emoji", long.get(), "abc", 20)],
        "crossing the gate makes the plugin newly relevant"
    );

    // Falling back below the gate invalidates the in-flight request.
    let below = scheduler.submit_query("ab", 30);
    assert!(scheduler.tick(30).is_empty());
    assert_eq!(
        cancellations(&scheduler, &emoji),
        vec![(30, long.get(), CancelReason::NoLongerRelevant)],
        "work for a plugin that stopped being relevant must be cancelled"
    );
    assert_eq!(
        scheduler.in_flight(&emoji),
        1,
        "a cooperative cancellation keeps the slot until the plugin answers"
    );
    assert_eq!(
        scheduler.complete(&emoji, long, 40),
        CompletionOutcome::Stale,
        "results for a cancelled generation must be reported as unusable"
    );
    assert_eq!(scheduler.in_flight(&emoji), 0);

    assert_eq!(
        gates(&scheduler, &emoji),
        vec![
            (0, short.get(), GateReason::MinimumQueryLength),
            (10, padded.get(), GateReason::MinimumQueryLength),
            (30, below.get(), GateReason::MinimumQueryLength),
        ]
    );
}

#[test]
fn empty_query_support_must_be_declared_per_plugin() {
    // Spec 8.9 + 19.4: empty-query support is explicit, so an undeclared plugin
    // is simply not relevant to an empty query.
    let declared = plugin("dev.crikey.clipboard");
    let undeclared = plugin("dev.crikey.calculator");
    let mut scheduler = scheduler();
    scheduler.register_plugin(
        declared.clone(),
        PluginPolicy {
            activation: ActivationPolicy {
                supports_empty_query: true,
                ..ActivationPolicy::default()
            },
            ..modern(50, Some(200), true, true)
        },
    );
    scheduler.register_plugin(undeclared.clone(), modern(50, Some(200), true, true));

    let first = scheduler.submit_query("", 0);
    assert_eq!(
        shape(&scheduler.tick(0)),
        vec![("dev.crikey.clipboard", first.get(), "", 0)],
        "only the plugin that declared empty-query support may be invoked"
    );
    assert_eq!(
        gates(&scheduler, &undeclared),
        vec![(0, first.get(), GateReason::EmptyQueryUnsupported)]
    );

    // A non-empty query makes the undeclared plugin newly relevant; the
    // declared one is already relevant and is therefore debounced.
    let second = scheduler.submit_query("a", 10);
    assert_eq!(
        shape(&scheduler.tick(10)),
        vec![("dev.crikey.calculator", second.get(), "a", 10)],
        "the newly relevant plugin leads while the running one waits"
    );
    assert_eq!(
        shape(&scheduler.tick(60)),
        vec![("dev.crikey.clipboard", second.get(), "a", 60)],
        "the already-relevant plugin dispatches on its trailing edge"
    );

    // Whitespace normalizes to empty, so the undeclared plugin loses relevance
    // and its in-flight request is invalidated.
    let third = scheduler.submit_query("   ", 70);
    assert!(
        shape(&scheduler.tick(70))
            .iter()
            .all(|(id, ..)| *id != "dev.crikey.calculator"),
        "a whitespace-only query must be treated as empty"
    );
    assert_eq!(
        cancellations(&scheduler, &undeclared),
        vec![(70, second.get(), CancelReason::NoLongerRelevant)]
    );
    assert_eq!(
        shape(&scheduler.tick(120)),
        vec![("dev.crikey.clipboard", third.get(), "   ", 120)],
        "the declared plugin still serves the empty query, verbatim"
    );
}

#[test]
fn prefix_and_keyword_activation_decide_relevance() {
    // Spec 8.11 + 19.4: declared activation metadata keeps plugins out of
    // queries they cannot serve. Matching is on normalized tokens, not on a
    // naive substring test.
    let repositories = plugin("dev.crikey.repositories");
    let git = plugin("dev.crikey.git");
    let mut scheduler = scheduler();
    scheduler.register_plugin(
        repositories.clone(),
        PluginPolicy {
            activation: ActivationPolicy {
                prefixes: vec!["repo".to_string()],
                ..ActivationPolicy::default()
            },
            ..modern(0, None, true, true)
        },
    );
    scheduler.register_plugin(
        git.clone(),
        PluginPolicy {
            activation: ActivationPolicy {
                keywords: vec!["git".to_string()],
                ..ActivationPolicy::default()
            },
            ..modern(0, None, true, true)
        },
    );

    let unrelated = scheduler.submit_query("notes", 0);
    assert!(
        scheduler.tick(0).is_empty(),
        "a query matching no activation metadata must reach no plugin"
    );

    let embedded = scheduler.submit_query("xrepo", 10);
    assert!(
        scheduler.tick(10).is_empty(),
        "a prefix must be at the start of the query, not anywhere in it"
    );

    let repo = scheduler.submit_query("REPO crikey", 20);
    assert_eq!(
        shape(&scheduler.tick(20)),
        vec![("dev.crikey.repositories", repo.get(), "REPO crikey", 20)],
        "prefix matching is case insensitive and the plugin receives the raw text"
    );

    let digital = scheduler.submit_query("digital", 30);
    assert!(
        scheduler.tick(30).is_empty(),
        "a keyword must match a whole token, never a substring of one"
    );
    assert_eq!(
        cancellations(&scheduler, &repositories),
        vec![(30, repo.get(), CancelReason::NoLongerRelevant)],
        "leaving the prefix must invalidate the repository request"
    );
    assert_eq!(
        scheduler.complete(&repositories, repo, 35),
        CompletionOutcome::Stale
    );

    let keyworded = scheduler.submit_query("git status", 40);
    assert_eq!(
        shape(&scheduler.tick(40)),
        vec![("dev.crikey.git", keyworded.get(), "git status", 40)],
        "the keyword plugin becomes newly relevant and leads"
    );

    assert_eq!(
        gates(&scheduler, &repositories),
        vec![
            (0, unrelated.get(), GateReason::PrefixMismatch),
            (10, embedded.get(), GateReason::PrefixMismatch),
            (30, digital.get(), GateReason::PrefixMismatch),
            (40, keyworded.get(), GateReason::PrefixMismatch),
        ]
    );
    assert_eq!(
        gates(&scheduler, &git),
        vec![
            (0, unrelated.get(), GateReason::KeywordMismatch),
            (10, embedded.get(), GateReason::KeywordMismatch),
            (20, repo.get(), GateReason::KeywordMismatch),
            (30, digital.get(), GateReason::KeywordMismatch),
        ]
    );
}

#[test]
fn combined_activation_uses_prefix_or_first_token_keyword_gating() {
    let combined = plugin("dev.crikey.combined-activation");
    let mut scheduler = scheduler();
    scheduler.register_plugin(
        combined.clone(),
        PluginPolicy {
            activation: ActivationPolicy {
                prefixes: vec![" REPO ".to_string()],
                keywords: vec![" GH ".to_string()],
                ..ActivationPolicy::default()
            },
            ..modern(0, None, true, true)
        },
    );

    let prefix = scheduler.submit_query("repo crikey", 0);
    assert_eq!(
        shape(&scheduler.tick(0)),
        vec![("dev.crikey.combined-activation", prefix.get(), "repo crikey", 0)],
        "a matching prefix admits even when the first token is not a keyword"
    );
    assert_eq!(
        scheduler.complete(&combined, prefix, 1),
        CompletionOutcome::Accepted
    );

    let keyword = scheduler.submit_query("GH issues", 10);
    assert_eq!(
        shape(&scheduler.tick(10)),
        vec![("dev.crikey.combined-activation", keyword.get(), "GH issues", 10)],
        "a matching first-token keyword admits even when no prefix matches"
    );
    assert_eq!(
        scheduler.complete(&combined, keyword, 11),
        CompletionOutcome::Accepted
    );

    let later_keyword = scheduler.submit_query("open gh", 20);
    assert!(
        scheduler.tick(20).is_empty(),
        "a keyword in a later token must not pass the manifest-compatible gate"
    );
    assert_eq!(
        gates(&scheduler, &combined),
        vec![(20, later_keyword.get(), GateReason::PrefixMismatch)]
    );
}

// ---------------------------------------------------------------------------
// Declared concurrency
// ---------------------------------------------------------------------------

#[test]
fn a_serial_plugin_holds_back_dispatch_until_its_slot_is_free() {
    // Spec 13.3 + 13.5 + 19.4: a plugin declaring one simultaneous suggestion
    // request must never receive a second one concurrently, whatever the
    // debounce policy says.
    let python = plugin("dev.crikey.python");
    let mut scheduler = scheduler();
    scheduler.register_plugin(python.clone(), serial(modern(0, None, true, true), 1));

    let first = scheduler.submit_query("a", 0);
    assert_eq!(
        shape(&scheduler.tick(0)),
        vec![("dev.crikey.python", first.get(), "a", 0)]
    );

    let second = scheduler.submit_query("ab", 10);
    assert!(
        scheduler.tick(10).is_empty(),
        "a 0 ms debounce must still respect the concurrency limit"
    );
    assert_eq!(scheduler.in_flight(&python), 1, "the limit must not be exceeded");
    assert_eq!(
        scheduler.pending(&python),
        Some(second),
        "the blocked query waits as the single pending request"
    );
    assert!(
        scheduler.tick(1_000).is_empty(),
        "time alone must not release a request blocked by concurrency"
    );

    assert_eq!(
        scheduler.complete(&python, first, 1_010),
        CompletionOutcome::Stale,
        "the first request was superseded before it finished"
    );
    assert_eq!(scheduler.in_flight(&python), 0);
    assert_eq!(
        shape(&scheduler.tick(1_010)),
        vec![("dev.crikey.python", second.get(), "ab", 1_010)],
        "completion must release the pending request"
    );
}

#[test]
fn a_concurrency_limit_of_two_admits_exactly_two_in_flight_requests() {
    // Spec 13.5: the limit is a count, not a boolean. Two requests may be live;
    // a third waits for a completion even though its debounce elapsed.
    let native = plugin("dev.crikey.native");
    let mut scheduler = scheduler();
    scheduler.register_plugin(native.clone(), serial(modern(0, None, true, true), 2));

    let first = scheduler.submit_query("a", 0);
    assert_eq!(shape(&scheduler.tick(0)).len(), 1);
    let second = scheduler.submit_query("ab", 10);
    assert_eq!(
        shape(&scheduler.tick(10)),
        vec![("dev.crikey.native", second.get(), "ab", 10)],
        "the second slot is available"
    );
    assert_eq!(scheduler.in_flight(&native), 2);

    let third = scheduler.submit_query("abc", 20);
    assert!(scheduler.tick(20).is_empty(), "both slots are occupied");
    assert_eq!(scheduler.in_flight(&native), 2);
    assert_eq!(scheduler.pending(&native), Some(third));

    assert_eq!(scheduler.complete(&native, first, 30), CompletionOutcome::Stale);
    assert_eq!(scheduler.in_flight(&native), 1);
    assert_eq!(
        shape(&scheduler.tick(30)),
        vec![("dev.crikey.native", third.get(), "abc", 30)],
        "the freed slot takes the newest pending query"
    );
    assert_eq!(scheduler.in_flight(&native), 2);
    assert_eq!(
        scheduler.complete(&native, second, 40),
        CompletionOutcome::Stale,
        "the second request was superseded by the third"
    );
    assert_eq!(
        scheduler.complete(&native, third, 50),
        CompletionOutcome::Accepted,
        "the newest request still matches the visible query"
    );
    assert_eq!(scheduler.in_flight(&native), 0);
}

#[test]
fn a_freed_slot_dispatches_the_newest_generation_and_never_an_intermediate_one() {
    // Spec 8.8 + 31.5: work blocked behind a busy plugin is coalesced exactly
    // like work blocked behind a debounce interval.
    let python = plugin("dev.crikey.python");
    let mut scheduler = scheduler();
    scheduler.register_plugin(python.clone(), serial(modern(0, None, true, true), 1));

    let first = scheduler.submit_query("a", 0);
    assert_eq!(shape(&scheduler.tick(0)).len(), 1);

    let mut blocked = Vec::new();
    let mut typed = String::from("a");
    for at in [5, 10, 15] {
        typed.push('b');
        blocked.push(scheduler.submit_query(&typed, at));
        assert!(scheduler.tick(at).is_empty());
    }
    let newest = *blocked.last().expect("three queries were blocked");
    assert_eq!(scheduler.pending(&python), Some(newest));

    assert_eq!(scheduler.complete(&python, first, 20), CompletionOutcome::Stale);
    let released = scheduler.tick(20);
    assert_eq!(
        shape(&released),
        vec![("dev.crikey.python", newest.get(), "abbb", 20)],
        "only the newest blocked query may be dispatched"
    );
    assert_eq!(
        dispatch_marks(&scheduler, &python),
        vec![(0, first.get()), (20, newest.get())],
        "no intermediate generation may reach the plugin"
    );
    assert_eq!(scheduler.diagnostics().coalesced_requests, 2);
    assert!(
        scheduler.tick(1_000).is_empty(),
        "the released request must not be dispatched a second time"
    );
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

#[test]
fn changing_the_query_cancels_the_in_flight_generation() {
    // Spec 9.1 + 9.3 + 31.6: obsolete in-flight work is invalidated as soon as
    // the query moves on, and its eventual results are reported as stale.
    let notes = plugin("dev.crikey.notes");
    let mut scheduler = scheduler();
    scheduler.register_plugin(notes.clone(), modern(0, None, true, true));

    let first = scheduler.submit_query("a", 0);
    assert_eq!(shape(&scheduler.tick(0)).len(), 1);
    assert!(
        cancellations(&scheduler, &notes).is_empty(),
        "a live request must not be cancelled before the query changes"
    );

    let second = scheduler.submit_query("ab", 10);
    assert_eq!(shape(&scheduler.tick(10)).len(), 1);
    assert_eq!(
        cancellations(&scheduler, &notes),
        vec![(10, first.get(), CancelReason::QueryChanged)],
        "the superseded generation must be cancelled at the keystroke timestamp"
    );
    assert_eq!(scheduler.diagnostics().cancelled_requests, 1);

    assert_eq!(
        scheduler.complete(&notes, first, 20),
        CompletionOutcome::Stale,
        "the cancelled generation must never be reported as usable"
    );
    assert_eq!(
        scheduler.complete(&notes, first, 21),
        CompletionOutcome::Unknown,
        "a request may only be completed once"
    );
    assert_eq!(
        scheduler.complete(&notes, second, 25),
        CompletionOutcome::Accepted,
        "the current generation is still usable"
    );
    assert_eq!(
        scheduler.complete(&notes, scheduler.current_generation(), 26),
        CompletionOutcome::Unknown,
        "a generation that was never dispatched to this plugin is unknown"
    );
    assert_eq!(
        cancellations(&scheduler, &notes),
        vec![(10, first.get(), CancelReason::QueryChanged)],
        "completing a request must not manufacture further cancellations"
    );
}

#[test]
fn repeated_invalidation_emits_one_cancellation_notification_and_trace() {
    let notes = plugin("dev.crikey.cancel-once");
    let mut scheduler = scheduler();
    scheduler.register_plugin(notes.clone(), modern(0, None, true, true));

    let running = scheduler.submit_query("a", 0);
    assert_eq!(shape(&scheduler.tick(0)).len(), 1);
    assert_eq!(
        scheduler.cancel_plugin(&notes, CancelReason::QueryChanged, 10),
        vec![running]
    );

    let notifications = scheduler.drain_cancellations();
    assert_eq!(notifications.len(), 1);
    assert_eq!(notifications[0].plugin, notes);
    assert_eq!(notifications[0].generation, running);
    assert_eq!(notifications[0].reason, CancelReason::QueryChanged);
    assert_eq!(notifications[0].cancelled_at, 10);

    assert_eq!(
        scheduler.cancel_plugin(&notes, CancelReason::Disabled, 20),
        vec![running],
        "the already-invalid generation remains discoverable without another transition"
    );
    scheduler.set_policy(&notes, modern(50, Some(200), true, true), 30);
    assert!(
        scheduler.drain_cancellations().is_empty(),
        "repeated invalidation must not re-notify a cancellation already drained"
    );
    assert_eq!(
        cancellations(&scheduler, &notes),
        vec![(10, running.get(), CancelReason::QueryChanged)],
        "the first cancellation remains the single trace truth"
    );
    assert_eq!(scheduler.diagnostics().cancelled_requests, 1);
    assert_eq!(
        scheduler
            .plugin_diagnostics(&notes)
            .expect("registered plugin")
            .cancelled_requests,
        1
    );
}

#[test]
fn reconfiguring_a_plugin_invalidates_its_work_and_applies_the_new_policy() {
    // Spec 9.3: relevant configuration changes cancel outstanding work.
    // Spec 21.4: the replacement policy takes effect for the next query.
    let network = plugin("dev.crikey.network");
    let mut scheduler = scheduler();
    scheduler.register_plugin(network.clone(), modern(0, None, true, true));

    let first = scheduler.submit_query("a", 0);
    assert_eq!(shape(&scheduler.tick(0)).len(), 1);

    scheduler.set_policy(&network, modern(200, Some(400), true, true), 10);
    assert_eq!(
        cancellations(&scheduler, &network),
        vec![(10, first.get(), CancelReason::Reconfigured)],
        "a configuration change must invalidate the running request"
    );
    assert_eq!(
        scheduler.complete(&network, first, 15),
        CompletionOutcome::Stale,
        "results produced under the old policy must not be displayed"
    );

    // Reconfiguration resets relevance, so the next query leads again.
    let second = scheduler.submit_query("ab", 20);
    assert_eq!(
        shape(&scheduler.tick(20)),
        vec![("dev.crikey.network", second.get(), "ab", 20)]
    );
    scheduler.submit_query("abc", 30);
    assert!(scheduler.tick(30).is_empty());
    assert_eq!(
        scheduler.next_wakeup(),
        Some(230),
        "the new 200 ms debounce interval must govern the next dispatch"
    );
}

#[test]
fn changing_the_scheduling_profile_invalidates_in_flight_work() {
    // Spec 9.3 + 7.1: switching profiles cancels outstanding work, and the new
    // profile's dispatch rules apply immediately — legacy-strict is never
    // time-debounced (spec 8.4, 25.4, 31.14).
    let converted = plugin("dev.crikey.converted");
    let mut scheduler = scheduler();
    scheduler.register_plugin(converted.clone(), modern(50, Some(200), true, true));

    let first = scheduler.submit_query("a", 0);
    assert_eq!(shape(&scheduler.tick(0)).len(), 1);

    scheduler.set_policy(&converted, PluginPolicy::legacy_strict(), 10);
    assert_eq!(
        cancellations(&scheduler, &converted),
        vec![(10, first.get(), CancelReason::ProfileChanged)],
        "a profile change is distinguishable from an ordinary reconfiguration"
    );
    assert_eq!(
        scheduler.complete(&converted, first, 15),
        CompletionOutcome::Stale
    );

    let second = scheduler.submit_query("ab", 20);
    assert_eq!(
        shape(&scheduler.tick(20)),
        vec![("dev.crikey.converted", second.get(), "ab", 20)],
        "legacy-strict dispatches promptly rather than after a debounce interval"
    );
    assert_eq!(
        scheduler.next_wakeup(),
        None,
        "a legacy-strict plugin must never arm a debounce timer"
    );
}

#[test]
fn disabling_a_plugin_abandons_its_work_and_suppresses_dispatch() {
    // Spec 9.3: disabling cancels outstanding work. Unlike a superseded query,
    // a disabled plugin will never answer, so its slot must not stay reserved.
    let flaky = plugin("dev.crikey.flaky");
    let mut scheduler = scheduler();
    scheduler.register_plugin(flaky.clone(), serial(modern(0, None, true, true), 1));

    let first = scheduler.submit_query("a", 0);
    assert_eq!(shape(&scheduler.tick(0)).len(), 1);

    scheduler.disable_plugin(&flaky, 10);
    assert_eq!(
        cancellations(&scheduler, &flaky),
        vec![(10, first.get(), CancelReason::Disabled)]
    );
    assert_eq!(
        scheduler.in_flight(&flaky),
        0,
        "a disabled plugin must not keep a concurrency slot reserved"
    );
    assert_eq!(
        scheduler.complete(&flaky, first, 15),
        CompletionOutcome::Unknown,
        "a late answer from a disabled plugin belongs to no live request"
    );

    let while_disabled = scheduler.submit_query("ab", 20);
    assert!(
        scheduler.tick(20).is_empty(),
        "a disabled plugin must receive no queries"
    );
    assert_eq!(scheduler.pending(&flaky), None);
    assert_eq!(
        gates(&scheduler, &flaky),
        vec![(20, while_disabled.get(), GateReason::Disabled)]
    );

    scheduler.enable_plugin(&flaky, 30);
    let after = scheduler.submit_query("abc", 40);
    assert_eq!(
        shape(&scheduler.tick(40)),
        vec![("dev.crikey.flaky", after.get(), "abc", 40)],
        "re-enabling makes the plugin newly relevant again"
    );
}

#[test]
fn shutdown_cancels_every_plugin_and_stops_all_further_dispatch() {
    // Spec 9.3: CriKey closing invalidates every outstanding request, across
    // plugins and generations.
    let fast = plugin("dev.crikey.catalog");
    let slow = plugin("dev.crikey.websearch");
    let mut scheduler = scheduler();
    scheduler.register_plugin(fast.clone(), modern(0, None, true, true));
    scheduler.register_plugin(slow.clone(), modern(0, None, true, true));

    let first = scheduler.submit_query("a", 0);
    assert_eq!(shape(&scheduler.tick(0)).len(), 2);
    let second = scheduler.submit_query("ab", 10);
    assert_eq!(shape(&scheduler.tick(10)).len(), 2);
    assert_eq!(scheduler.in_flight(&fast), 2);
    assert_eq!(scheduler.in_flight(&slow), 2);

    scheduler.shutdown(50);

    for owner in [&fast, &slow] {
        let cancelled = cancellations(&scheduler, owner);
        assert!(
            cancelled.contains(&(50, second.get(), CancelReason::Shutdown)),
            "shutdown must cancel the live generation of {owner:?}, saw {cancelled:?}"
        );
        assert!(
            cancelled.contains(&(10, first.get(), CancelReason::QueryChanged)),
            "the generation superseded at 10 ms keeps its original cancellation reason"
        );
        assert_eq!(
            scheduler.in_flight(owner),
            0,
            "shutdown must release slots: a torn-down plugin will never answer"
        );
    }
    assert_eq!(scheduler.next_wakeup(), None);

    scheduler.submit_query("abc", 60);
    assert!(
        scheduler.tick(60).is_empty(),
        "a shut-down scheduler dispatches nothing"
    );
    assert!(
        scheduler.tick(10_000).is_empty(),
        "and never resumes on a later tick"
    );
}

// ---------------------------------------------------------------------------
// Cross-plugin independence and the query trace
// ---------------------------------------------------------------------------

#[test]
fn a_slow_plugin_never_delays_a_fast_one() {
    // Spec 13.6 + 31.8: an outstanding slow request must not hold back a fast
    // plugin's next dispatch, and `next_wakeup` must report the earliest
    // deadline across every plugin.
    let fast = plugin("dev.crikey.catalog");
    let slow = plugin("dev.crikey.websearch");
    let mut scheduler = scheduler();
    scheduler.register_plugin(fast.clone(), modern(20, None, false, true));
    scheduler.register_plugin(slow.clone(), modern(200, Some(300), false, true));

    let first = scheduler.submit_query("n", 0);
    assert!(scheduler.tick(0).is_empty());
    assert_eq!(
        scheduler.next_wakeup(),
        Some(20),
        "the earliest deadline across plugins must be reported"
    );

    assert_eq!(
        shape(&scheduler.tick(20)),
        vec![("dev.crikey.catalog", first.get(), "n", 20)],
        "the fast plugin must not wait for the slow one"
    );
    assert_eq!(scheduler.next_wakeup(), Some(200));
    assert!(scheduler.tick(199).is_empty());
    assert_eq!(
        shape(&scheduler.tick(200)),
        vec![("dev.crikey.websearch", first.get(), "n", 200)]
    );

    // The slow plugin never answers. The fast one keeps being served.
    assert_eq!(scheduler.complete(&fast, first, 205), CompletionOutcome::Accepted);
    let second = scheduler.submit_query("note", 210);
    assert!(scheduler.tick(210).is_empty());
    assert_eq!(
        shape(&scheduler.tick(230)),
        vec![("dev.crikey.catalog", second.get(), "note", 230)],
        "a stuck slow plugin must not block the fast plugin's next dispatch"
    );
    assert_eq!(
        scheduler.in_flight(&slow),
        1,
        "the slow request is still outstanding while the fast plugin advances"
    );
    assert_eq!(
        scheduler.next_wakeup(),
        Some(410),
        "the slow plugin keeps its own, later deadline"
    );
}

#[test]
fn the_query_trace_records_the_modern_scheduling_decisions() {
    // Spec 26.4: keystroke timestamps, query generations, modern debounce
    // decisions, plugin dispatch timestamps and cancellation timestamps must
    // all be inspectable for one query session.
    let notes = plugin("dev.crikey.notes");
    let mut scheduler = scheduler();
    scheduler.register_plugin(notes.clone(), modern(50, Some(200), true, true));

    let first = scheduler.submit_query("a", 0);
    scheduler.tick(0);
    let second = scheduler.submit_query("ab", 10);
    scheduler.tick(10);
    let third = scheduler.submit_query("abc", 20);
    scheduler.tick(20);
    scheduler.tick(70);

    assert_eq!(
        keystrokes(&scheduler),
        vec![(0, first.get(), 1), (10, second.get(), 2), (20, third.get(), 3)]
    );
    assert_eq!(
        decisions(&scheduler, &notes),
        vec![
            (0, first.get(), DebounceDecision::LeadingEdge),
            (10, second.get(), DebounceDecision::Deferred { until: 60 }),
            (
                20,
                third.get(),
                DebounceDecision::Coalesced { superseded: second }
            ),
            (70, third.get(), DebounceDecision::TrailingEdge),
        ],
        "every intake and dispatch decision must be traceable"
    );
    assert_eq!(
        dispatch_marks(&scheduler, &notes),
        vec![(0, first.get()), (70, third.get())]
    );
    assert_eq!(
        cancellations(&scheduler, &notes),
        vec![(10, first.get(), CancelReason::QueryChanged)]
    );
}

// ---------------------------------------------------------------------------
// Rapid-typing regression: no cross-generation reordering
// ---------------------------------------------------------------------------

#[test]
fn a_full_typing_session_never_dispatches_a_superseded_generation() {
    // Roadmap M2 exit criteria and acceptance 31.4-31.8, from the scheduler's
    // side: over a realistic burst-pause-correct-burst session with three very
    // different policies and completion latencies, every dispatch must carry
    // the newest generation that existed when it left, generations must never
    // regress per plugin, the undispatched backlog must stay bounded, and every
    // plugin must end up holding the final query.
    let core = plugin("dev.crikey.catalog");
    let python = plugin("dev.crikey.python");
    let network = plugin("dev.crikey.network");

    let mut scheduler = scheduler();
    scheduler.register_plugin(core.clone(), serial(modern(0, None, true, true), 4));
    scheduler.register_plugin(python.clone(), serial(modern(60, Some(240), true, true), 2));
    scheduler.register_plugin(network.clone(), serial(modern(200, Some(400), true, true), 1));

    // Deterministic script: type a phrase, pause, correct it, type again.
    let mut script: Vec<(Millis, String)> = Vec::new();
    let mut typed = String::new();
    let mut at: Millis = 0;
    for character in "visual studio code".chars() {
        typed.push(character);
        script.push((at, typed.clone()));
        at += 7;
    }
    at += 300;
    for _ in 0..4 {
        typed.pop();
        script.push((at, typed.clone()));
        at += 5;
    }
    at += 120;
    for character in " ins".chars() {
        typed.push(character);
        script.push((at, typed.clone()));
        at += 6;
    }
    let final_text = typed.clone();
    let end = at + 1_500;

    // Completion latency per plugin: a fast local index, a Python worker and a
    // network-backed plugin (spec 25.2).
    let latency = |owner: &PluginId| match owner.0.as_str() {
        "dev.crikey.catalog" => 2,
        "dev.crikey.python" => 35,
        _ => 150,
    };

    let mut submitted: Vec<(Millis, Generation)> = Vec::new();
    let mut dispatched: Vec<DispatchedRequest> = Vec::new();
    let mut outstanding: Vec<(Millis, PluginId, Generation)> = Vec::new();
    let mut peak_backlog = 0usize;
    let mut cursor = 0usize;

    for now in 0..=end {
        outstanding.retain(|(due, owner, generation)| {
            if *due > now {
                return true;
            }
            scheduler.complete(owner, *generation, now);
            false
        });

        while cursor < script.len() && script[cursor].0 == now {
            let generation = scheduler.submit_query(&script[cursor].1, now);
            submitted.push((now, generation));
            cursor += 1;
        }

        for request in scheduler.tick(now) {
            outstanding.push((
                now + latency(&request.plugin),
                request.plugin.clone(),
                request.generation,
            ));
            dispatched.push(request);
        }

        peak_backlog = peak_backlog.max(scheduler.diagnostics().queued_requests);
    }

    let newest_by = |instant: Millis| -> Generation {
        submitted
            .iter()
            .filter(|(at, _)| *at <= instant)
            .map(|(_, generation)| *generation)
            .max()
            .expect("a query was always submitted before the first dispatch")
    };

    assert!(!dispatched.is_empty(), "the session must produce dispatches");
    for request in &dispatched {
        assert_eq!(
            request.generation,
            newest_by(request.dispatched_at),
            "{:?} received {} at {} ms although a newer generation already existed",
            request.plugin,
            request.generation,
            request.dispatched_at
        );
    }

    let final_generation = submitted
        .last()
        .map(|(_, generation)| *generation)
        .expect("the script submits queries");

    for owner in [&core, &python, &network] {
        let own: Vec<&DispatchedRequest> = dispatched.iter().filter(|r| r.plugin == *owner).collect();
        assert!(!own.is_empty(), "{owner:?} must be served during the session");
        for pair in own.windows(2) {
            assert!(
                pair[1].generation > pair[0].generation,
                "{owner:?} dispatched {} after {}: generations must never regress",
                pair[1].generation,
                pair[0].generation
            );
            assert!(
                pair[1].dispatched_at >= pair[0].dispatched_at,
                "dispatch timestamps must not travel backwards for {owner:?}"
            );
        }
        let last = own.last().expect("checked non-empty");
        assert_eq!(
            last.generation, final_generation,
            "{owner:?} must end the session holding the final query"
        );
        assert_eq!(
            last.query, final_text,
            "the final dispatch must carry the final query text"
        );
        assert_eq!(
            scheduler.pending(owner),
            None,
            "no work may be left queued for {owner:?}"
        );
        assert_eq!(
            scheduler.in_flight(owner),
            0,
            "every request for {owner:?} completed"
        );
    }

    assert_eq!(
        scheduler.next_wakeup(),
        None,
        "a quiescent scheduler must not keep a timer armed"
    );

    let count = |owner: &PluginId| dispatched.iter().filter(|r| r.plugin == *owner).count();
    assert_eq!(
        count(&core),
        script.len(),
        "a 0 ms policy with free slots must serve every keystroke"
    );
    assert!(
        count(&python) < count(&core),
        "a 60 ms debounce must absorb keystrokes: {} of {}",
        count(&python),
        count(&core)
    );
    assert!(
        count(&network) < count(&python),
        "a 200 ms debounce must absorb more still: {} against {}",
        count(&network),
        count(&python)
    );
    // Each network dispatch needs either a 200 ms quiet period or a 400 ms
    // burst window; the session is well under 2 s of input.
    assert!(
        count(&network) <= 8,
        "sustained typing must not accumulate network dispatches, saw {}",
        count(&network)
    );

    // Spec 12.4 + 31.4: only the newest undispatched query is retained, so the
    // backlog can never exceed one request per registered plugin.
    assert!(
        peak_backlog <= 3,
        "undispatched backlog must stay bounded by the plugin count, peaked at {peak_backlog}"
    );
    let diagnostics = scheduler.diagnostics();
    assert!(
        diagnostics.peak_queue_depth <= 3,
        "the scheduler's own peak-depth counter must agree, reported {}",
        diagnostics.peak_queue_depth
    );
    assert_eq!(diagnostics.queued_requests, 0);
    assert_eq!(diagnostics.in_flight_requests, 0);
    assert!(
        diagnostics.coalesced_requests > 0,
        "rapid typing must be visible as coalesced requests"
    );
    assert!(
        diagnostics.cancelled_requests > 0,
        "superseded in-flight work must be visible as cancellations"
    );
}

#[test]
fn maximum_wait_shorter_than_debounce_still_fires_at_the_maximum() {
    let search = plugin("dev.crikey.short-maximum");
    let mut scheduler = scheduler();
    scheduler.register_plugin(search.clone(), modern(100, Some(20), false, true));

    let generation = scheduler.submit_query("query", 0);
    assert!(scheduler.tick(0).is_empty());
    assert_eq!(
        scheduler.next_wakeup(),
        Some(20),
        "the configured maximum wait must not be widened to the debounce interval"
    );
    assert!(scheduler.tick(19).is_empty());
    assert_eq!(
        shape(&scheduler.tick(20)),
        vec![("dev.crikey.short-maximum", generation.get(), "query", 20)]
    );
}

#[test]
fn equal_timestamps_restart_the_quiet_period_without_double_dispatch() {
    let weather = plugin("dev.crikey.equal-time");
    let mut scheduler = scheduler();
    scheduler.register_plugin(weather.clone(), modern(50, None, false, true));

    let first = scheduler.submit_query("a", 10);
    assert!(scheduler.tick(10).is_empty());
    let second = scheduler.submit_query("ab", 10);
    assert!(scheduler.tick(10).is_empty());
    assert_eq!(scheduler.next_wakeup(), Some(60));
    assert_eq!(
        shape(&scheduler.tick(60)),
        vec![("dev.crikey.equal-time", second.get(), "ab", 60)]
    );
    assert!(scheduler.tick(60).is_empty());
    assert_eq!(
        scheduler.diagnostics().coalesced_requests,
        1,
        "the equal-time update replaces the first pending request exactly once"
    );
    assert_ne!(first, second);
}

#[test]
fn backwards_timestamps_are_clamped_and_never_early_wake_work() {
    let history = plugin("dev.crikey.backwards-time");
    let mut scheduler = scheduler();
    scheduler.register_plugin(history.clone(), modern(50, None, false, true));

    scheduler.submit_query("first", 100);
    assert!(scheduler.tick(100).is_empty());
    let newest = scheduler.submit_query("newest", 90);
    assert!(scheduler.tick(90).is_empty());
    assert_eq!(
        scheduler.next_wakeup(),
        Some(150),
        "a late timestamp must not pull a deadline earlier"
    );
    assert!(scheduler.tick(149).is_empty());
    assert_eq!(
        shape(&scheduler.tick(150)),
        vec![("dev.crikey.backwards-time", newest.get(), "newest", 150)]
    );
    let trace_times: Vec<Millis> = scheduler
        .trace()
        .iter()
        .filter_map(|event| match event {
            QueryTraceEvent::Keystroke { at, .. }
            | QueryTraceEvent::Debounce { at, .. }
            | QueryTraceEvent::Dispatched { at, .. } => Some(*at),
            _ => None,
        })
        .collect();
    assert!(
        trace_times.windows(2).all(|pair| pair[0] <= pair[1]),
        "clamped timestamps keep the trace chronological: {trace_times:?}"
    );
}
