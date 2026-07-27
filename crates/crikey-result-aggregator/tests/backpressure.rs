//! Bounded inbound result queue and the aggregator boundary
//! (spec 8.1, 8.12, 9.3, 9.5, 11.7, 12.3 - 12.6, 24.3, 25.5, 26.4;
//! roadmap M2 "bounded request and result queues with named overflow policies,
//! per-plugin budgets and fair queuing"; acceptance 31.4, 31.7, 31.8, 31.24,
//! 31.25).
//!
//! These tests are written before the implementation. They pin a second,
//! *transport-side* bound that does not exist yet and that is deliberately a
//! separate object from [`MemoryResultAggregator`]:
//!
//! * [`ResultLimits`] bounds what is **retained and displayed** for a query.
//!   It is enforced at merge time and its rejections are [`RejectReason`]s.
//! * [`InboundResultQueue`] bounds what is **resident in memory in flight**
//!   between plugin workers and the aggregator. It is enforced at submit time
//!   and its rejections are [`QueueReject`]s.
//!
//! Keeping them apart is the point: a plugin can be inside every retained-item
//! quota and still be producing faster than the UI drains, and it can be inside
//! every transport bound and still breach a retained quota. Both bounds must
//! hold, neither may be reported as the other, and every decision either one
//! takes must be atomic and individually diagnosable.
//!
//! # Surface under test
//!
//! * `InboundResultQueue::new(QueueLimits)` - boundary-wide hard bounds
//!   (`capacity_batches`, `capacity_items`) summed over every plugin.
//! * `register(PluginId, IntakePolicy)` - policies are **explicit**. An
//!   unregistered plugin is refused rather than silently defaulted, so no
//!   plugin can ever reach the aggregator without a named overflow policy.
//! * `IntakePolicy { capacity_batches, capacity_items, pause_at_batches,
//!   resume_at_batches, overflow }` - per-plugin budgets plus the watermarks
//!   driving the producer pause signal (spec 12.3, 8.12).
//! * `OverflowPolicy` - the named policies of spec 12.4:
//!   `RejectLowPriority` (modern: capacity above the watermark is reserved for
//!   high-priority batches), `PauseProducer` (legacy: never shed part of a
//!   complete publication, hold the producer instead - spec 12.2, 12.3),
//!   `ReplaceOldest`, and `Disconnect` for a producer that will not stop.
//! * `submit(at_ms, InboundBatch) -> Result<ProducerState, QueueReject>` - the
//!   `Ok` value *is* the backpressure signal the transport reads.
//! * `begin_generation(Generation)` / `retire_before(Generation)` - mirror the
//!   aggregator's generation gating. Both are O(1): they retag the boundary and
//!   leave resident entries to be reclaimed lazily. Reclaiming every plugin's
//!   queue on the UI thread once per keystroke is exactly the unbounded work
//!   this milestone exists to remove, so entries for retired generations stay
//!   resident, are dropped *first* whenever room is needed, and can never be
//!   merged.
//! * `drain_into(at_ms, &mut impl ResultAggregator, DrainBudget) -> DrainReport`
//!   - one round-robin pass with a rotating cursor and per-plugin budgets, so a
//!     high-volume plugin cannot monopolize a frame (spec 8.12, 25.5).
//! * `depth()` / `plugin_depth()` / `producer_state()` / `diagnostics()` /
//!   `take_events()` - bounded-depth evidence, cumulative loss accounting and
//!   the bounded recent record of structured, timestamped decisions (spec 26.4
//!   "result-batch sizes", "rejected stale responses", and presentation).
//!
//! Every timestamp below is a virtual millisecond passed in by the test. No
//! test reads a clock, sleeps, or spawns a thread.

use std::collections::BTreeMap;

use crikey_core::{
    ArgumentPolicy, Category, Generation, GenerationTracker, HitPolicy, Item, ItemId, PluginId,
};
use crikey_result_aggregator::{
    BatchPriority, BatchState, DrainBudget, DrainReport, InboundBatch, InboundResultQueue, IntakePolicy,
    MemoryResultAggregator, MergedBatch, OverflowPolicy, ProducerState, QueueDepth, QueueEvent,
    QueueEventKind, QueueLimits, QueueReject, RejectReason, ResultAggregator, ResultBatch, ResultLimits,
};

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

/// Retained-item limits wide enough that nothing here trips them by accident.
/// The per-frame snapshot budget is deliberately generous so that a `None` from
/// `take_ui_update` always means "no repaint is owed", never "budget spent".
fn generous_result_limits() -> ResultLimits {
    ResultLimits {
        max_items_per_batch: 64,
        max_items_per_plugin_per_query: 512,
        max_items_per_query: 4_096,
        max_icon_reference_bytes_per_batch: usize::MAX,
        max_metadata_bytes_per_batch: usize::MAX,
        max_ui_updates_per_frame: 8,
    }
}

/// Generous everywhere except the per-plugin retained quota under test.
fn retained_quota_of(per_plugin: usize) -> ResultLimits {
    ResultLimits {
        max_items_per_plugin_per_query: per_plugin,
        ..generous_result_limits()
    }
}

/// Boundary-wide transport bounds wide enough to leave the per-plugin policy in
/// charge.
fn generous_queue_limits() -> QueueLimits {
    QueueLimits {
        capacity_batches: 256,
        capacity_items: 4_096,
    }
}

/// An intake policy with no practical bound. Tests tighten exactly the one
/// field they are about via struct update syntax, which keeps every threshold
/// that matters visible at the call site.
fn unbounded_intake(overflow: OverflowPolicy) -> IntakePolicy {
    IntakePolicy {
        capacity_batches: usize::MAX,
        capacity_items: usize::MAX,
        pause_at_batches: usize::MAX,
        resume_at_batches: usize::MAX - 1,
        overflow,
    }
}

fn plugin(id: &str) -> PluginId {
    PluginId(id.to_owned())
}

