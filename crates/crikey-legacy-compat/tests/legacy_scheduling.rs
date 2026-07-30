//! Live `legacy-strict` dispatch and the legacy catalog lifecycle
//! (spec 7.1, 7.2, 8.1, 8.4, 8.8, 8.10, 8.11, 9.2, 9.3, 9.5, 9.6, 14.5, 14.8,
//! 14.9, 26.4; ADR-0006; roadmap M3; acceptance 31.4, 31.7, 31.8,
//! 31.14 - 31.18).
//!
//! These tests are written before the implementation. They drive
//! `LegacyRuntime` — the object that owns several registered legacy plugin
//! instances and decides, per keystroke, what each one is told to do. They are
//! deliberately *not* unit tests of `ObsoleteWorkManager`: that type answers
//! "given a query change, is this one instance idle or busy", which is a strict
//! subset of the contract below. It knows nothing about broadcast, about which
//! plugin owns a selected item, about the difference between one-time
//! initialization and a catalog rebuild, about instance obsolescence across a
//! reload, or about the fact that a keystroke must never abort an `on_catalog()`
//! that is already running. All of that lives here.
//!
//! # Execution model
//!
//! * Time is virtual. Every timestamp is an explicit `Millis` argument. No test
//!   sleeps, reads a wall clock, spawns a thread, or touches the network.
//! * The runtime never performs I/O. Its only outbound edge is
//!   `LegacyWorkerHandle`, the trait the real `LegacyWorker` implements and that
//!   these tests substitute with `ScriptedWorker`. Inbound responses arrive
//!   through `LegacyRuntime::deliver`, so the runtime is a pure state machine
//!   over an explicit clock.
//! * **Intake decides; `tick` dispatches.** `submit_query`, `select_item`,
//!   `catalog_rebuild`, `deliver` and `reload` each take a timestamp, evaluate
//!   the per-instance serial dispatcher immediately, and record their
//!   `LegacyTraceEvent`s at that timestamp. `tick(now)` is the only call that
//!   hands a callback across the worker boundary, and it is also where the
//!   legacy deadline ladder (soft warning, hung-worker watchdog) is evaluated.
//!   Tests therefore call `tick(t)` at the same virtual millisecond as the
//!   intake whose dispatch they assert on.
//! * The one exception is cooperative termination:
//!   `LegacyWorkerHandle::request_termination` is raised **synchronously from
//!   intake**, not from `tick`. Raising `should_terminate()` sets a flag the
//!   plugin polls; it schedules nothing, and spec 31.17 requires it to happen at
//!   the keystroke timestamp.
//! * `deliver` retires the in-flight callback whether or not its answer is
//!   accepted. A plugin that answers a superseded query has its result rejected,
//!   but its instance still becomes free — otherwise an uncooperative plugin
//!   could starve every later query.
//! * Registration carries no timestamp, so the one-time `on_start` is enqueued
//!   by `register` and crosses the boundary on the first `tick`. Every test
//!   drains it through `boot` before asserting on query scheduling.
//!
//! # Surface under test
//!
//! * `LegacyRuntime::new(W, LegacyDeadlines)` where `W: LegacyWorkerHandle`,
//!   plus `worker()` for inspecting the double.
//! * `register(PluginId, PackageId) -> InstanceId` and
//!   `register_with(LegacyRegistration)` — the latter carries the two documented
//!   dynamic-cache opt-ins of spec 14.9, and nothing else may enable caching.
//! * `submit_query(&str, Millis) -> Generation`,
//!   `select_item(&ItemId, Millis) -> Result<Generation, LegacyRuntimeError>`,
//!   `catalog_rebuild(&PluginId, Millis) -> Result<(), LegacyRuntimeError>`,
//!   `reload(&PluginId, Millis) -> Result<InstanceId, LegacyRuntimeError>`,
//!   `tick(Millis) -> Vec<LegacyRequest>`,
//!   `deliver(LegacyResponse, Millis) -> Delivery`,
//!   `shutdown(Millis) -> ShutdownReport`.
//! * Observation: `trace()`, `diagnostics(&PluginId)`,
//!   `instance_state(&PluginId)`, `should_terminate(&PluginId)`,
//!   `dynamic_cache_policy(&PluginId)`,
//!   `deadline_policy(&PluginId, LegacyCallback)`, `catalog(&PluginId)`,
//!   `visible_items()`, `visible_generation()`.
//!
//! The scheduling verdicts in the trace are `crikey_input_scheduler::
//! LegacyDispatch` values verbatim. The legacy layer does not get a second,
//! parallel vocabulary for "dispatched now" / "queued behind running work".

use std::collections::BTreeMap;

use crikey_core::{ArgumentPolicy, Category, Generation, HitPolicy, Item, ItemId, PluginId};
use crikey_input_scheduler::{LegacyDispatch, Millis, SchedulingProfile};
use crikey_legacy_compat::{
    CatalogRejectReason, DeadlinePolicy, Delivery, DynamicCachePolicy, InstanceId, LegacyCallback,
    LegacyCompatibility, LegacyDeadlines, LegacyInstanceState, LegacyPluginDiagnostics, LegacyRegistration,
    LegacyRequest, LegacyRequestKind, LegacyResponse, LegacyRuntime, LegacyTraceEvent, LegacyWorkerHandle,
    PackageId, TerminationReason, WorkerError,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn plugin(id: &str) -> PluginId {
    PluginId(id.to_string())
}

fn package(id: &str) -> PackageId {
    PackageId(id.to_string())
}

fn item(owner: &PluginId, id: &str, label: &str) -> Item {
    Item {
        stable_id: ItemId(id.to_string()),
        plugin_id: owner.clone(),
        category: Category::Keyword,
        label: label.to_string(),
        description: String::new(),
        target: id.to_string(),
        search_terms: Vec::new(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Optional,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    }
}

/// Deadlines with a deliberately wide spread between the modern hard query
/// budget a `modern` plugin would be killed on and the legacy ladder that
/// replaces it for legacy callbacks (spec 9.6).
fn deadlines() -> LegacyDeadlines {
    LegacyDeadlines {
        modern_hard_query_ms: 250,
        soft_warning_ms: 5_000,
        hung_worker_ms: 120_000,
        teardown_ms: 250,
    }
}

fn runtime() -> LegacyRuntime<ScriptedWorker> {
    LegacyRuntime::new(ScriptedWorker::default(), deadlines())
}

/// Drains the one-time `on_start` of every registered instance so query tests
/// begin from a genuinely idle instance rather than one that is busy
/// initializing.
fn boot(runtime: &mut LegacyRuntime<ScriptedWorker>, at_ms: Millis) {
    for request in runtime.tick(at_ms) {
        assert!(
            matches!(request.kind, LegacyRequestKind::Start),
            "spec 14.8: the first callback a freshly registered legacy instance receives is its \
             one-time on_start; observed {:?}",
            request.kind
        );
        let accepted = runtime.deliver(LegacyResponse::started(&request), at_ms);
        assert_eq!(
            accepted,
            Delivery::Accepted,
            "spec 14.8: completion of the one-time initialization is accepted"
        );
    }
}

// ---------------------------------------------------------------------------
// The scriptable worker double
//
// `LegacyWorkerHandle` is the outbound half of the worker surface: everything
// the runtime pushes towards the CPython child process. Responses come back
// through `LegacyRuntime::deliver`, which is what keeps the runtime free of I/O
// and lets each test decide exactly when a plugin answers, and whether it
// answers at all.
//
// The double also models the one piece of worker-side state the contract
// depends on: the flag a legacy plugin reads through `should_terminate()`.
// Raising it is the host's job; honouring it is the plugin's, and one test
// below exists precisely because a plugin may refuse.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminationRecord {
    at_ms: Millis,
    plugin: PluginId,
    instance: InstanceId,
    generation: Generation,
    reason: TerminationReason,
}

/// A dispatched callback projected onto comparable scalars.
///
/// `LegacyRequestKind` carries `Item` values for `on_execute`, and the core
/// deliberately does not make `Item` comparable, so request kinds cannot be
/// compared directly. Projecting keeps whole-schedule `assert_eq!` failures
/// readable.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Start,
    Catalog,
    InitialSuggest(String),
    ArgumentSuggest(String, String),
    Other(LegacyCallback),
}

