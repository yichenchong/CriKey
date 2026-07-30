//! Live legacy-plugin provider (spec 7.1, 14.3, 14.5, 24.1; acceptance 31.9,
//! 31.10, 31.14 - 31.18; roadmap M3).
//!
//! This is the composition edge that makes the Legacy Compatibility Layer part
//! of the *running* launcher rather than only of the `crikey dev` commands. It
//! discovers legacy packages from configured roots, starts one out-of-process
//! CPython worker per usable package, and drives [`LegacyRuntime`] so that a
//! query typed into the app reaches every loaded legacy plugin and their
//! published suggestions cross the same [`QueryPipeline`] intake and
//! presentation boundary the built-in application provider uses.
//!
//! # Where the two schedulers meet
//!
//! Two distinct state machines cooperate here, and keeping them straight is the
//! whole design:
//!
//! * [`LegacyRuntime`] is the legacy *execution* engine. It applies the
//!   `legacy-strict` rules (broadcast, no gating, no debounce, no dynamic
//!   caching) and hands `on_suggest` across the worker boundary. Its answers
//!   re-enter through [`LegacyRuntime::deliver`] and land in its
//!   [`LegacyRuntime::visible_items`], already filtered to the current legacy
//!   generation (acceptance 31.7).
//! * [`QueryPipeline`] is the app's *presentation* boundary. Legacy plugins are
//!   registered in it under [`PluginPolicy::legacy_strict`] — the very policy
//!   the pipeline already derives for a resolved legacy manifest — so host
//!   time-debouncing, host gating and dynamic-result caching stay off (spec
//!   7.1, 14.5). The items the legacy runtime produced for a query are handed
//!   to the pipeline as a [`ResultBatch`] under the pipeline's own generation,
//!   so a superseded answer is refused at the pipeline's intake boundary rather
//!   than filtered after the fact.
//!
//! # Isolation
//!
//! Legacy Python never runs in the CriKey process: every callback executes in a
//! child interpreter owned by a [`LegacyWorker`]. The worker interaction is kept
//! behind the [`LegacyWorkerHandle`] indirection exactly as
//! `crikey-cli`'s `legacy_commands` does, so the blocking child call lives in
//! [`LegacyWorkerPool::dispatch`] and never leaks onto a caller that thinks it
//! is only enqueuing work. The call is still synchronous on whatever thread
//! invokes [`LegacyProvider::drive_query`]; a production integration must invoke
//! that drive off the user-interface thread (a supervisor thread), which the
//! handle indirection makes a placement decision rather than a rewrite. See the
//! module's `deferred` note in the crate summary.
//!
//! # Containment
//!
//! Every failure is contained (spec 24.1, acceptance 31.9, 31.10): a package
//! that will not load, a worker that will not spawn, a plugin that crashes or a
//! callback that fails degrades to "that plugin is unavailable" with a recorded
//! diagnostic. None of them aborts discovery, wedges the pipeline, or takes down
//! the process.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crikey_core::{Generation, Item, PluginId};
use crikey_input_scheduler::{Millis, PluginPolicy};
use crikey_legacy_compat::{
    discover_interpreter, shim_root, InstanceId, Interpreter, LegacyDeadlines, LegacyPackage, LegacyRequest,
    LegacyResponse, LegacyRuntime, LegacyWorker, LegacyWorkerHandle, PackageLoader, TerminationReason,
    WorkerError, WorkerOptions, WORKER_ENTRY_FILE,
};
use crikey_python_host::RuntimeProfile;
use crikey_ui::{ResultRow, ViewModel};

use crate::{BatchState, QueryPipeline, ResultBatch};

/// Bound on the startup handshake with a child interpreter, in milliseconds.
/// A liveness guard: a shim that never answers becomes a recorded unavailable
/// plugin rather than a launcher that hangs on startup.
const STARTUP_BUDGET_MS: Millis = 30_000;

/// Bound on one legacy callback, in milliseconds. Generous because spec 9.6
/// permits a slow legacy callback and forbids killing the worker merely for
/// being slow; the cooperative ladder in [`LegacyRuntime`] is what reacts to a
/// long callback, not this transport budget.
const CALL_BUDGET_MS: Millis = 120_000;

