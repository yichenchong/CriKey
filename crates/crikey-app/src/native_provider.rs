//! Live native-plugin provider (contract §6; spec 16.1, 16.5, 16.6, 24.1;
//! acceptance 31.7, 31.8, 31.9, 31.21, 31.23).
//!
//! Native executables never run in the CriKey process. This module discovers
//! native packages, starts one supervised [`NativeWorker`] per package, and
//! drives those workers through the app's [`QueryPipeline`]. The provider is
//! the composition boundary between package manifests and the bounded,
//! generation-aware result intake used by the launcher.
//!
//! # Isolation
//!
//! Every native callback executes in the child owned by a [`NativeWorker`].
//! Workers are given the package directory as their working directory, and a
//! worker key includes that source directory even when two packages point at
//! the same executable. A plugin can therefore not accidentally answer for a
//! sibling package (spec 16.6; contract §11.1).
//!
//! # Containment
//!
//! Discovery, manifest parsing, entrypoint resolution, worker startup, and
//! runtime dispatch are all failure boundaries. Load-time failures become
//! [`NativeUnavailable`] diagnostics; a worker crash or later dispatch fault is
//! recorded once in `dispatch_failures` and contributes no rows. A healthy
//! sibling continues to serve, and teardown reaps every child (spec 24.1,
//! 24.3; acceptance 31.9, 31.10, 31.23).
//!
//! # Generation tagging
//!
//! [`NativeProvider::drive_query`] mints the pipeline generation and delivers
//! every native batch under that generation. [`NativeDriver`] tags each
//! asynchronous job with the generation submitted by the UI and refuses a
//! superseded answer before publishing it, so stale native rows never cross the
//! presentation boundary (spec 8.1; acceptance 31.7).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crikey_core::{ActionId, ArgumentPolicy, ExecutionPolicy, Generation, Item, ItemId, PluginId};
use crikey_input_scheduler::Millis;
use crikey_native_host::{
    CancelHandle, ExecuteOutcome, HostError, LaunchSpec, NativeSuggestRequest, NativeSupervisor, Suggestions,
    SupervisorConfig, WorkerOptions,
};
use crikey_plugin_model::{Manifest, Runtime};
use crikey_plugin_supervisor::{BudgetKind, OwnedBudgetGuard, PluginBudgetHandle};
use crikey_ui::{ResultRow, ViewModel};

use crate::{
    ActionRequestId, BatchState, CatalogBuildResult, CatalogDispatchError, ObsoleteCatalogBuild,
    PluginActionCompletion, QueryPipeline, ResultBatch,
};

/// Identifies a native worker by its executable and the package source
/// directory that supplied it. The source directory is intentionally part of
/// the key: two packages sharing an entrypoint still own separate processes.
type WorkerKey = (String, String);

/// One native package that could not be made available, and why.
///
/// This is a load-time diagnostic only. Runtime worker failures are reported by
/// [`NativeProvider::dispatch_failures`] (contract §11.5; spec 26.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeUnavailable {
    /// The package directory name as reported by discovery.
    pub package: String,
    /// The namespaced host identity once the manifest supplied one.
    pub plugin: Option<PluginId>,
    /// A single-line, attributable reason for the load refusal.
    pub reason: String,
}

/// One loaded native plugin and the worker key that serves it. The launch
/// recipe is retained so catalog requests can use an independent process.
#[derive(Debug, Clone)]
struct LoadedPlugin {
    plugin: PluginId,
    key: WorkerKey,
    launch: LaunchSpec,
    worker_options: WorkerOptions,
    budget: PluginBudgetHandle,
}

/// Maximum number of catalog tasks retained before the host drains results.
///
/// Admission counts active and completed-but-undrained tasks together, so the
/// synchronous result channel cannot back up behind a hidden UI.
const CATALOG_RESULT_CAPACITY: usize = 64;
/// Capacity of the bounded native suggestion-result channel. Dispatchers use
/// blocking send as backpressure; shutdown drains this channel while joining
/// calls so a full channel cannot strand a worker thread.
const NATIVE_RESULT_CAPACITY: usize = 64;

/// One native catalog build running away from the query worker.
#[derive(Debug)]
struct NativeCatalogTask {
    join: Option<JoinHandle<()>>,
}

/// Bounded catalog request/result mailbox.
///
/// Admission bounds the number of task threads and retained results. Each
/// admitted task emits at most one result, so a synchronous channel of the
/// same capacity never blocks a worker that is completing a legal task.
#[derive(Debug)]
struct NativeCatalogDispatcher {
    result_tx: SyncSender<(u64, CatalogBuildResult)>,
    result_rx: Receiver<(u64, CatalogBuildResult)>,
    tasks: BTreeMap<u64, NativeCatalogTask>,
    latest: BTreeMap<PluginId, u64>,
    next_id: u64,
}

impl Default for NativeCatalogDispatcher {
    fn default() -> Self {
        let (result_tx, result_rx) = mpsc::sync_channel(CATALOG_RESULT_CAPACITY);
        Self {
            result_tx,
            result_rx,
            tasks: BTreeMap::new(),
            latest: BTreeMap::new(),
            next_id: 0,
        }
    }
}

impl NativeCatalogDispatcher {
    fn next_id(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.next_id
    }

    fn insert(&mut self, id: u64, plugin: PluginId, join: JoinHandle<()>) {
        self.latest.insert(plugin.clone(), id);
        self.tasks.insert(id, NativeCatalogTask { join: Some(join) });
    }

    fn take(&mut self) -> Vec<CatalogBuildResult> {
        let mut ready = Vec::new();
        while let Ok((id, result)) = self.result_rx.try_recv() {
            let Some(mut task) = self.tasks.remove(&id) else {
                continue;
            };
            if let Some(join) = task.join.take() {
                let _ = join.join();
            }
            // `latest` is the newest request ever issued for this plugin,
            // not merely the newest still-running task. Keeping it after its
            // completion ensures an older task that retires later remains
            // explicitly obsolete.
            let result = match result {
                CatalogBuildResult::Complete(build)
                    if self.latest.get(&build.plugin).is_some_and(|latest| *latest != id) =>
                {
                    CatalogBuildResult::Obsolete(ObsoleteCatalogBuild {
                        plugin: build.plugin,
                        instance: build.instance,
                        generation: build.generation,
                    })
                }
                other => other,
            };
            ready.push(result);
        }
        ready
    }

    fn shutdown(&mut self) -> Vec<CatalogBuildResult> {
        for task in self.tasks.values_mut() {
            if let Some(join) = task.join.take() {
                let _ = join.join();
            }
        }
        let results = self.take();
        self.latest.clear();
        self.tasks.clear();
        results
    }
}