fn call_of(request: &LegacyRequest) -> Call {
    match &request.kind {
        LegacyRequestKind::Start => Call::Start,
        LegacyRequestKind::Catalog => Call::Catalog,
        LegacyRequestKind::InitialSuggest { query } => Call::InitialSuggest(query.clone()),
        LegacyRequestKind::ArgumentSuggest { query, selected } => {
            Call::ArgumentSuggest(query.clone(), selected.0.clone())
        }
        _ => Call::Other(request.callback()),
    }
}

#[derive(Debug, Default)]
struct ScriptedWorker {
    dispatched: Vec<(Millis, LegacyRequest)>,
    terminations: Vec<TerminationRecord>,
    stops: Vec<(Millis, Millis)>,
    terminate_flags: BTreeMap<PluginId, bool>,
}

impl LegacyWorkerHandle for ScriptedWorker {
    fn dispatch(&mut self, at_ms: Millis, request: &LegacyRequest) -> Result<(), WorkerError> {
        // A newly started callback begins with a clear cooperative-termination
        // flag: `should_terminate()` describes the work in flight *now*.
        self.terminate_flags.insert(request.plugin.clone(), false);
        self.dispatched.push((at_ms, request.clone()));
        Ok(())
    }

    fn request_termination(
        &mut self,
        at_ms: Millis,
        plugin: &PluginId,
        instance: InstanceId,
        generation: Generation,
        reason: TerminationReason,
    ) -> Result<(), WorkerError> {
        self.terminate_flags.insert(plugin.clone(), true);
        self.terminations.push(TerminationRecord {
            at_ms,
            plugin: plugin.clone(),
            instance,
            generation,
            reason,
        });
        Ok(())
    }

    fn stop(&mut self, at_ms: Millis, budget_ms: Millis) -> Result<(), WorkerError> {
        self.stops.push((at_ms, budget_ms));
        Ok(())
    }
}

impl ScriptedWorker {
    /// What a plugin running inside this worker would read from the legacy
    /// `Plugin.should_terminate()` API right now (spec 9.2).
    fn observed_should_terminate(&self, of: &PluginId) -> bool {
        self.terminate_flags.get(of).copied().unwrap_or(false)
    }

    /// `(timestamp, generation, call)` for everything this worker was actually
    /// told to run, in dispatch order.
    fn calls(&self, of: &PluginId) -> Vec<(Millis, u64, Call)> {
        self.dispatched
            .iter()
            .filter(|(_, request)| &request.plugin == of)
            .map(|(at_ms, request)| (*at_ms, request.generation.get(), call_of(request)))
            .collect()
    }

    fn terminations(&self, of: &PluginId) -> Vec<TerminationRecord> {
        self.terminations
            .iter()
            .filter(|record| &record.plugin == of)
            .cloned()
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Projections
// ---------------------------------------------------------------------------

fn state(runtime: &LegacyRuntime<ScriptedWorker>, of: &PluginId) -> LegacyInstanceState {
    runtime
        .instance_state(of)
        .unwrap_or_else(|| panic!("{of:?} is registered"))
}

fn diagnostics(runtime: &LegacyRuntime<ScriptedWorker>, of: &PluginId) -> LegacyPluginDiagnostics {
    runtime
        .diagnostics(of)
        .unwrap_or_else(|| panic!("{of:?} is registered"))
}

/// Every serial-dispatcher verdict recorded for one instance, in the shared
/// `LegacyDispatch` vocabulary (spec 8.4, 14.5).
fn decisions(runtime: &LegacyRuntime<ScriptedWorker>, of: &PluginId) -> Vec<(Millis, LegacyDispatch)> {
    runtime
        .trace()
        .iter()
        .filter_map(|event| match event {
            LegacyTraceEvent::Decision {
                at_ms,
                plugin,
                dispatch,
            } if plugin == of => Some((*at_ms, *dispatch)),
            _ => None,
        })
        .collect()
}

/// The observed callback order for one instance (spec 14.8).
fn callbacks(runtime: &LegacyRuntime<ScriptedWorker>, of: &PluginId) -> Vec<LegacyCallback> {
    runtime
        .trace()
        .iter()
        .filter_map(|event| match event {
            LegacyTraceEvent::Dispatched { plugin, callback, .. } if plugin == of => Some(*callback),
            _ => None,
        })
        .collect()
}

fn broadcasts(runtime: &LegacyRuntime<ScriptedWorker>) -> Vec<(Millis, Generation, Vec<PluginId>)> {
    runtime
        .trace()
        .iter()
        .filter_map(|event| match event {
            LegacyTraceEvent::Broadcast {
                at_ms,
                generation,
                plugins,
            } => Some((*at_ms, *generation, plugins.clone())),
            _ => None,
        })
        .collect()
}

fn routings(runtime: &LegacyRuntime<ScriptedWorker>) -> Vec<(Millis, Generation, PluginId, ItemId)> {
    runtime
        .trace()
        .iter()
        .filter_map(|event| match event {
            LegacyTraceEvent::Routed {
                at_ms,
                generation,
                plugin,
                owner_of,
            } => Some((*at_ms, *generation, plugin.clone(), owner_of.clone())),
            _ => None,
        })
        .collect()
}

/// Asserts the trace records `event`, failing with the whole trace otherwise.
fn expect_trace(runtime: &LegacyRuntime<ScriptedWorker>, event: LegacyTraceEvent, clause: &str) {
    assert!(
        runtime.trace().contains(&event),
        "{clause}; the legacy scheduling trace must record {event:?}, observed {:?}",
        runtime.trace()
    );
}

/// Position of `event` in the trace, failing with the whole trace when absent.
fn trace_index(runtime: &LegacyRuntime<ScriptedWorker>, event: &LegacyTraceEvent) -> usize {
    runtime
        .trace()
        .iter()
        .position(|candidate| candidate == event)
        .unwrap_or_else(|| {
            panic!(
                "spec 26.4: the legacy scheduling trace must record {event:?}; observed {:?}",
                runtime.trace()
            )
        })
}

fn item_ids(items: &[Item]) -> Vec<&str> {
    items.iter().map(|item| item.stable_id.0.as_str()).collect()
}

fn labels(items: &[Item]) -> Vec<&str> {
    items.iter().map(|item| item.label.as_str()).collect()
}

/// The single request dispatched to `of` in this tick.
fn only_for<'a>(dispatched: &'a [LegacyRequest], of: &PluginId) -> &'a LegacyRequest {
    let mut matching = dispatched.iter().filter(|request| &request.plugin == of);
    let first = matching
        .next()
        .unwrap_or_else(|| panic!("expected a request dispatched to {of:?}, got none"));
    assert!(
        matching.next().is_none(),
        "spec 14.5 / 31.16: at most one callback per legacy plugin instance may be in flight, so \
         at most one request per plugin may be dispatched in a tick"
    );
    first
}

// ---------------------------------------------------------------------------
// Broadcast, no gating, no debounce
// ---------------------------------------------------------------------------

