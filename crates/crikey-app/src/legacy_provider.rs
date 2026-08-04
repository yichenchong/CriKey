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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crikey_core::{Action, ActionId, ArgumentPolicy, ExecutionPolicy, Generation, Item, ItemId, PluginId};
use crikey_input_scheduler::{Millis, PluginPolicy};
use crikey_legacy_compat::{
    discover_interpreter, shim_root, InstanceId, Interpreter, LegacyDeadlines, LegacyOutcome, LegacyPackage,
    LegacyRequest, LegacyRequestKind, LegacyResponse, LegacyRuntime, LegacyWorker, LegacyWorkerHandle,
    PackageLoader, TerminateHandle, TerminationReason, WorkerError, WorkerOptions, WORKER_ENTRY_FILE,
};
use crikey_plugin_model::ConcurrencySection;
use crikey_plugin_supervisor::{
    shared_budget_from_section, BudgetKind, OwnedBudgetGuard, PluginBudgetHandle, PluginHealth,
};
use crikey_python_host::RuntimeProfile;
use crikey_ui::{ResultRow, ViewModel};

use crate::{
    ActionRequestId, BatchState, CatalogBuild, CatalogBuildResult, CatalogDispatchError, DisabledPlugins,
    PluginActionCompletion, QueryPipeline, ResultBatch, DISABLED_BY_CONFIGURATION,
};

/// Bound on the startup handshake with a child interpreter, in milliseconds.
/// A liveness guard: a shim that never answers becomes a recorded unavailable
/// plugin rather than a launcher that hangs on startup.
const STARTUP_BUDGET_MS: Millis = 30_000;

/// Bound on one legacy callback, in milliseconds. Generous because spec 9.6
/// permits a slow legacy callback and forbids killing the worker merely for
/// being slow; the cooperative ladder in [`LegacyRuntime`] is what reacts to a
/// long callback, not this transport budget.
const CALL_BUDGET_MS: Millis = 120_000;

/// Catalog rebuilds that may be queued before the supervisor thread runs them.
const CATALOG_REQUEST_CAPACITY: usize = 64;

/// Dispatch rounds one catalog drive may spend before giving up.
///
/// Legacy callbacks are serialized per instance (§14.5), so a rebuild
/// requested while another callback is running is dispatched only on a later
/// tick. The bound is what stops a plugin that keeps generating work from
/// wedging the supervisor thread; it is not a deadline on the plugin.
const CATALOG_PUMP_ROUNDS: usize = 8;

/// Admitted-but-unstarted legacy actions the UI thread may queue.
const ACTION_QUEUE_CAPACITY: usize = 8;

/// Completions retained for the UI thread before the oldest is refused.
const ACTION_COMPLETION_CAPACITY: usize = 64;

/// Actions admitted but not yet retired across every legacy plugin.
const ACTION_IN_FLIGHT_CAPACITY: usize = 32;

/// Out-of-band cooperative termination state for one legacy callback.
///
/// A newer query can arrive while the provider thread is blocked in
/// `LegacyWorker::call`. The watcher performs the control-frame write so the
/// submitting thread only latches a flag and never waits on the child pipe.
#[derive(Debug)]
struct LegacyCallControl {
    requested: AtomicBool,
    finished: AtomicBool,
    handle: Mutex<Option<TerminateHandle>>,
    wake: Condvar,
    wake_lock: Mutex<()>,
}

impl LegacyCallControl {
    fn new() -> Self {
        Self {
            requested: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            handle: Mutex::new(None),
            wake: Condvar::new(),
            wake_lock: Mutex::new(()),
        }
    }

    fn install(&self, handle: TerminateHandle) {
        let signal_now = self.requested.load(Ordering::Acquire);
        *self.handle.lock().unwrap_or_else(|error| error.into_inner()) = Some(handle.clone());
        if signal_now {
            handle.signal();
        }
        self.wake.notify_all();
    }

    fn signal(&self) {
        self.requested.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    fn watch_signal(self: Arc<Self>) {
        let mut wake = self.wake_lock.lock().unwrap_or_else(|error| error.into_inner());
        while !self.requested.load(Ordering::Acquire) && !self.finished.load(Ordering::Acquire) {
            wake = self.wake.wait(wake).unwrap_or_else(|error| error.into_inner());
        }
        if self.requested.load(Ordering::Acquire) && !self.finished.load(Ordering::Acquire) {
            if let Some(handle) = self
                .handle
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                .cloned()
            {
                handle.signal();
            }
        }
    }

    fn finish(&self) {
        let _wake = self.wake_lock.lock().unwrap_or_else(|error| error.into_inner());
        self.finished.store(true, Ordering::Release);
        self.wake.notify_all();
    }
}

#[derive(Debug, Default)]
struct LegacyCancellation {
    calls: Mutex<BTreeMap<PluginId, Arc<LegacyCallControl>>>,
}

impl LegacyCancellation {
    fn register(&self, plugin: PluginId, control: Arc<LegacyCallControl>) {
        self.calls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(plugin, control);
    }