fn item(owner: &PluginId, stable_id: &str) -> Item {
    Item {
        stable_id: ItemId(stable_id.to_owned()),
        plugin_id: owner.clone(),
        category: Category::Application,
        label: stable_id.to_owned(),
        description: String::new(),
        target: format!("/usr/bin/{stable_id}"),
        search_terms: Vec::new(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    }
}

fn batch(generation: Generation, owner: &PluginId, state: BatchState, ids: &[&str]) -> ResultBatch {
    ResultBatch {
        generation,
        plugin: owner.clone(),
        state,
        items: ids.iter().map(|id| item(owner, id)).collect(),
    }
}

fn partial(generation: Generation, owner: &PluginId, ids: &[&str]) -> ResultBatch {
    batch(generation, owner, BatchState::Partial, ids)
}

/// A batch the UI is waiting on: results the user is about to look at.
fn high(batch: ResultBatch) -> InboundBatch {
    InboundBatch {
        batch,
        priority: BatchPriority::High,
    }
}

/// A batch the UI can live without: low-ranked tail results or late enrichment
/// (spec 12.3 "the UI no longer needs additional low-ranked results").
fn low(batch: ResultBatch) -> InboundBatch {
    InboundBatch {
        batch,
        priority: BatchPriority::Low,
    }
}

fn ids(items: &[Item]) -> Vec<&str> {
    items.iter().map(|it| it.stable_id.0.as_str()).collect()
}

fn depth(batches: usize, items: usize, obsolete: usize) -> QueueDepth {
    QueueDepth {
        batches,
        items,
        obsolete,
    }
}

fn budget(batches_per_plugin: usize, items_per_plugin: usize, total_batches: usize) -> DrainBudget {
    DrainBudget {
        batches_per_plugin,
        items_per_plugin,
        total_batches,
    }
}

/// A drain budget large enough not to be the thing under test.
fn unlimited_budget() -> DrainBudget {
    budget(64, 4_096, 256)
}

fn kinds(events: &[QueueEvent]) -> Vec<QueueEventKind> {
    events.iter().map(|event| event.kind.clone()).collect()
}

/// The two independently bounded halves of the boundary, plus the tracker that
/// mints generations. They are separate objects on purpose; only the generation
/// hand-off is shared, so that is the only thing this harness hides.
#[derive(Debug)]
struct Boundary {
    queue: InboundResultQueue,
    aggregator: MemoryResultAggregator,
    generations: GenerationTracker,
}

impl Boundary {
    fn new(queue_limits: QueueLimits, result_limits: ResultLimits) -> Self {
        Self {
            queue: InboundResultQueue::new(queue_limits),
            aggregator: MemoryResultAggregator::new(result_limits),
            generations: GenerationTracker::new(),
        }
    }

    /// Mints the next generation, installs it on both halves, and drains the
    /// empty repaint the aggregator schedules, so each test starts quiet.
    fn begin_generation(&mut self) -> Generation {
        let generation = self.generations.advance();
        self.queue.begin_generation(generation);
        self.aggregator.begin_generation(generation);
        let _ = self.aggregator.take_ui_update();
        self.aggregator.begin_frame();
        generation
    }

    fn drain(&mut self, at_ms: u64, budget: DrainBudget) -> DrainReport {
        self.queue.drain_into(at_ms, &mut self.aggregator, budget)
    }

    fn visible(&self) -> Vec<&str> {
        ids(self.aggregator.items())
    }

    fn producer(&self, plugin: &PluginId) -> ProducerState {
        self.queue
            .producer_state(plugin)
            .expect("a registered plugin always has a producer state")
    }
}

// ---------------------------------------------------------------------------
// Policies are explicit (spec 12.4: every queue has a *named* overflow policy).
// ---------------------------------------------------------------------------

#[test]
fn an_unregistered_plugin_is_refused_and_never_occupies_the_queue() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let ghost = plugin("dev.crikey.ghost");
    let generation = boundary.begin_generation();

    let refusal = boundary
        .queue
        .submit(10, high(partial(generation, &ghost, &["g1"])))
        .expect_err("a plugin with no registered intake policy has no bound to enforce");

    assert_eq!(refusal, QueueReject::Unregistered);
    assert_eq!(boundary.queue.depth(), depth(0, 0, 0));
    assert_eq!(boundary.queue.producer_state(&ghost), None);
    assert_eq!(
        boundary.queue.diagnostics().rejected(QueueReject::Unregistered),
        1
    );

    let report = boundary.drain(20, unlimited_budget());
    assert_eq!(report.merged, 0);
    assert!(boundary.aggregator.items().is_empty());
}

// ---------------------------------------------------------------------------
// Transport bounds: batches and items resident in flight (spec 12.4, 31.24).
// ---------------------------------------------------------------------------

#[test]
fn the_per_plugin_batch_budget_bounds_resident_depth() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary.queue.register(
        apps.clone(),
        IntakePolicy {
            capacity_batches: 2,
            pause_at_batches: 2,
            resume_at_batches: 1,
            ..unbounded_intake(OverflowPolicy::PauseProducer)
        },
    );
    let generation = boundary.begin_generation();

    for (at, id) in [(10, "a1"), (11, "a2")] {
        boundary
            .queue
            .submit(at, high(partial(generation, &apps, &[id])))
            .expect("the first two batches fit the per-plugin budget");
    }

    let refusal = boundary
        .queue
        .submit(12, high(partial(generation, &apps, &["a3"])))
        .expect_err("the third batch would exceed the per-plugin budget");

    assert_eq!(refusal, QueueReject::QueueFull);
    assert_eq!(boundary.queue.plugin_depth(&apps), depth(2, 2, 0));
    assert_eq!(boundary.queue.diagnostics().peak_batches(), 2);
    assert_eq!(boundary.producer(&apps), ProducerState::Paused);
}

#[test]
fn the_per_plugin_item_budget_rejects_the_breaching_batch_whole() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary.queue.register(
        apps.clone(),
        IntakePolicy {
            capacity_items: 4,
            ..unbounded_intake(OverflowPolicy::PauseProducer)
        },
    );
    let generation = boundary.begin_generation();

    boundary
        .queue
        .submit(10, high(partial(generation, &apps, &["a1", "a2", "a3"])))
        .expect("three items fit an item budget of four");

    let refusal = boundary
        .queue
        .submit(11, high(partial(generation, &apps, &["a4", "a5"])))
        .expect_err("two more items would exceed the item budget");

    // Never truncated to fit: the whole batch is refused and the transport may
    // resend it once the queue drains.
    assert_eq!(refusal, QueueReject::QueueFull);
    assert_eq!(boundary.queue.plugin_depth(&apps), depth(1, 3, 0));

    boundary
        .queue
        .submit(12, high(partial(generation, &apps, &["a4"])))
        .expect("one item still fits");
    assert_eq!(boundary.queue.plugin_depth(&apps), depth(2, 4, 0));
}

#[test]
fn the_boundary_budget_bounds_the_sum_across_plugins() {
    let mut boundary = Boundary::new(
        QueueLimits {
            capacity_batches: 3,
            capacity_items: 64,
        },
        generous_result_limits(),
    );
    let apps = plugin("dev.crikey.apps");
    let web = plugin("dev.crikey.web");
    for owner in [&apps, &web] {
        boundary.queue.register(
            owner.clone(),
            IntakePolicy {
                capacity_batches: 8,
                ..unbounded_intake(OverflowPolicy::PauseProducer)
            },
        );
    }
    let generation = boundary.begin_generation();

    boundary
        .queue
        .submit(10, high(partial(generation, &apps, &["a1"])))
        .expect("boundary has room");
    boundary
        .queue
        .submit(11, high(partial(generation, &apps, &["a2"])))
        .expect("boundary has room");
    boundary
        .queue
        .submit(12, high(partial(generation, &web, &["w1"])))
        .expect("boundary is now exactly full");

    let refusal = boundary
        .queue
        .submit(13, high(partial(generation, &web, &["w2"])))
        .expect_err("the boundary bound binds even though this plugin's own budget has room");

    assert_eq!(refusal, QueueReject::BoundaryFull);
    // No cross-plugin theft: a full boundary never evicts another plugin's
    // live work to make room.
    assert_eq!(boundary.queue.plugin_depth(&apps), depth(2, 2, 0));
    assert_eq!(boundary.queue.plugin_depth(&web), depth(1, 1, 0));
    assert_eq!(boundary.queue.depth(), depth(3, 3, 0));
}

// ---------------------------------------------------------------------------
// Named overflow policies (spec 12.4).
// ---------------------------------------------------------------------------

#[test]
fn low_priority_batches_are_shed_at_the_watermark_while_the_reserve_stays_open() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary.queue.register(
        apps.clone(),
        IntakePolicy {
            capacity_batches: 4,
            pause_at_batches: 2,
            resume_at_batches: 1,
            ..unbounded_intake(OverflowPolicy::RejectLowPriority)
        },
    );
    let generation = boundary.begin_generation();

    boundary
        .queue
        .submit(10, high(partial(generation, &apps, &["a1"])))
        .expect("below the watermark");
    boundary
        .queue
        .submit(11, low(partial(generation, &apps, &["a2"])))
        .expect("still below the watermark when it arrives");

    let shed = boundary
        .queue
        .submit(12, low(partial(generation, &apps, &["a3"])))
        .expect_err("at the watermark, low-priority traffic is what gives way");
    assert_eq!(shed, QueueReject::LowPriorityShed);
    assert_eq!(boundary.queue.plugin_depth(&apps), depth(2, 2, 0));

    // The capacity above the watermark is reserved for results the UI is
    // actually waiting on.
    boundary
        .queue
        .submit(13, high(partial(generation, &apps, &["a4"])))
        .expect("high priority uses the reserve");
    boundary
        .queue
        .submit(14, high(partial(generation, &apps, &["a5"])))
        .expect("high priority uses the reserve up to the hard bound");

    let full = boundary
        .queue
        .submit(15, high(partial(generation, &apps, &["a6"])))
        .expect_err("the reserve is finite; nothing grows without bound");
    assert_eq!(full, QueueReject::QueueFull);

    boundary.drain(20, unlimited_budget());
    assert_eq!(boundary.visible(), ["a1", "a2", "a4", "a5"]);

    let diagnostics = boundary.queue.diagnostics();
    assert_eq!(diagnostics.rejected(QueueReject::LowPriorityShed), 1);
    assert_eq!(diagnostics.rejected(QueueReject::QueueFull), 1);
    assert_eq!(diagnostics.admitted(), 4);
    assert_eq!(diagnostics.peak_batches(), 4);
}