#[test]
fn an_initial_suggestion_request_is_broadcast_to_every_loaded_legacy_plugin() {
    let (a, b, c) = (
        plugin("legacy.alpha"),
        plugin("legacy.bravo"),
        plugin("legacy.charlie"),
    );
    let mut runtime = runtime();
    runtime.register(a.clone(), package("alpha"));
    runtime.register(b.clone(), package("bravo"));
    runtime.register(c.clone(), package("charlie"));
    boot(&mut runtime, 0);

    // An empty query and a one-character query are exactly the two cases a
    // host-imposed minimum length would suppress. `legacy-strict` has none.
    let empty = runtime.submit_query("", 10);
    let dispatched = runtime.tick(10);
    assert_eq!(
        dispatched.len(),
        3,
        "spec 14.5 / 31.15: the initial suggestion request is broadcast to every loaded legacy \
         plugin"
    );
    for request in &dispatched {
        let accepted = runtime.deliver(LegacyResponse::suggestions(request, Vec::new()), 11);
        assert_eq!(
            accepted,
            Delivery::Published { items: 0 },
            "spec 7.1: an empty legacy answer is still one complete publication"
        );
    }

    let single = runtime.submit_query("z", 20);
    assert_eq!(
        runtime.tick(20).len(),
        3,
        "spec 8.11 / 14.5: no prefix or keyword relevance gating narrows a legacy broadcast"
    );

    for of in [&a, &b, &c] {
        assert_eq!(
            runtime.worker().calls(of),
            vec![
                (0, 0, Call::Start),
                (10, empty.get(), Call::InitialSuggest(String::new())),
                (20, single.get(), Call::InitialSuggest("z".to_string())),
            ],
            "spec 7.1 / 8.10 / 8.11 / 31.15: {of:?} must receive every initial suggestion \
             request, including the empty query and a single character, with no host-imposed \
             minimum query length and no prefix gating"
        );
    }

    assert_eq!(
        broadcasts(&runtime),
        vec![
            (10, empty, vec![a.clone(), b.clone(), c.clone()]),
            (20, single, vec![a.clone(), b.clone(), c.clone()]),
        ],
        "spec 26.4: each broadcast is recorded once, naming its generation and every recipient in \
         registration order"
    );
    assert!(
        routings(&runtime).is_empty(),
        "spec 14.5: with no item selected a suggestion request is a broadcast, never an argument \
         request routed to a single owner"
    );
    assert!(
        !SchedulingProfile::LegacyStrict.allows_host_gating(),
        "spec 7.1: the legacy-strict profile itself refuses host gating, so the runtime has no \
         gate to apply"
    );
}

#[test]
fn consecutive_legacy_keystrokes_are_never_time_debounced() {
    let p = plugin("legacy.prompt");
    let mut runtime = runtime();
    runtime.register(p.clone(), package("prompt"));
    boot(&mut runtime, 0);

    assert_eq!(
        state(&runtime, &p).profile,
        SchedulingProfile::LegacyStrict,
        "spec 7.1: an unchanged legacy plugin is registered as legacy-strict by default"
    );

    // Four keystrokes well inside any plausible debounce window (spec 25.4
    // bands start at tens of milliseconds). Each is answered at once, so the
    // instance is idle when the next arrives and nothing observed below can be
    // attributed to serialization rather than to debouncing.
    let mut expected: Vec<(Millis, u64, Call)> = vec![(0, 0, Call::Start)];
    for (at_ms, text) in [(100, "g"), (103, "gi"), (105, "git"), (106, "git ")] {
        let generation = runtime.submit_query(text, at_ms);
        let dispatched = runtime.tick(at_ms);
        let request = only_for(&dispatched, &p).clone();
        expected.push((at_ms, generation.get(), Call::InitialSuggest(text.to_string())));
        assert_eq!(
            runtime.worker().calls(&p),
            expected,
            "spec 7.1 / 8.4 / 31.14: keystroke {text:?} must reach an idle legacy-strict plugin \
             at its own timestamp {at_ms}, verbatim, never at the end of a debounce interval"
        );
        runtime.deliver(LegacyResponse::suggestions(&request, Vec::new()), at_ms);
    }

    assert!(
        !SchedulingProfile::LegacyStrict.allows_time_debounce(),
        "spec 8.4 / 31.14: legacy-strict plugins are never time-debounced"
    );
}

// ---------------------------------------------------------------------------
// Serialization, supersession, coalescing
// ---------------------------------------------------------------------------

#[test]
fn callbacks_are_serialized_per_instance_while_other_instances_are_never_delayed() {
    let slow = plugin("legacy.slow");
    let fast = plugin("legacy.fast");
    let mut runtime = runtime();
    let slow_instance = runtime.register(slow.clone(), package("slow"));
    runtime.register(fast.clone(), package("fast"));
    boot(&mut runtime, 0);

    let g1 = runtime.submit_query("a", 10);
    let first = runtime.tick(10);
    let slow_g1 = only_for(&first, &slow).clone();
    let fast_g1 = only_for(&first, &fast).clone();
    runtime.deliver(LegacyResponse::suggestions(&fast_g1, Vec::new()), 12);

    // `slow` is still inside its first callback. A second keystroke must not
    // start a second callback on that instance...
    let g2 = runtime.submit_query("ab", 20);
    runtime.tick(20);
    assert_eq!(
        state(&runtime, &slow),
        LegacyInstanceState {
            instance: slow_instance,
            profile: SchedulingProfile::LegacyStrict,
            started: true,
            running: Some(g1),
            running_callback: Some(LegacyCallback::OnSuggest),
            pending: Some(g2),
            pending_query: Some("ab".to_string()),
            pending_depth: 1,
        },
        "spec 14.5 / 31.16: while one legacy callback is in flight no second callback may be \
         started for the same instance; the newer query waits"
    );

    // ...and must not delay any other instance.
    let g3 = runtime.submit_query("abc", 30);
    runtime.tick(30);
    assert_eq!(
        runtime.worker().calls(&fast),
        vec![
            (0, 0, Call::Start),
            (10, g1.get(), Call::InitialSuggest("a".to_string())),
            (20, g2.get(), Call::InitialSuggest("ab".to_string())),
        ],
        "spec 31.8: a slow legacy plugin never delays a fast one — `fast` is dispatched at each \
         keystroke it is idle for, at that keystroke's own timestamp, independently of `slow`"
    );
    assert_eq!(
        runtime.worker().calls(&slow),
        vec![
            (0, 0, Call::Start),
            (10, g1.get(), Call::InitialSuggest("a".to_string())),
        ],
        "spec 14.5 / 31.16: two keystrokes arrived while `slow` was busy and neither started a \
         concurrent callback on it"
    );

    // The in-flight callback finally returns; only then does the newest query run.
    runtime.deliver(LegacyResponse::suggestions(&slow_g1, Vec::new()), 40);
    runtime.tick(40);
    assert_eq!(
        runtime.worker().calls(&slow),
        vec![
            (0, 0, Call::Start),
            (10, g1.get(), Call::InitialSuggest("a".to_string())),
            (40, g3.get(), Call::InitialSuggest("abc".to_string())),
        ],
        "spec 14.5: the serialized instance resumes with the newest query only after its callback \
         returned, and the intermediate generation is never run"
    );
    assert_eq!(
        decisions(&runtime, &slow),
        vec![
            (0, LegacyDispatch::Now(Generation::ZERO)),
            (0, LegacyDispatch::Idle),
            (10, LegacyDispatch::Now(g1)),
            (
                20,
                LegacyDispatch::QueuedBehindRunning {
                    obsolete: g1,
                    queued: g2,
                }
            ),
            (
                30,
                LegacyDispatch::QueuedBehindRunning {
                    obsolete: g1,
                    queued: g3,
                }
            ),
            (40, LegacyDispatch::Now(g3)),
        ],
        "spec 8.4 / 26.4: every serial-dispatcher verdict is recorded in the shared LegacyDispatch \
         vocabulary"
    );
}

