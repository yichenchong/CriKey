//! Contract for the composed M2 scheduling pipeline (spec 7, 8, 9, 11.4 - 11.7,
//! 12, 13.3 - 13.6, 24.3 - 24.4, 25.2 - 25.5, 26.4; roadmap M2, §31.4 - 8,
//! §31.24 - 25).
//!
//! Every piece M2 needs already has, or is getting, its own unit suite: the
//! debouncer, the obsolete-work manager, the bounded request queues, the
//! aggregator's safety limits. None of them can fail the way the *launcher*
//! fails. A debouncer that defers correctly still loses if the host dispatches
//! the query it deferred; an aggregator that rejects a stale batch still loses
//! if the row it already published came from one. What is defended here is
//! therefore the composition and nothing else:
//!
//! ```text
//! keystroke -> generation -> policy -> dispatch -> worker -> batch
//!           -> aggregation -> ranking -> presentation
//! ```
//!
//! driven end to end through [`QueryPipeline`], with scripted workers standing
//! in for real ones. Anything provable against a bare `Debouncer` belongs in
//! `crikey-input-scheduler`, not here.
//!
//! # Determinism
//!
//! Time is a `u64` of virtual milliseconds that this file advances by hand.
//! Nothing sleeps, spawns a thread, reads a clock or opens a handle. The driver
//! only ever moves the clock to a timestamp some component *asked* for -
//! [`QueryPipeline::next_wakeup`] or a scripted worker's next due batch - so a
//! scheduling bug surfaces as a wrong timestamp instead of as a flake.
//!
//! Scripted items carry their generation inside their [`ItemId`]
//! (`fast-native#g7#2`), which is what makes "no stale row was displayed"
//! checkable against the rows themselves rather than against a bookkeeping
//! counter that a bug could keep consistent with itself.
//!
//! # Pinned scheduler API
//!
//! The scheduler surface is the merged pin owned by `ModernSchedulerTests` and
//! `ResilienceSchedulerTests` (`local://scheduler-api.md`), used verbatim, plus
//! two additions agreed with both that nothing else can supply:
//!
//! ```ignore
//! /// One in-flight request the scheduler invalidated, whatever the cause.
//! /// Drained by the host so the worker can actually be signalled (spec 8.7).
//! #[derive(Debug, Clone, PartialEq, Eq)]
//! pub struct CancelledRequest {
//!     pub plugin: PluginId,
//!     pub generation: Generation,
//!     pub reason: CancelReason,
//!     pub cancelled_at: Millis,
//! }
//!
//! impl QueryScheduler {
//!     pub fn drain_cancellations(&mut self) -> Vec<CancelledRequest>;
//!     pub fn record_ranking(&mut self, generation: Generation, ranked_items: usize, now: Millis);
//! }
//!
//! pub enum QueryTraceEvent {
//!     // ... every merged variant, plus:
//!     Ranking { at: Millis, generation: Generation, ranked_items: usize },
//! }
//! ```
//!
//! `drain_cancellations` exists because `cancel_plugin` invalidates queued work
//! as well: a host calling it on every keystroke would turn supersession into
//! cancellation, and the newest-wins accounting of spec 8.8 would stop being
//! observable. `Ranking` exists because `Presentation` alone cannot tell a
//! reorder from a redraw, and spec 26.4 asks for both.
//!
//! # Pinned composition root
//!
//! ```ignore
//! /// Bounds and fairness policy for one composed pipeline.
//! pub struct PipelineConfig {
//!     pub scheduler: SchedulerConfig,
//!     pub limits: ResultLimits,
//!     pub intake_limits: QueueLimits,
//!     pub default_intake_policy: IntakePolicy,
//!     pub drain_budget: DrainBudget,
//! }
//!
//! pub enum PipelineError {
//!     AlreadyRegistered { plugin: PluginId },
//!     QueueRejected {
//!         plugin: PluginId,
//!         generation: Generation,
//!         reason: QueueReject,
//!     },
//!     AggregatorRejected {
//!         plugin: PluginId,
//!         generation: Generation,
//!         reason: RejectReason,
//!     },
//! }
//!
//! /// Everything one `tick` owes workers and intake observers.
//! pub struct PipelineTick {
//!     pub dispatches: Vec<DispatchedRequest>,
//!     pub cancellations: Vec<CancelledRequest>,
//!     pub drain_report: DrainReport,
//!     pub errors: Vec<PipelineError>,
//! }
//!
//! impl QueryPipeline {
//!     pub fn new(config: PipelineConfig) -> Self;
//!     pub fn register_plugin(&mut self, plugin: PluginId, policy: PluginPolicy)
//!         -> Result<(), PipelineError>;
//!     pub fn register_plugin_with_intake(
//!         &mut self,
//!         plugin: PluginId,
//!         policy: PluginPolicy,
//!         intake_policy: IntakePolicy,
//!     ) -> Result<(), PipelineError>;
//!     pub fn register_manifest(&mut self, manifest: &Manifest)
//!         -> Result<PluginId, PipelineError>;
//!
//!     /// One keystroke mints and visibly opens the next empty generation.
//!     /// It never dispatches, but captures cancellations synchronously.
//!     pub fn keystroke(&mut self, text: &str, now: Millis) -> Generation;
//!     /// Advances virtual time and fairly drains bounded result intake.
//!     pub fn tick(&mut self, now: Millis) -> PipelineTick;
//!     pub fn next_wakeup(&self) -> Option<Millis>;
//!
//!     /// A worker publication is admitted atomically to intake. Successful
//!     /// result traces are committed only after the aggregator merges it.
//!     pub fn deliver(&mut self, batch: ResultBatch, now: Millis)
//!         -> Result<(), PipelineError>;
//!     pub fn deliver_with_priority(
//!         &mut self,
//!         batch: ResultBatch,
//!         priority: BatchPriority,
//!         now: Millis,
//!     ) -> Result<(), PipelineError>;
//!     pub fn complete(&mut self, plugin: &PluginId, generation: Generation, now: Millis)
//!         -> CompletionOutcome;
//!
//!     /// Fair-drains, ranks and publishes at most one coalesced frame. Every
//!     /// generation first publishes an empty frame, so old rows never persist.
//!     pub fn present(&mut self, now: Millis) -> Option<ViewModel>;
//!
//!     pub fn rows(&self) -> &[ResultRow];
//!     pub fn visible_generation(&self) -> Option<Generation>;
//!     pub fn diagnostics(&self) -> SchedulerDiagnostics;
//!     pub fn plugin_diagnostics(&self, plugin: &PluginId) -> Option<PluginDiagnostics>;
//!     pub fn intake_depth(&self) -> QueueDepth;
//!     pub fn intake_diagnostics(&self) -> &QueueDiagnostics;
//!     pub fn take_intake_events(&mut self) -> Vec<QueueEvent>;
//!     pub fn health(&self, plugin: &PluginId) -> PluginHealth;
//!     pub fn trace(&self) -> &[QueryTraceEvent];
//! }
//! ```
//!
//! Ranking at this boundary does **not** re-match plugin answers against the
//! query: a plugin that answered `2+2` with `4` has already decided relevance,
//! and re-matching would delete it. The pipeline orders the aggregated set by
//! [`Item::score_hint`], strongest first, *stably*, so equal hints keep
//! first-acceptance order and rows do not move under the selection
//! (spec 11.5, 11.6).

use std::collections::BTreeMap;

use crikey_app::{
    BatchPriority, BatchState, DrainBudget, IntakePolicy, OverflowPolicy, PipelineConfig, PipelineError,
    PipelineTick, QueryPipeline, QueueEventKind, QueueLimits, QueueReject, RejectReason, ResultBatch,
    ResultLimits,
};
use crikey_core::{ArgumentPolicy, Category, Generation, HitPolicy, Item, ItemId, PluginId};
use crikey_input_scheduler::{
    BatchCompletion, CancelReason, CompletionOutcome, DebounceDecision, DebouncePolicy, LegacyDispatch,
    Millis, PluginPolicy, QueryTraceEvent, SchedulerConfig, SchedulingProfile,
};
use crikey_plugin_model::Manifest;
use crikey_ui::{ResultRow, ViewModel};

// ---------------------------------------------------------------------------
// Manifests (spec 19.4)
// ---------------------------------------------------------------------------

/// Native plugin: fast, leading and trailing, inside the 30 - 50 ms band
/// recommended for a local native plugin (spec 25.4).
const FAST_NATIVE_MANIFEST: &str = r#"
manifest-version = 1

[plugin]
id = "dev.crikey.fast-native"
name = "Fast Native"
version = "1.0.0"
runtime = "native"
entrypoint = "fast"

[query]
debounce-ms = 30
leading-edge = true
trailing-edge = true
max-concurrent-requests = 1
"#;

/// Modern Python plugin: trailing only, with a maximum wait, and slow enough
/// that it would visibly delay a fast neighbour if the pipeline let it
/// (spec 25.2, 25.4, 31.8).
const SLOW_PYTHON_MANIFEST: &str = r#"
manifest-version = 1

[plugin]
id = "dev.crikey.slow-python"
name = "Slow Python"
version = "1.0.0"
runtime = "python"
entrypoint = "slow"

[query]
debounce-ms = 60
maximum-wait-ms = 120
leading-edge = false
trailing-edge = true
max-concurrent-requests = 1
"#;