/// One legacy package that could not be made to serve suggestions, and why.
///
/// A diagnostic, never a panic: the launcher keeps every other plugin. The
/// `plugin` is present once identity is known (the package loaded but its worker
/// did not start), and absent when the package itself never loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyUnavailable {
    /// The package the user can recognize, spelled as discovery reported it.
    pub package: String,
    /// The host plugin id, once the package loaded far enough to have one.
    pub plugin: Option<PluginId>,
    /// A single-line, attributable reason (spec 26.2).
    pub reason: String,
}

/// The outbound half of the legacy worker surface for every loaded plugin.
///
/// Owns one child process per plugin and buffers each reply until the provider
/// feeds it back through [`LegacyRuntime::deliver`]. Implementing
/// [`LegacyWorkerHandle`] is what keeps the blocking child call — the one place
/// legacy Python actually executes — behind the runtime's own dispatch edge
/// rather than in the provider's control flow.
#[derive(Debug, Default)]
pub struct LegacyWorkerPool {
    workers: BTreeMap<PluginId, LegacyWorker>,
    replies: VecDeque<LegacyResponse>,
    failures: Vec<(PluginId, String)>,
}

impl LegacyWorkerPool {
    fn insert(&mut self, plugin: PluginId, worker: LegacyWorker) {
        self.workers.insert(plugin, worker);
    }

    /// Removes every reply the workers have produced since the last drain. Taken
    /// out wholesale so the borrow of the pool ends before the runtime re-enters
    /// it during delivery.
    fn drain_replies(&mut self) -> VecDeque<LegacyResponse> {
        std::mem::take(&mut self.replies)
    }
}

impl LegacyWorkerHandle for LegacyWorkerPool {
    fn dispatch(&mut self, _at_ms: Millis, request: &LegacyRequest) -> Result<(), WorkerError> {
        let Some(worker) = self.workers.get_mut(&request.plugin) else {
            // Only registered plugins are ever dispatched, and each has a
            // worker, so this is a should-never-happen guarded by an
            // attributable error rather than a silent success that would wedge
            // the instance waiting for a reply that can never come.
            return Err(WorkerError::Io {
                plugin: Some(request.plugin.clone()),
                operation: format!("dispatching {:?} to a legacy worker", request.callback()),
                message: "no live worker is registered for this plugin".to_owned(),
            });
        };
        // The callback genuinely runs in the child process here (spec 4.2).
        match worker.call(request.clone()) {
            Ok(response) => {
                self.replies.push_back(response);
                Ok(())
            }
            Err(error) => {
                // A crashed or unresponsive worker is contained: the runtime
                // frees the instance on this `Err`, and the failure is recorded
                // so a diagnostic can name the plugin (spec 24.1, 26.2).
                self.failures.push((request.plugin.clone(), error.to_string()));
                Err(error)
            }
        }
    }

    fn request_termination(
        &mut self,
        _at_ms: Millis,
        plugin: &PluginId,
        _instance: InstanceId,
        _generation: Generation,
        _reason: TerminationReason,
    ) -> Result<(), WorkerError> {
        // Raising the cooperative flag is the host's half of spec 9.2; honouring
        // it is the plugin's. Advisory and non-blocking.
        if let Some(worker) = self.workers.get(plugin) {
            worker.terminate_handle().signal();
        }
        Ok(())
    }

    fn lower_termination(
        &mut self,
        _at_ms: Millis,
        plugin: &PluginId,
        _instance: InstanceId,
        _generation: Generation,
    ) -> Result<(), WorkerError> {
        // The child's terminate flag is sticky per process, so fresh,
        // non-obsolete work must lower it before dispatch; otherwise a raise
        // from an earlier superseded generation would make every later callback
        // abandon (Finding 1; spec 9.2, 9.5, acceptance 31.17). `clear` swaps
        // the shared host atomic and writes the authoritative
        // `set_terminate:false` frame under the stdin lock. Advisory and
        // non-blocking, mirroring `request_termination`.
        if let Some(worker) = self.workers.get(plugin) {
            worker.terminate_handle().clear();
        }
        Ok(())
    }