#[test]
fn a_superseding_keystroke_raises_should_terminate_at_the_keystroke_timestamp() {
    let p = plugin("legacy.searcher");
    let mut runtime = runtime();
    let instance = runtime.register(p.clone(), package("searcher"));
    boot(&mut runtime, 0);

    let g1 = runtime.submit_query("proj", 10);
    runtime.tick(10);
    assert!(
        !runtime.should_terminate(&p),
        "spec 9.2: freshly dispatched work is not obsolete, so should_terminate() is false"
    );
    assert!(
        !runtime.worker().observed_should_terminate(&p),
        "spec 9.2: the worker-side flag the plugin polls starts clear for a new callback"
    );

    let g2 = runtime.submit_query("proje", 47);

    assert!(
        runtime.should_terminate(&p),
        "spec 9.2 / 31.17: a superseding keystroke makes should_terminate() true for the in-flight \
         obsolete work"
    );
    assert!(
        runtime.worker().observed_should_terminate(&p),
        "spec 9.2 / 31.17: the cooperative flag must reach the worker, not merely the host's \
         bookkeeping — the plugin polls it from inside its own callback"
    );
    assert_eq!(
        runtime.worker().terminations(&p),
        vec![TerminationRecord {
            at_ms: 47,
            plugin: p.clone(),
            instance,
            generation: g1,
            reason: TerminationReason::QuerySuperseded,
        }],
        "spec 9.3 / 31.17: termination is requested for the obsolete generation at the keystroke \
         timestamp, not at the next tick and not at the next dispatch"
    );
    expect_trace(
        &runtime,
        LegacyTraceEvent::TerminationRequested {
            at_ms: 47,
            plugin: p.clone(),
            generation: g1,
            reason: TerminationReason::QuerySuperseded,
        },
        "spec 26.4",
    );

    // Re-raising is idempotent: further keystrokes against already-obsolete work
    // must not flood the worker with duplicate termination requests.
    let g3 = runtime.submit_query("projec", 60);
    assert_eq!(
        runtime.worker().terminations(&p).len(),
        1,
        "spec 9.2 / 26.4: work that is already obsolete is not re-terminated once per keystroke"
    );
    assert_eq!(
        diagnostics(&runtime, &p).terminations_requested,
        1,
        "spec 26.4: the diagnostics counter matches the single cooperative request issued"
    );
    assert_eq!(
        state(&runtime, &p).pending,
        Some(g3),
        "spec 14.5: the newest keystroke is what waits behind the obsolete callback"
    );
    assert_ne!(
        g2, g3,
        "spec 8.1: every keystroke mints its own monotonic generation"
    );
}

#[test]
fn only_the_newest_undispatched_query_is_retained_under_sustained_typing() {
    let p = plugin("legacy.indexer");
    let mut runtime = runtime();
    runtime.register(p.clone(), package("indexer"));
    boot(&mut runtime, 0);

    let g1 = runtime.submit_query("a", 10);
    let dispatched = runtime.tick(10);
    let in_flight = only_for(&dispatched, &p).clone();

    let mut expected_decisions = vec![
        (0, LegacyDispatch::Now(Generation::ZERO)),
        (0, LegacyDispatch::Idle),
        (10, LegacyDispatch::Now(g1)),
    ];
    let mut newest = g1;
    for (at_ms, text) in [
        (20, "ab"),
        (21, "abc"),
        (22, "abcd"),
        (23, "abcde"),
        (24, "abcdef"),
    ] {
        newest = runtime.submit_query(text, at_ms);
        runtime.tick(at_ms);
        expected_decisions.push((
            at_ms,
            LegacyDispatch::QueuedBehindRunning {
                obsolete: g1,
                queued: newest,
            },
        ));

        let observed = state(&runtime, &p);
        assert_eq!(
            observed.pending_depth, 1,
            "spec 8.8 / 14.5 / 31.4: sustained typing against a busy legacy plugin retains \
             exactly one undispatched request; an unbounded queue must never form"
        );
        assert_eq!(
            (observed.pending, observed.pending_query.as_deref()),
            (Some(newest), Some(text)),
            "spec 14.5: the single retained request is always the newest one"
        );
    }

    assert_eq!(
        decisions(&runtime, &p),
        expected_decisions,
        "spec 8.4 / 26.4: each keystroke against busy work is recorded as QueuedBehindRunning, \
         naming both the obsolete and the queued generation"
    );
    let counters = diagnostics(&runtime, &p);
    assert_eq!(
        counters.max_pending_depth, 1,
        "spec 31.4: the high-water mark of the legacy pending queue is one request"
    );
    assert_eq!(
        counters.replaced, 4,
        "spec 8.8: four intermediate undispatched queries were replaced by a newer one"
    );
    assert_eq!(
        runtime.worker().calls(&p),
        vec![
            (0, 0, Call::Start),
            (10, g1.get(), Call::InitialSuggest("a".to_string())),
        ],
        "spec 14.5: none of the replaced generations ever crossed the worker boundary"
    );

    // When the instance frees up, the survivor is the newest query — never an
    // intermediate one, and never a backlog.
    runtime.deliver(LegacyResponse::suggestions(&in_flight, Vec::new()), 30);
    let resumed = runtime.tick(30);
    assert_eq!(
        resumed.len(),
        1,
        "spec 8.8 / 31.4: exactly one request was retained, so exactly one is dispatched"
    );
    assert_eq!(
        (resumed[0].generation, call_of(&resumed[0])),
        (newest, Call::InitialSuggest("abcdef".to_string())),
        "spec 14.5: the retained request carries the newest query text at its own generation"
    );
}

// ---------------------------------------------------------------------------
// The named exit criterion: a plugin that ignores should_terminate()
// ---------------------------------------------------------------------------