/// The out-of-band cancellation state for one in-flight native call.
///
/// `NativeDriver::submit` can supersede a job while the provider thread is
/// blocked inside a worker call. The cancellation request is latched before
/// the worker handle necessarily exists, so a call that is still starting is
/// cancelled as soon as its supervised worker is obtained.
#[derive(Debug)]
struct CallControl {
    requested: AtomicBool,
    finished: AtomicBool,
    handle: Mutex<Option<CancelHandle>>,
    wake: Condvar,
    wake_lock: Mutex<()>,
}

impl CallControl {
    fn new() -> Self {
        Self {
            requested: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            handle: Mutex::new(None),
            wake: Condvar::new(),
            wake_lock: Mutex::new(()),
        }
    }

    fn install(&self, handle: CancelHandle) {
        let cancel_now = self.requested.load(Ordering::Acquire);
        *self.handle.lock().unwrap_or_else(|error| error.into_inner()) = Some(handle.clone());
        if cancel_now {
            // This runs on the dispatch thread, never on NativeDriver::submit.
            handle.cancel();
        }
        self.wake.notify_all();
    }

    fn cancel(&self) {
        self.requested.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    /// Waits on a short-lived helper thread so the submitting/UI thread never
    /// performs a potentially backpressured control-frame write.
    fn watch_cancel(self: Arc<Self>) {
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
                handle.cancel();
            }
        }
    }

    fn finish(&self) {
        let _wake = self.wake_lock.lock().unwrap_or_else(|error| error.into_inner());
        self.finished.store(true, Ordering::Release);
        self.wake.notify_all();
    }
}

/// Cancellation registry shared with the non-blocking driver submission path.
#[derive(Debug, Default)]
struct NativeCancellation {
    calls: Mutex<BTreeMap<(u64, PluginId), Arc<CallControl>>>,
}

impl NativeCancellation {
    fn register(&self, generation: u64, plugin: PluginId, control: Arc<CallControl>) {
        self.calls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert((generation, plugin), control);
    }

    fn unregister(&self, generation: u64, plugin: &PluginId) {
        self.calls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&(generation, plugin.clone()));
    }

    fn cancel_before(&self, generation: u64) {
        let controls = self
            .calls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|((old_generation, _), _)| *old_generation < generation)
            .map(|(_, control)| Arc::clone(control))
            .collect::<Vec<_>>();
        for control in controls {
            control.cancel();
        }
    }

    fn cancel_all(&self) {
        let controls = self
            .calls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for control in controls {
            control.cancel();
        }
    }
}

#[derive(Debug)]
struct DispatchResult {
    generation: u64,
    plugin: PluginId,
    result: Result<Suggestions, HostError>,
}

#[derive(Debug)]
struct InFlightCall {
    generation: u64,
    plugin: PluginId,
    join: Option<JoinHandle<()>>,
}

/// Owns one independently supervised child per native package and bounds
/// runtime diagnostics. The map intentionally stores one supervisor per
/// plugin: [`NativeSupervisor::worker`] requires `&mut self`, and one shared
/// mutex would serialize unrelated plugin calls (acceptance 31.8). A
/// dispatcher holds only its own plugin's lock across that plugin's
/// `worker`/`suggest` call; there is no cross-plugin lock ordering.
struct NativeWorkerPool {
    supervisors: BTreeMap<WorkerKey, Arc<Mutex<NativeSupervisor>>>,
    failures: Vec<(PluginId, String)>,
    /// Plugins already reported as failed. A dead worker must not produce one
    /// diagnostic per later keystroke (contract §11.5).
    recorded: BTreeSet<PluginId>,
    /// Bounded with [`NATIVE_RESULT_CAPACITY`]; dispatchers apply backpressure
    /// rather than growing an unbounded queue.
    result_tx: SyncSender<DispatchResult>,
    result_rx: Receiver<DispatchResult>,
    in_flight: BTreeMap<(u64, PluginId), InFlightCall>,
    cancellation: Arc<NativeCancellation>,
}