    fn stop(&mut self, _at_ms: Millis, _budget_ms: Millis) -> Result<(), WorkerError> {
        for (_, worker) in std::mem::take(&mut self.workers) {
            // A tidy teardown is best effort: the child is reaped on drop even
            // if the orderly shutdown reports an error, so no worker is leaked.
            let _ = worker.shutdown();
        }
        Ok(())
    }
}
/// Composes legacy discovery, the legacy runtime and the app's query pipeline.
///
/// Constructed once at startup with [`LegacyProvider::load`], then driven per
/// keystroke with [`LegacyProvider::drive_query`]. It owns the runtime (and
/// therefore, through the pool, every child process) so that dropping the
/// provider tears down every legacy worker.
#[derive(Debug)]
pub struct LegacyProvider {
    runtime: LegacyRuntime<LegacyWorkerPool>,
    plugins: Vec<PluginId>,
    unavailable: Vec<LegacyUnavailable>,
}

impl LegacyProvider {
    /// Discovers legacy packages under `roots`, starts a worker for each usable
    /// one, and registers it with both the legacy runtime and `pipeline`.
    ///
    /// Never returns an error: a failure at any step becomes a recorded
    /// [`LegacyUnavailable`] and the remaining packages are loaded anyway
    /// (acceptance 31.9, 31.10). `cache_root` is where archive packages are
    /// extracted (spec 14.3); it is not created unless an archive is met.
    ///
    /// On return, every registered plugin's one-time `on_start` has completed,
    /// so the caller may truthfully advance
    /// [`StartupStage::LegacyPlugins`](crate::StartupStage::LegacyPlugins).
    pub fn load(
        pipeline: &mut QueryPipeline,
        roots: &[PathBuf],
        cache_root: PathBuf,
        deadlines: LegacyDeadlines,
    ) -> Self {
        let mut provider = Self {
            runtime: LegacyRuntime::new(LegacyWorkerPool::default(), deadlines),
            plugins: Vec::new(),
            unavailable: Vec::new(),
        };

        // A host that cannot run CPython cannot run any legacy plugin, and
        // saying so once is more honest than reporting every package as broken
        // for the host's reason (spec 14.11).
        let interpreter = match discover_interpreter(&RuntimeProfile::LegacyCompatibility) {
            Ok(interpreter) => interpreter,
            Err(error) => {
                provider.unavailable.push(LegacyUnavailable {
                    package: String::new(),
                    plugin: None,
                    reason: format!("no supported CPython for the legacy worker: {error}"),
                });
                return provider;
            }
        };

        // The shim must be on disk before any worker can speak the protocol.
        let shim = shim_root();
        if !shim.join(WORKER_ENTRY_FILE).is_file() {
            provider.unavailable.push(LegacyUnavailable {
                package: String::new(),
                plugin: None,
                reason: format!(
                    "the legacy worker entry `{WORKER_ENTRY_FILE}` is not in `{}`",
                    shim.display()
                ),
            });
            return provider;
        }

        let loader = PackageLoader::new(cache_root);
        let packages = match loader.discover(roots) {
            Ok(packages) => packages,
            Err(error) => {
                provider.unavailable.push(LegacyUnavailable {
                    package: String::new(),
                    plugin: None,
                    reason: format!("legacy package discovery failed: {error}"),
                });
                Vec::new()
            }
        };

        for package in &packages {
            provider.register_package(pipeline, &interpreter, &shim, package);
        }

        // Run the one-time `on_start` for every registered instance before any
        // query can arrive: `on_suggest` is serialized behind it (spec 14.8),
        // and a plugin that reads its settings in `on_start` would otherwise see
        // an unconfigured first query.
        provider.runtime.tick(0);
        provider.settle(0);

        provider
    }

    fn register_package(
        &mut self,
        pipeline: &mut QueryPipeline,
        interpreter: &Interpreter,
        shim: &std::path::Path,
        package: &LegacyPackage,
    ) {
        let plugin = plugin_of(package);
        let options = WorkerOptions::new(plugin.clone(), shim.to_path_buf())
            .with_startup_timeout_ms(STARTUP_BUDGET_MS)
            .with_call_timeout_ms(CALL_BUDGET_MS);

        let worker = match LegacyWorker::spawn(interpreter, package, options) {
            Ok(worker) => worker,
            Err(error) => {
                self.unavailable.push(LegacyUnavailable {
                    package: package.id.to_string(),
                    plugin: Some(plugin),
                    reason: format!("the legacy worker did not start: {error}"),
                });
                return;
            }
        };

        // Reuse the pipeline's own `legacy-strict` policy rather than inventing
        // a second scheduling path: this is exactly what the pipeline derives
        // for a resolved legacy manifest (spec 7.1, 14.5).
        if let Err(error) = pipeline.register_plugin(plugin.clone(), PluginPolicy::legacy_strict()) {
            self.unavailable.push(LegacyUnavailable {
                package: package.id.to_string(),
                plugin: Some(plugin),
                reason: format!("the query pipeline refused the legacy plugin: {error:?}"),
            });
            // The worker was spawned; reap it rather than leak a child for a
            // plugin the pipeline will never dispatch.
            let _ = worker.shutdown();
            return;
        }

        self.runtime.worker_mut().insert(plugin.clone(), worker);
        self.runtime.register(plugin.clone(), package.id.clone());
        self.plugins.push(plugin);
    }