/// Roadmap M3 exit criterion: *"the synthetic legacy test-plugin suite passes,
/// **including a plugin that ignores `should_terminate()`**"*.
///
/// The plugin modelled here is hostile in the exact way spec 9.5 says CriKey
/// must survive: it is told to stop, it observes that it was told to stop, it
/// keeps working anyway, and it answers long after its query stopped being the
/// visible one. Correctness may not depend on its cooperation. Its late answer
/// is refused at the intake boundary, it cannot change a single displayed item,
/// and the newest query still runs.
#[test]
fn a_plugin_that_ignores_should_terminate_cannot_change_what_is_displayed() {
    let hostile = plugin("legacy.ignores-should-terminate");
    let mut runtime = runtime();
    runtime.register(hostile.clone(), package("ignores-should-terminate"));
    boot(&mut runtime, 0);

    let obsolete = runtime.submit_query("ala", 10);
    let dispatched = runtime.tick(10);
    let obsolete_request = only_for(&dispatched, &hostile).clone();

    let current = runtime.submit_query("alan", 40);

    // The plugin polls the flag, sees it, and carries on regardless. Everything
    // asserted after this line must hold *because the host is correct*, not
    // because the plugin behaved.
    let seen_by_plugin = runtime.worker().observed_should_terminate(&hostile);
    assert!(
        seen_by_plugin,
        "spec 9.2 / 31.17: this fixture only means anything if the plugin genuinely saw the \
         cooperative termination request it is about to ignore"
    );

    let late = runtime.deliver(
        LegacyResponse::suggestions(
            &obsolete_request,
            vec![item(&hostile, "hostile.stale", "STALE - MUST NEVER BE DISPLAYED")],
        ),
        900,
    );
    assert_eq!(
        late,
        Delivery::RejectedStale {
            generation: obsolete,
            current,
        },
        "spec 14.5 / 31.7: a result from a superseded query generation is rejected at the intake \
         boundary, however long after the fact it arrives"
    );
    assert!(
        runtime.visible_items().is_empty(),
        "spec 31.7: the ignored-termination plugin's stale answer changed nothing that is \
         displayed"
    );
    expect_trace(
        &runtime,
        LegacyTraceEvent::StaleRejected {
            at_ms: 900,
            plugin: hostile.clone(),
            generation: obsolete,
            current,
        },
        "spec 26.4 / 31.7",
    );

    let counters = diagnostics(&runtime, &hostile);
    assert_eq!(
        counters.stale_rejected, 1,
        "spec 26.4: the stale rejection is counted against the offending plugin"
    );
    assert_eq!(
        counters.late_answers_after_termination_request, 1,
        "spec 9.5 / 26.2: answering after an unheeded cooperative request is exactly the reportable \
         'long callback that does not check should_terminate()'"
    );

    // The uncooperative callback still frees the instance when it finally
    // returns, so the newest query is not starved by the plugin's misbehaviour.
    let resumed = runtime.tick(900);
    assert_eq!(
        (resumed.len(), resumed.first().map(|request| request.generation)),
        (1, Some(current)),
        "spec 14.5: the newest query runs as soon as the uncooperative callback returns"
    );
    let fresh = runtime.deliver(
        LegacyResponse::suggestions(&resumed[0], vec![item(&hostile, "hostile.fresh", "Alan Turing")]),
        910,
    );
    assert_eq!(
        fresh,
        Delivery::Published { items: 1 },
        "spec 14.5: the answer for the current generation is published normally"
    );
    assert_eq!(
        item_ids(runtime.visible_items()),
        vec!["hostile.fresh"],
        "spec 31.7: only the current generation's answer is ever displayed"
    );
}

// ---------------------------------------------------------------------------
// Routing after a selection
// ---------------------------------------------------------------------------

#[test]
fn argument_suggestions_after_a_selection_reach_only_the_owning_plugin() {
    let owner = plugin("legacy.docs");
    let bystander = plugin("legacy.downloads");
    let mut runtime = runtime();
    runtime.register(owner.clone(), package("docs"));
    runtime.register(bystander.clone(), package("downloads"));
    boot(&mut runtime, 0);

    let initial = runtime.submit_query("do", 10);
    let broadcast = runtime.tick(10);
    let owner_request = only_for(&broadcast, &owner).clone();
    let bystander_request = only_for(&broadcast, &bystander).clone();
    runtime.deliver(
        LegacyResponse::suggestions(&owner_request, vec![item(&owner, "docs.report", "Report")]),
        12,
    );
    runtime.deliver(
        LegacyResponse::suggestions(
            &bystander_request,
            vec![item(&bystander, "downloads.iso", "linux.iso")],
        ),
        12,
    );

    let selected = ItemId("docs.report".to_string());
    let after_select = runtime
        .select_item(&selected, 20)
        .expect("spec 14.5: an item published by a registered legacy plugin can be selected");
    let routed = runtime.tick(20);
    assert_eq!(
        routed.len(),
        1,
        "spec 14.5: once an item is selected the suggestion request is routed, not broadcast"
    );
    assert_eq!(
        (routed[0].plugin.clone(), call_of(&routed[0])),
        (
            owner.clone(),
            Call::ArgumentSuggest(String::new(), "docs.report".to_string())
        ),
        "spec 14.5: the argument-suggestion request goes to the plugin that owns the selected item"
    );
    runtime.deliver(
        LegacyResponse::suggestions(&routed[0], vec![item(&owner, "docs.report/page-1", "Page 1")]),
        21,
    );

    let typed = runtime.submit_query("pa", 30);
    let argument = runtime.tick(30);
    assert_eq!(
        (
            argument.len(),
            argument.first().map(|request| request.plugin.clone()),
            argument.first().map(call_of),
        ),
        (
            1,
            Some(owner.clone()),
            Some(Call::ArgumentSuggest("pa".to_string(), "docs.report".to_string())),
        ),
        "spec 14.5: argument text typed after a selection keeps being routed to the owning plugin"
    );

    assert_eq!(
        runtime.worker().calls(&bystander),
        vec![
            (0, 0, Call::Start),
            (10, initial.get(), Call::InitialSuggest("do".to_string())),
        ],
        "spec 14.5: a plugin that does not own the selected item receives nothing after the \
         selection — argument suggestions are routed, never broadcast"
    );
    assert_eq!(
        broadcasts(&runtime),
        vec![(10, initial, vec![owner.clone(), bystander.clone()])],
        "spec 14.5: exactly one broadcast happened, and it was before the selection"
    );
    assert_eq!(
        routings(&runtime),
        vec![
            (20, after_select, owner.clone(), selected.clone()),
            (30, typed, owner.clone(), selected.clone()),
        ],
        "spec 26.4: every routed request is recorded with its owner and the item it belongs to"
    );
}

// ---------------------------------------------------------------------------
// Dynamic suggestion caching
// ---------------------------------------------------------------------------

/// Boots one plugin, answers `"git"` once, then submits the byte-identical
/// query again and ticks. What the second submission did is the whole question
/// of spec 14.9. Returns the runtime and the second generation.
fn replay_identical_query(registration: LegacyRegistration) -> (LegacyRuntime<ScriptedWorker>, Generation) {
    let of = registration.plugin.clone();
    let mut runtime = LegacyRuntime::new(ScriptedWorker::default(), deadlines());
    runtime.register_with(registration);
    boot(&mut runtime, 0);

    runtime.submit_query("git", 10);
    let first = runtime.tick(10);
    let request = only_for(&first, &of).clone();
    let published = runtime.deliver(
        LegacyResponse::suggestions(&request, vec![item(&of, "git.status", "git status")]),
        11,
    );
    assert_eq!(
        published,
        Delivery::Published { items: 1 },
        "spec 14.5: the first answer is published for its own generation"
    );

    let repeat = runtime.submit_query("git", 20);
    runtime.tick(20);
    (runtime, repeat)
}