impl std::fmt::Debug for NativeWorkerPool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeWorkerPool")
            .field("supervisors", &self.supervisors.keys().collect::<Vec<_>>())
            .field("failures", &self.failures)
            .field("in_flight", &self.in_flight.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Default for NativeWorkerPool {
    fn default() -> Self {
        let (result_tx, result_rx) = mpsc::sync_channel(NATIVE_RESULT_CAPACITY);
        Self {
            supervisors: BTreeMap::new(),
            failures: Vec::new(),
            recorded: BTreeSet::new(),
            result_tx,
            result_rx,
            in_flight: BTreeMap::new(),
            cancellation: Arc::new(NativeCancellation::default()),
        }
    }
}

impl NativeWorkerPool {
    /// Only one blocking call may own one plugin supervisor at a time. A
    /// cancelled but uncooperative call remains in this set until its host
    /// deadline, so newer generations skip that plugin instead of piling up
    /// unbounded waiter threads.
    fn has_in_flight(&self, plugin: &PluginId) -> bool {
        self.in_flight
            .keys()
            .any(|(_, active_plugin)| active_plugin == plugin)
    }
    /// Records a runtime failure at most once for one host plugin identity.
    fn record_dispatch_failure(&mut self, plugin: PluginId, reason: String) {
        if self.recorded.insert(plugin.clone()) {
            self.failures.push((plugin, reason));
        }
    }

    /// Removes one completed call and joins its short-lived dispatcher thread.
    fn finish_call(&mut self, generation: u64, plugin: &PluginId) {
        let key = (generation, plugin.clone());
        if let Some(mut call) = self.in_flight.remove(&key) {
            self.cancellation.unregister(call.generation, &call.plugin);
            if let Some(join) = call.join.take() {
                let _ = join.join();
            }
        }
    }

    /// Cancels all prior generations before a newer one is dispatched.
    fn cancel_before(&self, generation: u64) {
        self.cancellation.cancel_before(generation);
    }

    /// Cancels and joins every in-flight dispatcher, then asks each supervisor
    /// to reap its child (spec 24.3).
    fn shutdown(&mut self) {
        self.cancellation.cancel_all();
        let calls = std::mem::take(&mut self.in_flight);
        for (_, mut call) in calls {
            self.cancellation.unregister(call.generation, &call.plugin);
            if let Some(join) = call.join.take() {
                // A dispatcher applies bounded-channel backpressure while
                // publishing its one result. Drain while waiting so shutdown
                // cannot deadlock behind a full result channel.
                while !join.is_finished() {
                    let _ = self.result_rx.recv_timeout(Duration::from_millis(10));
                }
                let _ = join.join();
            }
        }
        for supervisor in self.supervisors.values() {
            supervisor
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .shutdown_all();
        }
    }
}

/// Composes native discovery, supervised workers and the app query pipeline.
///
/// Constructed with [`NativeProvider::load`] and driven by
/// [`NativeProvider::drive_query`]. Dropping the provider performs the same
/// best-effort child teardown as [`NativeProvider::shutdown`], so a caller that
/// exits through an error path cannot leave native children behind (spec 24.3).
#[derive(Debug)]
pub struct NativeProvider {
    pool: NativeWorkerPool,
    loaded: Vec<LoadedPlugin>,
    catalog: NativeCatalogDispatcher,
    plugins: Vec<PluginId>,
    unavailable: Vec<NativeUnavailable>,
    /// Current async suggestion snapshots keyed by the owning plugin and item
    /// id. Stable item ids are only unique within an owner.
    action_items: Arc<Mutex<BTreeMap<(PluginId, ItemId), Item>>>,
    collection_window: Duration,
}

/// How long one query waits for native workers before presenting what arrived.
///
/// This is the bound that stops a slow plugin holding a healthy sibling's
/// result until its own call deadline: whatever has not answered inside the
/// window contributes no rows to this frame. Production always uses it.
pub const DEFAULT_COLLECTION_WINDOW: Duration = Duration::from_millis(100);

/// Upper bound on a native worker call. The effective suggestion deadline
/// comes from `performance.suggest-hard-timeout-ms` and defaults to the
/// manifest model's 500 ms value; this cap remains the action/transport bound.
const NATIVE_CALL_TIMEOUT_MS: u64 = 5_000;

impl NativeProvider {
    /// Discovers native `crikey.toml` packages under `roots`, resolves the
    /// current platform entrypoint, and starts one worker per usable package
    /// (contract §6; spec 19.1-19.3, 16.6).
    ///
    /// Every failure is recorded as [`NativeUnavailable`] and discovery
    /// continues with the remaining package directories. The worker's working
    /// directory is the package directory itself so shipped witness/config
    /// files are visible to the child (contract §3.1(8), §11.1).
    pub fn load(pipeline: &mut QueryPipeline, roots: &[PathBuf]) -> Self {
        Self::load_with_collection_window(pipeline, roots, DEFAULT_COLLECTION_WINDOW)
    }

    /// [`Self::load`] with an explicit collection window.
    ///
    /// Exists for tests whose subject is that a worker's rows cross the
    /// provider boundary at all, not how long that takes. Those tests would
    /// otherwise race a real subprocess round-trip against the production
    /// window and fail on a loaded machine for reasons unrelated to what they
    /// assert. A test about the window itself must use [`Self::load`].
    pub fn load_with_collection_window(
        pipeline: &mut QueryPipeline,
        roots: &[PathBuf],
        collection_window: Duration,
    ) -> Self {
        let mut provider = Self {
            pool: NativeWorkerPool::default(),
            loaded: Vec::new(),
            catalog: NativeCatalogDispatcher::default(),
            plugins: Vec::new(),
            unavailable: Vec::new(),
            action_items: Arc::new(Mutex::new(BTreeMap::new())),
            collection_window,
        };

        for root in roots {
            let entries = match fs::read_dir(root) {
                Ok(entries) => entries,
                Err(error) => {
                    provider.record_unavailable(
                        root.display().to_string(),
                        None,
                        format!("cannot scan native plugin root: {error}"),
                    );
                    continue;
                }
            };

            // Directory iteration order is not stable across platforms. Keep
            // discovery deterministic so diagnostics and plugin order are
            // reproducible.
            let mut directories: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.join("crikey.toml").is_file())
                .collect();
            directories.sort();

            for directory in directories {
                provider.register_plugin_dir(pipeline, &directory);
            }
        }

        provider
    }

    /// Loads one candidate package, starts its worker, and registers the
    /// namespaced plugin with the manifest-derived scheduling policy. No
    /// failure here can abort sibling discovery (spec 24.1).
    fn register_plugin_dir(&mut self, pipeline: &mut QueryPipeline, directory: &Path) {
        let package = directory
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| directory.display().to_string());
        let manifest_path = directory.join("crikey.toml");

        let text = match fs::read_to_string(&manifest_path) {
            Ok(text) => text,
            Err(error) => {
                self.record_unavailable(package, None, format!("cannot read crikey.toml: {error}"));
                return;
            }
        };
        let manifest = match Manifest::parse(&text) {
            Ok(manifest) => manifest,
            Err(error) => {
                self.record_unavailable(package, None, format!("invalid crikey.toml: {error}"));
                return;
            }
        };

        // This provider owns only native packages; other runtimes remain for
        // their respective providers.
        if manifest.plugin.runtime != Runtime::Native {
            return;
        }

        let plugin = PluginId(format!("native.{}", manifest.plugin.id));
        let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
        let entrypoint = match manifest.entrypoint_for(os, arch) {
            Ok(entrypoint) => entrypoint.to_owned(),
            Err(error) => {
                self.record_unavailable(package, Some(plugin), format!("no usable entrypoint: {error}"));
                return;
            }
        };

        // Manifest entrypoints are paths, not shell command lines. Resolve a
        // relative path within the package and preserve absolute paths exactly;
        // never whitespace-split an entrypoint (contract §11.1).
        let executable_path = Path::new(&entrypoint);
        let executable = if executable_path.is_absolute() {
            executable_path.to_path_buf()
        } else {
            directory.join(executable_path)
        };
        if !executable.is_file() {
            self.record_unavailable(
                package,
                Some(plugin),
                format!("native entrypoint is not a file: {}", executable.display()),
            );
            return;
        }

        // Source directory identity is load-bearing even when entrypoint paths
        // are identical. One package gets one worker; no worker is shared
        // across distinct package directories.
        let key: WorkerKey = (
            executable.to_string_lossy().into_owned(),
            directory.to_string_lossy().into_owned(),
        );
        let launch = LaunchSpec {
            plugin: plugin.clone(),
            executable,
            arguments: Vec::new(),
            working_dir: Some(directory.to_path_buf()),
            environment: Vec::new(),
        };
        let suggest_timeout_ms = manifest
            .performance
            .suggest_hard_timeout_ms
            .min(NATIVE_CALL_TIMEOUT_MS);
        let options = WorkerOptions::new().with_call_timeout_ms(suggest_timeout_ms);

        // Register first so this provider receives the exact shared budget
        // handle that the query pipeline stores for the plugin. A worker
        // startup failure remains an unavailable plugin and never gets added
        // to the provider's loaded set.
        let budget = match pipeline.register_namespaced_manifest(plugin.clone(), &manifest) {
            Ok(budget) => budget,
            Err(error) => {
                self.record_unavailable(
                    package,
                    Some(plugin),
                    format!("the query pipeline refused the native plugin: {error:?}"),
                );
                return;
            }
        };

        let mut supervisor = NativeSupervisor::new(SupervisorConfig::default());
        if let Err(error) = supervisor.register(launch.clone(), options.clone()) {
            self.record_unavailable(
                package,
                Some(plugin.clone()),
                format!("native worker registration failed: {error}"),
            );
            let _ = pipeline.unregister_plugin(&plugin);
            return;
        }
        // `register` is intentionally lazy, but startup is a load-time
        // boundary for the provider. Obtain the first worker through the
        // supervisor so a refused handshake is reported as unavailable while
        // all later workers remain restartable through the same registration.
        if let Err(error) = supervisor.worker(&plugin, 0) {
            self.record_unavailable(
                package,
                Some(plugin.clone()),
                format!("native worker did not start: {error}"),
            );
            let _ = pipeline.unregister_plugin(&plugin);
            return;
        }
        let supervisor = Arc::new(Mutex::new(supervisor));
        self.pool.supervisors.insert(key.clone(), supervisor);

        self.plugins.push(plugin.clone());
        self.loaded.push(LoadedPlugin {
            plugin,
            key,
            launch,
            worker_options: options,
            budget,
        });
    }

    fn record_unavailable(&mut self, package: String, plugin: Option<PluginId>, reason: String) {
        self.unavailable.push(NativeUnavailable {
            package,
            plugin,
            reason,
        });
    }

    /// The namespaced native plugins that loaded successfully.
    pub fn plugins(&self) -> &[PluginId] {
        &self.plugins
    }

    /// Packages refused during discovery or startup, with one reason each.
    pub fn unavailable(&self) -> &[NativeUnavailable] {
        &self.unavailable
    }

    /// Runtime dispatch failures, newest last. Completed outcomes are drained
    /// before the snapshot is returned, so a result that missed the bounded
    /// presentation window still contributes its failure diagnostic. A
    /// crashed worker is recorded once and is deliberately not copied into
    /// [`Self::unavailable`] (contract §11.5).
    pub fn dispatch_failures(&mut self) -> &[(PluginId, String)] {
        self.drain_completed_results();
        &self.pool.failures
    }

    /// Starts one bounded catalog rebuild for an exactly owned plugin.
    ///
    /// Catalog work gets a fresh supervised native process rather than the
    /// query supervisor's worker lock, so a slow rebuild cannot delay healthy
    /// suggestions. The budget is the exact handle created at registration.
    pub fn request_catalog_build(
        &mut self,
        plugin: &PluginId,
        instance: u64,
        generation: Generation,
    ) -> Result<u64, CatalogDispatchError> {
        let loaded = self
            .loaded
            .iter()
            .find(|loaded| &loaded.plugin == plugin)
            .ok_or_else(|| CatalogDispatchError::UnknownPlugin {
                plugin: plugin.clone(),
            })?;
        let budget = Arc::clone(&loaded.budget);
        let launch = loaded.launch.clone();
        // Catalog callbacks are not suggestion callbacks; keep their
        // independent long transport budget rather than applying the manifest
        // suggestion hard deadline to catalog construction.
        let worker_options = loaded
            .worker_options
            .clone()
            .with_call_timeout_ms(NATIVE_CALL_TIMEOUT_MS);
        if self.catalog.tasks.len() >= CATALOG_RESULT_CAPACITY {
            return Err(CatalogDispatchError::QueueFull {
                plugin: plugin.clone(),
            });
        }

        let guard = budget.try_acquire_owned(BudgetKind::Catalog).ok_or_else(|| {
            CatalogDispatchError::BudgetRefused {
                plugin: plugin.clone(),
            }
        })?;
        let request_id = self.catalog.next_id();
        let sender = self.catalog.result_tx.clone();
        let plugin_for_thread = plugin.clone();
        let thread_plugin = plugin.clone();
        let join = thread::Builder::new()
            .name(format!("crikey-native-catalog-{}", plugin.0))
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let mut supervisor = NativeSupervisor::new(SupervisorConfig::default());
                    supervisor
                        .register(launch, worker_options)
                        .map_err(|error| error.to_string())?;
                    let result = {
                        let worker = supervisor
                            .worker(&plugin_for_thread, 0)
                            .map_err(|error| error.to_string())?;
                        worker.build_catalog().map_err(|error| error.to_string())
                    };
                    supervisor.shutdown_all();
                    result
                }));
                let completion = match result {
                    Ok(Ok(mut items)) => {
                        for item in &mut items {
                            item.plugin_id = plugin_for_thread.clone();
                        }
                        CatalogBuildResult::Complete(crate::CatalogBuild {
                            plugin: plugin_for_thread,
                            instance,
                            generation,
                            items,
                        })
                    }
                    Ok(Err(reason)) => CatalogBuildResult::Failed {
                        plugin: thread_plugin.clone(),
                        instance,
                        generation,
                        reason: format!("native catalog build failed: {reason}"),
                    },
                    Err(_) => CatalogBuildResult::Failed {
                        plugin: thread_plugin,
                        instance,
                        generation,
                        reason: "native catalog worker panicked".to_owned(),
                    },
                };
                let _ = sender.send((request_id, completion));
                drop(guard);
            })
            .map_err(|error| CatalogDispatchError::ThreadSpawn {
                plugin: plugin.clone(),
                reason: error.to_string(),
            })?;
        self.catalog.insert(request_id, plugin.clone(), join);
        Ok(request_id)
    }

    /// Retires completed catalog tasks and returns their tagged outcomes.
    ///
    /// A completion from an older request for the same plugin is surfaced as
    /// [`CatalogBuildResult::Obsolete`] and must not be published. Failures
    /// enter the provider's bounded runtime diagnostics.
    pub fn take_catalog_results(&mut self) -> Vec<CatalogBuildResult> {
        let results = self.catalog.take();
        for result in &results {
            if let CatalogBuildResult::Failed { plugin, reason, .. } = result {
                self.pool.record_dispatch_failure(plugin.clone(), reason.clone());
            }
        }
        results
    }

    /// Clones the provider-owned action budget handles for the asynchronous
    /// driver endpoint. These are the same `Arc`s retained by the pipeline.
    fn action_budgets(&self) -> BTreeMap<PluginId, PluginBudgetHandle> {
        self.loaded
            .iter()
            .map(|loaded| (loaded.plugin.clone(), Arc::clone(&loaded.budget)))
            .collect()
    }

    /// Validates and executes one admitted plugin action on the exact native
    /// worker that owns the item's plugin id.
    fn execute_action_request(&mut self, request: NativeActionRequest) -> crikey_core::Result<()> {
        let _guard = request.guard;
        let plugin = request.plugin;
        let item_id = request.item.stable_id.clone();
        let item = self
            .action_items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&(plugin.clone(), item_id.clone()))
            .cloned()
            .ok_or_else(|| {
                crikey_core::CoreError::Invalid("native action item snapshot is no longer current".to_owned())
            })?;
        if item.plugin_id != plugin {
            return Err(crikey_core::CoreError::Invalid(format!(
                "native action item `{}` is owned by `{}`, not `{}`",
                item.stable_id.0, item.plugin_id.0, plugin.0
            )));
        }
        let loaded = self
            .loaded
            .iter()
            .find(|loaded| loaded.plugin == plugin)
            .ok_or_else(|| {
                crikey_core::CoreError::Invalid(format!(
                    "native action owner `{}` is no longer loaded",
                    plugin.0
                ))
            })?;
        let action = item
            .actions
            .iter()
            .find(|action| action.action_id == request.action_id)
            .ok_or_else(|| {
                crikey_core::CoreError::Invalid("selected action is no longer available".to_owned())
            })?;
        if action.execution_policy != ExecutionPolicy::Plugin {
            return Err(crikey_core::CoreError::Invalid(
                "native action request is not plugin-owned".to_owned(),
            ));
        }
        if !action.applicable_categories.is_empty() && !action.applicable_categories.contains(&item.category)
        {
            return Err(crikey_core::CoreError::Invalid(
                "native action is not applicable to the selected item category".to_owned(),
            ));
        }
        match item.argument_policy {
            ArgumentPolicy::Forbidden if request.argument.is_some() => {
                return Err(crikey_core::CoreError::Invalid(
                    "native action item forbids arguments".to_owned(),
                ));
            }
            ArgumentPolicy::Required if request.argument.as_deref().is_none_or(str::is_empty) => {
                return Err(crikey_core::CoreError::Invalid(
                    "native action item requires an argument".to_owned(),
                ));
            }
            ArgumentPolicy::Optional | ArgumentPolicy::Forbidden | ArgumentPolicy::Required => {}
        }

        let supervisor = self.pool.supervisors.get(&loaded.key).cloned().ok_or_else(|| {
            crikey_core::CoreError::Invalid(format!("native worker for `{}` is unavailable", plugin.0))
        })?;
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let mut supervisor = supervisor.lock().unwrap_or_else(|error| error.into_inner());
            let worker = supervisor.worker(&plugin, 0).map_err(|error| error.to_string())?;
            worker
                .execute(
                    &request.item.stable_id,
                    Some(&request.action_id),
                    request.argument.as_deref(),
                )
                .map_err(|error| error.to_string())
        }));
        match outcome {
            Ok(Ok(ExecuteOutcome::Ok)) => Ok(()),
            Ok(Ok(ExecuteOutcome::Failed(error))) => {
                let reason = format!("native plugin action failed: {}", error.message);
                self.pool.record_dispatch_failure(plugin.clone(), reason.clone());
                Err(crikey_core::CoreError::Invalid(format!(
                    "native plugin `{}` action failed: {reason}",
                    plugin.0
                )))
            }
            Ok(Ok(ExecuteOutcome::Unsupported)) => {
                let reason = "native plugin does not support action execution".to_owned();
                self.pool.record_dispatch_failure(plugin.clone(), reason.clone());
                Err(crikey_core::CoreError::Invalid(format!(
                    "native plugin `{}` does not support action execution",
                    plugin.0
                )))
            }
            Ok(Err(reason)) => {
                self.pool.record_dispatch_failure(plugin.clone(), reason.clone());
                Err(crikey_core::CoreError::Invalid(format!(
                    "native plugin `{}` action failed: {reason}",
                    plugin.0
                )))
            }
            Err(_) => {
                self.pool
                    .record_dispatch_failure(plugin.clone(), "native action worker panicked".to_owned());
                Err(crikey_core::CoreError::Invalid(format!(
                    "native plugin `{}` action worker panicked",
                    plugin.0
                )))
            }
        }
    }

    /// Drives one query through the native workers and the current pipeline
    /// generation. Stale batches are refused by [`QueryPipeline::deliver`],
    /// while a worker failure contributes no rows and never disturbs siblings
    /// (spec 8.1, 24.1; acceptance 31.7, 31.9).
    pub fn drive_query(
        &mut self,
        pipeline: &mut QueryPipeline,
        query: &str,
        now: Millis,
    ) -> Option<ViewModel> {
        let generation = pipeline.keystroke(query, now);
        let (mut suggestions, dead_plugins) = self.collect_suggestions(query, generation, now);

        let mut at = now;
        for _ in 0..64 {
            let tick = pipeline.tick(at);
            for cancellation in tick.cancellations {
                let _ = pipeline.complete(&cancellation.plugin, cancellation.generation, at);
            }

            let mut dispatched_current = false;
            for request in tick.dispatches {
                if request.generation != generation {
                    let _ = pipeline.complete(&request.plugin, request.generation, at);
                    continue;
                }
                dispatched_current = true;
                if dead_plugins.contains(&request.plugin) {
                    let _ = pipeline.abort_request(&request.plugin, request.generation, at);
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
                    at,
                );
                let _ = pipeline.complete(&request.plugin, request.generation, at);
            }
            if suggestions.is_empty() && dispatched_current {
                break;
            }
            match pipeline.next_wakeup() {
                Some(next) if next > at => at = next,
                _ => break,
            }
        }

        // A completion that raced the presentation deadline is no longer
        // eligible to contribute rows, but its failure must still be
        // recorded. Completions arriving after this non-blocking drain are
        // consumed by the next collect, failure snapshot, or shutdown.
        self.drain_completed_results();

        let frame = pipeline.present(at);
        frame.filter(|frame| frame.generation == generation)
    }

    /// Cancels outstanding calls, joins their dispatch threads, and asks every
    /// registered supervisor to reap its child (spec 24.3).
    pub fn shutdown(&mut self, _now: Millis) {
        for result in self.catalog.shutdown() {
            if let CatalogBuildResult::Failed { plugin, reason, .. } = result {
                self.pool.record_dispatch_failure(plugin, reason);
            }
        }
        self.pool.shutdown();
        // Joining every dispatcher makes all of its outcomes observable on
        // the channel. Consume them before teardown returns so late successes
        // are retired and late failures cannot disappear with the provider.
        self.drain_completed_results();
    }

    /// Dispatches one independent call per plugin. Results are consumed from
    /// a completion channel as they arrive; the short collection budget keeps
    /// a slow sibling from delaying a healthy plugin until its call deadline.
    /// Calls that are still running remain registered so a newer generation
    /// can reach their [`CancelHandle`] (spec 13.3, 13.6; acceptance 31.8).
    fn collect_suggestions(
        &mut self,
        query: &str,
        generation: Generation,
        now: Millis,
    ) -> (BTreeMap<PluginId, Vec<Item>>, BTreeSet<PluginId>) {
        let generation_value = generation.get();
        self.pool.cancel_before(generation_value);

        // First retire completions from superseded calls. Their items are
        // intentionally discarded, but failures remain actionable diagnostics.
        self.drain_completed_results();
        self.action_items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        let request = NativeSuggestRequest {
            generation: generation_value,
            text: query.to_owned(),
            normalized: query.to_owned(),
            selected_item_id: None,
        };
        let targets: Vec<(PluginId, WorkerKey)> = self
            .loaded
            .iter()
            .map(|loaded| (loaded.plugin.clone(), loaded.key.clone()))
            .collect();
        let mut pending = 0usize;

        let mut dead_plugins = BTreeSet::new();
        for (plugin, key) in targets {
            if self.pool.has_in_flight(&plugin) {
                continue;
            }
            let supervisor = match self.pool.supervisors.get(&key) {
                Some(supervisor) => Arc::clone(supervisor),
                None => {
                    dead_plugins.insert(plugin);
                    continue;
                }
            };
            let control = Arc::new(CallControl::new());
            let control_for_thread = Arc::clone(&control);
            let sender = self.pool.result_tx.clone();
            let request_for_thread = request.clone();
            let plugin_for_thread = plugin.clone();
            self.pool
                .cancellation
                .register(generation_value, plugin.clone(), Arc::clone(&control));
            let join = thread::Builder::new()
                .name(format!("crikey-native-{}", plugin.0))
                .spawn(move || {
                    let result = {
                        let mut supervisor = supervisor.lock().unwrap_or_else(|error| error.into_inner());
                        match supervisor.worker(&plugin_for_thread, now) {
                            Ok(worker) => {
                                control_for_thread.install(worker.cancel_handle());
                                let watch_control = Arc::clone(&control_for_thread);
                                let watcher = thread::Builder::new()
                                    .name(format!("crikey-native-cancel-{}", plugin_for_thread.0))
                                    .spawn(move || watch_control.watch_cancel());
                                let result = worker.suggest(&request_for_thread);
                                control_for_thread.finish();
                                if let Ok(watcher) = watcher {
                                    let _ = watcher.join();
                                }
                                result
                            }
                            Err(error) => Err(error),
                        }
                    };
                    let _ = sender.send(DispatchResult {
                        generation: generation_value,
                        plugin: plugin_for_thread,
                        result,
                    });
                });
            let join = match join {
                Ok(join) => join,
                Err(error) => {
                    self.pool.cancellation.unregister(generation_value, &plugin);
                    self.pool.record_dispatch_failure(
                        plugin.clone(),
                        format!("native dispatch thread did not start: {error}"),
                    );
                    dead_plugins.insert(plugin);
                    continue;
                }
            };
            self.pool.in_flight.insert(
                (generation_value, plugin.clone()),
                InFlightCall {
                    generation: generation_value,
                    plugin,
                    join: Some(join),
                },
            );
            pending += 1;
        }

        let mut by_plugin = BTreeMap::new();
        let deadline = Instant::now() + self.collection_window;
        while pending != 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let completion = match self.pool.result_rx.recv_timeout(remaining) {
                Ok(completion) => completion,
                Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
                    break;
                }
            };
            let is_current = completion.generation == generation_value;
            self.pool.finish_call(completion.generation, &completion.plugin);
            let plugin = completion.plugin.clone();
            let failed = if is_current {
                pending = pending.saturating_sub(1);
                self.apply_dispatch_result(completion.plugin, completion.result, Some(&mut by_plugin))
            } else {
                self.apply_dispatch_result(completion.plugin, completion.result, None)
            };
            if is_current && failed {
                dead_plugins.insert(plugin);
            }
        }

        (by_plugin, dead_plugins)
    }
    /// Retires every completion that has reached the channel. Results that
    /// arrive outside the presentation window are intentionally not rendered,
    /// but failures are still recorded and every dispatcher is joined.
    fn drain_completed_results(&mut self) {
        while let Ok(completion) = self.pool.result_rx.try_recv() {
            self.pool.finish_call(completion.generation, &completion.plugin);
            self.apply_dispatch_result(completion.plugin, completion.result, None);
        }
    }

    fn apply_dispatch_result(
        &mut self,
        plugin: PluginId,
        result: Result<Suggestions, HostError>,
        output: Option<&mut BTreeMap<PluginId, Vec<Item>>>,
    ) -> bool {
        match result {
            Ok(suggestions) if matches!(suggestions.state, crikey_native_host::BatchState::Failed) => {
                let reason = suggestions
                    .error
                    .map(|error| error.message)
                    .filter(|message| !message.is_empty())
                    .unwrap_or_else(|| "the native plugin reported a failure".to_owned());
                self.pool.record_dispatch_failure(plugin, reason);
                false
            }
            Ok(suggestions) => {
                if let Some(output) = output {
                    let mut items = suggestions.items;
                    // The loader owns identity: a plugin cannot forge a sibling
                    // namespace by changing the stable item's owner.
                    for item in &mut items {
                        item.plugin_id = plugin.clone();
                        self.action_items
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .insert((plugin.clone(), item.stable_id.clone()), item.clone());
                    }
                    output.insert(plugin, items);
                }
                false
            }
            Err(error) => {
                self.pool
                    .record_dispatch_failure(plugin.clone(), error.to_string());
                self.action_items
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .retain(|(owner, _), _| owner != &plugin);
                true
            }
        }
    }
}