#[test]
fn the_pause_producer_policy_sheds_nothing_and_loses_no_batch() {
    // The legacy shape: a `set_suggestions()` publication is complete by
    // definition, so the boundary holds the producer instead of shedding part
    // of it (spec 12.2, 12.3).
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let legacy = plugin("keypirinha.calc");
    boundary.queue.register(
        legacy.clone(),
        IntakePolicy {
            capacity_batches: 2,
            pause_at_batches: 1,
            resume_at_batches: 0,
            ..unbounded_intake(OverflowPolicy::PauseProducer)
        },
    );
    let generation = boundary.begin_generation();

    assert_eq!(
        boundary
            .queue
            .submit(10, low(partial(generation, &legacy, &["l1"])))
            .expect("a low-priority batch is still admitted under this policy"),
        ProducerState::Paused
    );
    boundary
        .queue
        .submit(11, low(partial(generation, &legacy, &["l2"])))
        .expect("the hard bound has not been reached yet");

    let held = boundary
        .queue
        .submit(12, low(partial(generation, &legacy, &["l3"])))
        .expect_err("at the hard bound the producer must stop reading");
    assert_eq!(held, QueueReject::QueueFull);
    assert_eq!(boundary.producer(&legacy), ProducerState::Paused);
    assert_eq!(
        boundary
            .queue
            .diagnostics()
            .rejected(QueueReject::LowPriorityShed),
        0
    );

    boundary.drain(20, unlimited_budget());
    assert_eq!(boundary.producer(&legacy), ProducerState::Running);

    // Held, not dropped: the transport resends and the publication arrives
    // whole and in order.
    boundary
        .queue
        .submit(21, low(partial(generation, &legacy, &["l3"])))
        .expect("the producer resumed, so the held batch is admitted");
    boundary.drain(22, unlimited_budget());
    assert_eq!(boundary.visible(), ["l1", "l2", "l3"]);
}

#[test]
fn the_replace_oldest_policy_keeps_the_newest_batch_and_reports_the_eviction() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary.queue.register(
        apps.clone(),
        IntakePolicy {
            capacity_batches: 2,
            pause_at_batches: 2,
            resume_at_batches: 1,
            ..unbounded_intake(OverflowPolicy::ReplaceOldest)
        },
    );
    let generation = boundary.begin_generation();
    let _ = boundary.queue.take_events();

    for (at, id) in [(10, "a1"), (11, "a2")] {
        boundary
            .queue
            .submit(at, high(partial(generation, &apps, &[id])))
            .expect("both fit");
    }
    boundary
        .queue
        .submit(12, high(partial(generation, &apps, &["a3"])))
        .expect("the newest batch always wins under this policy");

    assert_eq!(boundary.queue.plugin_depth(&apps), depth(2, 2, 0));
    assert_eq!(boundary.queue.diagnostics().evicted_oldest(), 1);
    assert!(kinds(&boundary.queue.take_events()).contains(&QueueEventKind::EvictedOldest { items: 1 }));

    boundary.drain(20, unlimited_budget());
    assert_eq!(boundary.visible(), ["a2", "a3"]);
}

#[test]
fn replace_oldest_refuses_a_batch_that_still_would_not_fit_and_evicts_nothing() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary.queue.register(
        apps.clone(),
        IntakePolicy {
            capacity_items: 3,
            ..unbounded_intake(OverflowPolicy::ReplaceOldest)
        },
    );
    let generation = boundary.begin_generation();

    boundary
        .queue
        .submit(10, high(partial(generation, &apps, &["a1", "a2"])))
        .expect("two items fit");

    let refusal = boundary
        .queue
        .submit(11, high(partial(generation, &apps, &["b1", "b2", "b3", "b4"])))
        .expect_err("four items exceed the budget even with the queue emptied");

    // Atomic: a doomed submission must not destroy resident work on its way to
    // being refused.
    assert_eq!(refusal, QueueReject::QueueFull);
    assert_eq!(boundary.queue.plugin_depth(&apps), depth(1, 2, 0));
    assert_eq!(boundary.queue.diagnostics().evicted_oldest(), 0);

    boundary.drain(20, unlimited_budget());
    assert_eq!(boundary.visible(), ["a1", "a2"]);
}

#[test]
fn the_disconnect_policy_stops_admission_and_keeps_queued_work_drainable() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let rogue = plugin("dev.crikey.rogue");
    boundary.queue.register(
        rogue.clone(),
        IntakePolicy {
            capacity_batches: 1,
            pause_at_batches: 1,
            resume_at_batches: 0,
            ..unbounded_intake(OverflowPolicy::Disconnect)
        },
    );
    let generation = boundary.begin_generation();

    boundary
        .queue
        .submit(10, high(partial(generation, &rogue, &["r1"])))
        .expect("the first batch fits");
    let cut = boundary
        .queue
        .submit(11, high(partial(generation, &rogue, &["r2"])))
        .expect_err("a producer that overruns a full queue is a protocol safety breach");

    assert_eq!(cut, QueueReject::Disconnected);
    assert_eq!(boundary.producer(&rogue), ProducerState::Disconnected);
    assert_eq!(boundary.queue.plugin_depth(&rogue), depth(1, 1, 0));

    // Work admitted before the breach was valid and is still delivered.
    boundary.drain(20, unlimited_budget());
    assert_eq!(boundary.visible(), ["r1"]);

    let after = boundary
        .queue
        .submit(21, high(partial(generation, &rogue, &["r3"])))
        .expect_err("a drained queue does not readmit a disconnected producer");
    assert_eq!(after, QueueReject::Disconnected);
    assert_eq!(
        boundary.queue.diagnostics().rejected(QueueReject::Disconnected),
        2
    );
}

#[test]
fn a_disconnected_producer_is_revived_only_by_re_registration() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let rogue = plugin("dev.crikey.rogue");
    let strict = IntakePolicy {
        capacity_batches: 1,
        pause_at_batches: 1,
        resume_at_batches: 0,
        ..unbounded_intake(OverflowPolicy::Disconnect)
    };
    boundary.queue.register(rogue.clone(), strict);
    let first = boundary.begin_generation();

    boundary
        .queue
        .submit(10, high(partial(first, &rogue, &["r1"])))
        .expect("the first batch fits");
    boundary
        .queue
        .submit(11, high(partial(first, &rogue, &["r2"])))
        .expect_err("overrun disconnects");

    // A disconnect is a transport fact, not a per-query one: a new keystroke
    // must not silently readmit a plugin the host cut off.
    let second = boundary.begin_generation();
    assert_eq!(boundary.producer(&rogue), ProducerState::Disconnected);
    assert_eq!(
        boundary
            .queue
            .submit(20, high(partial(second, &rogue, &["r3"])))
            .expect_err("still disconnected"),
        QueueReject::Disconnected
    );

    boundary.queue.register(rogue.clone(), strict);
    assert_eq!(boundary.producer(&rogue), ProducerState::Running);
    boundary
        .queue
        .submit(30, high(partial(second, &rogue, &["r4"])))
        .expect("re-registration restores the producer");
    boundary.drain(31, unlimited_budget());
    assert_eq!(boundary.visible(), ["r4"]);
}

// ---------------------------------------------------------------------------
// Obsolete entries are reclaimed first (spec 12.4 "drop obsolete requests").
// ---------------------------------------------------------------------------