#[test]
fn dynamic_suggestions_are_never_cached_by_default_and_only_under_an_explicit_opt_in() {
    let of = plugin("legacy.git");
    assert!(
        !SchedulingProfile::LegacyStrict.allows_dynamic_result_cache(),
        "spec 7.1 / 14.9: the legacy-strict profile itself forbids a dynamic result cache, so the \
         default answer cannot come from one"
    );

    // Default: an unchanged legacy plugin. The identical query re-dispatches.
    let (refusing, _) = replay_identical_query(LegacyRegistration::new(of.clone(), package("git")));
    assert_eq!(
        refusing.dynamic_cache_policy(&of),
        Some(DynamicCachePolicy::Refused),
        "spec 14.9 / 31.18: dynamic legacy suggestions are not cached by default"
    );
    assert_eq!(
        refusing.worker().calls(&of),
        vec![
            (0, 0, Call::Start),
            (10, 1, Call::InitialSuggest("git".to_string())),
            (20, 2, Call::InitialSuggest("git".to_string())),
        ],
        "spec 14.9 / 31.18: repeating the identical query must re-dispatch to the plugin instead \
         of replaying the previous answer"
    );
    assert!(
        refusing.visible_items().is_empty(),
        "spec 31.18: nothing is displayed for the repeated query until the plugin answers it \
         again — the previous answer is not replayed"
    );
    expect_trace(
        &refusing,
        LegacyTraceEvent::CacheRefused {
            at_ms: 20,
            plugin: of.clone(),
            query: "git".to_string(),
        },
        "spec 26.4 / 31.18",
    );
    let refused_counters = diagnostics(&refusing, &of);
    assert_eq!(
        (refused_counters.cache_refusals, refused_counters.cache_hits),
        (1, 0),
        "spec 26.4: the refusal is counted and no cache hit was served"
    );
    assert_eq!(
        refused_counters.dispatched, 3,
        "spec 14.9: on_start plus two identical suggestion callbacks actually ran"
    );

    // Opt-in 1: explicit per-plugin compatibility metadata (spec 14.9).
    let (by_metadata, repeat) = replay_identical_query(LegacyRegistration {
        compatibility: LegacyCompatibility {
            dynamic_suggestion_cache: true,
        },
        ..LegacyRegistration::new(of.clone(), package("git"))
    });
    assert_eq!(
        by_metadata.dynamic_cache_policy(&of),
        Some(DynamicCachePolicy::CompatibilityMetadata),
        "spec 14.9: explicit per-plugin compatibility metadata is one of the two documented ways \
         to permit dynamic caching"
    );
    assert_eq!(
        by_metadata.worker().calls(&of),
        vec![
            (0, 0, Call::Start),
            (10, 1, Call::InitialSuggest("git".to_string())),
        ],
        "spec 14.9: under the metadata opt-in the identical query is served from cache and the \
         plugin is not asked again"
    );
    assert_eq!(
        item_ids(by_metadata.visible_items()),
        vec!["git.status"],
        "spec 14.9: the cached answer is what gets displayed"
    );
    assert_eq!(
        by_metadata.visible_generation(),
        repeat,
        "spec 8.1: a cache hit is still published under the current generation, never under the \
         generation that produced it"
    );
    expect_trace(
        &by_metadata,
        LegacyTraceEvent::CacheServed {
            at_ms: 20,
            plugin: of.clone(),
            query: "git".to_string(),
        },
        "spec 26.4 / 14.9",
    );
    assert_eq!(
        diagnostics(&by_metadata, &of).cache_hits,
        1,
        "spec 26.4: the cache hit is counted, so an opt-in is auditable"
    );

    // Opt-in 2: a user-enabled `legacy-optimized` override (spec 7.2, 14.9).
    let (by_override, _) = replay_identical_query(LegacyRegistration {
        profile: SchedulingProfile::LegacyOptimized,
        ..LegacyRegistration::new(of.clone(), package("git"))
    });
    assert_eq!(
        by_override.dynamic_cache_policy(&of),
        Some(DynamicCachePolicy::LegacyOptimizedOverride),
        "spec 7.2 / 14.9: a user-enabled legacy-optimized override is the other documented way to \
         permit dynamic caching, and it is never the default for an unchanged plugin"
    );
    assert_eq!(
        by_override.worker().calls(&of),
        vec![
            (0, 0, Call::Start),
            (10, 1, Call::InitialSuggest("git".to_string())),
        ],
        "spec 7.2: under the explicit override the identical query is served from cache"
    );
    assert_eq!(
        diagnostics(&by_override, &of).cache_hits,
        1,
        "spec 26.4: the override's cache hit is counted like any other"
    );
}

// ---------------------------------------------------------------------------
// Catalog lifecycle
// ---------------------------------------------------------------------------

#[test]
fn a_catalog_rebuild_does_not_re_run_one_time_initialization() {
    let of = plugin("legacy.catalog-only");
    let mut runtime = runtime();
    let instance = runtime.register(of.clone(), package("catalog-only"));
    boot(&mut runtime, 0);
    assert!(
        state(&runtime, &of).started,
        "spec 14.8: the instance recorded that its one-time initialization ran"
    );

    for (requested_at, answered_at) in [(10, 20), (30, 40)] {
        runtime
            .catalog_rebuild(&of, requested_at)
            .expect("spec 14.8: on_catalog() may be called repeatedly");
        let dispatched = runtime.tick(requested_at);
        let request = only_for(&dispatched, &of).clone();
        let updated = runtime.deliver(
            LegacyResponse::set_catalog(&request, vec![item(&of, "catalog.one", "One")]),
            answered_at,
        );
        assert_eq!(
            updated,
            Delivery::CatalogUpdated { total: 1 },
            "spec 14.8: a repeated on_catalog() call is accepted exactly like the first one"
        );
    }

    assert_eq!(
        callbacks(&runtime, &of),
        vec![
            LegacyCallback::OnStart,
            LegacyCallback::OnCatalog,
            LegacyCallback::OnCatalog,
        ],
        "spec 14.8: repeated catalog rebuilds must stay distinguishable from one-time \
         initialization in the observed callback order — on_start runs exactly once"
    );
    assert_eq!(
        runtime.worker().calls(&of),
        vec![
            (0, 0, Call::Start),
            (10, 0, Call::Catalog),
            (30, 0, Call::Catalog),
        ],
        "spec 14.8: catalog work is not query work, so it carries no query generation and is \
         never subject to query staleness"
    );
    assert_eq!(
        state(&runtime, &of).instance,
        instance,
        "spec 14.8: a rebuild reuses the live instance; it does not re-instantiate the plugin"
    );
    assert_eq!(
        diagnostics(&runtime, &of).catalog_rebuilds,
        2,
        "spec 26.4: both rebuilds are counted"
    );
}