/// `legacy-strict`. Declares a long debounce and a host gate on purpose: both
/// must be neutralized, because a legacy plugin is never time debounced and
/// never host gated (spec 8.4, 25.4, 31.14).
const LEGACY_STRICT_MANIFEST: &str = r#"
manifest-version = 1

[plugin]
id = "dev.crikey.legacy-strict"
name = "Legacy Strict"
version = "1.0.0"
runtime = "legacy-python"
entrypoint = "legacy"

[query]
debounce-ms = 250
leading-edge = false
trailing-edge = true

[activation]
minimum-query-length = 4
"#;

/// Leading edge only: answers the query that made it relevant, and nothing
/// typed after it (spec 8.5).
const LEADING_ONLY_MANIFEST: &str = r#"
manifest-version = 1

[plugin]
id = "dev.crikey.leading-only"
name = "Leading Only"
version = "1.0.0"
runtime = "native"
entrypoint = "leading"

[query]
debounce-ms = 40
leading-edge = true
trailing-edge = false
"#;

/// Trailing edge only: answers once typing pauses (spec 8.5).
const TRAILING_ONLY_MANIFEST: &str = r#"
manifest-version = 1

[plugin]
id = "dev.crikey.trailing-only"
name = "Trailing Only"
version = "1.0.0"
runtime = "native"
entrypoint = "trailing"

[query]
debounce-ms = 40
leading-edge = false
trailing-edge = true
"#;

/// A trailing edge that could never fire under continuous typing without the
/// maximum wait of spec 8.6.
const MAX_WAIT_MANIFEST: &str = r#"
manifest-version = 1

[plugin]
id = "dev.crikey.max-wait"
name = "Max Wait"
version = "1.0.0"
runtime = "native"
entrypoint = "maxwait"

[query]
debounce-ms = 40
maximum-wait-ms = 100
leading-edge = false
trailing-edge = true
"#;

/// The debounce interval `FAST_NATIVE_MANIFEST` declares.
const FAST_DEBOUNCE_MS: Millis = 30;
/// Dispatch to partial batch, and to final batch, for the fast worker.
const FAST_FIRST_MS: Millis = 4;
const FAST_FINAL_MS: Millis = 6;
/// The debounce interval `SLOW_PYTHON_MANIFEST` declares.
const SLOW_DEBOUNCE_MS: Millis = 60;
/// Dispatch to partial and final batch for the slow worker: far beyond its soft
/// deadline, so "a slow plugin never delays a fast one" is not vacuous (§31.8).
const SLOW_FIRST_MS: Millis = 150;
const SLOW_FINAL_MS: Millis = 320;
/// The legacy worker's callback duration. Longer than the typing interval, so
/// successive keystrokes really do arrive while it is busy (spec 13.4).
const LEGACY_CALLBACK_MS: Millis = 90;
/// The debounce interval both single-edge manifests declare.
const EDGE_DEBOUNCE_MS: Millis = 40;
/// The maximum wait `MAX_WAIT_MANIFEST` declares.
const MAX_WAIT_CEILING_MS: Millis = 100;

/// Virtual timestamp every session starts at. Non-zero, so an implementation
/// that confuses "no deadline" with "deadline at zero" cannot pass by accident.
const START: Millis = 1_000;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Deliberately tight queues: a pipeline that leaks pending work hits the bound
/// inside one burst of typing instead of after a million keystrokes.
fn config() -> PipelineConfig {
    PipelineConfig {
        scheduler: SchedulerConfig {
            request_queue_capacity: 8,
            result_queue_capacity: 64,
            per_plugin_dispatch_budget: 1,
            dispatch_budget_per_tick: 16,
            trace_capacity: 8_192,
        },
        limits: ResultLimits {
            max_items_per_batch: 8,
            ..ResultLimits::default()
        },
        intake_limits: QueueLimits {
            capacity_batches: 64,
            capacity_items: 512,
        },
        default_intake_policy: IntakePolicy {
            capacity_batches: 16,
            capacity_items: 128,
            pause_at_batches: 16,
            resume_at_batches: 8,
            overflow: OverflowPolicy::PauseProducer,
        },
        drain_budget: DrainBudget {
            batches_per_plugin: 2,
            items_per_plugin: 16,
            total_batches: 16,
        },
    }
}