#[test]
fn begin_generation_is_constant_work_and_leaves_entries_for_lazy_reclamation() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary
        .queue
        .register(apps.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));
    let first = boundary.begin_generation();

    for (at, id) in [(10, "a1"), (11, "a2")] {
        boundary
            .queue
            .submit(at, high(partial(first, &apps, &[id])))
            .expect("both fit");
    }
    let _ = boundary.queue.take_events();

    boundary.begin_generation();

    // Retagging the boundary must not walk every plugin's queue on the UI
    // thread: the entries are still resident, merely marked obsolete.
    assert!(boundary.queue.take_events().is_empty());
    assert_eq!(boundary.queue.plugin_depth(&apps), depth(2, 2, 2));
}

#[test]
fn obsolete_entries_are_reclaimed_before_the_overflow_policy_runs() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary.queue.register(
        apps.clone(),
        IntakePolicy {
            capacity_batches: 2,
            pause_at_batches: 2,
            resume_at_batches: 1,
            ..unbounded_intake(OverflowPolicy::RejectLowPriority)
        },
    );
    let first = boundary.begin_generation();

    for (at, id) in [(10, "a1"), (11, "a2")] {
        boundary
            .queue
            .submit(at, high(partial(first, &apps, &[id])))
            .expect("both fit");
    }
    let second = boundary.begin_generation();
    let _ = boundary.queue.take_events();

    // A *low* priority batch: under the naive order it would be shed at the
    // watermark. Dead weight from a retired generation must go first.
    boundary
        .queue
        .submit(20, low(partial(second, &apps, &["b1"])))
        .expect("reclaiming the obsolete entries makes room");

    assert_eq!(
        kinds(&boundary.queue.take_events()),
        [
            QueueEventKind::DroppedObsolete { batches: 2, items: 2 },
            QueueEventKind::Admitted { items: 1 },
            QueueEventKind::ProducerResumed,
        ]
    );
    assert_eq!(boundary.queue.plugin_depth(&apps), depth(1, 1, 0));
    assert_eq!(boundary.queue.diagnostics().dropped_obsolete(), 2);
    assert_eq!(
        boundary
            .queue
            .diagnostics()
            .rejected(QueueReject::LowPriorityShed),
        0
    );
}

#[test]
fn obsolete_reclamation_crosses_plugins_to_satisfy_the_boundary_budget() {
    let mut boundary = Boundary::new(
        QueueLimits {
            capacity_batches: 3,
            capacity_items: 64,
        },
        generous_result_limits(),
    );
    let slow = plugin("dev.crikey.slow");
    let fast = plugin("dev.crikey.fast");
    for owner in [&slow, &fast] {
        boundary
            .queue
            .register(owner.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));
    }
    let first = boundary.begin_generation();

    boundary
        .queue
        .submit(10, high(partial(first, &slow, &["s1"])))
        .expect("room");
    boundary
        .queue
        .submit(11, high(partial(first, &slow, &["s2"])))
        .expect("room");
    boundary
        .queue
        .submit(12, high(partial(first, &fast, &["f1"])))
        .expect("the boundary is now full");

    let second = boundary.begin_generation();
    boundary
        .queue
        .submit(20, high(partial(second, &fast, &["f2"])))
        .expect("another plugin's obsolete entries are reclaimable dead weight");

    assert_eq!(boundary.queue.depth(), depth(1, 1, 0));
    assert_eq!(boundary.queue.plugin_depth(&slow), depth(0, 0, 0));
    assert_eq!(boundary.queue.diagnostics().dropped_obsolete(), 3);

    boundary.drain(30, unlimited_budget());
    assert_eq!(boundary.visible(), ["f2"]);
}

#[test]
fn obsolete_entries_are_dropped_at_drain_and_never_offered_to_the_aggregator() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary
        .queue
        .register(apps.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));
    let first = boundary.begin_generation();

    for (at, id) in [(10, "a1"), (11, "a2")] {
        boundary
            .queue
            .submit(at, high(partial(first, &apps, &[id])))
            .expect("both fit");
    }
    boundary.begin_generation();

    let report = boundary.drain(30, unlimited_budget());

    assert_eq!(report.dropped_obsolete, 2);
    assert_eq!(report.merged, 0);
    assert!(report.merge_rejected.is_empty());
    assert_eq!(boundary.queue.depth(), depth(0, 0, 0));
    assert!(boundary.aggregator.items().is_empty());
    // The aggregator was never even asked: staleness is settled at the
    // boundary, one hop before anything could be mutated.
    assert_eq!(
        boundary
            .queue
            .diagnostics()
            .merge_rejected(RejectReason::StaleGeneration),
        0
    );
}

#[test]
fn retire_before_makes_resident_entries_obsolete_and_refuses_later_submissions() {
    // Dismissal, shutdown and plugin disable retire the query without opening a
    // new one (spec 9.3).
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary
        .queue
        .register(apps.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));
    let generation = boundary.begin_generation();

    boundary
        .queue
        .submit(10, high(partial(generation, &apps, &["a1"])))
        .expect("admitted while the query was live");

    let floor = boundary.generations.advance();
    boundary.queue.retire_before(floor);
    boundary.aggregator.retire_before(floor);

    let refusal = boundary
        .queue
        .submit(20, high(partial(generation, &apps, &["a2"])))
        .expect_err("nothing is current after retirement");
    assert_eq!(refusal, QueueReject::StaleGeneration);
    assert_eq!(boundary.queue.plugin_depth(&apps), depth(1, 1, 1));

    let report = boundary.drain(30, unlimited_budget());
    assert_eq!(report.dropped_obsolete, 1);
    assert_eq!(report.merged, 0);
    assert!(boundary.aggregator.items().is_empty());
}

// ---------------------------------------------------------------------------
// The producer pause signal (spec 12.3).
// ---------------------------------------------------------------------------

#[test]
fn the_producer_pauses_at_the_watermark_and_resumes_only_after_hysteresis() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary.queue.register(
        apps.clone(),
        IntakePolicy {
            capacity_batches: 5,
            pause_at_batches: 3,
            resume_at_batches: 1,
            ..unbounded_intake(OverflowPolicy::RejectLowPriority)
        },
    );
    let generation = boundary.begin_generation();
    let _ = boundary.queue.take_events();

    assert_eq!(
        boundary
            .queue
            .submit(10, high(partial(generation, &apps, &["a1"])))
            .expect("admitted"),
        ProducerState::Running
    );
    assert_eq!(
        boundary
            .queue
            .submit(11, high(partial(generation, &apps, &["a2"])))
            .expect("admitted"),
        ProducerState::Running
    );
    assert_eq!(
        boundary
            .queue
            .submit(12, high(partial(generation, &apps, &["a3"])))
            .expect("admitted"),
        ProducerState::Paused
    );

    boundary.drain(20, budget(1, 64, 1));
    assert_eq!(boundary.queue.plugin_depth(&apps).batches, 2);
    // Two is still above the resume watermark: a single drained batch must not
    // restart a producer that would immediately refill the queue.
    assert_eq!(boundary.producer(&apps), ProducerState::Paused);

    boundary.drain(21, budget(1, 64, 1));
    assert_eq!(boundary.producer(&apps), ProducerState::Running);

    let diagnostics = boundary.queue.diagnostics();
    assert_eq!(diagnostics.pauses(), 1);
    assert_eq!(diagnostics.resumes(), 1);

    let observed = kinds(&boundary.queue.take_events());
    assert_eq!(
        observed
            .iter()
            .filter(|kind| matches!(
                kind,
                QueueEventKind::ProducerPaused | QueueEventKind::ProducerResumed
            ))
            .cloned()
            .collect::<Vec<_>>(),
        [QueueEventKind::ProducerPaused, QueueEventKind::ProducerResumed]
    );
}