impl Drop for NativeProvider {
    fn drop(&mut self) {
        self.shutdown(0);
    }
}

/// One asynchronous native query, tagged with the UI generation it belongs to.
#[derive(Debug)]
struct NativeJob {
    generation: Generation,
    query: String,
    now: Millis,
    builtin_rows: Vec<ResultRow>,
    builtin_pending: bool,
    selected: usize,
}

/// Single-slot replace-oldest mailbox for native queries.
#[derive(Debug)]
struct NativeRequestSlot {
    job: Option<NativeJob>,
    stop: bool,
}

const ACTION_QUEUE_CAPACITY: usize = 8;
const ACTION_COMPLETION_CAPACITY: usize = 64;
const ACTION_IN_FLIGHT_CAPACITY: usize = 32;
const ACTION_TIMEOUT_MS: u64 = NATIVE_CALL_TIMEOUT_MS;

/// One admitted action handed to the native provider supervisor.
#[derive(Debug)]
struct NativeActionRequest {
    request_id: ActionRequestId,
    plugin: PluginId,
    item: Item,
    action_id: ActionId,
    argument: Option<String>,
    guard: OwnedBudgetGuard,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

enum NativeWork {
    Query(NativeJob),
    Action(Box<NativeActionRequest>),
}

/// Bounded action endpoint retained by the live native driver.
#[derive(Debug)]
struct NativeActionEndpoint {
    sender: SyncSender<NativeActionRequest>,
    completions: Arc<Mutex<VecDeque<PluginActionCompletion>>>,
    budgets: BTreeMap<PluginId, PluginBudgetHandle>,
    items: Arc<Mutex<BTreeMap<(PluginId, ItemId), Item>>>,
    pending: Arc<Mutex<BTreeMap<ActionRequestId, Arc<AtomicBool>>>>,
    in_flight: Arc<AtomicUsize>,
    next_id: Arc<AtomicU64>,
    mailbox: Arc<(Mutex<NativeRequestSlot>, Condvar)>,
}
impl crate::PluginActionExecutor for NativeActionEndpoint {
    fn submit_plugin_action(
        &self,
        plugin: &PluginId,
        item: &Item,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> crikey_core::Result<ActionRequestId> {
        if item.plugin_id != *plugin {
            return Err(crikey_core::CoreError::Invalid(
                "selected native plugin result has stale ownership".to_owned(),
            ));
        }
        let item_id = item.stable_id.clone();
        let item = self
            .items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&(plugin.clone(), item_id))
            .cloned()
            .ok_or_else(|| {
                crikey_core::CoreError::Invalid(
                    "selected native plugin result is no longer current".to_owned(),
                )
            })?;
        if item.plugin_id != *plugin {
            return Err(crikey_core::CoreError::Invalid(
                "selected native plugin result has stale ownership".to_owned(),
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
                "native action mailbox is full".to_owned(),
            ));
        }
        let budget = match self.budgets.get(plugin) {
            Some(budget) => budget,
            None => {
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                return Err(crikey_core::CoreError::Invalid(format!(
                    "no native action runtime owns plugin `{}`",
                    plugin.0
                )));
            }
        };
        let guard = match budget.try_acquire_owned(BudgetKind::Action) {
            Some(guard) => guard,
            None => {
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                return Err(crikey_core::CoreError::Invalid(format!(
                    "native plugin `{}` action budget is full",
                    plugin.0
                )));
            }
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
        let cancelled = Arc::new(AtomicBool::new(false));
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(request_id.clone(), Arc::clone(&cancelled));
        let request = NativeActionRequest {
            request_id: request_id.clone(),
            plugin: plugin.clone(),
            item: item.clone(),
            action_id: action_id.clone(),
            argument: argument.map(str::to_owned),
            guard,
            deadline: Instant::now() + Duration::from_millis(ACTION_TIMEOUT_MS),
            cancelled,
        };
        if let Err(error) = self.sender.try_send(request) {
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&request_id);
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return Err(match error {
                mpsc::TrySendError::Full(_) => crikey_core::CoreError::Invalid(format!(
                    "native plugin `{}` action queue is full",
                    plugin.0
                )),
                mpsc::TrySendError::Disconnected(_) => crikey_core::CoreError::Invalid(format!(
                    "native plugin `{}` action runtime stopped",
                    plugin.0
                )),
            });
        }
        self.mailbox.1.notify_one();
        Ok(request_id)
    }