fn immediate_policy() -> PluginPolicy {
    PluginPolicy {
        profile: SchedulingProfile::Modern,
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

// ---------------------------------------------------------------------------
// Scripted workers
// ---------------------------------------------------------------------------

/// A worker whose entire behaviour is a function of the virtual timestamp it
/// was dispatched at.
#[derive(Debug, Clone, Copy)]
struct PluginScript {
    /// Dispatch to partial batch. `None`, or a value not strictly below
    /// `final_batch_after`, means the worker answers in a single batch.
    first_batch_after: Option<Millis>,
    /// Dispatch to terminal batch.
    final_batch_after: Millis,
    /// Items in every batch this worker sends.
    items_per_batch: usize,
    /// Score hint of this worker's strongest item; later items step down by
    /// one. Ranking order across workers is therefore decided by the fixture
    /// rather than by arrival order.
    top_score_hint: i32,
    /// Keeps answering for a generation the host already cancelled (§31.6).
    ignores_cancellation: bool,
}

impl PluginScript {
    /// A worker that answers in one batch and honours cancellation.
    fn prompt(final_batch_after: Millis, items_per_batch: usize, top_score_hint: i32) -> Self {
        Self {
            first_batch_after: None,
            final_batch_after,
            items_per_batch,
            top_score_hint,
            ignores_cancellation: false,
        }
    }
}

#[derive(Debug)]
struct Run {
    generation: Generation,
    dispatched_at: Millis,
    cancelled: bool,
    partial_sent: bool,
    finished: bool,
}

/// What a run owes next, and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Partial(Millis),
    Final(Millis),
}

impl Step {
    fn at(self) -> Millis {
        match self {
            Self::Partial(at) | Self::Final(at) => at,
        }
    }
}

fn next_step(run: &Run, script: &PluginScript) -> Step {
    let final_at = run.dispatched_at + script.final_batch_after;
    match script.first_batch_after {
        Some(delay) if !run.partial_sent && run.dispatched_at + delay < final_at => {
            Step::Partial(run.dispatched_at + delay)
        }
        _ => Step::Final(final_at),
    }
}

#[derive(Debug)]
struct Worker {
    plugin: PluginId,
    script: PluginScript,
    runs: Vec<Run>,
}

/// One batch a scripted worker put on the wire.
#[derive(Debug)]
struct Emission {
    batch: ResultBatch,
    /// The worker's callback returns after this batch.
    terminal: bool,
}

/// Every scripted worker attached to one pipeline.
#[derive(Debug, Default)]
struct Fleet {
    workers: Vec<Worker>,
}

impl Fleet {
    fn attach(&mut self, plugin: &PluginId, script: PluginScript) {
        self.workers.push(Worker {
            plugin: plugin.clone(),
            script,
            runs: Vec::new(),
        });
    }

    fn worker_mut(&mut self, plugin: &PluginId) -> &mut Worker {
        self.workers
            .iter_mut()
            .find(|worker| &worker.plugin == plugin)
            .unwrap_or_else(|| panic!("pipeline addressed unregistered worker {}", plugin.0))
    }

    /// Starts the runs the pipeline dispatched and flags the ones it cancelled.
    /// Reports whether anything changed, so the driver knows to tick again at
    /// the same timestamp.
    fn accept(&mut self, tick: &PipelineTick) -> bool {
        let mut changed = false;
        for dispatch in &tick.dispatches {
            self.worker_mut(&dispatch.plugin).runs.push(Run {
                generation: dispatch.generation,
                dispatched_at: dispatch.dispatched_at,
                cancelled: false,
                partial_sent: false,
                finished: false,
            });
            changed = true;
        }
        for cancellation in &tick.cancellations {
            let worker = self.worker_mut(&cancellation.plugin);
            if let Some(run) = worker
                .runs
                .iter_mut()
                .find(|run| run.generation == cancellation.generation)
            {
                run.cancelled = true;
                changed = true;
            }
        }
        changed
    }

    /// Earliest timestamp at which any run owes a batch.
    fn next_due(&self) -> Option<Millis> {
        self.workers
            .iter()
            .flat_map(|worker| worker.runs.iter().map(|run| next_step(run, &worker.script).at()))
            .min()
    }

    /// Every batch due at exactly `now`.
    fn emit(&mut self, now: Millis) -> Vec<Emission> {
        let mut emissions = Vec::new();
        for worker in &mut self.workers {
            let script = worker.script;
            let plugin = worker.plugin.clone();
            for run in &mut worker.runs {
                let step = next_step(run, &script);
                if step.at() != now {
                    continue;
                }

                // A worker that honours cancellation stops at its next due
                // point and says why, carrying nothing (spec 12.5).
                if run.cancelled && !script.ignores_cancellation {
                    run.finished = true;
                    emissions.push(Emission {
                        batch: ResultBatch {
                            generation: run.generation,
                            plugin: plugin.clone(),
                            state: BatchState::Cancelled,
                            items: Vec::new(),
                        },
                        terminal: true,
                    });
                    continue;
                }

                let (offset, state, terminal) = match step {
                    Step::Partial(_) => {
                        run.partial_sent = true;
                        (0, BatchState::Partial, false)
                    }
                    Step::Final(_) => {
                        run.finished = true;
                        (
                            usize::from(run.partial_sent) * script.items_per_batch,
                            BatchState::Final,
                            true,
                        )
                    }
                };
                emissions.push(Emission {
                    batch: ResultBatch {
                        generation: run.generation,
                        plugin: plugin.clone(),
                        state,
                        items: scripted_items(&plugin, run.generation, offset, &script),
                    },
                    terminal,
                });
            }
            worker.runs.retain(|run| !run.finished);
        }
        emissions
    }
}

/// Items a worker answers with, tagged so a row can be traced back to the
/// generation that produced it.
fn scripted_items(
    plugin: &PluginId,
    generation: Generation,
    offset: usize,
    script: &PluginScript,
) -> Vec<Item> {
    let owner = short_name(plugin);
    (offset..offset + script.items_per_batch)
        .map(|index| Item {
            stable_id: ItemId(format!("{owner}#g{}#{index}", generation.get())),
            plugin_id: plugin.clone(),
            category: Category::Application,
            label: format!("{owner} answer {index}"),
            description: format!("scripted answer for {generation}"),
            target: format!("app://{owner}/{index}"),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: HitPolicy::Recorded,
            score_hint: script.top_score_hint - index as i32,
            metadata: BTreeMap::new(),
            actions: Vec::new(),
        })
        .collect()
}

fn short_name(plugin: &PluginId) -> &str {
    plugin.0.rsplit('.').next().unwrap_or(plugin.0.as_str())
}

fn plugin_id(id: &str) -> PluginId {
    PluginId(id.to_owned())
}

// ---------------------------------------------------------------------------
// Virtual-time driver
// ---------------------------------------------------------------------------

/// Upper bound on same-timestamp rounds. Reached only by a pipeline that
/// dispatches without ever consuming its queue, which is the bug this bounds.
const SETTLE_ROUNDS: usize = 16;

/// One presented frame, reduced to what the exit criteria talk about.
#[derive(Debug, Clone)]
struct Frame {
    at: Millis,
    /// Generation the live query text belongs to.
    generation: Generation,
    rows: Vec<ItemId>,
    /// Generation encoded in each row's id, in row order.
    row_generations: Vec<u64>,
    /// Owner encoded in each row's id, in row order.
    owners: Vec<String>,
    pending_plugins: bool,
}

impl Frame {
    fn capture(at: Millis, view: &ViewModel) -> Self {
        Self {
            at,
            generation: view.generation,
            rows: view.rows.iter().map(|row| row.item.clone()).collect(),
            row_generations: view.rows.iter().map(row_generation).collect(),
            owners: view.rows.iter().map(row_owner).collect(),
            pending_plugins: view.pending_plugins,
        }
    }

    /// The single generation every visible row came from. `None` for a frame
    /// with no rows; a frame mixing generations is a contract violation, not a
    /// value, so it fails here rather than being reported.
    fn row_generation(&self) -> Option<u64> {
        let first = *self.row_generations.first()?;
        assert!(
            self.row_generations.iter().all(|value| *value == first),
            "frame at {} mixed generations {:?}",
            self.at,
            self.row_generations
        );
        Some(first)
    }
}

fn row_generation(row: &ResultRow) -> u64 {
    let id = row.item.0.as_str();
    let start = id
        .find("#g")
        .unwrap_or_else(|| panic!("scripted row id `{id}` lost its generation tag"))
        + 2;
    let rest = &id[start..];
    let end = rest.find('#').unwrap_or(rest.len());
    rest[..end]
        .parse()
        .unwrap_or_else(|_| panic!("scripted row id `{id}` has no generation digits"))
}

fn row_owner(row: &ResultRow) -> String {
    row.item.0.split('#').next().unwrap_or_default().to_owned()
}

/// The most recent frame that actually carried rows.
fn last_populated(frames: &[Frame]) -> &Frame {
    frames
        .iter()
        .rev()
        .find(|frame| !frame.rows.is_empty())
        .expect("no frame ever carried a row")
}

/// Runs the pipeline and its workers to a fixed point at `now`, then presents.
///
/// Presenting once per timestamp rather than once per batch is deliberate: it
/// is the frame budget of spec 25.5, and it is what makes a frame count a
/// meaningful assertion.
fn settle(pipeline: &mut QueryPipeline, fleet: &mut Fleet, now: Millis, frames: &mut Vec<Frame>) {
    for _ in 0..SETTLE_ROUNDS {
        let tick = pipeline.tick(now);
        let mut progressed = fleet.accept(&tick);
        for emission in fleet.emit(now) {
            let plugin = emission.batch.plugin.clone();
            let generation = emission.batch.generation;
            let terminal = emission.terminal;
            let _ = pipeline.deliver(emission.batch, now);
            if terminal {
                pipeline.complete(&plugin, generation, now);
            }
            progressed = true;
        }
        if !progressed {
            if let Some(view) = pipeline.present(now) {
                frames.push(Frame::capture(now, &view));
            }
            return;
        }
    }
    panic!("pipeline never reached a fixed point at {now}");
}

/// The next timestamp anything actually asked for, within `deadline`.
fn advance(pipeline: &QueryPipeline, fleet: &Fleet, now: Millis, deadline: Millis) -> Option<Millis> {
    [pipeline.next_wakeup(), fleet.next_due()]
        .into_iter()
        .flatten()
        .filter(|at| *at > now && *at <= deadline)
        .min()
}

/// Advances from `from` to `deadline`, stopping only where work is due.
fn run_until(
    pipeline: &mut QueryPipeline,
    fleet: &mut Fleet,
    from: Millis,
    deadline: Millis,
    frames: &mut Vec<Frame>,
) {
    let mut now = from;
    settle(pipeline, fleet, now, frames);
    while let Some(next) = advance(pipeline, fleet, now, deadline) {
        now = next;
        settle(pipeline, fleet, now, frames);
    }
    if now < deadline {
        settle(pipeline, fleet, deadline, frames);
    }
}

/// Types `text` one character at a time, `interval` apart, running the pipeline
/// through every scheduled wake-up in between.
fn type_query(
    pipeline: &mut QueryPipeline,
    fleet: &mut Fleet,
    text: &str,
    start: Millis,
    interval: Millis,
    frames: &mut Vec<Frame>,
) -> Vec<Generation> {
    assert!(interval > 0, "keystrokes need distinct timestamps");
    let mut generations = Vec::new();
    let mut typed = String::new();
    let mut at = start;
    for character in text.chars() {
        typed.push(character);
        generations.push(pipeline.keystroke(&typed, at));
        run_until(pipeline, fleet, at, at + interval - 1, frames);
        at += interval;
    }
    generations
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn manifest(text: &str) -> Manifest {
    Manifest::parse(text).expect("fixture manifest must parse and validate")
}

fn register(pipeline: &mut QueryPipeline, fleet: &mut Fleet, text: &str, script: PluginScript) -> PluginId {
    let plugin = pipeline
        .register_manifest(&manifest(text))
        .expect("fixture plugin registers once");
    fleet.attach(&plugin, script);
    plugin
}

/// The three-plugin launcher every stress test uses: one fast native plugin,
/// one slow Python plugin, one `legacy-strict` plugin.
///
/// The slow plugin's items outrank the fast plugin's, so its arrival shows up
/// as a reorder and not merely as extra rows.
fn three_plugin_launcher() -> (QueryPipeline, Fleet, [PluginId; 3]) {
    let mut pipeline = QueryPipeline::new(config());
    let mut fleet = Fleet::default();

    let fast = register(
        &mut pipeline,
        &mut fleet,
        FAST_NATIVE_MANIFEST,
        PluginScript {
            first_batch_after: Some(FAST_FIRST_MS),
            final_batch_after: FAST_FINAL_MS,
            items_per_batch: 2,
            top_score_hint: 40,
            ignores_cancellation: false,
        },
    );
    let slow = register(
        &mut pipeline,
        &mut fleet,
        SLOW_PYTHON_MANIFEST,
        PluginScript {
            first_batch_after: Some(SLOW_FIRST_MS),
            final_batch_after: SLOW_FINAL_MS,
            items_per_batch: 2,
            top_score_hint: 90,
            ignores_cancellation: true,
        },
    );
    let legacy = register(
        &mut pipeline,
        &mut fleet,
        LEGACY_STRICT_MANIFEST,
        PluginScript::prompt(LEGACY_CALLBACK_MS, 2, 10),
    );

    (pipeline, fleet, [fast, slow, legacy])
}

// ---------------------------------------------------------------------------
// Trace helpers (spec 26.4)
// ---------------------------------------------------------------------------

/// Exhaustive on purpose: a new trace category must be classified here rather
/// than silently escaping the ordering and coverage assertions below.
fn event_at(event: &QueryTraceEvent) -> Millis {
    match event {
        QueryTraceEvent::Keystroke { at, .. }
        | QueryTraceEvent::Debounce { at, .. }
        | QueryTraceEvent::LegacyDispatch { at, .. }
        | QueryTraceEvent::Dispatched { at, .. }
        | QueryTraceEvent::Cancelled { at, .. }
        | QueryTraceEvent::FirstResult { at, .. }
        | QueryTraceEvent::FinalResult { at, .. }
        | QueryTraceEvent::ResultBatch { at, .. }
        | QueryTraceEvent::StaleResultRejected { at, .. }
        | QueryTraceEvent::RequestDropped { at, .. }
        | QueryTraceEvent::Ranking { at, .. }
        | QueryTraceEvent::Presentation { at, .. } => *at,
    }
}

/// Every `(timestamp, generation)` this plugin was dispatched at.
fn dispatches(pipeline: &QueryPipeline, plugin: &PluginId) -> Vec<(Millis, Generation)> {
    pipeline
        .trace()
        .iter()
        .filter_map(|event| match event {
            QueryTraceEvent::Dispatched {
                at,
                plugin: owner,
                generation,
            } if owner == plugin => Some((*at, *generation)),
            _ => None,
        })
        .collect()
}

fn presentations(pipeline: &QueryPipeline) -> Vec<(Millis, Generation, usize)> {
    pipeline
        .trace()
        .iter()
        .filter_map(|event| match event {
            QueryTraceEvent::Presentation {
                at,
                generation,
                visible_items,
            } => Some((*at, *generation, *visible_items)),
            _ => None,
        })
        .collect()
}

fn debounce_events(pipeline: &QueryPipeline) -> Vec<&QueryTraceEvent> {
    pipeline
        .trace()
        .iter()
        .filter(|event| matches!(event, QueryTraceEvent::Debounce { .. }))
        .collect()
}

fn is_ascending<T: PartialOrd + Copy>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] <= pair[1])
}