#[test]
fn pause_and_resume_are_reported_once_per_transition() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary.queue.register(
        apps.clone(),
        IntakePolicy {
            capacity_batches: 4,
            pause_at_batches: 2,
            resume_at_batches: 1,
            ..unbounded_intake(OverflowPolicy::PauseProducer)
        },
    );
    let generation = boundary.begin_generation();

    for (at, id) in [(10, "a1"), (11, "a2"), (12, "a3"), (13, "a4")] {
        boundary
            .queue
            .submit(at, high(partial(generation, &apps, &[id])))
            .expect("all four fit the hard bound");
    }
    boundary
        .queue
        .submit(14, high(partial(generation, &apps, &["a5"])))
        .expect_err("the fifth does not");

    // Staying paused is not a new event, and neither is being refused while
    // already paused.
    assert_eq!(boundary.queue.diagnostics().pauses(), 1);

    boundary.drain(20, budget(3, 64, 3));
    assert_eq!(boundary.queue.plugin_depth(&apps).batches, 1);
    assert_eq!(boundary.queue.diagnostics().resumes(), 1);
    assert_eq!(boundary.queue.diagnostics().pauses(), 1);
}

// ---------------------------------------------------------------------------
// Fair draining (spec 8.12, 25.5; acceptance 31.8).
// ---------------------------------------------------------------------------

#[test]
fn a_backlog_from_one_plugin_never_starves_another_plugins_single_batch() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let noisy = plugin("dev.crikey.noisy");
    let quiet = plugin("dev.crikey.quiet");
    for owner in [&noisy, &quiet] {
        boundary
            .queue
            .register(owner.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));
    }
    let generation = boundary.begin_generation();

    for (at, id) in [(10, "n1"), (11, "n2"), (12, "n3"), (13, "n4")] {
        boundary
            .queue
            .submit(at, high(partial(generation, &noisy, &[id])))
            .expect("admitted");
    }
    boundary
        .queue
        .submit(14, high(partial(generation, &quiet, &["q1"])))
        .expect("admitted last, behind four batches of backlog");

    // One batch per plugin, two batches of work in this frame. A FIFO queue
    // would spend both on the backlog.
    let report = boundary.drain(20, budget(1, 64, 2));

    assert_eq!(report.merged, 2);
    assert_eq!(boundary.visible(), ["n1", "q1"]);
    assert_eq!(boundary.queue.depth().batches, 3);
}

#[test]
fn consecutive_drains_rotate_the_starting_plugin() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let first_plugin = plugin("dev.crikey.a");
    let second_plugin = plugin("dev.crikey.b");
    let third_plugin = plugin("dev.crikey.c");
    for owner in [&first_plugin, &second_plugin, &third_plugin] {
        boundary
            .queue
            .register(owner.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));
    }
    let generation = boundary.begin_generation();

    for (owner, batch_ids) in [
        (&first_plugin, ["a1", "a2"]),
        (&second_plugin, ["b1", "b2"]),
        (&third_plugin, ["c1", "c2"]),
    ] {
        for (offset, id) in batch_ids.into_iter().enumerate() {
            boundary
                .queue
                .submit(10 + offset as u64, high(partial(generation, owner, &[id])))
                .expect("admitted");
        }
    }

    // Two batches per frame across three plugins: without a rotating cursor the
    // third plugin would never be served.
    boundary.drain(20, budget(1, 64, 2));
    boundary.drain(21, budget(1, 64, 2));
    boundary.drain(22, budget(1, 64, 2));

    assert_eq!(boundary.visible(), ["a1", "b1", "c1", "a2", "b2", "c2"]);
    assert_eq!(boundary.queue.depth(), depth(0, 0, 0));
}

#[test]
fn the_item_budget_caps_a_drain_without_starving_an_oversized_batch() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary
        .queue
        .register(apps.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));
    let generation = boundary.begin_generation();

    boundary
        .queue
        .submit(
            10,
            high(partial(generation, &apps, &["a1", "a2", "a3", "a4", "a5"])),
        )
        .expect("admitted");
    boundary
        .queue
        .submit(11, high(partial(generation, &apps, &["a6"])))
        .expect("admitted");

    // A batch bigger than the whole per-drain item budget must still make
    // progress, or it wedges the queue forever.
    let report = boundary.drain(20, budget(5, 2, 8));
    assert_eq!(report.merged, 1);
    assert_eq!(boundary.visible(), ["a1", "a2", "a3", "a4", "a5"]);
    assert_eq!(boundary.queue.plugin_depth(&apps), depth(1, 1, 0));

    boundary.drain(21, budget(5, 2, 8));
    assert_eq!(boundary.visible(), ["a1", "a2", "a3", "a4", "a5", "a6"]);
}

#[test]
fn the_total_batch_budget_bounds_a_drain_and_leaves_the_rest_resident() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary
        .queue
        .register(apps.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));
    let generation = boundary.begin_generation();

    for (at, id) in [(10, "a1"), (11, "a2"), (12, "a3")] {
        boundary
            .queue
            .submit(at, high(partial(generation, &apps, &[id])))
            .expect("admitted");
    }

    let report = boundary.drain(20, budget(5, 64, 2));
    assert_eq!(report.merged, 2);
    assert_eq!(boundary.visible(), ["a1", "a2"]);
    assert_eq!(boundary.queue.depth().batches, 1);

    boundary.drain(21, budget(5, 64, 2));
    assert_eq!(boundary.visible(), ["a1", "a2", "a3"]);
}

// ---------------------------------------------------------------------------
// Terminal stream states (spec 12.5).
// ---------------------------------------------------------------------------

#[test]
fn post_terminal_submissions_are_refused_at_the_boundary_without_consuming_capacity() {
    for terminal in [BatchState::Final, BatchState::Cancelled, BatchState::Failed] {
        let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
        let apps = plugin("dev.crikey.apps");
        boundary.queue.register(
            apps.clone(),
            IntakePolicy {
                capacity_batches: 4,
                ..unbounded_intake(OverflowPolicy::RejectLowPriority)
            },
        );
        let generation = boundary.begin_generation();

        boundary
            .queue
            .submit(10, high(batch(generation, &apps, terminal, &["t1"])))
            .expect("the terminal batch itself is admitted");

        let refusal = boundary
            .queue
            .submit(11, high(partial(generation, &apps, &["t2"])))
            .expect_err("the stream ended: later traffic never enters the queue");
        assert_eq!(refusal, QueueReject::StreamTerminated);
        assert_eq!(boundary.queue.plugin_depth(&apps), depth(1, 1, 0));

        boundary.drain(20, unlimited_budget());
        assert_eq!(boundary.visible(), ["t1"]);
        assert_eq!(boundary.aggregator.plugin_state(&apps), Some(terminal));

        // Draining the terminal batch does not reopen the stream.
        assert_eq!(
            boundary
                .queue
                .submit(21, high(partial(generation, &apps, &["t3"])))
                .expect_err("still terminated"),
            QueueReject::StreamTerminated
        );
        assert_eq!(
            boundary
                .queue
                .diagnostics()
                .rejected(QueueReject::StreamTerminated),
            2
        );
    }
}

#[test]
fn a_new_generation_reopens_the_stream_and_its_obsolete_entry_frees_the_budget() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary.queue.register(
        apps.clone(),
        IntakePolicy {
            capacity_batches: 1,
            pause_at_batches: 1,
            resume_at_batches: 0,
            ..unbounded_intake(OverflowPolicy::RejectLowPriority)
        },
    );
    let first = boundary.begin_generation();

    boundary
        .queue
        .submit(10, high(batch(first, &apps, BatchState::Final, &["f1"])))
        .expect("admitted, and the queue is now full and the stream terminal");

    let second = boundary.begin_generation();
    boundary
        .queue
        .submit(20, high(partial(second, &apps, &["s1"])))
        .expect("the new query reopens the stream and reclaims the obsolete entry");

    let report = boundary.drain(30, unlimited_budget());
    assert_eq!(report.dropped_obsolete, 0);
    assert_eq!(report.merged, 1);
    assert_eq!(boundary.queue.diagnostics().dropped_obsolete(), 1);
    assert_eq!(boundary.visible(), ["s1"]);
    assert_eq!(boundary.aggregator.plugin_state(&apps), Some(BatchState::Partial));
}