    fn unregister(&self, plugin: &PluginId) {
        self.calls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(plugin);
    }

    fn signal_all(&self) {
        let controls = self
            .calls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for control in controls {
            control.signal();
        }
    }
}

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
    dead: BTreeSet<PluginId>,
    cancellation: Arc<LegacyCancellation>,
}

impl LegacyWorkerPool {
    fn insert(&mut self, plugin: PluginId, worker: LegacyWorker) {
        self.workers.insert(plugin, worker);
    }

    fn record_failure(&mut self, plugin: PluginId, reason: String) {
        if self.dead.insert(plugin.clone()) {
            self.failures.push((plugin, reason));
        }
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
            let error = WorkerError::Io {
                plugin: Some(request.plugin.clone()),
                operation: format!("dispatching {:?} to a legacy worker", request.callback()),
                message: "no live worker is registered for this plugin".to_owned(),
            };
            self.record_failure(request.plugin.clone(), error.to_string());
            return Err(error);
        };
        let control = Arc::new(LegacyCallControl::new());
        self.cancellation
            .register(request.plugin.clone(), Arc::clone(&control));
        let control_for_thread = Arc::clone(&control);
        let handle = worker.terminate_handle();
        control.install(handle);
        let watcher = thread::Builder::new()
            .name(format!("crikey-legacy-terminate-{}", request.plugin.0))
            .spawn(move || control_for_thread.watch_signal());
        // The callback genuinely runs in the child process here (spec 4.2).
        let result = worker.call(request.clone());
        control.finish();
        if let Ok(watcher) = watcher {
            let _ = watcher.join();
        }
        self.cancellation.unregister(&request.plugin);
        if let Err(error) = &result {
            // A crashed or unresponsive worker is contained: the runtime
            // frees the instance on this `Err`, and the failure is recorded
            // once so a diagnostic can name the plugin (spec 24.1, 26.2).
            self.workers.remove(&request.plugin);
            self.record_failure(request.plugin.clone(), error.to_string());
        }
        match result {
            Ok(response) => {
                self.replies.push_back(response);
                Ok(())
            }
            Err(error) => Err(error),
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
    budgets: BTreeMap<PluginId, PluginBudgetHandle>,
    unavailable: Vec<LegacyUnavailable>,
    cancellation: Arc<LegacyCancellation>,
    /// Catalog rebuilds admitted against the §13.5 catalog budget but not yet
    /// dispatched. Held here rather than started at request time because the
    /// callback blocks on a child interpreter and must run on the supervisor
    /// thread, serialized with every other callback for that instance (§14.5).
    catalog_requests: Vec<LegacyCatalogTask>,
    /// The suggestion items currently on offer, by exact owner. An action is
    /// executed against this snapshot, never against the item the UI echoed
    /// back, so a superseded row cannot launch a plugin callback.
    action_items: Arc<Mutex<BTreeMap<(PluginId, ItemId), Item>>>,
    /// Catalog items each plugin published. Kept apart from the suggestion
    /// snapshot because a catalog outlives the query that was on screen when
    /// it was built; clearing it per keystroke would make catalog rows
    /// unlaunchable a moment after they appeared.
    catalog_items: Arc<Mutex<BTreeMap<(PluginId, ItemId), Item>>>,
}

/// One admitted, not-yet-dispatched legacy catalog rebuild.
#[derive(Debug)]
struct LegacyCatalogTask {
    plugin: PluginId,
    instance: u64,
    generation: Generation,
    /// Held for the whole rebuild, not merely for its admission: the declared
    /// `max-catalog-tasks` bounds concurrent builds, so releasing the slot at
    /// request time would bound nothing.
    _guard: OwnedBudgetGuard,
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
        disabled: &DisabledPlugins,
    ) -> Self {
        let cancellation = Arc::new(LegacyCancellation::default());
        let mut provider = Self {
            runtime: LegacyRuntime::new(
                LegacyWorkerPool {
                    cancellation: Arc::clone(&cancellation),
                    ..LegacyWorkerPool::default()
                },
                deadlines,
            ),
            plugins: Vec::new(),
            budgets: BTreeMap::new(),
            unavailable: Vec::new(),
            cancellation,
            catalog_requests: Vec::new(),
            action_items: Arc::new(Mutex::new(BTreeMap::new())),
            catalog_items: Arc::new(Mutex::new(BTreeMap::new())),
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
            provider.register_package(pipeline, &interpreter, &shim, package, disabled);
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
        disabled: &DisabledPlugins,
    ) {
        let plugin = plugin_of(package);
        // Held back before the child interpreter is spawned: an operator who
        // disabled a plugin must not pay for its process, and the only proof
        // that it did not run is that nothing started it (spec 21.2).
        if disabled.blocks(&plugin) {
            self.unavailable.push(LegacyUnavailable {
                package: package.id.to_string(),
                plugin: Some(plugin),
                reason: DISABLED_BY_CONFIGURATION.to_owned(),
            });
            return;
        }
        // Legacy manifests use the strict host policy; its independent
        // concurrency declaration defaults to one suggestion slot. Create the
        // provider-owned handle before starting the worker, then pass the same
        // Arc into the pipeline registration.
        let budget = shared_budget_from_section(&ConcurrencySection::default());
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
        if let Err(error) = pipeline.register_plugin_with_budget_default_intake(
            plugin.clone(),
            PluginPolicy::legacy_strict(),
            budget.clone(),
        ) {
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
        self.budgets.insert(plugin.clone(), budget);
        self.plugins.push(plugin);
    }

    /// The legacy plugins that loaded and are being served through the pipeline.
    pub fn plugins(&self) -> &[PluginId] {
        &self.plugins
    }

    /// Returns the shared budget retained for a loaded legacy plugin.
    pub fn plugin_budget(&self, plugin: &PluginId) -> Option<&PluginBudgetHandle> {
        self.budgets.get(plugin)
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

    /// Admits one catalog rebuild for an exactly owned legacy plugin.
    ///
    /// Admission happens here and dispatch in [`Self::drive_catalogs`]: the
    /// `on_catalog` callback runs in the child interpreter and must not be
    /// started on the caller's thread. The §13.5 catalog slot is claimed at
    /// admission and held until the build retires.
    pub fn request_catalog_build(
        &mut self,
        plugin: &PluginId,
        instance: u64,
        generation: Generation,
    ) -> Result<(), CatalogDispatchError> {
        let budget = self
            .budgets
            .get(plugin)
            .ok_or_else(|| CatalogDispatchError::UnknownPlugin {
                plugin: plugin.clone(),
            })?;
        if self.catalog_requests.len() >= CATALOG_REQUEST_CAPACITY {
            return Err(CatalogDispatchError::QueueFull {
                plugin: plugin.clone(),
            });
        }
        let guard = budget.try_acquire_owned(BudgetKind::Catalog).ok_or_else(|| {
            CatalogDispatchError::BudgetRefused {
                plugin: plugin.clone(),
            }
        })?;
        self.catalog_requests.push(LegacyCatalogTask {
            plugin: plugin.clone(),
            instance,
            generation,
            _guard: guard,
        });
        Ok(())
    }

    /// Runs every admitted catalog rebuild and returns their outcomes.
    ///
    /// Must be called on the supervisor thread: `on_catalog` executes in the
    /// child interpreter. A rebuild carries no query generation, so it is
    /// never subject to query staleness (spec 14.8); the `instance` and
    /// `generation` echoed back are the caller's own publication tags.
    pub fn drive_catalogs(&mut self, now: Millis) -> Vec<CatalogBuildResult> {
        let tasks = std::mem::take(&mut self.catalog_requests);
        if tasks.is_empty() {
            return Vec::new();
        }

        let mut results = Vec::with_capacity(tasks.len());
        let mut accepted = Vec::with_capacity(tasks.len());
        for task in tasks {
            match self.runtime.catalog_rebuild(&task.plugin, now) {
                Ok(()) => accepted.push(task),
                Err(error) => results.push(CatalogBuildResult::Failed {
                    plugin: task.plugin.clone(),
                    instance: task.instance,
                    generation: task.generation,
                    reason: format!("the legacy runtime refused a catalog rebuild: {error}"),
                }),
            }
        }

        // One tick is not enough in general: a rebuild requested while another
        // callback is running is queued behind it and dispatched on a later
        // tick, because no two callbacks may run concurrently on one instance.
        for _ in 0..CATALOG_PUMP_ROUNDS {
            let dispatched = self.runtime.tick(now);
            self.settle(now);
            if dispatched.is_empty() {
                break;
            }
        }

        for task in accepted {
            if self.runtime.worker().dead.contains(&task.plugin) {
                results.push(CatalogBuildResult::Failed {
                    plugin: task.plugin.clone(),
                    instance: task.instance,
                    generation: task.generation,
                    reason: "the legacy worker died during the catalog build".to_owned(),
                });
                continue;
            }
            let items = self
                .runtime
                .catalog(&task.plugin)
                .iter()
                .cloned()
                .map(with_default_action)
                .collect::<Vec<_>>();
            // A catalog row must stay launchable for as long as it is on
            // screen, which outlasts the query that was live when it was built.
            let mut published = self
                .catalog_items
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            published.retain(|(owner, _), _| owner != &task.plugin);
            for item in &items {
                published.insert((task.plugin.clone(), item.stable_id.clone()), item.clone());
            }
            drop(published);
            results.push(CatalogBuildResult::Complete(CatalogBuild {
                plugin: task.plugin,
                instance: task.instance,
                generation: task.generation,
                items,
            }));
        }
        results
    }

    /// Clones the provider-owned budget handles for the action endpoint. Each
    /// clone is the exact `Arc` the query pipeline also holds, so an action and
    /// a suggestion contend for one plugin's declared budget rather than two.
    fn action_budgets(&self) -> BTreeMap<PluginId, PluginBudgetHandle> {
        self.budgets
            .iter()
            .map(|(plugin, budget)| (plugin.clone(), Arc::clone(budget)))
            .collect()
    }

    /// Runs one admitted action's `on_execute` in its owning child interpreter.
    ///
    /// Called only from the supervisor thread, which is also the only thread
    /// that drives queries and catalogs, so this callback can never overlap
    /// another callback on the same instance (spec 14.5).
    fn execute_action_request(&mut self, request: LegacyActionRequest) -> crikey_core::Result<()> {
        let _guard = request.guard;
        let plugin = request.plugin;
        let item = lookup_action_item(&self.action_items, &self.catalog_items, &plugin, &request.item_id)
            .ok_or_else(|| {
                crikey_core::CoreError::Invalid("legacy action item is no longer current".to_owned())
            })?;
        let action = item
            .actions
            .iter()
            .find(|action| action.action_id == request.action_id)
            .cloned()
            .ok_or_else(|| {
                crikey_core::CoreError::Invalid("selected action is no longer available".to_owned())
            })?;
        if action.execution_policy != ExecutionPolicy::Plugin {
            return Err(crikey_core::CoreError::Invalid(
                "legacy action request is not plugin-owned".to_owned(),
            ));
        }
        if !action.applicable_categories.is_empty() && !action.applicable_categories.contains(&item.category)
        {
            return Err(crikey_core::CoreError::Invalid(
                "legacy action is not applicable to the selected item category".to_owned(),
            ));
        }
        match item.argument_policy {
            ArgumentPolicy::Forbidden if request.argument.is_some() => {
                return Err(crikey_core::CoreError::Invalid(
                    "legacy action item forbids arguments".to_owned(),
                ));
            }
            ArgumentPolicy::Required if request.argument.as_deref().is_none_or(str::is_empty) => {
                return Err(crikey_core::CoreError::Invalid(
                    "legacy action item requires an argument".to_owned(),
                ));
            }
            ArgumentPolicy::Optional | ArgumentPolicy::Forbidden | ArgumentPolicy::Required => {}
        }

        let instance = self
            .runtime
            .instance_state(&plugin)
            .map_or(InstanceId(1), |state| state.instance);
        let call = LegacyRequest {
            plugin: plugin.clone(),
            instance,
            // `on_execute` is not query work, so it carries no generation and
            // is never refused as stale (spec 14.8).
            generation: Generation::ZERO,
            kind: LegacyRequestKind::Execute {
                item: Box::new(item),
                // The host-supplied default corresponds to Keypirinha's
                // "no secondary action chosen", which the protocol spells as
                // an absent action; a plugin distinguishes the two.
                action: (action.action_id.0 != LEGACY_EXECUTE_ACTION_ID).then_some(action),
            },
        };
        let Some(worker) = self.runtime.worker_mut().workers.get_mut(&plugin) else {
            return Err(crikey_core::CoreError::Invalid(format!(
                "no live legacy worker owns `{}`",
                plugin.0
            )));
        };
        let outcome = catch_unwind(AssertUnwindSafe(|| worker.call(call)));
        match outcome {
            Ok(Ok(response)) => match response.outcome {
                LegacyOutcome::Executed | LegacyOutcome::Acknowledged => Ok(()),
                LegacyOutcome::Failed(failure) => {
                    let reason = format!("legacy plugin action raised: {}", failure.message);
                    self.runtime
                        .worker_mut()
                        .record_failure(plugin.clone(), reason.clone());
                    Err(crikey_core::CoreError::Invalid(reason))
                }
                other => Err(crikey_core::CoreError::Invalid(format!(
                    "legacy plugin `{}` answered `on_execute` with {other:?}",
                    plugin.0
                ))),
            },
            Ok(Err(error)) => {
                // A transport fault reaps the child, exactly as a failed
                // suggestion dispatch does, so the plugin degrades to
                // unavailable rather than to a wedged endpoint.
                let reason = error.to_string();
                self.runtime.worker_mut().workers.remove(&plugin);
                self.runtime
                    .worker_mut()
                    .record_failure(plugin.clone(), reason.clone());
                Err(crikey_core::CoreError::Invalid(format!(
                    "legacy plugin `{}` action failed: {reason}",
                    plugin.0
                )))
            }
            Err(_) => {
                self.runtime
                    .worker_mut()
                    .record_failure(plugin.clone(), "legacy action dispatch panicked".to_owned());
                Err(crikey_core::CoreError::Invalid(format!(
                    "legacy plugin `{}` action worker panicked",
                    plugin.0
                )))
            }
        }
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
        let (mut suggestions, dead_plugins) = self.collect_suggestions(query, now);

        let tick = pipeline.tick(now);
        for cancellation in tick.cancellations {
            let _ = pipeline.complete(&cancellation.plugin, cancellation.generation, now);
        }

        for request in tick.dispatches {
            if request.generation != generation {
                let _ = pipeline.complete(&request.plugin, request.generation, now);
                continue;
            }
            if dead_plugins.contains(&request.plugin) {
                let _ = pipeline.abort_request(&request.plugin, request.generation, now);
                continue;
            }
            let items = suggestions.remove(&request.plugin).unwrap_or_default();
            let _ = pipeline.deliver(
                ResultBatch {
                    generation: request.generation,
                    plugin: request.plugin.clone(),
                    state: BatchState::Final,
                    items,
                },
                now,
            );
            let _ = pipeline.complete(&request.plugin, request.generation, now);
        }

        let frame = pipeline.present(now);
        frame.filter(|frame| frame.generation == generation)
    }

    /// Cooperative teardown of every legacy worker (spec 9.6, 24.3).
    pub fn shutdown(&mut self, now: Millis) {
        self.cancellation.signal_all();
        let _ = self.runtime.shutdown(now);
    }

    /// Runs the legacy runtime for one query and groups the resulting visible
    /// items by their owning plugin.
    fn collect_suggestions(
        &mut self,
        query: &str,
        now: Millis,
    ) -> (BTreeMap<PluginId, Vec<Item>>, BTreeSet<PluginId>) {
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
                .push(with_default_action(item.clone()));
        }
        // The snapshot an action is validated against. Replaced wholesale each
        // keystroke so a row that is no longer offered cannot be launched.
        {
            let mut items = self
                .action_items
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            items.clear();
            for (plugin, offered) in &by_plugin {
                for item in offered {
                    items.insert((plugin.clone(), item.stable_id.clone()), item.clone());
                }
            }
        }
        (by_plugin, self.runtime.worker().dead.clone())
    }

    /// Feeds every buffered child reply back into the runtime at `now`.
    fn settle(&mut self, now: Millis) {
        let replies = self.runtime.worker_mut().drain_replies();
        for response in replies {
            let _ = self.runtime.deliver(response, now);
        }
    }
}

/// Action id of the single plugin-owned action every legacy item carries.
const LEGACY_EXECUTE_ACTION_ID: &str = "legacy.execute";

/// Gives a legacy item the one action it can actually perform.
///
/// The legacy protocol carries no per-item action list: `decode_item` always
/// yields an empty one, because Keypirinha's actions are a package-level
/// concept the shim does not model. Without an action the presentation layer
/// has no default to run, so pressing Enter on a legacy row would do nothing
/// at all. The host therefore supplies the one action the contract does
/// define: hand the item back to its owning plugin's `on_execute` (spec 14.5).
fn with_default_action(mut item: Item) -> Item {
    if !item.actions.is_empty() {
        return item;
    }
    item.actions.push(Action {
        action_id: ActionId(LEGACY_EXECUTE_ACTION_ID.to_owned()),
        label: "Execute".to_owned(),
        description: "Hand this result back to the legacy plugin that produced it".to_owned(),
        // Legacy plugins classify their own items and the host must not second
        // guess the classification, so the action applies to every category.
        applicable_categories: Vec::new(),
        icon_reference: None,
        execution_policy: ExecutionPolicy::Plugin,
    });
    item
}

/// Finds the item an action names, preferring the live suggestion snapshot
/// over the plugin's published catalog.
///
/// Both maps are consulted because a legacy plugin surfaces rows two ways and
/// either can be on screen: suggestions answer the current query, while a
/// catalog row survives across keystrokes.
fn lookup_action_item(
    suggestions: &Mutex<BTreeMap<(PluginId, ItemId), Item>>,
    catalog: &Mutex<BTreeMap<(PluginId, ItemId), Item>>,
    plugin: &PluginId,
    item_id: &ItemId,
) -> Option<Item> {
    let key = (plugin.clone(), item_id.clone());
    if let Some(item) = suggestions
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&key)
        .cloned()
    {
        return Some(item);
    }
    catalog
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&key)
        .cloned()
}