// ---------------------------------------------------------------------------
// §31.4 - Rapid typing does not create an unbounded request queue
// ---------------------------------------------------------------------------

#[test]
fn rapid_typing_holds_one_undispatched_query_per_plugin_and_drains_to_empty() {
    let (mut pipeline, mut fleet, [fast, slow, legacy]) = three_plugin_launcher();
    let mut frames = Vec::new();

    // Twenty keystrokes 5 ms apart: below every declared debounce interval and
    // well inside the legacy worker's callback duration, so each plugin is
    // offered far more work than it can possibly take.
    let generations = type_query(
        &mut pipeline,
        &mut fleet,
        "firefox developer ed",
        START,
        5,
        &mut frames,
    );
    assert_eq!(generations.len(), 20);
    assert!(
        is_ascending(&generations) && generations[0] < generations[19],
        "generations must be minted monotonically: {generations:?}"
    );

    let peak = pipeline.diagnostics().peak_queue_depth;
    assert!(
        peak <= 3,
        "newest-wins queuing bounds undispatched work at one query per plugin, saw {peak}"
    );
    assert!(
        peak <= config().scheduler.request_queue_capacity,
        "peak depth {peak} exceeded the configured request queue capacity"
    );
    for plugin in [&fast, &slow, &legacy] {
        let per_plugin = pipeline
            .plugin_diagnostics(plugin)
            .unwrap_or_else(|| panic!("{} is registered", plugin.0));
        assert!(
            per_plugin.peak_queue_depth <= 1,
            "{} queued {} undispatched requests under ReplaceOldest",
            plugin.0,
            per_plugin.peak_queue_depth
        );
    }

    // The bound must come from supersession, not from refusal: a pipeline that
    // stayed bounded by throwing arrivals away would break spec 8.8.
    let diagnostics = pipeline.diagnostics();
    assert_eq!(
        diagnostics.rejected_requests(),
        0,
        "no arrival may be refused while every queue is newest-wins"
    );
    assert!(
        diagnostics.coalesced_requests > 0,
        "twenty keystrokes 5 ms apart must supersede pending work"
    );
    assert!(
        diagnostics.dispatched_requests < 3 * generations.len() as u64,
        "every plugin was dispatched for every keystroke: {} dispatches",
        diagnostics.dispatched_requests
    );

    run_until(&mut pipeline, &mut fleet, START + 100, START + 2_000, &mut frames);

    let drained = pipeline.diagnostics();
    assert_eq!(drained.queued_requests, 0, "queued work outlived the session");
    assert_eq!(
        drained.in_flight_requests, 0,
        "in-flight work outlived the session"
    );
    for plugin in [&fast, &slow, &legacy] {
        assert_eq!(
            pipeline.health(plugin).queue_depth,
            0,
            "{} still reports queued work to diagnostics (spec 24.3)",
            plugin.0
        );
    }
    assert!(
        !pipeline.rows().is_empty(),
        "the session ended with nothing on screen"
    );
}

// ---------------------------------------------------------------------------
// §31.7 - Results from stale query generations are never displayed
// ---------------------------------------------------------------------------

#[test]
fn superseded_batches_are_rejected_and_no_frame_ever_mixes_generations() {
    let (mut pipeline, mut fleet, [_, _, legacy]) = three_plugin_launcher();
    let mut frames = Vec::new();

    let generations = type_query(&mut pipeline, &mut fleet, "fire storm", START, 8, &mut frames);
    let newest = *generations.last().expect("keystrokes minted generations");
    run_until(&mut pipeline, &mut fleet, START + 80, START + 2_000, &mut frames);

    // The legacy worker was dispatched on the first keystroke and its callback
    // runs for 90 ms, so it certainly answered a generation the user had
    // already typed past. None of it may reach a row.
    assert!(
        pipeline.diagnostics().rejected_stale_results > 0,
        "a worker answering for superseded generations must be refused"
    );
    assert!(
        pipeline.health(&legacy).stale_results_rejected > 0,
        "stale refusals must reach per-plugin diagnostics (spec 24.3)"
    );

    assert!(!frames.is_empty(), "nothing was ever presented");
    for frame in &frames {
        // Opening a generation clears the prior answer synchronously. Any
        // populated frame must therefore belong exactly to its live query.
        if let Some(shown) = frame.row_generation() {
            assert_eq!(
                shown,
                frame.generation.get(),
                "frame at {} displayed generation {shown} for live query {}",
                frame.at,
                frame.generation
            );
        }
    }

    assert_eq!(
        last_populated(&frames).row_generation(),
        Some(newest.get()),
        "the settled screen must show the newest generation and nothing else"
    );
    assert_eq!(
        pipeline.visible_generation(),
        Some(newest),
        "the pipeline disagrees with its own last frame"
    );
}

// ---------------------------------------------------------------------------
// §31.7 - No cross-generation reordering
// ---------------------------------------------------------------------------

#[test]
fn presented_generations_never_move_backwards() {
    let (mut pipeline, mut fleet, _) = three_plugin_launcher();
    let mut frames = Vec::new();

    type_query(&mut pipeline, &mut fleet, "fire storm", START, 8, &mut frames);
    run_until(&mut pipeline, &mut fleet, START + 80, START + 2_000, &mut frames);

    let shown: Vec<u64> = frames.iter().filter_map(Frame::row_generation).collect();
    assert!(
        is_ascending(&shown),
        "displayed answers moved back to an older generation: {shown:?}"
    );
    assert!(
        shown.windows(2).any(|pair| pair[0] < pair[1]),
        "the screen never advanced a generation, so ordering is untested: {shown:?}"
    );

    let live: Vec<u64> = frames.iter().map(|frame| frame.generation.get()).collect();
    assert!(
        is_ascending(&live),
        "the live query generation went backwards: {live:?}"
    );

    let traced: Vec<Generation> = presentations(&pipeline)
        .into_iter()
        .map(|(_, generation, _)| generation)
        .collect();
    assert!(
        is_ascending(&traced),
        "the trace records presentations out of generation order: {traced:?}"
    );
    assert_eq!(
        traced.len(),
        frames.len(),
        "every presented frame must appear exactly once in the trace (spec 26.4)"
    );
}

// ---------------------------------------------------------------------------
// §31.3, §31.8 - A slow plugin never delays a fast one
// ---------------------------------------------------------------------------

#[test]
fn fast_plugin_rows_are_presented_while_the_slow_plugin_is_still_running() {
    let (mut pipeline, mut fleet, [fast, slow, legacy]) = three_plugin_launcher();
    let mut frames = Vec::new();

    let generation = pipeline.keystroke("fire", START);
    // Stop before the slow worker can possibly answer: its trailing edge alone
    // lands at START + 60, and its first batch 150 ms after that.
    let early_deadline = START + SLOW_DEBOUNCE_MS + SLOW_FIRST_MS - 1;
    run_until(&mut pipeline, &mut fleet, START, early_deadline, &mut frames);

    let early = frames
        .iter()
        .find(|frame| !frame.rows.is_empty())
        .expect("the fast plugin published nothing");
    assert!(
        early.at <= START + FAST_DEBOUNCE_MS + FAST_FINAL_MS,
        "the first rows waited until {}, long after the fast plugin answered",
        early.at
    );
    assert!(
        early.owners.iter().all(|owner| owner != short_name(&slow)),
        "the slow plugin cannot have contributed to the first frame: {:?}",
        early.owners
    );
    assert!(
        early.owners.iter().any(|owner| owner == short_name(&fast)),
        "the fast plugin's answer never reached the screen: {:?}",
        early.owners
    );
    assert!(
        early.pending_plugins,
        "the frame must admit that plugins are still working (spec 6.2.7)"
    );

    let slow_state = pipeline
        .plugin_diagnostics(&slow)
        .expect("the slow plugin is registered");
    assert_eq!(
        slow_state.in_flight_requests, 1,
        "the slow plugin should still be running while the fast one is on screen"
    );
    assert_eq!(
        pipeline.visible_generation(),
        Some(generation),
        "the visible rows belong to the only generation there is"
    );
    let early_at = early.at;
    let early_rows = early.rows.len();

    // Let the slow plugin land. Its items outrank everyone else's, so its
    // arrival is observable as a reorder rather than merely as extra rows.
    run_until(
        &mut pipeline,
        &mut fleet,
        early_deadline,
        START + 1_000,
        &mut frames,
    );

    let settled = last_populated(&frames);
    assert!(
        settled.at > early_at,
        "the slow plugin never produced a later frame"
    );
    assert_eq!(
        settled.owners.first().map(String::as_str),
        Some(short_name(&slow)),
        "ranking ignored score_hint when the strongest answer arrived: {:?}",
        settled.owners
    );
    assert!(
        settled.owners.iter().any(|owner| owner == short_name(&fast))
            && settled.owners.iter().any(|owner| owner == short_name(&legacy)),
        "reranking dropped answers that had already been accepted: {:?}",
        settled.owners
    );
    assert!(
        !settled.pending_plugins,
        "every plugin finished but the frame still claims pending work"
    );
    assert!(
        settled.rows.len() > early_rows,
        "the settled screen must hold more than the fast plugin's first batch"
    );
}