// ---------------------------------------------------------------------------
// Stale generations: rejected before anything is mutated
// (spec 8.1, 9.5; acceptance 31.7).
// ---------------------------------------------------------------------------

#[test]
fn a_late_batch_for_a_retired_generation_cannot_mutate_visible_state() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary
        .queue
        .register(apps.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));

    let first = boundary.begin_generation();
    boundary
        .queue
        .submit(10, high(partial(first, &apps, &["a1"])))
        .expect("admitted");
    boundary.drain(20, unlimited_budget());

    let second = boundary.begin_generation();
    boundary
        .queue
        .submit(30, high(partial(second, &apps, &["b1"])))
        .expect("admitted");
    boundary.drain(40, unlimited_budget());
    let repaint = boundary
        .aggregator
        .take_ui_update()
        .expect("merging the new generation owes the UI a repaint");
    assert_eq!(ids(&repaint), ["b1"]);

    let refusal = boundary
        .queue
        .submit(900, high(partial(first, &apps, &["a2"])))
        .expect_err("the generation retired 780 virtual milliseconds ago");

    assert_eq!(refusal, QueueReject::StaleGeneration);
    assert_eq!(boundary.queue.depth(), depth(0, 0, 0));
    assert_eq!(boundary.visible(), ["b1"]);
    // Nothing changed, so no repaint is owed. A stale batch that merely
    // scheduled a redundant repaint would already have touched visible state.
    assert!(
        boundary.aggregator.take_ui_update().is_none(),
        "a refused batch must not schedule a repaint"
    );

    let events = boundary.queue.take_events();
    let late = events
        .iter()
        .find(|event| event.kind == QueueEventKind::Rejected(QueueReject::StaleGeneration))
        .expect("the rejection is recorded for the query trace (spec 26.4)");
    assert_eq!(late.at_ms, 900);
    assert_eq!(late.plugin, apps);
    assert_eq!(late.generation, first);
    assert_eq!(
        boundary
            .queue
            .diagnostics()
            .rejected(QueueReject::StaleGeneration),
        1
    );
}

#[test]
fn queued_entries_from_the_previous_generation_never_reorder_into_the_new_list() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary
        .queue
        .register(apps.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));
    let first = boundary.begin_generation();

    for (at, id) in [(10, "old1"), (11, "old2")] {
        boundary
            .queue
            .submit(at, high(partial(first, &apps, &[id])))
            .expect("admitted while current");
    }

    let second = boundary.begin_generation();
    for (at, id) in [(20, "new1"), (21, "new2")] {
        boundary
            .queue
            .submit(at, high(partial(second, &apps, &[id])))
            .expect("admitted");
    }

    let report = boundary.drain(30, unlimited_budget());

    // The retired entries sit ahead of the current ones in arrival order; a
    // queue that merged in arrival order alone would lead the list with them.
    assert_eq!(report.dropped_obsolete, 2);
    assert_eq!(report.merged, 2);
    assert_eq!(boundary.visible(), ["new1", "new2"]);
}

#[test]
fn a_generation_the_aggregator_never_began_is_refused_at_merge_without_mutation() {
    // Defence in depth: the boundary and the aggregator gate generations
    // independently, and the second gate must be as non-destructive as the
    // first even when the two disagree.
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary
        .queue
        .register(apps.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));

    let first = boundary.begin_generation();
    boundary
        .queue
        .submit(10, high(partial(first, &apps, &["a1"])))
        .expect("admitted");
    boundary.drain(20, unlimited_budget());
    let _ = boundary.aggregator.take_ui_update();

    let second = boundary.generations.advance();
    boundary.queue.begin_generation(second);

    boundary
        .queue
        .submit(30, high(partial(second, &apps, &["b1"])))
        .expect("the boundary considers this current");

    let report = boundary.drain(40, unlimited_budget());

    assert_eq!(report.merged, 0);
    assert_eq!(
        report.merge_rejected,
        [(apps.clone(), RejectReason::StaleGeneration)]
    );
    assert_eq!(boundary.visible(), ["a1"]);
    assert!(
        boundary.aggregator.take_ui_update().is_none(),
        "a batch refused at merge must not schedule a repaint"
    );

    let diagnostics = boundary.queue.diagnostics();
    assert_eq!(diagnostics.merge_rejected(RejectReason::StaleGeneration), 1);
    assert_eq!(diagnostics.rejected(QueueReject::StaleGeneration), 0);
}

#[test]
fn a_non_cooperative_slow_plugin_neither_delays_nor_pollutes_the_fast_one() {
    // Acceptance 31.7 and 31.8 in one scenario: the slow plugin ignores
    // cancellation entirely and answers the first query long after a second one
    // replaced it (spec 9.5).
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let fast = plugin("dev.crikey.calc");
    let slow = plugin("dev.crikey.web");
    for owner in [&fast, &slow] {
        boundary
            .queue
            .register(owner.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));
    }

    let first = boundary.begin_generation();
    boundary
        .queue
        .submit(30, high(partial(first, &fast, &["fast-1"])))
        .expect("the fast plugin answers at 30ms");
    let first_frame = boundary.drain(40, unlimited_budget());

    // The frame completes at 40ms with the slow plugin still working: a drain
    // never waits on a plugin that has produced nothing.
    assert_eq!(first_frame.merged, 1);
    assert_eq!(boundary.queue.depth(), depth(0, 0, 0));
    assert_eq!(boundary.visible(), ["fast-1"]);

    let second = boundary.begin_generation();
    boundary
        .queue
        .submit(130, high(partial(second, &fast, &["fast-2"])))
        .expect("the fast plugin answers the new query too");
    boundary.drain(140, unlimited_budget());
    let repaint = boundary
        .aggregator
        .take_ui_update()
        .expect("the second generation owes the UI a repaint");
    assert_eq!(ids(&repaint), ["fast-2"]);

    let late = boundary
        .queue
        .submit(900, high(batch(first, &slow, BatchState::Final, &["slow-1"])))
        .expect_err("the slow plugin's answer belongs to a query the user abandoned");

    assert_eq!(late, QueueReject::StaleGeneration);
    assert_eq!(boundary.queue.plugin_depth(&slow), depth(0, 0, 0));
    assert_eq!(boundary.visible(), ["fast-2"]);
    assert!(
        boundary.aggregator.take_ui_update().is_none(),
        "the late answer left visible state exactly as it was"
    );
    assert_eq!(boundary.aggregator.plugin_state(&slow), None);
}

// ---------------------------------------------------------------------------
// Transport bounds are not retained-item limits (spec 11.7 vs 12.4).
// ---------------------------------------------------------------------------

#[test]
fn the_transport_budget_is_independent_of_the_retained_item_limit() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary.queue.register(
        apps.clone(),
        IntakePolicy {
            capacity_batches: 2,
            pause_at_batches: 2,
            resume_at_batches: 1,
            ..unbounded_intake(OverflowPolicy::PauseProducer)
        },
    );
    let generation = boundary.begin_generation();

    for (at, id) in [(10, "a1"), (11, "a2")] {
        boundary
            .queue
            .submit(at, high(partial(generation, &apps, &[id])))
            .expect("admitted");
    }
    let refusal = boundary
        .queue
        .submit(12, high(partial(generation, &apps, &["a3"])))
        .expect_err("the transport is saturated");

    // Retained quota is 512 and nothing has been retained yet: this refusal is
    // purely about memory in flight, and must never be reported as a quota
    // breach or shrink what the query is finally allowed to show.
    assert_eq!(refusal, QueueReject::QueueFull);
    assert_eq!(
        boundary
            .queue
            .diagnostics()
            .merge_rejected(RejectReason::QuotaExceeded),
        0
    );

    boundary.drain(20, unlimited_budget());
    boundary
        .queue
        .submit(21, high(partial(generation, &apps, &["a3"])))
        .expect("the same batch fits once the transport drains");
    boundary.drain(22, unlimited_budget());
    assert_eq!(boundary.visible(), ["a1", "a2", "a3"]);
}