    fn poll_plugin_actions(&self) -> Vec<PluginActionCompletion> {
        let mut completions = Vec::new();
        let mut mailbox = self.completions.lock().unwrap_or_else(|error| error.into_inner());
        while completions.len() < ACTION_COMPLETION_CAPACITY {
            let Some(completion) = mailbox.pop_front() else {
                break;
            };
            self.pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&completion.request_id);
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            completions.push(completion);
        }
        completions
    }

    fn cancel_plugin_action(&self, request_id: &ActionRequestId) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(request_id)
            .is_some_and(|cancelled| {
                cancelled.store(true, Ordering::Release);
                true
            })
    }

    fn owns_item(&self, plugin: &PluginId, item_id: &ItemId) -> bool {
        self.items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .any(|((owner, id), _)| owner == plugin && id == item_id)
    }

    fn submit_plugin_action_by_id(
        &self,
        plugin: &PluginId,
        item_id: &ItemId,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> crikey_core::Result<ActionRequestId> {
        let item = self
            .items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .find(|((owner, id), _)| owner == plugin && id == item_id)
            .map(|(_, item)| item.clone())
            .ok_or_else(|| {
                crikey_core::CoreError::Invalid(
                    "selected native plugin result is no longer current".to_owned(),
                )
            })?;
        if item.plugin_id != *plugin {
            return Err(crikey_core::CoreError::Invalid(
                "selected native plugin result has stale ownership".to_owned(),
            ));
        }
        self.submit_plugin_action(plugin, &item, action_id, argument)
    }
}
fn enqueue_native_completion(
    mailbox: &Mutex<VecDeque<PluginActionCompletion>>,
    completion: PluginActionCompletion,
) {
    let mut mailbox = mailbox.lock().unwrap_or_else(|error| error.into_inner());
    if mailbox.len() >= ACTION_COMPLETION_CAPACITY {
        eprintln!(
            "crikey: native action completion mailbox overflow for {} / {}",
            completion.plugin.0, completion.action_id.0
        );
        return;
    }
    mailbox.push_back(completion);
}