// ---------------------------------------------------------------------------
// §31.6 - Obsolete in-flight queries are cancelled or logically invalidated
// ---------------------------------------------------------------------------

#[test]
fn a_plugin_that_ignores_cancellation_cannot_change_what_is_on_screen() {
    let mut pipeline = QueryPipeline::new(config());
    let mut fleet = Fleet::default();
    let mut frames = Vec::new();

    // Registered by explicit policy rather than by manifest: cancellation
    // safety is a property of the profile, not of any particular `crikey.toml`.
    let stubborn = plugin_id("dev.crikey.stubborn");
    pipeline
        .register_plugin(
            stubborn.clone(),
            PluginPolicy {
                profile: SchedulingProfile::Modern,
                debounce: DebouncePolicy {
                    debounce_ms: 0,
                    maximum_wait_ms: None,
                    leading_edge: true,
                    trailing_edge: true,
                    minimum_query_length: 0,
                },
                ..PluginPolicy::modern()
            },
        )
        .expect("first registration succeeds");
    fleet.attach(&stubborn, PluginScript::prompt(40, 2, 50));

    assert_eq!(
        pipeline.register_plugin(stubborn.clone(), PluginPolicy::modern()),
        Err(PipelineError::AlreadyRegistered {
            plugin: stubborn.clone()
        }),
        "one plugin id may only be registered once"
    );

    let obsolete = pipeline.keystroke("f", START);
    let dispatch = pipeline.tick(START);
    assert_eq!(
        dispatch.dispatches.len(),
        1,
        "a zero-debounce leading-edge plugin is dispatched on the keystroke tick"
    );
    fleet.accept(&dispatch);

    // Supersede it while it is running. The successor cannot be dispatched yet:
    // a cancelled request keeps its concurrency slot until `complete`.
    let current = pipeline.keystroke("fi", START + 5);
    let supersede = pipeline.tick(START + 5);
    assert!(
        supersede.cancellations.iter().any(|cancellation| {
            cancellation.plugin == stubborn
                && cancellation.generation == obsolete
                && cancellation.reason == CancelReason::QueryChanged
        }),
        "superseding a generation must invalidate the in-flight request: {:?}",
        supersede.cancellations
    );
    fleet.accept(&supersede);

    // The worker ignores the cancellation and answers for the dead generation.
    let before: Vec<ItemId> = pipeline.rows().iter().map(|row| row.item.clone()).collect();
    let script = PluginScript::prompt(40, 2, 50);
    let refusal = pipeline.deliver(
        ResultBatch {
            generation: obsolete,
            plugin: stubborn.clone(),
            state: BatchState::Final,
            items: scripted_items(&stubborn, obsolete, 0, &script),
        },
        START + 10,
    );
    assert_eq!(
        refusal,
        Err(PipelineError::QueueRejected {
            plugin: stubborn.clone(),
            generation: obsolete,
            reason: QueueReject::StaleGeneration,
        }),
        "a batch for a cancelled generation must be refused at the boundary"
    );
    let after: Vec<ItemId> = pipeline.rows().iter().map(|row| row.item.clone()).collect();
    assert_eq!(before, after, "a refused batch mutated the visible rows");

    assert_eq!(
        pipeline.complete(&stubborn, obsolete, START + 10),
        CompletionOutcome::Stale,
        "completing a cancelled generation is stale, not an error"
    );
    let health = pipeline.health(&stubborn);
    assert!(
        health.cancellations_ignored > 0,
        "a worker that answered after cancellation must be counted (spec 24.3)"
    );
    assert!(
        health.stale_results_rejected > 0,
        "the refused batch must reach per-plugin diagnostics"
    );

    // "Safely" means the pipeline is undamaged: releasing the slot lets the
    // live generation dispatch, finish and reach the screen as normal.
    fleet
        .worker_mut(&stubborn)
        .runs
        .retain(|run| run.generation != obsolete);
    run_until(&mut pipeline, &mut fleet, START + 10, START + 500, &mut frames);

    assert_eq!(
        dispatches(&pipeline, &stubborn)
            .iter()
            .map(|(_, generation)| *generation)
            .collect::<Vec<_>>(),
        vec![obsolete, current],
        "the successor must be dispatched once its predecessor released the slot"
    );
    assert_eq!(
        last_populated(&frames).row_generation(),
        Some(current.get()),
        "the live generation did not survive its predecessor's cancellation"
    );
    assert_eq!(pipeline.visible_generation(), Some(current));
}

// ---------------------------------------------------------------------------
// §8.5, §19.4 - Manifest debounce edges decide dispatch
// ---------------------------------------------------------------------------

#[test]
fn manifest_leading_and_trailing_edges_decide_when_each_plugin_is_dispatched() {
    let mut pipeline = QueryPipeline::new(config());
    let mut fleet = Fleet::default();
    let mut frames = Vec::new();

    let leading = register(
        &mut pipeline,
        &mut fleet,
        LEADING_ONLY_MANIFEST,
        PluginScript::prompt(2, 1, 20),
    );
    let trailing = register(
        &mut pipeline,
        &mut fleet,
        TRAILING_ONLY_MANIFEST,
        PluginScript::prompt(2, 1, 30),
    );

    let first = pipeline.keystroke("f", START);
    run_until(&mut pipeline, &mut fleet, START, START + 9, &mut frames);
    assert_eq!(
        dispatches(&pipeline, &leading),
        vec![(START, first)],
        "a leading-edge plugin is dispatched on the keystroke that made it relevant"
    );
    assert!(
        dispatches(&pipeline, &trailing).is_empty(),
        "a trailing-edge plugin must not be dispatched before its quiet period"
    );

    let second = pipeline.keystroke("fi", START + 10);
    run_until(&mut pipeline, &mut fleet, START + 10, START + 500, &mut frames);

    assert_eq!(
        dispatches(&pipeline, &leading),
        vec![(START, first)],
        "leading edge only: the query typed after the leading edge is never sent"
    );
    assert_eq!(
        dispatches(&pipeline, &trailing),
        vec![(START + 10 + EDGE_DEBOUNCE_MS, second)],
        "trailing edge only: exactly one dispatch, a full debounce after the last \
         keystroke, carrying the newest generation"
    );

    // The manifest decided dispatch, so it also decided what is on screen.
    let owners: Vec<String> = pipeline.rows().iter().map(row_owner).collect();
    assert!(
        owners.iter().any(|owner| owner == short_name(&trailing)),
        "the trailing plugin's answer never landed: {owners:?}"
    );
    assert!(
        owners.iter().all(|owner| owner != short_name(&leading)),
        "the leading plugin only ever answered the superseded generation, so none \
         of its rows may survive: {owners:?}"
    );
}

// ---------------------------------------------------------------------------
// §8.6, §31.14 - Maximum wait, and legacy immunity to time debouncing
// ---------------------------------------------------------------------------