#[test]
fn a_retained_quota_breach_is_reported_at_merge_and_not_at_admission() {
    let mut boundary = Boundary::new(generous_queue_limits(), retained_quota_of(2));
    let apps = plugin("dev.crikey.apps");
    boundary
        .queue
        .register(apps.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));
    let generation = boundary.begin_generation();

    boundary
        .queue
        .submit(10, high(partial(generation, &apps, &["q1", "q2"])))
        .expect("the transport has no opinion about retained quota");
    boundary
        .queue
        .submit(11, high(partial(generation, &apps, &["q3"])))
        .expect("still no opinion");

    let report = boundary.drain(20, unlimited_budget());

    assert_eq!(report.merged, 1);
    assert_eq!(
        report.merge_rejected,
        [(apps.clone(), RejectReason::QuotaExceeded)]
    );
    assert_eq!(boundary.visible(), ["q1", "q2"]);
    // The rejected batch left the queue: a quota breach is not backpressure and
    // must not wedge the transport.
    assert_eq!(boundary.queue.depth(), depth(0, 0, 0));

    let diagnostics = boundary.queue.diagnostics();
    assert_eq!(diagnostics.merge_rejected(RejectReason::QuotaExceeded), 1);
    assert_eq!(diagnostics.rejected(QueueReject::QueueFull), 0);
    assert_eq!(diagnostics.rejected(QueueReject::LowPriorityShed), 0);
}

#[test]
fn queue_depth_measures_batches_in_flight_not_the_size_of_the_retained_list() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary
        .queue
        .register(apps.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));
    let generation = boundary.begin_generation();

    // Three enrichment passes over the same identity (spec 12.6).
    for at in [10, 11, 12] {
        boundary
            .queue
            .submit(at, high(partial(generation, &apps, &["dup"])))
            .expect("admitted");
    }
    assert_eq!(boundary.queue.plugin_depth(&apps), depth(3, 3, 0));

    let report = boundary.drain(20, unlimited_budget());

    assert_eq!(report.merged, 3);
    assert_eq!(boundary.visible(), ["dup"]);
    assert_eq!(boundary.queue.diagnostics().peak_batches(), 3);
    assert_eq!(boundary.queue.diagnostics().peak_items(), 3);
}

// ---------------------------------------------------------------------------
// Diagnosability (spec 24.3, 26.4).
// ---------------------------------------------------------------------------

#[test]
fn every_boundary_decision_is_recorded_exactly_once() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary.queue.register(
        apps.clone(),
        IntakePolicy {
            capacity_batches: 2,
            pause_at_batches: 2,
            resume_at_batches: 1,
            ..unbounded_intake(OverflowPolicy::RejectLowPriority)
        },
    );

    let first = boundary.begin_generation();
    boundary
        .queue
        .submit(10, high(partial(first, &apps, &["a1"])))
        .expect("admitted");
    boundary
        .queue
        .submit(11, low(partial(first, &apps, &["a2"])))
        .expect("admitted just below the watermark");
    boundary
        .queue
        .submit(12, low(partial(first, &apps, &["a3"])))
        .expect_err("shed at the watermark");
    boundary
        .queue
        .submit(13, high(partial(first, &apps, &["a4"])))
        .expect_err("the hard bound binds even for high priority");
    boundary.drain(20, budget(1, 64, 1));

    let second = boundary.begin_generation();
    boundary
        .queue
        .submit(30, high(partial(second, &apps, &["b1"])))
        .expect("the obsolete entry still occupies the bound but one slot is free");
    boundary.drain(40, unlimited_budget());

    let events = boundary.queue.take_events();
    assert_eq!(
        kinds(&events),
        [
            QueueEventKind::Admitted { items: 1 },
            QueueEventKind::Admitted { items: 1 },
            QueueEventKind::ProducerPaused,
            QueueEventKind::Rejected(QueueReject::LowPriorityShed),
            QueueEventKind::Rejected(QueueReject::QueueFull),
            QueueEventKind::Merged { items: 1 },
            QueueEventKind::ProducerResumed,
            QueueEventKind::Admitted { items: 1 },
            QueueEventKind::ProducerPaused,
            QueueEventKind::DroppedObsolete { batches: 1, items: 1 },
            QueueEventKind::Merged { items: 1 },
            QueueEventKind::ProducerResumed,
        ]
    );

    // Every event carries the plugin, the generation it concerns and the
    // virtual timestamp the query trace needs (spec 26.4).
    assert!(events.iter().all(|event| event.plugin == apps));
    assert_eq!(
        events.iter().map(|event| event.at_ms).collect::<Vec<_>>(),
        [10, 11, 11, 12, 13, 20, 20, 30, 30, 40, 40, 40]
    );
    assert_eq!(events.iter().filter(|event| event.generation == first).count(), 8);

    // Counters and the event log are two views of the same decisions.
    let diagnostics = boundary.queue.diagnostics();
    assert_eq!(diagnostics.admitted(), 3);
    assert_eq!(diagnostics.merged(), 2);
    assert_eq!(diagnostics.dropped_obsolete(), 1);
    assert_eq!(diagnostics.evicted_oldest(), 0);
    assert_eq!(diagnostics.rejected(QueueReject::LowPriorityShed), 1);
    assert_eq!(diagnostics.rejected(QueueReject::QueueFull), 1);
    assert_eq!(diagnostics.pauses(), 2);
    assert_eq!(diagnostics.resumes(), 2);
    assert_eq!(diagnostics.peak_batches(), 2);
    assert_eq!(diagnostics.peak_items(), 2);

    // The whole scenario, four rejections and a generation switch included,
    // never let more than the configured two batches sit in memory.
    assert_eq!(boundary.queue.depth(), depth(0, 0, 0));
}

#[test]
fn diagnostic_events_form_a_bounded_ring_and_count_every_eviction() {
    let mut boundary = Boundary::new(
        QueueLimits {
            capacity_batches: 2,
            capacity_items: 64,
        },
        generous_result_limits(),
    );
    let apps = plugin("dev.crikey.apps");
    boundary
        .queue
        .register(apps.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));
    let generation = boundary.begin_generation();

    for (at, id) in [(10, "a1"), (20, "a2"), (30, "a3")] {
        boundary
            .queue
            .submit(at, high(partial(generation, &apps, &[id])))
            .expect("one resident batch fits");
        let report = boundary.drain(at + 1, unlimited_budget());
        assert_eq!(report.merged, 1);
    }

    assert_eq!(boundary.queue.diagnostics().events_dropped(), 4);
    let events = boundary.queue.take_events();
    assert_eq!(events.len(), 2);
    assert_eq!(
        kinds(&events),
        [
            QueueEventKind::Admitted { items: 1 },
            QueueEventKind::Merged { items: 1 },
        ]
    );
    assert_eq!(
        events.iter().map(|event| event.at_ms).collect::<Vec<_>>(),
        [30, 31]
    );
    assert_eq!(
        boundary.queue.diagnostics().events_dropped(),
        4,
        "reading the ring does not erase its cumulative loss accounting"
    );
}