    /// The legacy plugins that loaded and are being served through the pipeline.
    pub fn plugins(&self) -> &[PluginId] {
        &self.plugins
    }

    /// Packages that could not be served, each with an attributable reason.
    pub fn unavailable(&self) -> &[LegacyUnavailable] {
        &self.unavailable
    }

    /// Runtime dispatch failures observed since startup (a worker that crashed
    /// while answering a live query), newest last. Distinct from
    /// [`Self::unavailable`], which is load-time only.
    pub fn dispatch_failures(&self) -> &[(PluginId, String)] {
        &self.runtime.worker().failures
    }

    /// Drives one query end to end and returns the pipeline frame it produced.
    ///
    /// The call path is: `keystroke` mints the pipeline generation; the legacy
    /// runtime is driven for the same query text to compute each plugin's
    /// suggestions in its child process; `tick` dispatches the registered legacy
    /// plugins; each plugin's suggestions are delivered as a [`ResultBatch`]
    /// under the pipeline generation and the request is completed; `present`
    /// drains intake, ranks and coalesces one frame. Stale answers are refused
    /// at the pipeline's intake boundary because only the current generation is
    /// ever delivered.
    ///
    /// Returns `None` when the pipeline reported an error at any stage or the
    /// frame belonged to a superseded generation, so a caller never publishes a
    /// stale or partial legacy frame.
    pub fn drive_query(
        &mut self,
        pipeline: &mut QueryPipeline,
        query: &str,
        now: Millis,
    ) -> Option<ViewModel> {
        let generation = pipeline.keystroke(query, now);
        let mut suggestions = self.collect_suggestions(query, now);

        let tick = pipeline.tick(now);
        let tick_succeeded = tick.errors.is_empty();
        for cancellation in tick.cancellations {
            let _ = pipeline.complete(&cancellation.plugin, cancellation.generation, now);
        }

        let mut delivered = true;
        for request in tick.dispatches {
            if request.generation != generation {
                let _ = pipeline.complete(&request.plugin, request.generation, now);
                continue;
            }
            let items = suggestions.remove(&request.plugin).unwrap_or_default();
            let admitted = pipeline
                .deliver(
                    ResultBatch {
                        generation: request.generation,
                        plugin: request.plugin.clone(),
                        state: BatchState::Final,
                        items,
                    },
                    now,
                )
                .is_ok();
            delivered &= admitted;
            let _ = pipeline.complete(&request.plugin, request.generation, now);
        }

        let frame = pipeline.present(now);
        let presentation_succeeded = pipeline.take_errors().is_empty();
        if !tick_succeeded || !delivered || !presentation_succeeded {
            return None;
        }
        frame.filter(|frame| frame.generation == generation)
    }

    /// Cooperative teardown of every legacy worker (spec 9.6, 24.3).
    pub fn shutdown(&mut self, now: Millis) {
        let _ = self.runtime.shutdown(now);
    }

    /// Runs the legacy runtime for one query and groups the resulting visible
    /// items by their owning plugin.
    fn collect_suggestions(&mut self, query: &str, now: Millis) -> BTreeMap<PluginId, Vec<Item>> {
        // A plain query with nothing selected broadcasts to every loaded legacy
        // plugin (spec 14.5): no length floor, no prefix or keyword gating, no
        // debounce (acceptance 31.14, 31.15).
        self.runtime.submit_query(query, now);
        self.runtime.tick(now);
        self.settle(now);

        let mut by_plugin: BTreeMap<PluginId, Vec<Item>> = BTreeMap::new();
        for item in self.runtime.visible_items() {
            by_plugin
                .entry(item.plugin_id.clone())
                .or_default()
                .push(item.clone());
        }
        by_plugin
    }