#[test]
fn a_long_catalog_build_is_serialized_but_never_killed_on_the_modern_query_deadline() {
    let of = plugin("legacy.big-catalog");
    let mut runtime = runtime();
    runtime.register(of.clone(), package("big-catalog"));
    boot(&mut runtime, 0);

    runtime
        .catalog_rebuild(&of, 10)
        .expect("spec 14.8: a catalog rebuild can be requested at any time");
    let dispatched = runtime.tick(10);
    let build = only_for(&dispatched, &of).clone();

    // A keystroke arrives while the rebuild runs. It must be serialized behind
    // the rebuild (spec 14.8) — and it must NOT make the rebuild obsolete: a
    // catalog build is not query work, and spec 9.2 lists only obsolete
    // queries, reload, shutdown, disable and instance supersession as reasons
    // for should_terminate() to become true.
    let queued = runtime.submit_query("x", 20);
    runtime.tick(20);
    let busy = state(&runtime, &of);
    assert_eq!(
        (busy.running_callback, busy.pending, busy.pending_query.clone()),
        (
            Some(LegacyCallback::OnCatalog),
            Some(queued),
            Some("x".to_string())
        ),
        "spec 14.8: on_catalog() is serialized with other callbacks for the same instance, so the \
         keystroke waits behind it"
    );
    assert!(
        !runtime.should_terminate(&of),
        "spec 9.2 / 14.8: a keystroke does not make an in-flight catalog build obsolete"
    );
    assert!(
        runtime.worker().terminations(&of).is_empty(),
        "spec 14.8: a query change never requests cooperative termination of a catalog build"
    );

    // Past the modern hard query deadline. A modern plugin would be killed here.
    runtime.tick(300);
    assert_eq!(
        runtime.deadline_policy(&of, LegacyCallback::OnCatalog),
        Some(DeadlinePolicy::Cooperative {
            soft_warning_ms: 5_000,
            hung_worker_ms: 120_000,
        }),
        "spec 9.6 / 14.8: legacy callbacks get soft warnings and a long hung-worker watchdog, \
         never the modern hard query deadline"
    );
    assert_eq!(
        deadlines().modern_policy(),
        DeadlinePolicy::HardKill { after_ms: 250 },
        "spec 9.6: the modern budget this catalog build has already exceeded is a hard kill, and \
         that is exactly the policy the legacy layer refuses to apply"
    );
    assert_eq!(
        state(&runtime, &of).running_callback,
        Some(LegacyCallback::OnCatalog),
        "spec 9.6 / 14.8: the catalog build is still running well past the modern hard deadline"
    );
    assert!(
        runtime.worker().stops.is_empty() && runtime.worker().terminations(&of).is_empty(),
        "spec 9.6: exceeding the modern budget must neither stop the worker nor terminate the \
         legacy callback"
    );

    // The legacy ladder does apply: a soft latency warning, emitted once.
    runtime.tick(5_100);
    runtime.tick(5_200);
    expect_trace(
        &runtime,
        LegacyTraceEvent::SoftLatencyWarning {
            at_ms: 5_100,
            plugin: of.clone(),
            callback: LegacyCallback::OnCatalog,
            elapsed_ms: 5_090,
        },
        "spec 9.6",
    );
    assert_eq!(
        diagnostics(&runtime, &of).soft_latency_warnings,
        1,
        "spec 9.6 / 26.4: the soft warning is emitted once per callback, not once per tick"
    );

    // ...and only the substantially longer watchdog reports a suspected hang.
    runtime.tick(120_100);
    expect_trace(
        &runtime,
        LegacyTraceEvent::HungWorkerSuspected {
            at_ms: 120_100,
            plugin: of.clone(),
            callback: LegacyCallback::OnCatalog,
            elapsed_ms: 120_090,
        },
        "spec 9.6",
    );

    // The build still completes normally, and only then does the query run.
    let updated = runtime.deliver(
        LegacyResponse::set_catalog(&build, vec![item(&of, "big.one", "One")]),
        120_200,
    );
    assert_eq!(
        updated,
        Delivery::CatalogUpdated { total: 1 },
        "spec 14.8: the catalog build was never killed, so its result is still accepted"
    );
    let resumed = runtime.tick(120_200);
    assert_eq!(
        (resumed.len(), resumed.first().map(call_of)),
        (1, Some(Call::InitialSuggest("x".to_string()))),
        "spec 14.8: the keystroke serialized behind on_catalog() runs once the rebuild returns"
    );
}

#[test]
fn set_catalog_replaces_the_live_catalog_and_merge_catalog_extends_it() {
    let of = plugin("legacy.notes");
    let mut runtime = runtime();
    runtime.register(of.clone(), package("notes"));
    boot(&mut runtime, 0);

    let rebuild = |runtime: &mut LegacyRuntime<ScriptedWorker>, at_ms: Millis| {
        runtime
            .catalog_rebuild(&of, at_ms)
            .expect("spec 14.8: on_catalog() may be called repeatedly");
        let dispatched = runtime.tick(at_ms);
        only_for(&dispatched, &of).clone()
    };

    let first = rebuild(&mut runtime, 10);
    let replaced = runtime.deliver(
        LegacyResponse::set_catalog(
            &first,
            vec![item(&of, "notes.a", "Alpha"), item(&of, "notes.b", "Bravo")],
        ),
        20,
    );
    assert_eq!(
        replaced,
        Delivery::CatalogUpdated { total: 2 },
        "spec 14.8: set_catalog() publishes a complete catalog"
    );
    assert_eq!(
        item_ids(runtime.catalog(&of)),
        vec!["notes.a", "notes.b"],
        "spec 14.8: set_catalog() establishes the live catalog"
    );

    let second = rebuild(&mut runtime, 30);
    let merged = runtime.deliver(
        LegacyResponse::merge_catalog(
            &second,
            vec![
                item(&of, "notes.b", "Bravo (revised)"),
                item(&of, "notes.c", "Charlie"),
            ],
        ),
        40,
    );
    assert_eq!(
        merged,
        Delivery::CatalogUpdated { total: 3 },
        "spec 14.8: merge_catalog() extends rather than replaces"
    );
    assert_eq!(
        item_ids(runtime.catalog(&of)),
        vec!["notes.a", "notes.b", "notes.c"],
        "spec 14.8 / 10.2: a merge keeps existing items, appends new ones, and updates an existing \
         item in place by its stable id rather than duplicating it"
    );
    assert_eq!(
        labels(runtime.catalog(&of)),
        vec!["Alpha", "Bravo (revised)", "Charlie"],
        "spec 14.8: the merged item replaces the earlier item carrying the same stable id"
    );

    let third = rebuild(&mut runtime, 50);
    runtime.deliver(
        LegacyResponse::set_catalog(&third, vec![item(&of, "notes.d", "Delta")]),
        60,
    );
    assert_eq!(
        item_ids(runtime.catalog(&of)),
        vec!["notes.d"],
        "spec 14.8: set_catalog() replaces the whole catalog, discarding what a merge had added"
    );

    expect_trace(
        &runtime,
        LegacyTraceEvent::CatalogMerged {
            at_ms: 40,
            plugin: of.clone(),
            added: 1,
            total: 3,
        },
        "spec 26.4",
    );
    expect_trace(
        &runtime,
        LegacyTraceEvent::CatalogReplaced {
            at_ms: 60,
            plugin: of.clone(),
            items: 1,
        },
        "spec 26.4",
    );
}

#[test]
fn a_catalog_update_from_an_obsolete_instance_is_rejected_without_mutating_the_catalog() {
    let of = plugin("legacy.reloaded");
    let mut runtime = runtime();
    let original = runtime.register(of.clone(), package("reloaded"));
    boot(&mut runtime, 0);

    // Establish a live catalog under the original instance.
    runtime
        .catalog_rebuild(&of, 10)
        .expect("spec 14.8: on_catalog() may be called repeatedly");
    let settled_tick = runtime.tick(10);
    let settled = only_for(&settled_tick, &of).clone();
    runtime.deliver(
        LegacyResponse::set_catalog(
            &settled,
            vec![item(&of, "live.a", "Alpha"), item(&of, "live.b", "Bravo")],
        ),
        20,
    );

    // A second rebuild is in flight when the package is reloaded.
    runtime
        .catalog_rebuild(&of, 30)
        .expect("spec 14.8: on_catalog() may be called repeatedly");
    let orphaned_tick = runtime.tick(30);
    let orphaned = only_for(&orphaned_tick, &of).clone();
    assert_eq!(
        orphaned.instance, original,
        "spec 14.8: the in-flight rebuild belongs to the original instance"
    );

    let replacement = runtime
        .reload(&of, 40)
        .expect("spec 14.8: a loaded legacy package can be reloaded");
    assert_ne!(
        replacement, original,
        "spec 9.2: a reload supersedes the plugin instance rather than reusing it"
    );

    // The superseded instance answers anyway, with a catalog that must never
    // reach the live one.
    let rejected = runtime.deliver(
        LegacyResponse::set_catalog(&orphaned, vec![item(&of, "poison.z", "MUST NOT BE CATALOGUED")]),
        50,
    );
    assert_eq!(
        rejected,
        Delivery::RejectedObsoleteInstance {
            instance: original,
            current: replacement,
        },
        "spec 14.8: catalog updates from obsolete plugin instances are rejected"
    );
    assert_eq!(
        item_ids(runtime.catalog(&of)),
        vec!["live.a", "live.b"],
        "spec 14.8: the rejection leaves the live catalog completely unmutated"
    );
    expect_trace(
        &runtime,
        LegacyTraceEvent::CatalogRejected {
            at_ms: 50,
            plugin: of.clone(),
            instance: original,
            reason: CatalogRejectReason::ObsoleteInstance,
        },
        "spec 26.4",
    );
    assert_eq!(
        diagnostics(&runtime, &of).catalog_updates_rejected,
        1,
        "spec 26.4: rejected catalog updates are counted per plugin"
    );
}