/// Drives [`NativeProvider::drive_query`] away from the UI thread (spec 6.5;
/// acceptance 31.1, 31.8). The mailbox is bounded to one pending job, and a
/// newer generation replaces an unstarted older job.
#[derive(Debug)]
pub struct NativeDriver {
    mailbox: Arc<(Mutex<NativeRequestSlot>, Condvar)>,
    action_endpoint: Arc<NativeActionEndpoint>,
    catalog_results: Arc<Mutex<Vec<CatalogBuildResult>>>,
    outcome: Arc<Mutex<Option<ViewModel>>>,
    current: Arc<AtomicU64>,
    cancellation: Arc<NativeCancellation>,
    replacements: Arc<AtomicU64>,
    busy: Arc<AtomicBool>,
    has_plugins: bool,
    worker: Option<JoinHandle<()>>,
}

impl NativeDriver {
    /// Moves the provider and pipeline to a supervisor thread. A thread spawn
    /// refusal degrades to an inert driver while dropping the provider reaps
    /// all children (spec 24.1, 24.3).
    pub fn spawn(
        mut provider: NativeProvider,
        mut pipeline: QueryPipeline,
        publish: Box<dyn Fn(&ViewModel) + Send + 'static>,
    ) -> Self {
        let has_plugins = !provider.plugins().is_empty();
        let mailbox = Arc::new((
            Mutex::new(NativeRequestSlot {
                job: None,
                stop: false,
            }),
            Condvar::new(),
        ));
        let outcome = Arc::new(Mutex::new(None));
        let catalog_results = Arc::new(Mutex::new(Vec::new()));
        let current = Arc::new(AtomicU64::new(0));
        let cancellation = Arc::clone(&provider.pool.cancellation);
        let replacements = Arc::new(AtomicU64::new(0));
        let (action_sender, action_receiver) = mpsc::sync_channel(ACTION_QUEUE_CAPACITY);
        let completion_mailbox = Arc::new(Mutex::new(VecDeque::with_capacity(ACTION_COMPLETION_CAPACITY)));
        let action_endpoint = Arc::new(NativeActionEndpoint {
            sender: action_sender,
            completions: Arc::clone(&completion_mailbox),
            budgets: provider.action_budgets(),
            items: Arc::clone(&provider.action_items),
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            in_flight: Arc::new(AtomicUsize::new(0)),
            next_id: Arc::new(AtomicU64::new(0)),
            mailbox: Arc::clone(&mailbox),
        });
        let busy = Arc::new(AtomicBool::new(false));
        let thread_mailbox = Arc::clone(&mailbox);
        let thread_catalog_results = Arc::clone(&catalog_results);
        let thread_outcome = Arc::clone(&outcome);
        let thread_current = Arc::clone(&current);
        let thread_busy = Arc::clone(&busy);
        let thread_completion_mailbox = Arc::clone(&completion_mailbox);
        let spawned = std::thread::Builder::new()
            .name("crikey-native".to_owned())
            .spawn(move || {
                let (lock, cvar) = &*thread_mailbox;
                let mut last_now: Millis = 0;
                loop {
                    for result in provider.take_catalog_results() {
                        thread_catalog_results
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .push(result);
                    }
                    let work = {
                        let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
                        loop {
                            if slot.stop {
                                while let Ok(request) = action_receiver.try_recv() {
                                    let completion = PluginActionCompletion {
                                        request_id: request.request_id.clone(),
                                        plugin: request.plugin.clone(),
                                        item_id: request.item.stable_id.clone(),
                                        action_id: request.action_id.clone(),
                                        outcome: Err(crikey_core::CoreError::Cancelled),
                                    };
                                    enqueue_native_completion(&thread_completion_mailbox, completion);
                                }
                                drop(slot);
                                provider.shutdown(last_now);
                                return;
                            }
                            if let Ok(request) = action_receiver.try_recv() {
                                break NativeWork::Action(Box::new(request));
                            }
                            if let Some(job) = slot.job.take() {
                                break NativeWork::Query(job);
                            }
                            slot = cvar
                                .wait_timeout(slot, Duration::from_millis(10))
                                .unwrap_or_else(|error| error.into_inner())
                                .0;
                        }
                    };
                    let job = match work {
                        NativeWork::Action(request) => {
                            let request_id = request.request_id.clone();
                            let plugin = request.plugin.clone();
                            let item_id = request.item.stable_id.clone();
                            let action_id = request.action_id.clone();
                            let deadline = request.deadline;
                            let cancelled = Arc::clone(&request.cancelled);
                            let result = if cancelled.load(Ordering::Acquire) {
                                Err(crikey_core::CoreError::Cancelled)
                            } else if Instant::now() >= deadline {
                                Err(crikey_core::CoreError::Invalid(format!(
                                    "native plugin `{}` action timed out before execution",
                                    plugin.0
                                )))
                            } else {
                                match catch_unwind(AssertUnwindSafe(|| {
                                    provider.execute_action_request(*request)
                                })) {
                                    Ok(result) => result,
                                    Err(_) => Err(crikey_core::CoreError::Invalid(format!(
                                        "native plugin `{}` action worker panicked",
                                        plugin.0
                                    ))),
                                }
                            };
                            let result = if cancelled.load(Ordering::Acquire) {
                                Err(crikey_core::CoreError::Cancelled)
                            } else if Instant::now() >= deadline {
                                Err(crikey_core::CoreError::Invalid(format!(
                                    "native plugin `{}` action timed out",
                                    plugin.0
                                )))
                            } else {
                                result
                            };
                            let completion = PluginActionCompletion {
                                request_id,
                                plugin,
                                item_id,
                                action_id,
                                outcome: result,
                            };
                            enqueue_native_completion(&thread_completion_mailbox, completion);
                            continue;
                        }
                        NativeWork::Query(job) => job,
                    };
                    last_now = job.now;

                    thread_busy.store(true, Ordering::Release);
                    let native = provider.drive_query(&mut pipeline, &job.query, job.now);
                    let mut rows = job.builtin_rows;
                    let mut pending = job.builtin_pending;
                    if let Some(frame) = native {
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

                    // Hold the mailbox lock across the staleness check and the
                    // single-slot outcome publication. `submit` stores its
                    // newer generation before taking this lock, and a queued
                    // replacement is itself a supersession in flight.
                    let slot = lock.lock().unwrap_or_else(|error| error.into_inner());
                    if slot.stop
                        || slot.job.is_some()
                        || thread_current.load(Ordering::Acquire) != job.generation.get()
                    {
                        thread_busy.store(false, Ordering::Release);
                        continue;
                    }
                    *thread_outcome.lock().unwrap_or_else(|error| error.into_inner()) = Some(merged.clone());
                    publish(&merged);
                    thread_busy.store(false, Ordering::Release);
                    drop(slot);
                }
            });

        match spawned {
            Ok(worker) => Self {
                mailbox,
                action_endpoint,
                catalog_results,
                outcome,
                current,
                cancellation,
                replacements,
                busy,
                has_plugins,
                worker: Some(worker),
            },
            Err(_) => Self {
                mailbox,
                action_endpoint,
                catalog_results,
                outcome,
                current,
                cancellation,
                replacements,
                busy,
                has_plugins: false,
                worker: None,
            },
        }
    }

    /// Submits a query without waiting for a native child. Older generations
    /// are refused at intake; an accepted query replaces the single queued job
    /// when the supervisor is still busy (contract §6; acceptance 31.8).
    pub fn submit(
        &self,
        generation: Generation,
        query: &str,
        now: Millis,
        builtin: Vec<ResultRow>,
        builtin_pending: bool,
        selected: usize,
    ) {
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

        // Latch cancellation immediately at intake, rather than waiting for
        // the provider thread to finish its current blocking call.
        self.cancellation.cancel_before(generation_value);
        let (lock, cvar) = &*self.mailbox;
        let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
        if slot.stop {
            return;
        }
        if slot.job.is_some() {
            self.replacements.fetch_add(1, Ordering::Relaxed);
        }
        slot.job = Some(NativeJob {
            generation,
            query: query.to_owned(),
            now,
            builtin_rows: builtin,
            builtin_pending,
            selected,
        });
        drop(slot);
        cvar.notify_one();
    }

    /// Takes the newest merged frame published by the supervisor, if any.
    pub fn take_outcome(&self) -> Option<ViewModel> {
        self.outcome
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    /// Whether at least one native plugin loaded at provider startup.
    pub fn has_plugins(&self) -> bool {
        self.has_plugins
    }

    /// Takes bounded catalog outcomes collected by the provider supervisor.
    /// Complete results remain instance/generation tagged for the caller's
    /// stale-safe catalog publication path.
    pub fn take_catalog_results(&self) -> Vec<CatalogBuildResult> {
        std::mem::take(
            &mut *self
                .catalog_results
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
    }

    /// Whether the provider thread is currently executing a submitted query.
    /// This is an observation hook for bounded-mailbox diagnostics and does
    /// not block the caller.
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    /// Number of pending jobs replaced while the provider was busy.
    pub fn pending_replacements(&self) -> u64 {
        self.replacements.load(Ordering::Acquire)
    }

    /// Returns the exact plugin ids owned by this driver's action endpoint.
    pub fn plugins(&self) -> Vec<PluginId> {
        self.action_endpoint.budgets.keys().cloned().collect()
    }

    /// Returns the bounded action endpoint sharing this driver's per-plugin
    /// budget handles.
    pub fn action_executor(&self) -> Arc<dyn crate::PluginActionExecutor> {
        self.action_endpoint.clone()
    }
}

impl Drop for NativeDriver {
    fn drop(&mut self) {
        // Cancel in-flight callbacks before joining the supervisor so a
        // cooperative native worker gets a chance to return promptly.
        self.cancellation.cancel_all();
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