/// One admitted legacy action handed to the supervisor thread.
#[derive(Debug)]
struct LegacyActionRequest {
    request_id: ActionRequestId,
    plugin: PluginId,
    item_id: ItemId,
    action_id: ActionId,
    argument: Option<String>,
    /// The §13.5 action slot, held until the callback retires.
    guard: OwnedBudgetGuard,
    deadline: Instant,
}

/// Work the legacy supervisor thread can be handed.
#[derive(Debug)]
enum LegacyWork {
    Query(LegacyJob),
    Action(Box<LegacyActionRequest>),
}

/// Bounded action endpoint retained by the live legacy driver.
///
/// Submission validates and admits only; the callback itself runs on the
/// supervisor thread, so the UI thread never waits on a child interpreter.
#[derive(Debug)]
struct LegacyActionEndpoint {
    sender: SyncSender<LegacyActionRequest>,
    completions: Arc<Mutex<VecDeque<PluginActionCompletion>>>,
    budgets: BTreeMap<PluginId, PluginBudgetHandle>,
    items: Arc<Mutex<BTreeMap<(PluginId, ItemId), Item>>>,
    catalog_items: Arc<Mutex<BTreeMap<(PluginId, ItemId), Item>>>,
    in_flight: Arc<AtomicUsize>,
    next_id: Arc<AtomicU64>,
    mailbox: Arc<(Mutex<RequestSlot>, Condvar)>,
}