// ---------------------------------------------------------------------------
// Cooperative teardown
// ---------------------------------------------------------------------------

#[test]
fn reload_and_shutdown_terminate_cooperatively_and_complete_within_a_bounded_budget() {
    let rebuilding = plugin("legacy.rebuilding");
    let idle = plugin("legacy.idle");
    let mut runtime = runtime();
    let original = runtime.register(rebuilding.clone(), package("rebuilding"));
    runtime.register(idle.clone(), package("idle"));
    boot(&mut runtime, 0);

    runtime
        .catalog_rebuild(&rebuilding, 10)
        .expect("spec 14.8: on_catalog() may be called repeatedly");
    runtime.tick(10);

    // Reload asks the pending rebuild to stop and supersedes the instance.
    let replacement = runtime
        .reload(&rebuilding, 20)
        .expect("spec 14.8: a loaded legacy package can be reloaded");
    let reload_request = TerminationRecord {
        at_ms: 20,
        plugin: rebuilding.clone(),
        instance: original,
        generation: Generation::ZERO,
        reason: TerminationReason::PackageReload,
    };
    assert_eq!(
        runtime.worker().terminations(&rebuilding),
        vec![reload_request.clone()],
        "spec 9.2 / 14.8: a pending catalog rebuild is asked to stop when its package reloads"
    );

    // The fresh instance re-runs one-time initialization: a reload is not a
    // rebuild. It is deliberately never answered, so teardown must bound itself
    // without the plugin's cooperation.
    let restarted = runtime.tick(20);
    assert_eq!(
        (
            restarted.len(),
            restarted.first().map(|request| request.instance),
            restarted.first().map(call_of),
        ),
        (1, Some(replacement), Some(Call::Start)),
        "spec 14.8: a reloaded package runs one-time initialization again on its new instance, \
         which a repeated on_catalog() never does"
    );

    let report = runtime.shutdown(100);

    assert_eq!(
        runtime.worker().terminations(&rebuilding),
        vec![
            reload_request,
            TerminationRecord {
                at_ms: 100,
                plugin: rebuilding.clone(),
                instance: replacement,
                generation: Generation::ZERO,
                reason: TerminationReason::Shutdown,
            },
        ],
        "spec 9.2: shutdown raises should_terminate() on the callback still in flight"
    );
    assert!(
        runtime.worker().terminations(&idle).is_empty(),
        "spec 9.2: an instance with nothing in flight has no work to terminate"
    );
    assert_eq!(
        (report.requested_at_ms, report.instances, report.abandoned),
        (100, 2, 1),
        "spec 9.6 / 14.8: teardown accounts for every live instance and reports the one that never \
         honoured the cooperative request"
    );
    assert_eq!(
        report.completed_at_ms - report.requested_at_ms,
        deadlines().teardown_ms,
        "spec 14.8: teardown completes within a bounded budget even when a plugin never stops"
    );
    assert_eq!(
        runtime.worker().stops,
        vec![(100, deadlines().teardown_ms)],
        "spec 9.6: the worker is stopped exactly once, with the teardown budget it must respect"
    );
    assert!(
        runtime.tick(400).is_empty(),
        "spec 9.3: nothing is dispatched after shutdown"
    );
}

// ---------------------------------------------------------------------------
// Observability
// ---------------------------------------------------------------------------

#[test]
fn every_legacy_scheduling_decision_is_observable_in_the_trace() {
    let of = plugin("legacy.traced");
    let mut runtime = runtime();
    let instance = runtime.register(of.clone(), package("traced"));
    boot(&mut runtime, 0);

    let g1 = runtime.submit_query("a", 10);
    let dispatched = runtime.tick(10);
    let first = only_for(&dispatched, &of).clone();
    let g2 = runtime.submit_query("ab", 20);
    runtime.tick(20);
    let g3 = runtime.submit_query("abc", 30);
    runtime.tick(30);
    runtime.deliver(LegacyResponse::suggestions(&first, Vec::new()), 40);
    let resumed = runtime.tick(40);
    let newest = only_for(&resumed, &of).clone();
    runtime.deliver(
        LegacyResponse::suggestions(&newest, vec![item(&of, "traced.hit", "Hit")]),
        50,
    );
    runtime.tick(50);

    // The scheduling verdicts are the shared `LegacyDispatch` vocabulary, not a
    // parallel legacy-only one.
    assert_eq!(
        decisions(&runtime, &of),
        vec![
            (0, LegacyDispatch::Now(Generation::ZERO)),
            (0, LegacyDispatch::Idle),
            (10, LegacyDispatch::Now(g1)),
            (
                20,
                LegacyDispatch::QueuedBehindRunning {
                    obsolete: g1,
                    queued: g2,
                }
            ),
            (
                30,
                LegacyDispatch::QueuedBehindRunning {
                    obsolete: g1,
                    queued: g3,
                }
            ),
            (40, LegacyDispatch::Now(g3)),
            (50, LegacyDispatch::Idle),
        ],
        "spec 8.4 / 26.4: every serial-dispatcher verdict is recorded verbatim in the shared \
         crikey_input_scheduler::LegacyDispatch vocabulary rather than a parallel legacy one"
    );

    // Dispatch, cancellation, replacement and stale rejection are each
    // individually attributable, and they are recorded in causal order.
    let ordered = [
        LegacyTraceEvent::Dispatched {
            at_ms: 10,
            plugin: of.clone(),
            generation: g1,
            callback: LegacyCallback::OnSuggest,
        },
        LegacyTraceEvent::TerminationRequested {
            at_ms: 20,
            plugin: of.clone(),
            generation: g1,
            reason: TerminationReason::QuerySuperseded,
        },
        LegacyTraceEvent::Replaced {
            at_ms: 30,
            plugin: of.clone(),
            discarded: g2,
            retained: g3,
        },
        LegacyTraceEvent::StaleRejected {
            at_ms: 40,
            plugin: of.clone(),
            generation: g1,
            current: g3,
        },
        LegacyTraceEvent::Dispatched {
            at_ms: 40,
            plugin: of.clone(),
            generation: g3,
            callback: LegacyCallback::OnSuggest,
        },
    ];
    let positions: Vec<usize> = ordered.iter().map(|event| trace_index(&runtime, event)).collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "spec 26.4: dispatch, cooperative cancellation, pending replacement and stale rejection \
         must appear in causal order; observed positions {positions:?} in {:?}",
        runtime.trace()
    );

    assert_eq!(
        runtime.worker().terminations(&of),
        vec![TerminationRecord {
            at_ms: 20,
            plugin: of.clone(),
            instance,
            generation: g1,
            reason: TerminationReason::QuerySuperseded,
        }],
        "spec 9.2 / 26.4: the trace agrees with what the worker was actually told"
    );
    let counters = diagnostics(&runtime, &of);
    assert_eq!(
        (
            counters.dispatched,
            counters.replaced,
            counters.terminations_requested,
            counters.stale_rejected,
        ),
        (3, 1, 1, 1),
        "spec 26.4: the per-plugin counters summarise the same session the trace details — \
         on_start plus two suggestion callbacks dispatched, one pending replacement, one \
         cooperative termination, one stale rejection"
    );
    assert_eq!(
        item_ids(runtime.visible_items()),
        vec!["traced.hit"],
        "spec 31.7: exactly the current generation's answer is displayed at the end of the session"
    );
}