    /// Feeds every buffered child reply back into the runtime at `now`.
    fn settle(&mut self, now: Millis) {
        let replies = self.runtime.worker_mut().drain_replies();
        for response in replies {
            let _ = self.runtime.deliver(response, now);
        }
    }
}

/// One query handed to the legacy supervisor thread, tagged with the search
/// generation it belongs to.
#[derive(Debug)]
struct LegacyJob {
    generation: Generation,
    query: String,
    now: Millis,
    /// The built-in provider's rows for this generation. Prepended to the
    /// legacy rows so the merged frame keeps the built-in path's ordering.
    builtin_rows: Vec<ResultRow>,
    builtin_pending: bool,
    selected: usize,
}

/// The supervisor's request mailbox: a single slot with replace-oldest
/// overflow. A newer query overwrites an un-started one, which is exactly
/// `QueuePolicy::ReplaceOldest` with `queue_capacity` 1 — the policy the
/// built-in provider already runs under — so a slow legacy plugin never delays
/// a fast keystroke (acceptance 31.8) and the channel is bounded by
/// construction (never unbounded).
#[derive(Debug)]
struct RequestSlot {
    job: Option<LegacyJob>,
    stop: bool,
}

/// Drives [`LegacyProvider::drive_query`] on a dedicated supervisor thread so
/// the user-interface thread never blocks on a child interpreter (spec 6.5;
/// acceptance 31.1, 31.8).
///
/// The UI thread [`submit`](Self::submit)s a query and returns at once. The
/// supervisor drives the legacy pipeline — where a superseded answer is refused
/// at the intake boundary because `drive_query` only ever delivers the current
/// generation — merges the resulting rows behind the built-in rows, and
/// publishes the frame two ways: through the `publish` callback (which the
/// composition root forwards straight to the renderer, so a late answer shows
/// without waiting for the next keystroke) and into a single-slot outcome
/// mailbox the UI thread folds into its retained view model with
/// [`take_outcome`](Self::take_outcome).
///
/// A late answer never appears under a newer generation: the frame is tagged
/// with the search generation it was submitted under (never relabelled), the
/// supervisor drops it if the UI has already moved on (the `current` atomic),
/// and the view model's own `publish` refuses any generation that is not the
/// live one.
///
/// Every failure stays contained exactly as before: `drive_query` degrades a
/// crash, timeout or missing worker to a recorded diagnostic and an empty
/// answer, so the supervisor thread never panics and never aborts startup.
#[derive(Debug)]
pub struct LegacyDriver {
    mailbox: Arc<(Mutex<RequestSlot>, Condvar)>,
    outcome: Arc<Mutex<Option<ViewModel>>>,
    /// Search generation the UI last submitted. The supervisor re-reads it
    /// before publishing and drops any answer that is no longer current.
    current: Arc<AtomicU64>,
    has_plugins: bool,
    worker: Option<JoinHandle<()>>,
}