impl crate::PluginActionExecutor for LegacyActionEndpoint {
    fn submit_plugin_action(
        &self,
        plugin: &PluginId,
        item: &Item,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> crikey_core::Result<ActionRequestId> {
        if item.plugin_id != *plugin {
            return Err(crikey_core::CoreError::Invalid(
                "selected legacy plugin result has stale ownership".to_owned(),
            ));
        }
        self.submit_plugin_action_by_id(plugin, &item.stable_id, action_id, argument)
    }

    fn poll_plugin_actions(&self) -> Vec<PluginActionCompletion> {
        self.completions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drain(..)
            .collect()
    }

    fn owns_item(&self, plugin: &PluginId, item_id: &ItemId) -> bool {
        lookup_action_item(&self.items, &self.catalog_items, plugin, item_id).is_some()
    }

    fn submit_plugin_action_by_id(
        &self,
        plugin: &PluginId,
        item_id: &ItemId,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> crikey_core::Result<ActionRequestId> {
        // The item is resolved from the provider's own snapshot, never from
        // what the caller echoed back, so a superseded row cannot reach a
        // plugin callback.
        let item =
            lookup_action_item(&self.items, &self.catalog_items, plugin, item_id).ok_or_else(|| {
                crikey_core::CoreError::Invalid(
                    "selected legacy plugin result is no longer current".to_owned(),
                )
            })?;
        if item.plugin_id != *plugin {
            return Err(crikey_core::CoreError::Invalid(
                "selected legacy plugin result has stale ownership".to_owned(),
            ));
        }
        let reserved = self
            .in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < ACTION_IN_FLIGHT_CAPACITY).then_some(current + 1)
            })
            .is_ok();
        if !reserved {
            return Err(crikey_core::CoreError::Invalid(
                "legacy action mailbox is full".to_owned(),
            ));
        }
        let Some(budget) = self.budgets.get(plugin) else {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return Err(crikey_core::CoreError::Invalid(format!(
                "no legacy action runtime owns plugin `{}`",
                plugin.0
            )));
        };
        let Some(guard) = budget.try_acquire_owned(BudgetKind::Action) else {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return Err(crikey_core::CoreError::Invalid(format!(
                "legacy plugin `{}` action budget is full",
                plugin.0
            )));
        };
        let sequence = self
            .next_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.wrapping_add(1).max(1))
            })
            .unwrap_or(1);
        let request_id = ActionRequestId {
            plugin: plugin.clone(),
            sequence,
        };
        let request = LegacyActionRequest {
            request_id: request_id.clone(),
            plugin: plugin.clone(),
            item_id: item.stable_id.clone(),
            action_id: action_id.clone(),
            argument: argument.map(str::to_owned),
            guard,
            deadline: Instant::now() + Duration::from_millis(CALL_BUDGET_MS),
        };
        if let Err(error) = self.sender.try_send(request) {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return Err(match error {
                mpsc::TrySendError::Full(_) => crikey_core::CoreError::Invalid(format!(
                    "legacy plugin `{}` action queue is full",
                    plugin.0
                )),
                mpsc::TrySendError::Disconnected(_) => crikey_core::CoreError::Invalid(format!(
                    "the legacy action supervisor for `{}` is gone",
                    plugin.0
                )),
            });
        }
        // Wake the supervisor: it parks on this condvar between keystrokes, so
        // an action submitted while the launcher is idle must not wait for the
        // next query to be dispatched.
        let (lock, cvar) = &*self.mailbox;
        drop(lock.lock().unwrap_or_else(|error| error.into_inner()));
        cvar.notify_one();
        Ok(request_id)
    }
}