#[test]
fn maximum_wait_dispatches_during_uninterrupted_typing_while_legacy_is_never_debounced() {
    let mut pipeline = QueryPipeline::new(config());
    let mut fleet = Fleet::default();
    let mut frames = Vec::new();

    let bounded = register(
        &mut pipeline,
        &mut fleet,
        MAX_WAIT_MANIFEST,
        PluginScript::prompt(2, 1, 20),
    );
    let legacy = register(
        &mut pipeline,
        &mut fleet,
        LEGACY_STRICT_MANIFEST,
        PluginScript::prompt(LEGACY_CALLBACK_MS, 1, 10),
    );

    // Sixteen keystrokes 20 ms apart, ending at START + 300. Each resets the
    // 40 ms quiet period, so the trailing edge can never fire on its own before
    // typing stops; only the 100 ms ceiling can.
    let generations = type_query(
        &mut pipeline,
        &mut fleet,
        "firestorm search",
        START,
        20,
        &mut frames,
    );
    assert_eq!(generations.len(), 16);
    let last_keystroke = START + 15 * 20;

    let bounded_dispatches = dispatches(&pipeline, &bounded);
    assert_eq!(
        bounded_dispatches.first().map(|(at, _)| *at),
        Some(START + MAX_WAIT_CEILING_MS),
        "the first dispatch must land on the declared maximum wait, not on the \
         {EDGE_DEBOUNCE_MS} ms quiet period that typing kept resetting: {bounded_dispatches:?}"
    );
    let during_typing = bounded_dispatches
        .iter()
        .filter(|(at, _)| *at <= last_keystroke)
        .count();
    assert!(
        during_typing >= 2,
        "the ceiling must keep releasing work while typing continues: {bounded_dispatches:?}"
    );
    for (at, generation) in &bounded_dispatches {
        assert!(
            *generation <= *generations.last().expect("keystrokes minted generations"),
            "dispatched a generation that was never typed: {at} -> {generation}"
        );
    }

    // `legacy-strict` neutralizes the manifest's 250 ms debounce and its
    // four-character activation gate: dispatch is prompt, on the very first
    // one-character keystroke.
    let legacy_dispatches = dispatches(&pipeline, &legacy);
    assert_eq!(
        legacy_dispatches.first().copied(),
        Some((START, generations[0])),
        "a legacy-strict plugin is dispatched at the keystroke timestamp, whatever \
         its manifest declares: {legacy_dispatches:?}"
    );
    assert!(
        legacy_dispatches.len() < generations.len(),
        "serialized legacy dispatch cannot keep up with every keystroke: {legacy_dispatches:?}"
    );
    for pair in legacy_dispatches.windows(2) {
        assert!(
            pair[1].0 - pair[0].0 >= LEGACY_CALLBACK_MS,
            "two legacy callbacks overlapped on one instance (spec 13.4): {legacy_dispatches:?}"
        );
    }
    assert!(
        pipeline
            .plugin_diagnostics(&legacy)
            .is_some_and(|diagnostics| diagnostics.in_flight_requests <= 1),
        "a legacy instance may never hold two callbacks at once (spec 13.4)"
    );
    assert!(
        pipeline.trace().iter().any(|event| matches!(
            event,
            QueryTraceEvent::LegacyDispatch {
                plugin,
                decision: LegacyDispatch::QueuedBehindRunning { .. },
                ..
            } if plugin == &legacy
        )),
        "typing over a busy legacy plugin must record an obsolete-work replacement"
    );

    run_until(&mut pipeline, &mut fleet, START + 320, START + 1_500, &mut frames);
    assert_eq!(
        pipeline
            .plugin_diagnostics(&legacy)
            .map(|diagnostics| diagnostics.in_flight_requests),
        Some(0),
        "the legacy plugin never finished its last callback"
    );
    assert_eq!(
        pipeline.visible_generation(),
        generations.last().copied(),
        "the settled screen must answer the last thing typed"
    );
}

// ---------------------------------------------------------------------------
// §26.4 - The developer query trace
// ---------------------------------------------------------------------------

#[test]
fn query_trace_records_every_documented_category_for_one_session() {
    let (mut pipeline, mut fleet, [fast, slow, legacy]) = three_plugin_launcher();
    let mut frames = Vec::new();

    let generations = type_query(&mut pipeline, &mut fleet, "fire", START, 6, &mut frames);
    run_until(&mut pipeline, &mut fleet, START + 24, START + 2_000, &mut frames);

    assert_eq!(
        pipeline.diagnostics().trace_events_dropped,
        0,
        "the trace ring overflowed, so absence below would prove nothing"
    );
    let timestamps: Vec<Millis> = pipeline.trace().iter().map(event_at).collect();
    assert!(
        is_ascending(&timestamps),
        "the trace must read in timestamp order to be usable"
    );

    // Keystroke timestamps, and the query generations they minted.
    let keystrokes: Vec<(Millis, Generation, usize)> = pipeline
        .trace()
        .iter()
        .filter_map(|event| match event {
            QueryTraceEvent::Keystroke {
                at,
                generation,
                query_length,
            } => Some((*at, *generation, *query_length)),
            _ => None,
        })
        .collect();
    assert_eq!(
        keystrokes
            .iter()
            .map(|(at, _, length)| (*at, *length))
            .collect::<Vec<_>>(),
        vec![(START, 1), (START + 6, 2), (START + 12, 3), (START + 18, 4)],
        "every keystroke must be recorded with its timestamp and query length"
    );
    assert_eq!(
        keystrokes
            .iter()
            .map(|(_, generation, _)| *generation)
            .collect::<Vec<_>>(),
        generations,
        "traced generations must be the ones the pipeline handed out"
    );

    // Modern debounce decisions: an immediate leading edge for the fast plugin,
    // a deferral for the trailing-only slow plugin, and replacement in between.
    let debounces = debounce_events(&pipeline);
    assert!(
        pipeline.trace().iter().any(|event| matches!(
            event,
            QueryTraceEvent::Debounce {
                plugin,
                decision: DebounceDecision::LeadingEdge,
                ..
            } if plugin == &fast
        )),
        "the fast plugin's leading edge was never recorded: {debounces:?}"
    );
    assert!(
        pipeline.trace().iter().any(|event| matches!(
            event,
            QueryTraceEvent::Debounce {
                plugin,
                decision: DebounceDecision::Deferred { .. },
                ..
            } if plugin == &slow
        )),
        "the slow plugin's deferral was never recorded: {debounces:?}"
    );
    assert!(
        pipeline.trace().iter().any(|event| matches!(
            event,
            QueryTraceEvent::Debounce {
                decision: DebounceDecision::Coalesced { .. }
                    | DebounceDecision::TrailingEdge
                    | DebounceDecision::MaximumWait,
                ..
            }
        )),
        "no replacement or dispatch-edge decision across four keystrokes: {debounces:?}"
    );

    // Legacy dispatch and replacement decisions.
    assert!(
        pipeline.trace().iter().any(|event| matches!(
            event,
            QueryTraceEvent::LegacyDispatch { plugin, .. } if plugin == &legacy
        )),
        "the legacy plugin's dispatch decisions are missing from the trace"
    );

    // Plugin dispatch timestamps.
    for plugin in [&fast, &slow, &legacy] {
        assert!(
            !dispatches(&pipeline, plugin).is_empty(),
            "{} was never recorded as dispatched",
            plugin.0
        );
    }

    // Cancellation timestamps.
    let cancellations: Vec<(Millis, CancelReason)> = pipeline
        .trace()
        .iter()
        .filter_map(|event| match event {
            QueryTraceEvent::Cancelled { at, reason, .. } => Some((*at, *reason)),
            _ => None,
        })
        .collect();
    assert!(
        cancellations
            .iter()
            .any(|(_, reason)| *reason == CancelReason::QueryChanged),
        "typing over in-flight work must record a cancellation: {cancellations:?}"
    );

    // First- and final-result latency, measured from the request's own dispatch
    // rather than from the keystroke. The fast worker's script fixes both.
    let fast_first: Vec<Millis> = pipeline
        .trace()
        .iter()
        .filter_map(|event| match event {
            QueryTraceEvent::FirstResult {
                plugin, latency_ms, ..
            } if plugin == &fast => Some(*latency_ms),
            _ => None,
        })
        .collect();
    let fast_final: Vec<Millis> = pipeline
        .trace()
        .iter()
        .filter_map(|event| match event {
            QueryTraceEvent::FinalResult {
                plugin, latency_ms, ..
            } if plugin == &fast => Some(*latency_ms),
            _ => None,
        })
        .collect();
    assert!(
        !fast_first.is_empty() && fast_first.iter().all(|latency| *latency == FAST_FIRST_MS),
        "first-result latency must be measured from dispatch: {fast_first:?}"
    );
    assert!(
        !fast_final.is_empty() && fast_final.iter().all(|latency| *latency == FAST_FINAL_MS),
        "final-result latency must be measured from dispatch: {fast_final:?}"
    );

    // Result-batch sizes, with their completion state.
    let batches: Vec<(usize, BatchCompletion)> = pipeline
        .trace()
        .iter()
        .filter_map(|event| match event {
            QueryTraceEvent::ResultBatch {
                plugin,
                items,
                completion,
                ..
            } if plugin == &fast => Some((*items, *completion)),
            _ => None,
        })
        .collect();
    assert!(
        batches
            .iter()
            .any(|(items, completion)| *items == 2 && *completion == BatchCompletion::Partial),
        "a partial batch of the scripted size was never recorded: {batches:?}"
    );
    assert!(
        batches
            .iter()
            .any(|(items, completion)| *items == 2 && *completion == BatchCompletion::Final),
        "a final batch of the scripted size was never recorded: {batches:?}"
    );

    // Rejected stale responses.
    let stale: Vec<&PluginId> = pipeline
        .trace()
        .iter()
        .filter_map(|event| match event {
            QueryTraceEvent::StaleResultRejected { plugin, .. } => Some(plugin),
            _ => None,
        })
        .collect();
    assert!(
        stale.contains(&&legacy),
        "the worker still answering the first generation 90 ms later left no stale \
         record: {stale:?}"
    );

    // Ranking and presentation updates.
    let rankings: Vec<(Generation, usize)> = pipeline
        .trace()
        .iter()
        .filter_map(|event| match event {
            QueryTraceEvent::Ranking {
                generation,
                ranked_items,
                ..
            } => Some((*generation, *ranked_items)),
            _ => None,
        })
        .collect();
    let presented = presentations(&pipeline);
    assert!(!rankings.is_empty(), "no ranking pass was ever recorded");
    assert_eq!(
        rankings.len(),
        presented.len(),
        "every presented frame must be preceded by exactly one ranking pass"
    );
    let (_, last_generation, last_visible) = *presented
        .last()
        .expect("the session presented at least one frame");
    assert_eq!(
        last_visible,
        pipeline.rows().len(),
        "the last presentation must describe the rows actually on screen"
    );
    assert_eq!(
        Some(last_generation),
        pipeline.visible_generation(),
        "the last presentation must name the visible generation"
    );
    assert_eq!(
        last_generation,
        *generations.last().expect("keystrokes minted generations"),
        "the session settled on a generation the user never typed"
    );
}