impl LegacyDriver {
    /// Moves `provider` and its `pipeline` onto a supervisor thread and returns
    /// a handle the UI thread drives without ever blocking.
    ///
    /// `publish` runs on the supervisor thread with each merged frame. A thread
    /// that fails to spawn (only under resource exhaustion) degrades to an
    /// inert driver rather than a panic: `provider` is dropped, reaping every
    /// child, and [`has_plugins`](Self::has_plugins) reports false so the UI
    /// simply serves no legacy rows.
    pub fn spawn<P>(mut provider: LegacyProvider, mut pipeline: QueryPipeline, publish: P) -> Self
    where
        P: Fn(&ViewModel) + Send + 'static,
    {
        let has_plugins = !provider.plugins().is_empty();
        let mailbox = Arc::new((
            Mutex::new(RequestSlot {
                job: None,
                stop: false,
            }),
            Condvar::new(),
        ));
        let outcome = Arc::new(Mutex::new(None));
        let current = Arc::new(AtomicU64::new(0));

        let thread_mailbox = Arc::clone(&mailbox);
        let thread_outcome = Arc::clone(&outcome);
        let thread_current = Arc::clone(&current);
        let spawned = std::thread::Builder::new()
            .name("crikey-legacy".to_owned())
            .spawn(move || {
                let (lock, cvar) = &*thread_mailbox;
                let mut last_now: Millis = 0;
                loop {
                    let job = {
                        let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
                        loop {
                            if slot.stop {
                                // Cooperative teardown of every child before the
                                // thread exits, so no worker is leaked (spec 9.6,
                                // 24.3); dropping `provider` reaps whatever an
                                // orderly shutdown could not.
                                drop(slot);
                                provider.shutdown(last_now);
                                return;
                            }
                            if let Some(job) = slot.job.take() {
                                break job;
                            }
                            slot = cvar.wait(slot).unwrap_or_else(|error| error.into_inner());
                        }
                    };
                    last_now = job.now;

                    // The blocking child interpreter call happens here, on this
                    // thread — never on the caller's. Stale answers are refused
                    // at the pipeline's intake boundary inside `drive_query`.
                    let legacy = provider.drive_query(&mut pipeline, &job.query, job.now);

                    // Tag the frame with the search generation from the job, not
                    // the legacy pipeline's own counter: coalesced (superseded)
                    // queries are dropped at the mailbox, so that counter is not
                    // in lockstep with the generation the UI published under.
                    let mut rows = job.builtin_rows;
                    let mut pending = job.builtin_pending;
                    if let Some(frame) = legacy {
                        rows.extend(frame.rows.iter().cloned());
                        pending |= frame.pending_plugins;
                    }
                    let merged = ViewModel {
                        generation: job.generation,
                        query: job.query,
                        rows: rows.into(),
                        selected: job.selected,
                        pending_plugins: pending,
                        actions_open: false,
                    };

                    // A late answer must never appear under a newer generation:
                    // once the UI has moved on, drop this one unpublished.
                    if thread_current.load(Ordering::Acquire) != job.generation.get() {
                        continue;
                    }
                    *thread_outcome.lock().unwrap_or_else(|error| error.into_inner()) = Some(merged.clone());
                    publish(&merged);
                }
            });

        match spawned {
            Ok(worker) => Self {
                mailbox,
                outcome,
                current,
                has_plugins,
                worker: Some(worker),
            },
            Err(_) => Self {
                mailbox,
                outcome,
                current,
                has_plugins: false,
                worker: None,
            },
        }
    }

    /// Submits a query for asynchronous legacy processing and returns at once;
    /// the UI thread never waits on a plugin (spec 6.5, acceptance 31.1).
    ///
    /// `builtin_rows` are the built-in provider's rows for `generation`, which
    /// the merged frame keeps ahead of the legacy rows.
    pub fn submit(
        &self,
        generation: Generation,
        query: &str,
        now: Millis,
        builtin_rows: Vec<ResultRow>,
        builtin_pending: bool,
        selected: usize,
    ) {
        // Record the live generation first, so an answer for a query this call
        // supersedes is dropped rather than shown even if it finishes late.
        self.current.store(generation.get(), Ordering::Release);
        let (lock, cvar) = &*self.mailbox;
        let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
        if slot.stop {
            return;
        }
        slot.job = Some(LegacyJob {
            generation,
            query: query.to_owned(),
            now,
            builtin_rows,
            builtin_pending,
            selected,
        });
        drop(slot);
        cvar.notify_one();
    }

    /// Takes the latest merged frame the supervisor produced, if any, for the
    /// UI thread to fold into its retained view model so later navigation keeps
    /// the legacy rows. Single slot, replace-oldest: only the newest matters.
    pub fn take_outcome(&self) -> Option<ViewModel> {
        self.outcome
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    /// Whether any legacy plugin loaded and is being served by the supervisor.
    pub fn has_plugins(&self) -> bool {
        self.has_plugins
    }
}

impl Drop for LegacyDriver {
    fn drop(&mut self) {
        // Signal shutdown and join, so the supervisor thread and every child it
        // owns are torn down with the launcher — no thread leak.
        {
            let (lock, cvar) = &*self.mailbox;
            let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
            slot.stop = true;
            drop(slot);
            cvar.notify_one();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The host plugin id a legacy package answers as, matching the developer
/// commands' spelling so a package inspected with `crikey dev` and one served
/// by the launcher share one identity.
fn plugin_of(package: &LegacyPackage) -> PluginId {
    PluginId(format!("legacy.{}", package.id))
}