/// Appends a terminal action outcome for the UI thread, refusing to grow the
/// mailbox without bound when nothing is draining it.
fn enqueue_legacy_completion(
    mailbox: &Mutex<VecDeque<PluginActionCompletion>>,
    completion: PluginActionCompletion,
) {
    let mut mailbox = mailbox.lock().unwrap_or_else(|error| error.into_inner());
    if mailbox.len() >= ACTION_COMPLETION_CAPACITY {
        eprintln!(
            "crikey: legacy action completion mailbox overflow for {} / {}",
            completion.plugin.0, completion.action_id.0
        );
        return;
    }
    mailbox.push_back(completion);
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
    action_endpoint: Arc<LegacyActionEndpoint>,
    catalog_results: Arc<Mutex<Vec<CatalogBuildResult>>>,
    /// Per-plugin diagnostics refreshed by the supervisor thread after every
    /// unit of work, so the UI thread can report a throttled plugin without
    /// reaching into the pipeline it does not own.
    health: Arc<Mutex<Vec<(PluginId, PluginHealth)>>>,
    outcome: Arc<Mutex<Option<ViewModel>>>,
    /// Search generation the UI last submitted. The supervisor re-reads it
    /// before publishing and drops any answer that is no longer current.
    current: Arc<AtomicU64>,
    cancellation: Arc<LegacyCancellation>,
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
    ///
    /// Any catalog rebuild already admitted with
    /// [`LegacyProvider::request_catalog_build`] is run once on the supervisor
    /// thread before the first query, so a legacy plugin's catalog exists by
    /// the time the launcher can be typed into.
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
        let catalog_results = Arc::new(Mutex::new(Vec::new()));
        let health = Arc::new(Mutex::new(Vec::new()));
        let current = Arc::new(AtomicU64::new(0));
        let cancellation = Arc::clone(&provider.cancellation);
        let (action_sender, action_receiver) = mpsc::sync_channel(ACTION_QUEUE_CAPACITY);
        let completion_mailbox = Arc::new(Mutex::new(VecDeque::with_capacity(ACTION_COMPLETION_CAPACITY)));
        let action_endpoint = Arc::new(LegacyActionEndpoint {
            sender: action_sender,
            completions: Arc::clone(&completion_mailbox),
            budgets: provider.action_budgets(),
            items: Arc::clone(&provider.action_items),
            catalog_items: Arc::clone(&provider.catalog_items),
            in_flight: Arc::new(AtomicUsize::new(0)),
            next_id: Arc::new(AtomicU64::new(0)),
            mailbox: Arc::clone(&mailbox),
        });

        let thread_mailbox = Arc::clone(&mailbox);
        let thread_catalog_results = Arc::clone(&catalog_results);
        let thread_health = Arc::clone(&health);
        let thread_outcome = Arc::clone(&outcome);
        let thread_current = Arc::clone(&current);
        let thread_completion_mailbox = Arc::clone(&completion_mailbox);
        let thread_in_flight = Arc::clone(&action_endpoint.in_flight);
        let spawned = std::thread::Builder::new()
            .name("crikey-legacy".to_owned())
            .spawn(move || {
                let (lock, cvar) = &*thread_mailbox;
                let mut last_now: Millis = 0;
                // Catalog rebuilds admitted before the thread existed run
                // first: `on_catalog` is a child-interpreter callback and this
                // is the only thread allowed to make one.
                let startup_catalogs = provider.drive_catalogs(0);
                if !startup_catalogs.is_empty() {
                    thread_catalog_results
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .extend(startup_catalogs);
                }
                *thread_health.lock().unwrap_or_else(|error| error.into_inner()) =
                    pipeline.plugin_health_report();
                loop {
                    let work = {
                        let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
                        loop {
                            if slot.stop {
                                // Every admitted action is retired, so no UI
                                // request is left waiting on a thread that is
                                // about to exit.
                                while let Ok(request) = action_receiver.try_recv() {
                                    thread_in_flight.fetch_sub(1, Ordering::AcqRel);
                                    enqueue_legacy_completion(
                                        &thread_completion_mailbox,
                                        PluginActionCompletion {
                                            request_id: request.request_id.clone(),
                                            plugin: request.plugin.clone(),
                                            item_id: request.item_id.clone(),
                                            action_id: request.action_id.clone(),
                                            outcome: Err(crikey_core::CoreError::Cancelled),
                                        },
                                    );
                                }
                                // Cooperative teardown of every child before the
                                // thread exits, so no worker is leaked (spec 9.6,
                                // 24.3); dropping `provider` reaps whatever an
                                // orderly shutdown could not.
                                drop(slot);
                                provider.shutdown(last_now);
                                return;
                            }
                            // An action is a user-initiated launch and outranks
                            // a queued keystroke, which the mailbox will still
                            // coalesce to the newest query anyway.
                            if let Ok(request) = action_receiver.try_recv() {
                                break LegacyWork::Action(Box::new(request));
                            }
                            if let Some(job) = slot.job.take() {
                                break LegacyWork::Query(job);
                            }
                            slot = cvar.wait(slot).unwrap_or_else(|error| error.into_inner());
                        }
                    };

                    let job = match work {
                        LegacyWork::Action(request) => {
                            let request_id = request.request_id.clone();
                            let plugin = request.plugin.clone();
                            let item_id = request.item_id.clone();
                            let action_id = request.action_id.clone();
                            let expired = Instant::now() >= request.deadline;
                            let outcome = if expired {
                                Err(crikey_core::CoreError::Invalid(format!(
                                    "legacy plugin `{}` action timed out before execution",
                                    plugin.0
                                )))
                            } else {
                                // `on_execute` runs here, serialized with every
                                // other callback this thread makes, so no two
                                // callbacks ever overlap on one instance
                                // (spec 14.5).
                                match catch_unwind(AssertUnwindSafe(|| {
                                    provider.execute_action_request(*request)
                                })) {
                                    Ok(result) => result,
                                    Err(_) => Err(crikey_core::CoreError::Invalid(format!(
                                        "legacy plugin `{}` action worker panicked",
                                        plugin.0
                                    ))),
                                }
                            };
                            thread_in_flight.fetch_sub(1, Ordering::AcqRel);
                            enqueue_legacy_completion(
                                &thread_completion_mailbox,
                                PluginActionCompletion {
                                    request_id,
                                    plugin,
                                    item_id,
                                    action_id,
                                    outcome,
                                },
                            );
                            *thread_health.lock().unwrap_or_else(|error| error.into_inner()) =
                                pipeline.plugin_health_report();
                            continue;
                        }
                        LegacyWork::Query(job) => job,
                    };
                    last_now = job.now;

                    // The blocking child interpreter call happens here, on this
                    // thread — never on the caller's. Stale answers are refused
                    // at the pipeline's intake boundary inside `drive_query`.
                    let legacy = provider.drive_query(&mut pipeline, &job.query, job.now);
                    let catalogs = provider.drive_catalogs(job.now);
                    if !catalogs.is_empty() {
                        thread_catalog_results
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .extend(catalogs);
                    }
                    *thread_health.lock().unwrap_or_else(|error| error.into_inner()) =
                        pipeline.plugin_health_report();

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

                    // Keep the staleness check, queued-job check, and
                    // publication atomic with respect to `submit`. Without
                    // the mailbox lock, a newer submission could arrive after
                    // the load below but before this frame is published.
                    let slot = lock.lock().unwrap_or_else(|error| error.into_inner());
                    if slot.stop
                        || slot.job.is_some()
                        || thread_current.load(Ordering::Acquire) != job.generation.get()
                    {
                        continue;
                    }
                    *thread_outcome.lock().unwrap_or_else(|error| error.into_inner()) = Some(merged.clone());
                    publish(&merged);
                    drop(slot);
                }
            });

        match spawned {
            Ok(worker) => Self {
                mailbox,
                action_endpoint,
                catalog_results,
                health,
                outcome,
                current,
                cancellation,
                has_plugins,
                worker: Some(worker),
            },
            Err(_) => Self {
                mailbox,
                action_endpoint,
                catalog_results,
                health,
                outcome,
                current,
                cancellation,
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
        // Keep intake monotonic even when a caller hands us a decoded
        // generation from an older request. Rewinding `current` would let an
        // obsolete job pass the publication gate.
        let generation_value = generation.get();
        let mut observed = self.current.load(Ordering::Acquire);
        loop {
            if generation_value < observed {
                return;
            }
            match self.current.compare_exchange_weak(
                observed,
                generation_value,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(next) => observed = next,
            }
        }
        // Signal obsolete callbacks before queueing the replacement. The
        // legacy contract is cooperative termination, not a hard cancellation.
        self.cancellation.signal_all();

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

    /// Takes the catalog outcomes the supervisor produced. Complete results
    /// stay instance/generation tagged for the caller's stale-safe catalog
    /// publication path, exactly as the modern and native drivers do.
    pub fn take_catalog_results(&self) -> Vec<CatalogBuildResult> {
        std::mem::take(
            &mut *self
                .catalog_results
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
    }

    /// The exact plugin ids owned by this driver's action endpoint.
    pub fn plugins(&self) -> Vec<PluginId> {
        self.action_endpoint.budgets.keys().cloned().collect()
    }

    /// The bounded action endpoint sharing this driver's per-plugin budget
    /// handles, for registration with [`crate::PluginActionRouter`].
    pub fn action_executor(&self) -> Arc<dyn crate::PluginActionExecutor> {
        self.action_endpoint.clone()
    }

    /// Per-plugin diagnostics (spec 24.3) as of the supervisor thread's last
    /// unit of work, including the per-kind §13.5 refusal counters.
    pub fn health_report(&self) -> Vec<(PluginId, PluginHealth)> {
        self.health
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

impl Drop for LegacyDriver {
    fn drop(&mut self) {
        // Raise cooperative termination before joining the supervisor so an
        // in-flight callback can return instead of delaying teardown.
        self.cancellation.signal_all();
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