// ---------------------------------------------------------------------------
// Bounded intake composition regressions
// ---------------------------------------------------------------------------

#[test]
fn an_empty_tick_does_not_spend_the_same_timestamp_presentation_drain() {
    let mut bounded = config();
    bounded.drain_budget = DrainBudget {
        batches_per_plugin: 1,
        items_per_plugin: 8,
        total_batches: 1,
    };

    let mut pipeline = QueryPipeline::new(bounded);
    let plugin = plugin_id("dev.crikey.synchronous-intake");
    pipeline
        .register_plugin(plugin.clone(), immediate_policy())
        .expect("plugin registers");

    let generation = pipeline.keystroke("prompt", START);
    let dispatch = pipeline.tick(START);
    assert_eq!(dispatch.dispatches.len(), 1);
    assert_eq!(dispatch.drain_report.merged, 0);

    let script = PluginScript::prompt(0, 1, 10);
    pipeline
        .deliver(
            ResultBatch {
                generation,
                plugin: plugin.clone(),
                state: BatchState::Final,
                items: scripted_items(&plugin, generation, 0, &script),
            },
            START,
        )
        .expect("synchronous result enters intake after dispatch");
    assert_eq!(
        pipeline.complete(&plugin, generation, START),
        CompletionOutcome::Accepted
    );

    let frame = pipeline
        .present(START)
        .expect("same-timestamp presentation drains the synchronous result");
    assert_eq!(frame.generation, generation);
    assert_eq!(frame.rows.len(), 1);
    assert_eq!(pipeline.intake_depth().batches, 0);
    assert_eq!(pipeline.intake_diagnostics().merged(), 1);
    assert!(pipeline.trace().iter().any(|event| matches!(
        event,
        QueryTraceEvent::ResultBatch {
            plugin: owner,
            generation: traced,
            items: 1,
            completion: BatchCompletion::Final,
            at,
        } if owner == &plugin && *traced == generation && *at == START
    )));
}

#[test]
fn a_nonempty_tick_spends_only_one_same_timestamp_drain_budget() {
    let mut bounded = config();
    bounded.drain_budget = DrainBudget {
        batches_per_plugin: 1,
        items_per_plugin: 8,
        total_batches: 1,
    };

    let mut pipeline = QueryPipeline::new(bounded);
    let alpha = plugin_id("dev.crikey.same-turn-alpha");
    let beta = plugin_id("dev.crikey.same-turn-beta");
    for plugin in [&alpha, &beta] {
        pipeline
            .register_plugin(plugin.clone(), immediate_policy())
            .expect("plugin registers");
    }

    let generation = pipeline.keystroke("bounded", START);
    assert_eq!(pipeline.tick(START).dispatches.len(), 2);
    let script = PluginScript::prompt(0, 1, 10);
    for plugin in [&alpha, &beta] {
        pipeline
            .deliver(
                ResultBatch {
                    generation,
                    plugin: plugin.clone(),
                    state: BatchState::Partial,
                    items: scripted_items(plugin, generation, 0, &script),
                },
                START,
            )
            .expect("batch enters intake");
    }

    let drained = pipeline.tick(START);
    assert_eq!(drained.drain_report.merged, 1);
    assert_eq!(pipeline.intake_depth().batches, 1);

    let first_frame = pipeline
        .present(START)
        .expect("the first budgeted batch is presented");
    assert_eq!(first_frame.rows.len(), 1);
    assert_eq!(
        pipeline.intake_depth().batches,
        1,
        "present must not take a second budget at the same timestamp"
    );

    let second_frame = pipeline
        .present(START + 1)
        .expect("the next timestamp can drain the remaining batch");
    assert_eq!(second_frame.rows.len(), 2);
    assert_eq!(pipeline.intake_depth().batches, 0);
}

#[test]
fn an_empty_final_merges_traces_and_presents_the_current_empty_generation() {
    let mut pipeline = QueryPipeline::new(config());
    let plugin = plugin_id("dev.crikey.empty-final");
    pipeline
        .register_plugin(plugin.clone(), immediate_policy())
        .expect("plugin registers");

    let generation = pipeline.keystroke("nothing", START);
    assert_eq!(pipeline.visible_generation(), Some(generation));
    assert!(
        pipeline.rows().is_empty(),
        "the generation opens empty synchronously"
    );
    let opening = pipeline
        .present(START)
        .expect("the empty opening frame is published");
    assert_eq!(opening.generation, generation);
    assert!(opening.rows.is_empty());

    let dispatched = pipeline.tick(START);
    assert_eq!(dispatched.dispatches.len(), 1);
    pipeline
        .deliver(
            ResultBatch {
                generation,
                plugin: plugin.clone(),
                state: BatchState::Final,
                items: Vec::new(),
            },
            START + 1,
        )
        .expect("empty terminal publication enters intake");
    assert_eq!(pipeline.intake_depth().batches, 1);

    let drained = pipeline.tick(START + 1);
    assert!(drained.errors.is_empty());
    assert_eq!(drained.drain_report.merged, 1);
    assert_eq!(drained.drain_report.merged_batches.len(), 1);
    assert_eq!(drained.drain_report.merged_batches[0].plugin, plugin);
    assert_eq!(drained.drain_report.merged_batches[0].generation, generation);
    assert_eq!(drained.drain_report.merged_batches[0].state, BatchState::Final);
    assert_eq!(drained.drain_report.merged_batches[0].items, 0);
    assert_eq!(
        pipeline.complete(&plugin, generation, START + 1),
        CompletionOutcome::Accepted
    );

    let terminal = pipeline
        .present(START + 1)
        .expect("completion changes the empty frame's pending state");
    assert_eq!(terminal.generation, generation);
    assert!(terminal.rows.is_empty());
    assert_eq!(pipeline.visible_generation(), Some(generation));
    assert!(pipeline.trace().iter().any(|event| matches!(
        event,
        QueryTraceEvent::ResultBatch {
            plugin: owner,
            generation: traced,
            items: 0,
            completion: BatchCompletion::Final,
            ..
        } if owner == &plugin && *traced == generation
    )));
}

#[test]
fn rejected_finals_keep_completion_pending_until_a_corrected_merge_commits() {
    let mut bounded = config();
    bounded.limits.max_items_per_batch = 1;
    bounded.intake_limits = QueueLimits {
        capacity_batches: 1,
        capacity_items: 4,
    };
    bounded.default_intake_policy = IntakePolicy {
        capacity_batches: 1,
        capacity_items: 4,
        pause_at_batches: 1,
        resume_at_batches: 0,
        overflow: OverflowPolicy::PauseProducer,
    };
    bounded.drain_budget = DrainBudget {
        batches_per_plugin: 1,
        items_per_plugin: 4,
        total_batches: 1,
    };

    let mut pipeline = QueryPipeline::new(bounded);
    let plugin = plugin_id("dev.crikey.valid-owner");
    pipeline
        .register_plugin(plugin.clone(), immediate_policy())
        .expect("plugin registers");
    let one = PluginScript::prompt(0, 1, 10);
    let two = PluginScript::prompt(0, 2, 10);

    let owner_generation = pipeline.keystroke("owner", START);
    let _ = pipeline.present(START);
    assert_eq!(pipeline.tick(START).dispatches.len(), 1);
    let mut wrong_owner = scripted_items(&plugin, owner_generation, 0, &one);
    wrong_owner[0].plugin_id = plugin_id("dev.crikey.foreign-owner");
    pipeline
        .deliver(
            ResultBatch {
                generation: owner_generation,
                plugin: plugin.clone(),
                state: BatchState::Final,
                items: wrong_owner,
            },
            START + 1,
        )
        .expect("transport admits the whole publication");
    assert_eq!(
        pipeline.complete(&plugin, owner_generation, START + 1),
        CompletionOutcome::Accepted,
        "callback completion waits for the admitted terminal to merge"
    );
    let rejected_owner = pipeline.tick(START + 1);
    assert_eq!(
        rejected_owner.errors,
        vec![PipelineError::AggregatorRejected {
            plugin: plugin.clone(),
            generation: owner_generation,
            reason: RejectReason::OwnerMismatch,
        }]
    );
    assert_eq!(pipeline.intake_depth().batches, 0);
    assert!(!pipeline.trace().iter().any(|event| matches!(
        event,
        QueryTraceEvent::ResultBatch { generation, .. } if *generation == owner_generation
    )));
    assert_eq!(
        pipeline.diagnostics().in_flight_requests,
        1,
        "a rejected terminal merge must retain the active request and pending completion"
    );

    pipeline
        .deliver(
            ResultBatch {
                generation: owner_generation,
                plugin: plugin.clone(),
                state: BatchState::Final,
                items: scripted_items(&plugin, owner_generation, 0, &one),
            },
            START + 2,
        )
        .expect("failed terminal merge reopened the stream and freed capacity");
    assert_eq!(pipeline.tick(START + 2).drain_report.merged, 1);
    assert_eq!(
        pipeline.diagnostics().in_flight_requests,
        0,
        "the corrected terminal merge resolves the already-pending completion"
    );

    let quota_generation = pipeline.keystroke("quota", START + 3);
    let _ = pipeline.present(START + 3);
    assert_eq!(pipeline.tick(START + 3).dispatches.len(), 1);
    pipeline
        .deliver(
            ResultBatch {
                generation: quota_generation,
                plugin: plugin.clone(),
                state: BatchState::Final,
                items: scripted_items(&plugin, quota_generation, 0, &two),
            },
            START + 4,
        )
        .expect("transport bounds and retained-item quotas are distinct");
    assert_eq!(
        pipeline.complete(&plugin, quota_generation, START + 4),
        CompletionOutcome::Accepted,
        "quota-invalid terminal completion is provisional until merge"
    );
    let rejected_quota = pipeline.tick(START + 4);
    assert_eq!(
        rejected_quota.errors,
        vec![PipelineError::AggregatorRejected {
            plugin: plugin.clone(),
            generation: quota_generation,
            reason: RejectReason::QuotaExceeded,
        }]
    );
    assert_eq!(pipeline.intake_depth().batches, 0);
    assert!(!pipeline.trace().iter().any(|event| matches!(
        event,
        QueryTraceEvent::ResultBatch { generation, .. } if *generation == quota_generation
    )));
    assert_eq!(
        pipeline.diagnostics().in_flight_requests,
        1,
        "quota rejection cannot consume the active request or its completion"
    );

    pipeline
        .deliver(
            ResultBatch {
                generation: quota_generation,
                plugin: plugin.clone(),
                state: BatchState::Final,
                items: scripted_items(&plugin, quota_generation, 0, &one),
            },
            START + 5,
        )
        .expect("quota refusal consumed neither stream state nor intake capacity");
    assert_eq!(pipeline.tick(START + 5).drain_report.merged, 1);
    assert_eq!(
        pipeline.diagnostics().in_flight_requests,
        0,
        "the quota-corrected terminal commits and releases the callback"
    );
}