#[test]
fn rejected_terminal_merge_reopens_the_intake_stream() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    let impostor = plugin("dev.crikey.impostor");
    boundary
        .queue
        .register(apps.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));
    let generation = boundary.begin_generation();

    boundary
        .queue
        .submit(
            10,
            high(ResultBatch {
                generation,
                plugin: apps.clone(),
                state: BatchState::Final,
                items: vec![item(&impostor, "wrong-owner")],
            }),
        )
        .expect("transport admission does not enforce retained ownership");

    let rejected = boundary.drain(20, unlimited_budget());
    assert_eq!(rejected.merged, 0);
    assert!(rejected.merged_batches.is_empty());
    assert_eq!(
        rejected.merge_rejected,
        [(apps.clone(), RejectReason::OwnerMismatch)]
    );
    assert_eq!(
        boundary
            .queue
            .diagnostics()
            .rejected(QueueReject::StreamTerminated),
        0
    );
    assert_eq!(
        boundary
            .queue
            .diagnostics()
            .merge_rejected(RejectReason::OwnerMismatch),
        1
    );

    boundary
        .queue
        .submit(
            21,
            high(batch(generation, &apps, BatchState::Final, &["correct-owner"])),
        )
        .expect("a rejected pending terminal must not poison the stream");
    let accepted = boundary.drain(22, unlimited_budget());
    assert_eq!(accepted.merged, 1);
    assert_eq!(accepted.merged_batches[0].state, BatchState::Final);
    assert_eq!(boundary.visible(), ["correct-owner"]);
    assert_eq!(boundary.aggregator.plugin_state(&apps), Some(BatchState::Final));

    assert_eq!(
        boundary
            .queue
            .submit(23, high(partial(generation, &apps, &["too-late"])))
            .expect_err("the successfully merged terminal remains committed"),
        QueueReject::StreamTerminated
    );
}

#[test]
fn reregistration_preserves_a_queued_terminal_marker() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    let policy = IntakePolicy {
        capacity_batches: 4,
        pause_at_batches: 3,
        resume_at_batches: 1,
        ..unbounded_intake(OverflowPolicy::PauseProducer)
    };
    boundary.queue.register(apps.clone(), policy);
    let generation = boundary.begin_generation();
    boundary
        .queue
        .submit(
            10,
            high(batch(generation, &apps, BatchState::Final, &["terminal"])),
        )
        .expect("terminal queued");

    boundary.queue.register(apps.clone(), policy);
    assert_eq!(
        boundary
            .queue
            .submit(11, high(partial(generation, &apps, &["behind-terminal"])))
            .expect_err("reconfiguration cannot reopen a pending terminal"),
        QueueReject::StreamTerminated
    );
    assert_eq!(boundary.queue.plugin_depth(&apps), depth(1, 1, 0));

    let report = boundary.drain(20, unlimited_budget());
    assert_eq!(report.merged, 1);
    assert_eq!(boundary.aggregator.plugin_state(&apps), Some(BatchState::Final));
}

#[test]
fn reregistration_preserves_hysteresis_and_reconciles_a_new_policy() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    let initial = IntakePolicy {
        capacity_batches: 4,
        pause_at_batches: 2,
        resume_at_batches: 0,
        ..unbounded_intake(OverflowPolicy::PauseProducer)
    };
    boundary.queue.register(apps.clone(), initial);
    let generation = boundary.begin_generation();

    for (at, id) in [(10, "a1"), (11, "a2")] {
        boundary
            .queue
            .submit(at, high(partial(generation, &apps, &[id])))
            .expect("inside the hard bound");
    }
    assert_eq!(boundary.producer(&apps), ProducerState::Paused);
    boundary.drain(20, budget(1, 64, 1));
    assert_eq!(boundary.queue.plugin_depth(&apps), depth(1, 1, 0));
    assert_eq!(boundary.producer(&apps), ProducerState::Paused);

    boundary.queue.register(
        apps.clone(),
        IntakePolicy {
            pause_at_batches: 3,
            ..initial
        },
    );
    assert_eq!(
        boundary.producer(&apps),
        ProducerState::Paused,
        "depth inside the hysteresis band preserves the pause"
    );

    boundary.queue.register(
        apps.clone(),
        IntakePolicy {
            pause_at_batches: 3,
            resume_at_batches: 2,
            ..initial
        },
    );
    assert_eq!(
        boundary.producer(&apps),
        ProducerState::Running,
        "a replacement policy is reconciled against retained depth"
    );
}

#[test]
fn impossible_watermarks_are_normalized_without_pause_oscillation() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary.queue.register(
        apps.clone(),
        IntakePolicy {
            capacity_batches: 0,
            capacity_items: 8,
            pause_at_batches: 0,
            resume_at_batches: usize::MAX,
            overflow: OverflowPolicy::PauseProducer,
        },
    );
    let generation = boundary.begin_generation();

    assert_eq!(
        boundary
            .queue
            .submit(10, high(partial(generation, &apps, &["a1"])))
            .expect("normalization provides one coherent capacity slot"),
        ProducerState::Paused
    );
    let _ = boundary.queue.take_events();

    for at in [11, 12, 13] {
        let report = boundary.drain(at, budget(0, 0, 0));
        assert_eq!(report, DrainReport::default());
        assert_eq!(boundary.producer(&apps), ProducerState::Paused);
    }
    assert!(
        boundary.queue.take_events().is_empty(),
        "unchanged depth cannot manufacture pause/resume transitions"
    );

    let report = boundary.drain(20, budget(1, 8, 1));
    assert_eq!(report.merged, 1);
    assert_eq!(boundary.producer(&apps), ProducerState::Running);
    assert_eq!(boundary.queue.diagnostics().pauses(), 1);
    assert_eq!(boundary.queue.diagnostics().resumes(), 1);
}

#[test]
fn successful_drain_metadata_excludes_batches_rejected_at_merge() {
    let mut boundary = Boundary::new(generous_queue_limits(), retained_quota_of(1));
    let apps = plugin("dev.crikey.apps");
    boundary
        .queue
        .register(apps.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));
    let generation = boundary.begin_generation();
    boundary
        .queue
        .submit(10, high(partial(generation, &apps, &["kept"])))
        .expect("transport admits the first batch");
    boundary
        .queue
        .submit(11, high(partial(generation, &apps, &["over-quota"])))
        .expect("transport and retained quotas are distinct");

    let report = boundary.drain(20, unlimited_budget());
    assert_eq!(report.merged, 1);
    assert_eq!(
        report.merged_batches,
        [MergedBatch {
            plugin: apps.clone(),
            admitted_at_ms: 10,
            generation,
            state: BatchState::Partial,
            items: 1,
        }]
    );
    assert_eq!(report.merge_rejected, [(apps, RejectReason::QuotaExceeded)]);
}

#[test]
fn delayed_drain_reports_the_original_admission_timestamp() {
    let mut boundary = Boundary::new(generous_queue_limits(), generous_result_limits());
    let apps = plugin("dev.crikey.apps");
    boundary
        .queue
        .register(apps.clone(), unbounded_intake(OverflowPolicy::RejectLowPriority));
    let generation = boundary.begin_generation();

    boundary
        .queue
        .submit(17, high(partial(generation, &apps, &["kept"])))
        .expect("the batch is admitted at its submit timestamp");

    let report = boundary.drain(10_000, unlimited_budget());
    assert_eq!(
        report.merged_batches,
        [MergedBatch {
            plugin: apps,
            admitted_at_ms: 17,
            generation,
            state: BatchState::Partial,
            items: 1,
        }]
    );
    assert_eq!(
        boundary
            .queue
            .take_events()
            .iter()
            .map(|event| event.at_ms)
            .collect::<Vec<_>>(),
        [17, 10_000],
        "admission and merge decisions retain their own event timestamps"
    );
}

#[test]
fn a_zero_capacity_event_ring_counts_decisions_without_retaining_them() {
    let mut queue = InboundResultQueue::new(QueueLimits {
        capacity_batches: 0,
        capacity_items: 0,
    });
    let ghost = plugin("dev.crikey.ghost");
    let generations = GenerationTracker::new();
    let generation = generations.advance();
    queue.begin_generation(generation);

    for at in [10, 11, 12] {
        assert_eq!(
            queue
                .submit(at, high(partial(generation, &ghost, &["discarded"])))
                .expect_err("unregistered traffic is rejected"),
            QueueReject::Unregistered
        );
    }

    assert!(queue.take_events().is_empty());
    assert_eq!(queue.diagnostics().events_dropped(), 3);
    assert_eq!(queue.diagnostics().rejected(QueueReject::Unregistered), 3);
}