#[test]
fn a_response_before_the_superseding_tick_is_classified_against_captured_cancellation() {
    let mut pipeline = QueryPipeline::new(config());
    let plugin = plugin_id("dev.crikey.response-race");
    pipeline
        .register_plugin(plugin.clone(), immediate_policy())
        .expect("plugin registers");
    let script = PluginScript::prompt(0, 1, 10);

    let obsolete = pipeline.keystroke("o", START);
    let _ = pipeline.present(START);
    assert_eq!(pipeline.tick(START).dispatches.len(), 1);
    pipeline
        .deliver(
            ResultBatch {
                generation: obsolete,
                plugin: plugin.clone(),
                state: BatchState::Partial,
                items: scripted_items(&plugin, obsolete, 0, &script),
            },
            START + 1,
        )
        .expect("first generation publishes while it is current");
    assert_eq!(pipeline.tick(START + 1).drain_report.merged, 1);
    let populated = pipeline.present(START + 1).expect("partial result is presented");
    assert_eq!(populated.generation, obsolete);
    assert_eq!(populated.rows.len(), 1);

    let current = pipeline.keystroke("on", START + 2);
    assert_eq!(pipeline.visible_generation(), Some(current));
    assert!(
        pipeline.rows().is_empty(),
        "the old row was cleared on the keystroke"
    );

    let late = pipeline.deliver(
        ResultBatch {
            generation: obsolete,
            plugin: plugin.clone(),
            state: BatchState::Final,
            items: scripted_items(&plugin, obsolete, 1, &script),
        },
        START + 2,
    );
    assert_eq!(
        late,
        Err(PipelineError::QueueRejected {
            plugin: plugin.clone(),
            generation: obsolete,
            reason: QueueReject::StaleGeneration,
        })
    );
    assert_eq!(
        pipeline.complete(&plugin, obsolete, START + 2),
        CompletionOutcome::Stale
    );

    let empty = pipeline
        .present(START + 2)
        .expect("the current generation's empty frame is immediately publishable");
    assert_eq!(empty.generation, current);
    assert!(empty.rows.is_empty());
    let superseding_tick = pipeline.tick(START + 2);
    assert!(superseding_tick.cancellations.iter().any(|cancellation| {
        cancellation.plugin == plugin
            && cancellation.generation == obsolete
            && cancellation.reason == CancelReason::QueryChanged
    }));
    assert!(superseding_tick
        .dispatches
        .iter()
        .any(|dispatch| dispatch.plugin == plugin && dispatch.generation == current));
    assert_eq!(pipeline.health(&plugin).cancellations_ignored, 1);
}

#[test]
fn pipeline_intake_overflow_is_observable_and_each_drain_is_fair() {
    let mut bounded = config();
    bounded.intake_limits = QueueLimits {
        capacity_batches: 3,
        capacity_items: 16,
    };
    bounded.default_intake_policy = IntakePolicy {
        capacity_batches: 3,
        capacity_items: 16,
        pause_at_batches: 3,
        resume_at_batches: 1,
        overflow: OverflowPolicy::PauseProducer,
    };
    bounded.drain_budget = DrainBudget {
        batches_per_plugin: 1,
        items_per_plugin: 1,
        total_batches: 2,
    };

    let mut pipeline = QueryPipeline::new(bounded);
    let alpha = plugin_id("dev.crikey.intake-alpha");
    let beta = plugin_id("dev.crikey.intake-beta");
    pipeline
        .register_plugin(alpha.clone(), immediate_policy())
        .expect("alpha registers");
    pipeline
        .register_plugin(beta.clone(), immediate_policy())
        .expect("beta registers");

    let generation = pipeline.keystroke("fair", START);
    let _ = pipeline.present(START);
    assert_eq!(pipeline.tick(START).dispatches.len(), 2);
    let script = PluginScript::prompt(0, 1, 10);
    let batch = |plugin: &PluginId, offset| ResultBatch {
        generation,
        plugin: plugin.clone(),
        state: BatchState::Partial,
        items: scripted_items(plugin, generation, offset, &script),
    };

    pipeline
        .deliver_with_priority(batch(&alpha, 0), BatchPriority::Low, START + 1)
        .expect("alpha first batch enters");
    pipeline
        .deliver_with_priority(batch(&alpha, 1), BatchPriority::Low, START + 1)
        .expect("alpha second batch enters");
    pipeline
        .deliver_with_priority(batch(&beta, 0), BatchPriority::Low, START + 1)
        .expect("beta first batch enters");
    assert_eq!(
        pipeline.deliver_with_priority(batch(&beta, 1), BatchPriority::Low, START + 1),
        Err(PipelineError::QueueRejected {
            plugin: beta.clone(),
            generation,
            reason: QueueReject::BoundaryFull,
        })
    );
    assert_eq!(pipeline.intake_depth().batches, 3);
    assert_eq!(pipeline.intake_diagnostics().admitted(), 3);
    assert_eq!(
        pipeline.intake_diagnostics().rejected(QueueReject::BoundaryFull),
        1
    );
    assert!(pipeline.take_intake_events().iter().any(|event| {
        event.plugin == beta
            && event.generation == generation
            && event.kind == QueueEventKind::Rejected(QueueReject::BoundaryFull)
    }));

    let first = pipeline.tick(START + 2);
    assert!(first.errors.is_empty());
    assert_eq!(first.drain_report.merged, 2);
    let first_plugins = first
        .drain_report
        .merged_batches
        .iter()
        .map(|merged| merged.plugin.clone())
        .collect::<Vec<_>>();
    assert_eq!(first_plugins.len(), 2);
    assert_eq!(
        first_plugins.iter().filter(|plugin| *plugin == &alpha).count(),
        1,
        "one busy producer cannot consume its neighbour's per-drain share: {first_plugins:?}"
    );
    assert_eq!(
        first_plugins.iter().filter(|plugin| *plugin == &beta).count(),
        1,
        "one busy producer cannot consume its neighbour's per-drain share: {first_plugins:?}"
    );
    assert_eq!(pipeline.intake_depth().batches, 1);
    let first_frame = pipeline.present(START + 2).expect("first fair pass is presented");
    assert_eq!(first_frame.generation, generation);
    assert_eq!(first_frame.rows.len(), 2);

    pipeline
        .deliver_with_priority(batch(&beta, 1), BatchPriority::Low, START + 3)
        .expect("draining resumed the paused producer");
    let second = pipeline.tick(START + 3);
    assert!(second.errors.is_empty());
    assert_eq!(second.drain_report.merged, 2);
    let second_plugins = second
        .drain_report
        .merged_batches
        .iter()
        .map(|merged| merged.plugin.clone())
        .collect::<Vec<_>>();
    assert_eq!(second_plugins.len(), 2);
    assert_eq!(
        second_plugins.iter().filter(|plugin| *plugin == &alpha).count(),
        1,
        "the rotating start must not permit a duplicate monopoly: {second_plugins:?}"
    );
    assert_eq!(
        second_plugins.iter().filter(|plugin| *plugin == &beta).count(),
        1,
        "the rotating start must not permit a duplicate monopoly: {second_plugins:?}"
    );
    assert_eq!(pipeline.intake_depth().batches, 0);
    assert_eq!(pipeline.intake_diagnostics().merged(), 4);
    let final_frame = pipeline
        .present(START + 3)
        .expect("second fair pass is presented");
    assert_eq!(final_frame.generation, generation);
    assert!(final_frame
        .rows
        .iter()
        .all(|row| row_generation(row) == generation.get()));
}
