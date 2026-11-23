//! Live modern-Python plugin provider (contract §8; spec 15.6, 15.7, 24.1;
//! acceptance 31.7, 31.9, 31.10).
//!
//! This is the modern sibling of [`legacy_provider`](crate::legacy_provider).
//! It is the composition edge that makes managed-environment Python plugins part
//! of the *running* launcher rather than only of the `crikey dev` commands. It
//! discovers modern packages from configured roots, resolves and materialises
//! each one's content-addressed environment, starts one out-of-process CPython
//! [`ModernWorker`] per plugin, and drives them through the app's
//! [`QueryPipeline`] so a query typed into the launcher reaches every loaded
//! modern plugin and their published suggestions cross the same bounded intake
//! and presentation boundary the built-in application provider uses.
//!
//! # Isolation
//!
//! Modern Python never runs in the CriKey process: every `suggest` callback
//! executes in a child interpreter owned by a [`ModernWorker`], spawned with
//! `-S` and a host-assembled `PYTHONPATH` that excludes global site-packages
//! (spec 15.4). The blocking child call lives in
//! [`ModernProvider::collect_suggestions`] and is driven, in production, by the
//! [`ModernDriver`] supervisor thread so the user-interface thread never blocks
//! on a child interpreter.
//!
//! # Containment
//!
//! Every failure is contained (spec 24.1, acceptance 31.9, 31.10): a package
//! that will not load, a worker that will not spawn, an interpreter that crashes
//! mid-callback, or a plugin callback that raises degrades to "that plugin is
//! unavailable" with a recorded diagnostic. None of them aborts discovery,
//! wedges the pipeline, or takes down the process. Because each plugin runs in
//! its own worker process, a crash in one plugin never disturbs a healthy
//! sibling.
//!
//! # Generation tagging
//!
//! A query mints a pipeline generation, each loaded plugin's suggestions are
//! delivered under exactly that generation, and the presented frame is only
//! returned when it belongs to the current generation — so a superseded answer
//! is refused at the pipeline's intake boundary rather than shown out of order
//! (acceptance 31.7).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::preview_action_budget::{ForegroundActionAdmission, PreviewActionGate};
use crikey_core::{ActionId, ArgumentPolicy, ExecutionPolicy, Generation, Item, ItemId, PluginId};
use crikey_input_scheduler::Millis;
use crikey_package_manager::{
    resolve, EnvironmentInputs, EnvironmentStore, ImportPath, PackageError, PackageIndex,
};
use crikey_plugin_model::{Manifest, Permissions, Runtime, Startup};
use crikey_plugin_supervisor::{BudgetKind, PluginBudgetHandle};
use crikey_python_host::{
    discover_interpreter, sdk_root, BatchState as WorkerBatchState, CancelHandle, ExecuteOutcome,
    Interpreter, ModernWorker, RequiresPython, RuntimeCatalog, SuggestRequest, WorkerOptions,
    WORKER_ENTRY_FILE,
};
use crikey_ui::{ResultRow, ViewModel};

use crate::plugin_preview::{
    PreviewDispatcher, PreviewIdentity, PreviewPublish, PreviewRequest, PreviewState, PreviewUpdate,
};
use crate::{
    plugin_icons::PluginIconResolver, ActionRequestId, BatchState, CatalogBuildResult, CatalogDispatchError,
    DisabledPlugins, ObsoleteCatalogBuild, PluginActionCompletion, QueryPipeline, ResultBatch,
    DISABLED_BY_CONFIGURATION,
};
use crikey_core::{PreviewContent, PreviewImage, PreviewText, PreviewValue, PreviewValueKind};

/// Shared item snapshots paired with their producing source epoch.
type ModernItemStore = Arc<Mutex<BTreeMap<(PluginId, ItemId), (Item, u64)>>>;

struct ModernPreviewJob {
    request: PreviewRequest,
    publish: PreviewPublish,
    cancelled: Arc<AtomicBool>,
}

struct ModernPreviewEndpoint {
    sender: SyncSender<ModernPreviewJob>,
    gates: BTreeMap<PluginId, Arc<PreviewActionGate>>,
    action_in_flight: Arc<AtomicUsize>,
    incarnations: Mutex<BTreeMap<PluginId, u64>>,
    items: ModernItemStore,
    catalog_items: ModernItemStore,
    pending: Mutex<Vec<(PreviewIdentity, Arc<AtomicBool>)>>,
    stopped: AtomicBool,
    wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl std::fmt::Debug for ModernPreviewEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModernPreviewEndpoint").finish_non_exhaustive()
    }
}

impl ModernPreviewEndpoint {
    fn new(provider: &ModernProvider, sender: SyncSender<ModernPreviewJob>) -> Arc<Self> {
        let endpoint = Arc::new(Self {
            sender,
            gates: provider
                .action_budgets()
                .into_iter()
                .map(|(owner, budget)| (owner, PreviewActionGate::new(budget)))
                .collect(),
            action_in_flight: Arc::new(AtomicUsize::new(0)),
            incarnations: Mutex::new(BTreeMap::new()),
            items: Arc::clone(&provider.action_items),
            catalog_items: Arc::clone(&provider.catalog.items),
            pending: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            wake: Mutex::new(None),
        });
        endpoint.refresh_incarnations(provider);
        endpoint
    }

    fn refresh_incarnations(&self, provider: &ModernProvider) {
        let mut changed = false;
        for loaded in &provider.loaded {
            let connection = match provider.pool.workers.get(&loaded.key) {
                Some(WorkerLifecycle::Live(worker)) if worker.is_alive() => Some(worker.incarnation()),
                Some(WorkerLifecycle::CatalogBusy) => continue,
                _ => None,
            };
            // Never retain the UI snapshot lock across process cleanup.
            let mut snapshot = self
                .incarnations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            match connection {
                Some(connection) if snapshot.get(&loaded.plugin) == Some(&connection) => {}
                Some(connection) => {
                    snapshot.insert(loaded.plugin.clone(), connection);
                    changed = true;
                }
                None => changed |= snapshot.remove(&loaded.plugin).is_some(),
            }
        }
        if changed {
            let wake = self
                .wake
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            if let Some(wake) = wake {
                wake();
            }
        }
    }
    /// Runs only on the supervisor. Publish retirement before dropping a worker,
    /// whose bounded pipe cleanup must never delay the UI's invalidation wake.
    fn refresh_liveness(&self, provider: &mut ModernProvider) {
        for state in provider.pool.workers.values_mut() {
            if let WorkerLifecycle::Live(worker) = state {
                worker.poll_alive();
            }
        }
        self.refresh_incarnations(provider);
        for loaded in &provider.loaded {
            if matches!(provider.pool.workers.get(&loaded.key),
                Some(WorkerLifecycle::Live(worker)) if !worker.is_alive())
            {
                let reason = "the modern worker is no longer alive".to_owned();
                provider.pool.fail(&loaded.key, reason.clone());
                provider
                    .pool
                    .record_dispatch_failure(loaded.plugin.clone(), reason);
            }
        }
    }
    fn wake(&self) {
        let wake = self
            .wake
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(wake) = wake {
            wake();
        }
    }

    fn complete(&self, job: ModernPreviewJob, state: PreviewState) {
        let identity = job.request.identity;
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|(candidate, _)| candidate != &identity);
        let state = if job.cancelled.load(Ordering::Acquire) {
            PreviewState::Cancelled
        } else {
            state
        };
        (job.publish)(PreviewUpdate { identity, state });
    }

    /// Runs on the existing query supervisor, using the exact child instance
    /// which emitted the selected suggestion. Never starts a preview process.
    fn execute(&self, provider: &mut ModernProvider, job: ModernPreviewJob) {
        let identity = &job.request.identity;
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<PreviewState, String> {
            if job.cancelled.load(Ordering::Acquire) || self.action_in_flight.load(Ordering::Acquire) != 0 {
                return Ok(PreviewState::Cancelled);
            }
            let loaded = provider
                .loaded
                .iter()
                .find(|loaded| loaded.plugin == identity.plugin)
                .ok_or("modern preview owner unavailable")?;
            let gate = self
                .gates
                .get(&loaded.plugin)
                .ok_or("modern preview owner budget unavailable")?;
            let Some(_guard) = gate.try_preview(job.cancelled.clone()) else {
                return Ok(PreviewState::Cancelled);
            };
            if self.action_in_flight.load(Ordering::Acquire) != 0 {
                return Ok(PreviewState::Cancelled);
            }
            let worker = provider
                .pool
                .live_mut(&loaded.key)
                .ok_or("originating modern preview worker is unavailable")?;
            if !worker.is_alive() || worker.incarnation() != identity.incarnation {
                return Ok(PreviewState::Unavailable(
                    "originating preview worker was retired".into(),
                ));
            }
            let (current, origin) = self
                .lookup(&identity.plugin, &identity.item)
                .ok_or("selected modern preview item is no longer current")?;
            if origin != identity.incarnation {
                return Ok(PreviewState::Unavailable(
                    "selected item belongs to a retired worker".into(),
                ));
            }
            worker.cancel_handle().reset();
            let control = Arc::new(ModernCallControl::new());
            control.install(worker.cancel_handle());
            let watcher_control = Arc::clone(&control);
            let token = Arc::clone(&job.cancelled);
            let watcher = thread::Builder::new()
                .name("crikey-preview-cancel".into())
                .spawn(move || {
                    while !watcher_control.finished.load(Ordering::Acquire) {
                        if token.load(Ordering::Acquire) {
                            watcher_control.cancel();
                            Arc::clone(&watcher_control).watch_cancel();
                            break;
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                })
                .map_err(|error| error.to_string())?;
            let wire_identity = serde_json::json!({
                "view":identity.view, "query":identity.query.get(),
                "plugin":identity.plugin.0, "incarnation":identity.incarnation,
                "item":identity.item.0, "selection":identity.selection, "request":identity.request,
            });
            if job.cancelled.load(Ordering::Acquire) {
                control.cancel();
            }
            let reply = worker.preview(&current, &wire_identity);
            control.finish();
            let _ = watcher.join();
            if job.cancelled.load(Ordering::Acquire) {
                return Ok(PreviewState::Cancelled);
            }
            decode_modern_preview(reply.map_err(|error| error.to_string())?)
        }));
        let state = match result {
            Ok(Ok(state)) => state,
            Ok(Err(reason)) => PreviewState::Error(reason),
            Err(_) => PreviewState::Error("modern preview supervisor panicked".into()),
        };
        self.refresh_liveness(provider);
        self.complete(job, state);
    }

    fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        for (_, token) in self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
        {
            token.store(true, Ordering::Release);
        }
    }
}

impl PreviewDispatcher for ModernPreviewEndpoint {
    fn foreground_blocked(&self, _: &PluginId) -> bool {
        self.action_in_flight.load(Ordering::Acquire) != 0
    }
    fn lookup(&self, plugin: &PluginId, item: &ItemId) -> Option<(Item, u64)> {
        let key = (plugin.clone(), item.clone());
        let query = self
            .items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .cloned();
        if query
            .as_ref()
            .is_some_and(|(_, origin)| self.incarnation(plugin) == Some(*origin))
        {
            return query;
        }
        self.catalog_items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .cloned()
            .or(query)
    }
    fn origin(&self, plugin: &PluginId, item: &ItemId) -> Option<u64> {
        let key = (plugin.clone(), item.clone());
        let query = self
            .items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .map(|(_, origin)| *origin);
        if query.is_some() && query == self.incarnation(plugin) {
            return query;
        }
        self.catalog_items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .map(|(_, origin)| *origin)
            .or(query)
    }

    fn incarnation(&self, plugin: &PluginId) -> Option<u64> {
        if self.stopped.load(Ordering::Acquire) {
            return None;
        }
        self.incarnations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(plugin)
            .copied()
    }
    fn set_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        *self.wake.lock().unwrap_or_else(|error| error.into_inner()) = Some(wake);
    }

    fn submit(&self, request: PreviewRequest, publish: PreviewPublish) -> Result<(), String> {
        let id = &request.identity;
        if self.incarnation(&id.plugin) != Some(id.incarnation) {
            return Err("modern preview owner incarnation unavailable".into());
        }
        let (item, origin) = self
            .lookup(&id.plugin, &id.item)
            .ok_or("selected modern preview item is no longer current")?;
        if origin != id.incarnation {
            return Err("selected modern item belongs to a retired worker".into());
        }
        if item.metadata.get("crikey.preview").map(String::as_str) != Some("deferred") {
            return Err("selected modern item does not declare deferred preview".into());
        }
        let mut pending = self.pending.lock().unwrap_or_else(|error| error.into_inner());
        if pending.len() >= 2 || pending.iter().any(|(candidate, _)| candidate == id) {
            return Err("modern preview mailbox is full".into());
        }
        let identity = id.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        pending.push((identity.clone(), Arc::clone(&cancelled)));
        if self
            .sender
            .try_send(ModernPreviewJob {
                request,
                publish,
                cancelled,
            })
            .is_err()
        {
            pending.retain(|(candidate, _)| candidate != &identity);
            return Err("modern preview mailbox is full or unavailable".into());
        }
        Ok(())
    }

    fn cancel(&self, identity: &PreviewIdentity) {
        for (candidate, token) in self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
        {
            if candidate == identity {
                token.store(true, Ordering::Release);
            }
        }
    }
}

fn decode_modern_preview(reply: serde_json::Value) -> Result<PreviewState, String> {
    use serde_json::Value;
    let invalid = || "invalid modern preview content".to_owned();
    match reply.get("status").and_then(Value::as_str) {
        Some("empty") => Ok(PreviewState::Empty),
        Some("unsupported") => Ok(PreviewState::Unavailable(
            "worker does not support preview_v1".into(),
        )),
        Some("cancelled") => Ok(PreviewState::Cancelled),
        Some("failed") => {
            let detail = reply
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("preview callback failed");
            let mut end = detail.len().min(8192);
            while !detail.is_char_boundary(end) {
                end -= 1;
            }
            Ok(PreviewState::Error(detail[..end].to_owned()))
        }
        Some("content") => {
            let value = reply.get("content").ok_or_else(invalid)?;
            let text = match value.get("text") {
                Some(Value::Null) | None => None,
                Some(Value::String(text)) => {
                    if text.len() > 256 * 1024 {
                        return Err(invalid());
                    }
                    Some(
                        if value.get("markdown").and_then(Value::as_bool).unwrap_or(false) {
                            PreviewText::Markdown(text.clone())
                        } else {
                            PreviewText::Plain(text.clone())
                        },
                    )
                }
                _ => return Err(invalid()),
            };
            let image = match value.get("image") {
                Some(Value::Null) | None => None,
                Some(image) => {
                    let width = image.get("width").and_then(Value::as_u64).ok_or_else(invalid)?;
                    let height = image.get("height").and_then(Value::as_u64).ok_or_else(invalid)?;
                    if width == 0
                        || height == 0
                        || width > 1024
                        || height > 1024
                        || width * height * 4 > 1024 * 1024
                    {
                        return Err(invalid());
                    }
                    let bytes = image.get("rgba").and_then(Value::as_array).ok_or_else(invalid)?;
                    if bytes.len() as u64 != width * height * 4 {
                        return Err(invalid());
                    }
                    let rgba = bytes
                        .iter()
                        .map(|value| {
                            value
                                .as_u64()
                                .and_then(|value| u8::try_from(value).ok())
                                .ok_or_else(invalid)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    Some(PreviewImage {
                        width: width as u32,
                        height: height as u32,
                        rgba,
                    })
                }
            };
            let entries = value
                .get("values")
                .and_then(Value::as_array)
                .ok_or_else(invalid)?;
            if entries.len() > 128 {
                return Err(invalid());
            }
            let mut values = Vec::with_capacity(entries.len());
            for entry in entries {
                let label = entry.get("label").and_then(Value::as_str).ok_or_else(invalid)?;
                let value = entry.get("value").and_then(Value::as_str).ok_or_else(invalid)?;
                if label.len() > 8192 || value.len() > 8192 {
                    return Err(invalid());
                }
                let kind = match entry.get("kind").and_then(Value::as_str) {
                    Some("text") => PreviewValueKind::Text,
                    Some("link") => PreviewValueKind::Link,
                    Some("tag") => PreviewValueKind::Tag,
                    _ => return Err(invalid()),
                };
                values.push(PreviewValue {
                    label: label.into(),
                    value: value.into(),
                    kind,
                });
            }
            let content = PreviewContent { text, image, values };
            content.validate().map_err(|error| error.to_string())?;
            let prepared = crikey_ui::prepare_preview(Arc::new(content))?;
            Ok(PreviewState::Content(Arc::new(prepared)))
        }
        _ => Err(invalid()),
    }
}

/// Bound on the startup handshake with a child interpreter, in milliseconds.
/// A liveness guard: a worker that never answers becomes a recorded unavailable
/// plugin rather than a launcher that hangs on startup.
const STARTUP_BUDGET_MS: Millis = 30_000;

/// Upper bound on one modern `suggest` call, in milliseconds. The effective
/// deadline comes from `performance.suggest-hard-timeout-ms` and defaults to
/// the manifest model's 500 ms value.
const CALL_BUDGET_MS: Millis = 120_000;

/// Bound on the cooperative teardown of one modern worker, in milliseconds.
const SHUTDOWN_BUDGET_MS: Millis = 5_000;

/// A default requires-python constraint for a manifest that declares none, kept
/// permissive so an unconstrained plugin still finds any supported interpreter.
const DEFAULT_REQUIRES_PYTHON: &str = ">=3.8";

/// Identifies a shared worker process by the environment it runs in, the
/// entrypoint it hosts, *and* the plugin source directory it was born with.
///
/// The protocol has no per-call plugin routing, so a shared worker answers with
/// the code it was started with; keying on `(environment, entrypoint)` alone
/// would let two genuinely distinct plugins that happen to share an environment
/// id and an entrypoint string collapse onto one worker, which then serves the
/// wrong plugin's results. Adding the plugin source dir (pinned decision 1)
/// means a worker is shared only by a genuinely identical plugin; distinct
/// sources always get distinct workers, which is also what keeps one plugin's
/// crash from reaching a sibling (acceptance 31.10). The tuple is
/// `(environment_id, entrypoint, source_dir)`.
type WorkerKey = (String, String, String);

/// One modern package that could not be made to serve suggestions, and why.
///
/// A diagnostic, never a panic: the launcher keeps every other plugin. The
/// `plugin` is present once identity is known (the package loaded far enough to
/// have a namespaced id) and absent when the package itself never loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModernUnavailable {
    /// The package the user can recognize, spelled as discovery reported it.
    pub package: String,
    /// The host plugin id, once the package loaded far enough to have one.
    pub plugin: Option<PluginId>,
    /// A single-line, attributable reason (spec 26.2).
    pub reason: String,
}

/// One loaded modern plugin: its host identity, query worker key, package
/// directory for package-relative resources, and the immutable recipe used to
/// start an independent catalog worker.
#[derive(Debug, Clone)]
struct LoadedPlugin {
    plugin: PluginId,
    key: WorkerKey,
    /// The installed package directory, never a path supplied by plugin output.
    package_dir: PathBuf,
    interpreter: Interpreter,
    worker_options: WorkerOptions,
    budget: PluginBudgetHandle,
    soft_timeout: Duration,
    permissions: Permissions,
    /// Whether this plugin's catalog may be written to the persistent cache.
    catalog_persist: bool,
}

/// Maximum number of catalog tasks retained before the host drains results.
///
/// Admission counts active and completed-but-undrained tasks together, so the
/// synchronous result channel cannot back up behind a hidden UI.
const CATALOG_RESULT_CAPACITY: usize = 64;

/// One catalog build running away from the query worker.
#[derive(Debug)]
struct ModernCatalogTask {
    join: Option<JoinHandle<()>>,
    key: WorkerKey,
}

/// Returning a healthy, deliberately released lazy producer is not a failure.
#[derive(Debug)]
enum CatalogWorkerReturn {
    Retained(ModernWorker),
    Released,
    Failed(String),
}

/// Bounded catalog request/result mailbox.
///
/// Admission bounds the number of task threads and retained results. Each
/// admitted task emits at most one result, so a synchronous channel of the
/// same capacity never blocks a worker that is completing a legal task.
#[derive(Debug)]
struct ModernCatalogDispatcher {
    result_tx: SyncSender<(u64, CatalogBuildResult, CatalogWorkerReturn)>,
    result_rx: Receiver<(u64, CatalogBuildResult, CatalogWorkerReturn)>,
    tasks: BTreeMap<u64, ModernCatalogTask>,
    latest: BTreeMap<PluginId, u64>,
    next_id: u64,
    items: ModernItemStore,
}

impl Default for ModernCatalogDispatcher {
    fn default() -> Self {
        let (result_tx, result_rx) = mpsc::sync_channel(CATALOG_RESULT_CAPACITY);
        Self {
            result_tx,
            result_rx,
            tasks: BTreeMap::new(),
            latest: BTreeMap::new(),
            next_id: 0,
            items: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}

impl ModernCatalogDispatcher {
    fn next_id(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.next_id
    }

    fn insert(&mut self, id: u64, plugin: PluginId, key: WorkerKey, join: JoinHandle<()>) {
        self.latest.insert(plugin.clone(), id);
        self.tasks.insert(
            id,
            ModernCatalogTask {
                join: Some(join),
                key,
            },
        );
    }

    fn take(&mut self, pool: &mut ModernWorkerPool) -> Vec<CatalogBuildResult> {
        let mut ready = Vec::new();
        while let Ok((id, result, worker)) = self.result_rx.try_recv() {
            let Some(mut task) = self.tasks.remove(&id) else {
                continue;
            };
            if let Some(join) = task.join.take() {
                let _ = join.join();
            }
            let incarnation = match worker {
                CatalogWorkerReturn::Retained(worker) => {
                    let incarnation = worker.incarnation();
                    pool.workers.insert(task.key, WorkerLifecycle::Live(worker));
                    incarnation
                }
                CatalogWorkerReturn::Released => {
                    pool.workers.insert(task.key, WorkerLifecycle::NeverStarted);
                    0
                }
                CatalogWorkerReturn::Failed(reason) => {
                    pool.fail(&task.key, reason);
                    0
                }
            };
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
            if let CatalogBuildResult::Complete(build) = &result {
                let mut items = self.items.lock().unwrap_or_else(|error| error.into_inner());
                items.retain(|(owner, _), _| owner != &build.plugin);
                if incarnation != 0 {
                    for item in &build.items {
                        items.insert(
                            (build.plugin.clone(), item.stable_id.clone()),
                            (item.clone(), incarnation),
                        );
                    }
                }
            }
            ready.push(result);
        }
        ready
    }

    fn shutdown(&mut self, pool: &mut ModernWorkerPool) -> Vec<CatalogBuildResult> {
        for task in self.tasks.values_mut() {
            if let Some(join) = task.join.take() {
                let _ = join.join();
            }
        }
        let results = self.take(pool);
        self.latest.clear();
        self.tasks.clear();
        results
    }
}

/// Cancellation state for one in-flight modern suggestion call.
///
/// The UI submission path can supersede a query while the provider thread is
/// blocked reading the child response. A short-lived watcher owns the actual
/// control-frame write so `submit` never inherits pipe backpressure.
#[derive(Debug)]
struct ModernCallControl {
    requested: AtomicBool,
    finished: AtomicBool,
    handle: Mutex<Option<CancelHandle>>,
    wake: Condvar,
    wake_lock: Mutex<()>,
}

impl ModernCallControl {
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
            handle.cancel();
        }
        self.wake.notify_all();
    }

    fn cancel(&self) {
        self.requested.store(true, Ordering::Release);
        self.wake.notify_all();
    }

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

#[derive(Debug, Default)]
struct ModernCancellation {
    calls: Mutex<BTreeMap<(u64, PluginId), Arc<ModernCallControl>>>,
    /// Monotonic count of queries the driver's intake has admitted.
    ///
    /// Supersession has to be observable by the provider even while it is doing
    /// work that has no registered call to cancel — a lazy plugin's blocking
    /// startup handshake. A plain counter is used rather than a search
    /// generation because the UI's generation and the pipeline's are two
    /// independent sequences, and the provider only ever sees the latter. Each
    /// submission stamps its job with the value it produced here, so a provider
    /// serving a stamp older than the current count is serving an obsolete
    /// query. It lives on the registry because that is the one piece of
    /// supersession state the driver and the moved-out provider already share.
    intake: AtomicU64,
}

impl ModernCancellation {
    fn register(&self, generation: u64, plugin: PluginId, control: Arc<ModernCallControl>) {
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

    /// Admits one query and returns the stamp identifying it.
    fn admit(&self) -> u64 {
        self.intake.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// The stamp of the newest admitted query.
    fn intake(&self) -> u64 {
        self.intake.load(Ordering::Acquire)
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

/// Lifecycle of one shared modern worker slot.
///
/// Modelled explicitly because map occupancy is the wrong thing to decide a
/// spawn on. A worker that crashes has to stop being dispatched to, and the
/// obvious way to arrange that — removing it from the map — makes it
/// indistinguishable from a lazy plugin that has never started, so the very
/// next keystroke starts it again. A plugin that crashes on every query then
/// pays interpreter startup, and re-runs its import side effects, on every
/// keystroke while `recorded` suppresses every diagnostic after the first.
///
/// A child may therefore only be spawned from [`Self::NeverStarted`], and
/// [`Self::Failed`] is left only through an explicit supervised restart
/// ([`ModernProvider::restart_worker`]).
enum WorkerLifecycle {
    /// No child has ever been started for this key. The only state a spawn is
    /// allowed from.
    NeverStarted,
    /// A child is running and may be dispatched to.
    Live(ModernWorker),
    /// The same child is temporarily owned by an admitted catalog callback.
    CatalogBusy,
    /// The child failed to start, crashed, or lost its transport. The plugin
    /// stays unavailable with this reason until a supervised restart.
    Failed { reason: String },
}

impl WorkerLifecycle {
    /// A short human-readable state used in diagnostics. `ModernWorker` is not
    /// `Debug`, so the pool's own formatter renders this instead.
    fn describe(&self) -> String {
        match self {
            Self::NeverStarted => "never-started".to_owned(),
            Self::Live(_) => "live".to_owned(),
            Self::CatalogBusy => "catalog-busy".to_owned(),
            Self::Failed { reason } => format!("failed: {reason}"),
        }
    }
}

/// Owns one child process per shared worker key and records every runtime
/// dispatch failure.
///
/// This is the app-side pool the contract names: it keys live workers so that
/// truly identical plugins share a process while distinct entrypoints stay in
/// separate processes. Content-addressed *environment* reuse is delegated to the
/// [`EnvironmentStore`], which materialises one directory per environment id.
#[derive(Default)]
struct ModernWorkerPool {
    workers: BTreeMap<WorkerKey, WorkerLifecycle>,
    failures: Vec<(PluginId, String)>,
    /// Saturating count of callbacks that exceeded their soft deadline.
    soft_timeouts: BTreeMap<PluginId, u32>,
    /// Plugins already recorded as a dispatch failure, so a worker that dies is
    /// recorded once rather than every keystroke (this bounds `failures`).
    recorded: std::collections::BTreeSet<PluginId>,
}

impl std::fmt::Debug for ModernWorkerPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModernWorkerPool")
            .field(
                "workers",
                &self
                    .workers
                    .iter()
                    .map(|(key, state)| (key, state.describe()))
                    .collect::<Vec<_>>(),
            )
            .field("failures", &self.failures)
            .field("soft_timeouts", &self.soft_timeouts)
            .finish()
    }
}

impl ModernWorkerPool {
    /// Records a runtime dispatch failure for `plugin` at most once, so a worker
    /// that stays dead across many keystrokes bounds `failures` rather than
    /// growing it without limit.
    fn record_dispatch_failure(&mut self, plugin: PluginId, reason: String) {
        if self.recorded.insert(plugin.clone()) {
            self.failures.push((plugin, reason));
        }
    }
    /// The live child for `key`, or `None` when the slot has never started or
    /// has failed. Callers dispatch only through this, so a failed slot can
    /// never be mistaken for a startable one.
    fn live_mut(&mut self, key: &WorkerKey) -> Option<&mut ModernWorker> {
        match self.workers.get_mut(key) {
            Some(WorkerLifecycle::Live(worker)) => Some(worker),
            Some(
                WorkerLifecycle::NeverStarted | WorkerLifecycle::Failed { .. } | WorkerLifecycle::CatalogBusy,
            )
            | None => None,
        }
    }

    /// The reason `key`'s worker is in the failed state, if it is.
    fn failure_reason(&self, key: &WorkerKey) -> Option<&str> {
        match self.workers.get(key) {
            Some(WorkerLifecycle::Failed { reason }) => Some(reason.as_str()),
            _ => None,
        }
    }

    /// Whether `key` has never had a child, and may therefore be spawned.
    fn never_started(&self, key: &WorkerKey) -> bool {
        matches!(self.workers.get(key), Some(WorkerLifecycle::NeverStarted) | None)
    }

    /// Retires `key` into the failed state. Any child held in the slot is
    /// dropped here, which reaps it, and nothing may dispatch to or respawn
    /// the slot until a supervised restart.
    fn fail(&mut self, key: &WorkerKey, reason: String) {
        self.workers
            .insert(key.clone(), WorkerLifecycle::Failed { reason });
    }

    fn record_soft_timeout(&mut self, plugin: PluginId) {
        let count = self.soft_timeouts.entry(plugin).or_default();
        *count = count.saturating_add(1);
    }
}

/// Composes modern discovery, the per-plugin worker pool and the app's query
/// pipeline.
///
/// Constructed once at startup with [`ModernProvider::load`], then driven per
/// keystroke with [`ModernProvider::drive_query`]. It owns every worker (through
/// the pool) so dropping the provider tears down every child interpreter.
#[derive(Debug)]
pub struct ModernProvider {
    pool: ModernWorkerPool,
    loaded: Vec<LoadedPlugin>,
    catalog: ModernCatalogDispatcher,
    plugins: Vec<PluginId>,
    unavailable: Vec<ModernUnavailable>,
    /// Catalog-persistence declarations, keyed by plugin, recorded as soon as
    /// a manifest is parsed and independent of whether the plugin then loaded.
    catalog_declarations: BTreeMap<PluginId, bool>,
    /// Current async suggestion snapshots keyed by the owning plugin and item
    /// id, atomically paired with the incarnation that produced each suggestion.
    /// Stable item ids are only unique within an owner.
    action_items: ModernItemStore,
    cancellation: Arc<ModernCancellation>,
    /// Intake stamp of the query currently being served, set by the supervisor
    /// before each `drive_query`. Zero when nothing drives this provider
    /// through an intake, in which case the registry's counter is also zero and
    /// no work is ever considered obsolete.
    serving_intake: u64,
}

impl ModernProvider {
    /// Discovers modern python packages under `roots`, resolves and materialises
    /// each one's environment against an offline index rooted at `index_root`
    /// and an [`EnvironmentStore`] at `cache_root`, starts a worker for each
    /// usable one, and registers it with `pipeline`.
    ///
    /// Never returns an error: a failure at any step becomes a recorded
    /// [`ModernUnavailable`] and the remaining packages are loaded anyway
    /// (acceptance 31.9, 31.10).
    pub fn load(
        pipeline: &mut QueryPipeline,
        roots: &[PathBuf],
        index_root: Option<PathBuf>,
        cache_root: PathBuf,
        disabled: &DisabledPlugins,
    ) -> Self {
        let mut provider = Self {
            catalog_declarations: BTreeMap::new(),
            pool: ModernWorkerPool::default(),
            loaded: Vec::new(),
            catalog: ModernCatalogDispatcher::default(),
            plugins: Vec::new(),
            unavailable: Vec::new(),
            action_items: Arc::new(Mutex::new(BTreeMap::new())),
            cancellation: Arc::new(ModernCancellation::default()),
            serving_intake: 0,
        };

        // The worker shim must be on disk before any child can speak the
        // protocol; saying so once is more honest than reporting every package
        // as broken for the host's reason.
        let sdk = sdk_root();
        if !sdk.join(WORKER_ENTRY_FILE).is_file() {
            provider.unavailable.push(ModernUnavailable {
                package: String::new(),
                plugin: None,
                reason: format!(
                    "the modern worker entry `{WORKER_ENTRY_FILE}` is not in `{}`",
                    sdk.display()
                ),
            });
            return provider;
        }

        // An offline, deterministic index is the source of every resolvable
        // dependency. When no index root is configured the index is EMPTY rather
        // than a shared, world-writable directory: an empty index resolves no
        // declared dependency, so a plugin that declares one is recorded
        // unavailable with a clear resolution reason (below) while a
        // dependency-free plugin still loads. This keeps the hash-verification
        // trust root off a predictable, attacker-writable path (spec 15.4).
        let index = match Self::open_index(index_root.as_deref(), &cache_root) {
            Ok(index) => index,
            Err(error) => {
                provider.unavailable.push(ModernUnavailable {
                    package: String::new(),
                    plugin: None,
                    reason: format!("the offline package index failed to load: {error}"),
                });
                return provider;
            }
        };
        let store = EnvironmentStore::new(cache_root);

        // Probed once for the whole load: mapping each plugin's requires-python
        // to an interpreter needs to know which versions exist, and that answer
        // only comes from running them. Per-plugin probing would multiply
        // startup spawns by the number of interpreters installed (spec 14.11).
        let runtimes = RuntimeCatalog::for_process();

        for root in roots {
            let entries = match fs::read_dir(root) {
                Ok(entries) => entries,
                Err(error) => {
                    provider.unavailable.push(ModernUnavailable {
                        package: root.display().to_string(),
                        plugin: None,
                        reason: format!("cannot scan modern plugin root: {error}"),
                    });
                    continue;
                }
            };

            // Deterministic discovery order keeps the loaded set reproducible
            // across runs regardless of directory iteration order.
            let mut dirs: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.join("crikey.toml").is_file())
                .collect();
            dirs.sort();

            let context = ModernLoadContext {
                index: &index,
                store: &store,
                runtimes: &runtimes,
                sdk: &sdk,
                disabled,
            };
            for dir in dirs {
                provider.register_plugin_dir(pipeline, &context, &dir);
            }
        }
        // Modern plugin icon references are package-relative. Install one
        // immutable resolver after discovery so failed packages have no origin
        // and every loaded package is resolved under its own directory.
        provider.install_icon_resolver(pipeline);

        provider
    }
    /// Installs package-relative icon origins for every loaded modern plugin.
    ///
    /// The resolver never trusts a path emitted by a plugin: it joins only
    /// references that pass its relative-component check to this directory,
    /// and only for a plugin whose manifest permits the host-mediated package
    /// read in the first place.
    fn install_icon_resolver(&self, pipeline: &mut QueryPipeline) {
        let mut resolver = PluginIconResolver::default();
        for loaded in &self.loaded {
            resolver.insert_package(&loaded.plugin, loaded.package_dir.clone(), &loaded.permissions);
        }
        pipeline.set_plugin_icons(Arc::new(resolver));
    }

    /// Opens the offline package index, or an empty one when no index root is
    /// configured (see [`Self::load`]). The empty index lives under the per-user
    /// cache root, never a shared temporary directory, so it can never be a
    /// predictable world-writable trust root.
    fn open_index(index_root: Option<&Path>, cache_root: &Path) -> Result<PackageIndex, PackageError> {
        match index_root {
            Some(root) => PackageIndex::from_dir(root),
            None => {
                let empty = cache_root.join(".empty-index");
                fs::create_dir_all(&empty)?;
                PackageIndex::from_dir(&empty)
            }
        }
    }
}

/// Everything a modern plugin load needs that is the same for every candidate
/// directory.
///
/// Grouped rather than passed one by one: these five are resolved once per
/// `load` and never vary between packages, so threading them individually made
/// the per-directory signature grow every time a slice needed one more piece of
/// shared state, which is what a context type is for.
#[derive(Clone, Copy)]
struct ModernLoadContext<'a> {
    index: &'a PackageIndex,
    store: &'a EnvironmentStore,
    runtimes: &'a RuntimeCatalog,
    sdk: &'a Path,
    disabled: &'a DisabledPlugins,
}

impl ModernProvider {
    /// Loads one candidate `<dir>/crikey.toml`, resolves its environment, spawns
    /// (or reuses) its worker and registers it with the pipeline. Any failure is
    /// recorded and the function returns without disturbing other plugins.
    fn register_plugin_dir(
        &mut self,
        pipeline: &mut QueryPipeline,
        context: &ModernLoadContext<'_>,
        dir: &Path,
    ) {
        let ModernLoadContext {
            index,
            store,
            runtimes,
            sdk,
            disabled,
        } = *context;
        let package = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.display().to_string());

        let text = match fs::read_to_string(dir.join("crikey.toml")) {
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

        // Only modern-python packages are this provider's concern; a package of
        // any other runtime is silently left to its own host.
        if manifest.plugin.runtime != Runtime::Python {
            return;
        }

        let plugin = PluginId(format!("modern.{}", manifest.plugin.id));
        // Recorded the moment the manifest is understood, and deliberately not
        // from `loaded`: a refusal has to withdraw an earlier run's slice even
        // when this run never gets the plugin started (spec 22.1).
        self.catalog_declarations
            .insert(plugin.clone(), manifest.catalog.persist);
        // Held back before an environment is materialised and before a worker is
        // spawned: an operator who disabled a plugin must not pay for its
        // process or its dependency closure (spec 21.2).
        if disabled.blocks(&plugin) {
            self.record_unavailable(package, Some(plugin), DISABLED_BY_CONFIGURATION.to_owned());
            return;
        }

        let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
        let entrypoint = match manifest.entrypoint_for(os, arch) {
            Ok(entrypoint) => entrypoint.to_owned(),
            Err(error) => {
                self.record_unavailable(package, Some(plugin), format!("no usable entrypoint: {error}"));
                return;
            }
        };

        let requires_python = manifest
            .python
            .requires_python
            .clone()
            .unwrap_or_else(|| DEFAULT_REQUIRES_PYTHON.to_owned());
        let dependencies = manifest.python.dependencies.clone();

        // The manifest's requires-python selects the interpreter: the catalog
        // maps it to a runtime profile, and discovery then applies the same
        // ordered rules (override first) and re-checks the constraint against
        // the interpreter it actually started. A requirement nothing on this
        // host satisfies is a recorded failure naming the requirement and the
        // versions found — never a silent fall-through to the default
        // interpreter (spec 14.11).
        let requires = RequiresPython(requires_python.clone());
        let interpreter = match runtimes
            .profile_for(&requires)
            .and_then(|profile| discover_interpreter(&profile, &requires))
        {
            Ok(interpreter) => interpreter,
            Err(error) => {
                self.record_unavailable(
                    package,
                    Some(plugin),
                    format!("no supported CPython for the modern worker: {error}"),
                );
                return;
            }
        };

        let lockfile = match resolve(&requires_python, &dependencies, index) {
            Ok(lockfile) => lockfile,
            Err(error) => {
                self.record_unavailable(
                    package,
                    Some(plugin),
                    format!("dependency resolution failed: {error}"),
                );
                return;
            }
        };

        // The environment id is a pure function of these inputs, so two plugins
        // with identical inputs share one materialised environment and one
        // worker key, while conflicting dependency versions fall out to distinct
        // ids and distinct workers (acceptance 31.20).
        let inputs = EnvironmentInputs {
            python_version: interpreter.version().to_string(),
            os: os.to_owned(),
            arch: arch.to_owned(),
            locked: lockfile.packages,
            native_build_options: Vec::new(),
        };
        let environment_id = inputs.environment_id();
        let materialized = match store.ensure(&inputs, index) {
            Ok(materialized) => materialized,
            Err(error) => {
                self.record_unavailable(
                    package,
                    Some(plugin),
                    format!("the managed environment could not be prepared: {error}"),
                );
                return;
            }
        };

        // Import path order (spec 15.4): plugin source, packaged modules,
        // managed deps, CriKey SDK — never global site-packages.
        let import_path = ImportPath::assemble(dir, &[], &materialized, sdk);
        let key: WorkerKey = (
            environment_id.0,
            entrypoint.clone(),
            dir.to_string_lossy().into_owned(),
        );
        let suggest_timeout_ms = manifest.performance.suggest_hard_timeout_ms.min(CALL_BUDGET_MS);
        let soft_timeout = Duration::from_millis(
            manifest
                .performance
                .suggest_soft_timeout_ms
                .min(suggest_timeout_ms),
        );
        let worker_options = WorkerOptions::new(plugin.clone(), entrypoint, import_path)
            .with_startup_timeout_ms(STARTUP_BUDGET_MS)
            .with_call_timeout_ms(suggest_timeout_ms)
            .with_shutdown_timeout_ms(SHUTDOWN_BUDGET_MS)
            .with_background_execution(manifest.permissions.background_execution)
            .with_environment_inheritance(manifest.permissions.environment)
            // The host hands a modern plugin no writable directory of its own,
            // so the policy is scratch space and the usual device files. A
            // manifest that did not ask for the network gets TCP refused by
            // the kernel rather than merely undeclared (spec 20.2).
            .with_sandbox(crikey_sandbox::plugin_policy(
                Vec::<std::path::PathBuf>::new(),
                !manifest.permissions.network,
            ));

        // Register first so the pipeline creates the one shared per-plugin
        // budget before any worker runtime is admitted. The exact handle is
        // retained in `LoadedPlugin` and reused by every dispatch seam.
        let budget = match pipeline.register_namespaced_manifest(plugin.clone(), &manifest) {
            Ok(budget) => budget,
            Err(error) => {
                self.record_unavailable(
                    package,
                    Some(plugin),
                    format!("the query pipeline refused the modern plugin: {error:?}"),
                );
                return;
            }
        };
        let worker_options = worker_options.with_shared_budget(budget.clone());

        // Spawn only eager workers at load. Lazy workers retain their resolved
        // interpreter/options and are started on their first query.
        if manifest.performance.startup == Startup::Eager && self.pool.never_started(&key) {
            let worker = match ModernWorker::spawn(&interpreter, worker_options.clone()) {
                Ok(worker) => worker,
                Err(error) => {
                    self.record_unavailable(
                        package,
                        Some(plugin.clone()),
                        format!("the modern worker did not start: {error}"),
                    );
                    let _ = pipeline.unregister_plugin(&plugin);
                    return;
                }
            };
            self.pool
                .workers
                .insert(key.clone(), WorkerLifecycle::Live(worker));
        }

        self.plugins.push(plugin.clone());
        self.loaded.push(LoadedPlugin {
            plugin,
            key,
            package_dir: dir.to_owned(),
            interpreter,
            worker_options,
            budget,
            soft_timeout,
            catalog_persist: manifest.catalog.persist,
            permissions: manifest.permissions,
        });
    }

    fn record_unavailable(&mut self, package: String, plugin: Option<PluginId>, reason: String) {
        self.unavailable.push(ModernUnavailable {
            package,
            plugin,
            reason,
        });
    }
    /// Starts `plugin`'s child if its slot has never been started.
    ///
    /// Spawning is allowed from `NeverStarted` only: a slot that already failed
    /// stays failed and is reported through [`Self::failed_workers`] until
    /// [`Self::restart_worker`] supervises a retry. A failed spawn retires the
    /// slot here, so the caller cannot leave it startable by accident.
    fn ensure_worker(&mut self, plugin: &PluginId, key: &WorkerKey) -> Result<(), String> {
        match self.pool.workers.get(key) {
            Some(WorkerLifecycle::Live(_)) => return Ok(()),
            Some(WorkerLifecycle::CatalogBusy) => {
                return Err("modern worker is serving a catalog callback".into())
            }
            Some(WorkerLifecycle::Failed { reason }) => return Err(reason.clone()),
            Some(WorkerLifecycle::NeverStarted) | None => {}
        }
        let Some((interpreter, options)) = self
            .loaded
            .iter()
            .find(|loaded| &loaded.plugin == plugin && &loaded.key == key)
            .map(|loaded| (loaded.interpreter.clone(), loaded.worker_options.clone()))
        else {
            let reason = "modern plugin is not registered".to_owned();
            self.pool.fail(key, reason.clone());
            return Err(reason);
        };
        match ModernWorker::spawn(&interpreter, options) {
            Ok(worker) => {
                self.pool
                    .workers
                    .insert(key.clone(), WorkerLifecycle::Live(worker));
                Ok(())
            }
            Err(error) => {
                let reason = error.to_string();
                self.pool.fail(key, reason.clone());
                Err(reason)
            }
        }
    }

    /// Modern plugins whose worker is currently retired in the failed state,
    /// each with the reason it failed.
    ///
    /// Distinct from [`Self::dispatch_failures`], which is a bounded
    /// once-per-plugin log: this is live lifecycle state, so a plugin that
    /// crashed stays reported as unavailable for as long as it stays failed
    /// rather than being named once and then quietly retried.
    pub fn failed_workers(&self) -> Vec<(PluginId, String)> {
        self.loaded
            .iter()
            .filter_map(|loaded| {
                self.pool
                    .failure_reason(&loaded.key)
                    .map(|reason| (loaded.plugin.clone(), reason.to_owned()))
            })
            .collect()
    }

    /// Supervised restart of a failed modern worker: the one transition out of
    /// the failed state.
    ///
    /// Clears the failure verdict so the next query may spawn a fresh child,
    /// and clears the recorded diagnostic so a second failure is attributable
    /// rather than suppressed by the first. Returns an error when `plugin` is
    /// not loaded or its worker is not failed, because a caller that restarts
    /// a healthy or unknown plugin has a bug rather than a no-op.
    pub fn restart_worker(&mut self, plugin: &PluginId) -> Result<(), String> {
        let Some(key) = self
            .loaded
            .iter()
            .find(|loaded| &loaded.plugin == plugin)
            .map(|loaded| loaded.key.clone())
        else {
            return Err(format!("modern plugin `{}` is not loaded", plugin.0));
        };
        if self.pool.failure_reason(&key).is_none() {
            return Err(format!("modern plugin `{}` has no failed worker", plugin.0));
        }
        // Retire suggestion/origin bindings before permitting a replacement
        // process to start, even when it later reuses the same stable item IDs.
        self.action_items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|(owner, _), _| owner != plugin);
        self.pool.workers.insert(key, WorkerLifecycle::NeverStarted);
        self.pool.recorded.remove(plugin);
        Ok(())
    }

    /// Manifest grants used by the host-mediated action boundary.
    pub fn permissions(&self) -> BTreeMap<PluginId, Permissions> {
        self.loaded
            .iter()
            .map(|loaded| (loaded.plugin.clone(), loaded.permissions.clone()))
            .collect()
    }
    /// The modern plugins that loaded and are being served through the pipeline.
    pub fn plugins(&self) -> &[PluginId] {
        &self.plugins
    }

    /// Each loaded plugin's catalog-persistence declaration (spec 22.1).
    ///
    /// Read at discovery, before any catalog build is requested, because a
    /// refusal has to take effect even for a plugin that never publishes. A
    /// build can fail, be refused a budget, or simply not finish, and in every
    /// one of those cases a slice written by an earlier run is already loaded
    /// and already answering queries.
    pub fn catalog_declarations(&self) -> Vec<(PluginId, bool)> {
        self.catalog_declarations
            .iter()
            .map(|(plugin, persist)| (plugin.clone(), *persist))
            .collect()
    }

    /// Packages that could not be served, each with an attributable reason.
    pub fn unavailable(&self) -> &[ModernUnavailable] {
        &self.unavailable
    }

    /// Runtime dispatch failures observed since startup (a worker that crashed
    /// or a callback that raised while answering a live query), newest last.
    /// Distinct from [`Self::unavailable`], which is load-time only.
    pub fn dispatch_failures(&self) -> &[(PluginId, String)] {
        &self.pool.failures
    }
    /// Saturating counts of suggestion callbacks that exceeded each manifest's
    /// soft deadline. Hard timeout failures remain in [`Self::dispatch_failures`].
    pub fn soft_timeouts(&self) -> &BTreeMap<PluginId, u32> {
        &self.pool.soft_timeouts
    }

    /// Delivers each loaded plugin its own complete configuration state (spec 21.4).
    ///
    /// Returns one `(plugin, reason)` per plugin that could not be reached, so a
    /// dead worker is a named diagnostic rather than a configuration change that
    /// silently did nothing. Delivery continues past a failure: one broken plugin
    /// must not stop the others from being configured.
    ///
    /// A loaded plugin the state does not mention is sent an EMPTY map rather
    /// than skipped. Skipping would leave it applying whatever it last received,
    /// which is exactly the stale-state bug the complete-publication rule exists
    /// to prevent.
    pub fn publish_configuration(
        &mut self,
        configuration: &crate::PluginConfiguration,
    ) -> Vec<(PluginId, String)> {
        let mut failures = Vec::new();
        let empty = BTreeMap::new();
        let targets: Vec<(PluginId, WorkerKey)> = self
            .loaded
            .iter()
            .map(|loaded| (loaded.plugin.clone(), loaded.key.clone()))
            .collect();
        for (plugin, key) in targets {
            let values = configuration.get(&plugin).unwrap_or(&empty).clone();
            let Some(worker) = self.pool.live_mut(&key) else {
                failures.push((plugin, "modern worker is unavailable".to_owned()));
                continue;
            };
            // Caught rather than propagated for the same reason every other
            // dispatch seam here catches: a panic inside one plugin's transport
            // must not take down the supervisor thread publishing to the rest.
            let outcome = catch_unwind(AssertUnwindSafe(|| worker.send_configuration(&values, true)));
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    let reason = format!("configuration delivery failed: {error}");
                    self.pool.record_dispatch_failure(plugin.clone(), reason.clone());
                    failures.push((plugin, reason));
                }
                Err(_) => {
                    let reason = "configuration delivery panicked".to_owned();
                    self.pool.record_dispatch_failure(plugin.clone(), reason.clone());
                    failures.push((plugin, reason));
                }
            }
        }
        failures
    }

    /// Starts one bounded catalog rebuild for an exactly owned plugin.
    ///
    /// Transfers the plugin's sole worker to a bounded background catalog
    /// callback. Query/preview dispatch refuses the CatalogBusy slot until the
    /// same worker returns; catalog completion restores ownership before rows.
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
        let catalog_persist = loaded.catalog_persist;
        let key = loaded.key.clone();
        let interpreter = loaded.interpreter.clone();
        // Catalog callbacks are not suggestion callbacks; keep their
        // independent long transport budget rather than applying the manifest
        // suggestion hard deadline to catalog construction.
        let worker_options = loaded.worker_options.clone();
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
        let worker = match self.pool.workers.remove(&key) {
            Some(WorkerLifecycle::Live(worker)) => Some(worker),
            Some(WorkerLifecycle::NeverStarted) | None => None,
            Some(state @ (WorkerLifecycle::CatalogBusy | WorkerLifecycle::Failed { .. })) => {
                self.pool.workers.insert(key, state);
                return Err(CatalogDispatchError::BudgetRefused {
                    plugin: plugin.clone(),
                });
            }
        };
        self.pool
            .workers
            .insert(key.clone(), WorkerLifecycle::CatalogBusy);
        let request_id = self.catalog.next_id();
        let sender = self.catalog.result_tx.clone();
        let plugin_for_thread = plugin.clone();
        let thread_plugin = plugin.clone();
        let join = thread::Builder::new()
            .name(format!("crikey-modern-catalog-{}", plugin.0))
            .spawn(move || {
                let mut worker = worker;
                let was_live = worker.is_some();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    if worker.is_none() {
                        worker = Some(ModernWorker::spawn(&interpreter, worker_options)
                            .map_err(|error| error.to_string())?);
                    }
                    worker.as_mut().expect("worker installed")
                        .build_catalog_with_timeout_ms(CALL_BUDGET_MS).map_err(|error| error.to_string())
                }));
                let panicked = result.is_err();
                let completion = match result {
                    Ok(Ok(mut items)) => {
                        for item in &mut items {
                            item.plugin_id = plugin_for_thread.clone();
                        }
                        CatalogBuildResult::Complete(crate::CatalogBuild {
                            plugin: plugin_for_thread,
                            instance,
                            generation,
                            persist: catalog_persist,
                            items,
                        })
                    }
                    Ok(Err(reason)) => CatalogBuildResult::Failed {
                        plugin: thread_plugin.clone(),
                        instance,
                        generation,
                        reason: format!("modern catalog build failed: {reason}"),
                    },
                    Err(_) => CatalogBuildResult::Failed {
                        plugin: thread_plugin,
                        instance,
                        generation,
                        reason: "modern catalog worker panicked".to_owned(),
                    },
                };
                let keep_origin = worker.as_ref().is_some_and(ModernWorker::supports_preview)
                    && matches!(&completion, CatalogBuildResult::Complete(build)
                        if build.items.iter().any(|item| item.metadata.get("crikey.preview").map(String::as_str) == Some("deferred")));
                let worker = if !panicked && worker.as_ref().is_some_and(ModernWorker::is_alive) {
                    if !was_live && !keep_origin {
                        let _ = worker.take().expect("healthy producer").shutdown();
                        CatalogWorkerReturn::Released
                    } else {
                        CatalogWorkerReturn::Retained(worker.take().expect("healthy producer"))
                    }
                } else {
                    let reason = match &completion {
                        CatalogBuildResult::Failed { reason, .. } => reason.clone(),
                        _ => "modern catalog worker is no longer alive".to_owned(),
                    };
                    CatalogWorkerReturn::Failed(reason)
                };
                let _ = sender.send((request_id, completion, worker));
                drop(guard);
            })
            .map_err(|error| {
                self.pool.fail(&key, format!("catalog thread spawn failed: {error}"));
                CatalogDispatchError::ThreadSpawn { plugin: plugin.clone(), reason: error.to_string() }
            })?;
        self.catalog.insert(request_id, plugin.clone(), key, join);
        Ok(request_id)
    }

    /// Retires completed catalog tasks and returns their tagged outcomes.
    ///
    /// A completion from an older request for the same plugin is returned as
    /// [`CatalogBuildResult::Obsolete`] and must not be published. Failed
    /// workers are retained in the normal runtime diagnostics stream.
    pub fn take_catalog_results(&mut self) -> Vec<CatalogBuildResult> {
        let results = self.catalog.take(&mut self.pool);
        for result in &results {
            if let CatalogBuildResult::Failed { plugin, reason, .. } = result {
                self.pool.record_dispatch_failure(plugin.clone(), reason.clone());
            }
        }
        results
    }

    /// Clones the provider-owned action budget handles for the asynchronous
    /// driver endpoint. Each clone points at the exact `Arc` retained in the
    /// corresponding loaded plugin record and in the query pipeline.
    fn action_budgets(&self) -> BTreeMap<PluginId, PluginBudgetHandle> {
        self.loaded
            .iter()
            .map(|loaded| (loaded.plugin.clone(), Arc::clone(&loaded.budget)))
            .collect()
    }

    /// Validates and executes one admitted plugin action on its owning worker.
    ///
    /// The request owns the action guard. Keeping it in this function until
    /// the worker outcome is converted to `CoreResult` covers failures,
    /// cancellation, timeout and panic/unwind paths.
    fn execute_action_request(&mut self, request: ModernActionRequest) -> crikey_core::Result<()> {
        let _guard = request.guard.acquire(request.deadline, &request.cancelled)?;
        let plugin = request.plugin;
        let item = request.item;
        if item.plugin_id != plugin {
            return Err(crikey_core::CoreError::Invalid(format!(
                "modern action item `{}` is owned by `{}`, not `{}`",
                item.stable_id.0, item.plugin_id.0, plugin.0
            )));
        }
        let loaded = self
            .loaded
            .iter()
            .find(|loaded| loaded.plugin == plugin)
            .ok_or_else(|| {
                crikey_core::CoreError::Invalid(format!(
                    "modern action owner `{}` is no longer loaded",
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
                "modern action request is not plugin-owned".to_owned(),
            ));
        }
        if !action.applicable_categories.is_empty() && !action.applicable_categories.contains(&item.category)
        {
            return Err(crikey_core::CoreError::Invalid(
                "modern action is not applicable to the selected item category".to_owned(),
            ));
        }
        match item.argument_policy {
            ArgumentPolicy::Forbidden if request.argument.is_some() => {
                return Err(crikey_core::CoreError::Invalid(
                    "modern action item forbids arguments".to_owned(),
                ));
            }
            ArgumentPolicy::Required if request.argument.as_deref().is_none_or(str::is_empty) => {
                return Err(crikey_core::CoreError::Invalid(
                    "modern action item requires an argument".to_owned(),
                ));
            }
            ArgumentPolicy::Optional | ArgumentPolicy::Forbidden | ArgumentPolicy::Required => {}
        }

        let key = loaded.key.clone();
        let worker = self.pool.live_mut(&key).ok_or_else(|| {
            crikey_core::CoreError::Invalid(format!("modern worker for `{}` is unavailable", plugin.0))
        })?;
        if worker.incarnation() != request.origin {
            return Err(crikey_core::CoreError::Invalid(
                "modern action originating worker retired".into(),
            ));
        }
        worker.cancel_handle().reset();
        if request.cancelled.load(Ordering::Acquire) {
            return Err(crikey_core::CoreError::Cancelled);
        }
        if !worker.is_alive() {
            // Observing a dead child here retires the slot, exactly as the
            // suggestion path does: the plugin stays unavailable rather than
            // being respawned behind the user's back on the next keystroke.
            self.pool
                .fail(&key, "the modern worker is no longer alive".to_owned());
            self.pool
                .record_dispatch_failure(plugin.clone(), "modern worker is no longer alive".to_owned());
            self.action_items
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .retain(|(owner, _), _| owner != &plugin);
            return Err(crikey_core::CoreError::Invalid(format!(
                "modern action worker for `{}` is unavailable",
                plugin.0
            )));
        }

        let outcome = catch_unwind(AssertUnwindSafe(|| {
            worker.execute(&item, Some(&request.action_id.0), request.argument.as_deref())
        }));
        match outcome {
            Ok(Ok(ExecuteOutcome::Ok)) => Ok(()),
            Ok(Ok(ExecuteOutcome::Failed(error))) => {
                let reason = format!("modern plugin action failed: {}", error.message);
                self.pool.record_dispatch_failure(plugin.clone(), reason.clone());
                Err(crikey_core::CoreError::Invalid(format!(
                    "modern plugin `{}` action failed: {reason}",
                    plugin.0
                )))
            }
            Ok(Err(error)) => {
                let reason = error.to_string();
                self.pool.record_dispatch_failure(plugin.clone(), reason.clone());
                Err(crikey_core::CoreError::Invalid(format!(
                    "modern plugin `{}` action failed: {reason}",
                    plugin.0
                )))
            }
            Err(_) => {
                self.pool
                    .record_dispatch_failure(plugin.clone(), "modern action worker panicked".to_owned());
                Err(crikey_core::CoreError::Invalid(format!(
                    "modern plugin `{}` action worker panicked",
                    plugin.0
                )))
            }
        }
    }

    /// Drives one query end to end and returns the pipeline frame it produced.
    ///
    /// The call path is: `keystroke` mints the pipeline generation; each loaded
    /// plugin's `suggest` runs in its child process to compute its items; the
    /// pipeline is ticked (advancing the modern debounce timer as needed) until
    /// its registered plugins are dispatched; each plugin's items are delivered
    /// as a [`ResultBatch`] under the pipeline generation and the request is
    /// completed; `present` drains intake, ranks and coalesces one frame. Stale
    /// answers are refused at the pipeline's intake boundary because only the
    /// current generation is ever delivered.
    ///
    /// Returns `None` when the pipeline reported an error at any stage or the
    /// frame belonged to a superseded generation, so a caller never publishes a
    /// stale or partial modern frame. A crashing plugin never returns `None`:
    /// its failure is recorded and it simply contributes no rows.
    pub fn drive_query(
        &mut self,
        pipeline: &mut QueryPipeline,
        query: &str,
        now: Millis,
    ) -> Option<ViewModel> {
        let generation = pipeline.keystroke(query, now);
        // One reset per query, not one per tick: a generation can be dispatched
        // across several wake-ups, and clearing inside the collection below
        // would drop the action items an earlier tick's plugins just produced.
        self.cancellation.cancel_before(generation.get());
        self.action_items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();

        let mut at = now;
        let mut dead_plugins = BTreeSet::new();
        // Advance the pipeline until its registered plugins have been dispatched
        // for this generation. Modern plugins debounce, so a query that is not a
        // leading edge dispatches on a later timer wake-up; the loop follows the
        // scheduler's own wake-ups and is bounded by construction.
        for _ in 0..64 {
            // Ask the scheduler first. Collecting before this tick would start
            // a lazy plugin's child interpreter for a query its own
            // minimum-length, prefix, keyword or pattern gate excludes it from
            // — the whole point of `startup = "lazy"`.
            let tick = pipeline.tick(at);
            for cancellation in tick.cancellations {
                let _ = pipeline.complete(&cancellation.plugin, cancellation.generation, at);
            }

            let mut requests = Vec::new();
            for request in tick.dispatches {
                if request.generation != generation {
                    let _ = pipeline.complete(&request.plugin, request.generation, at);
                    continue;
                }
                if dead_plugins.contains(&request.plugin) {
                    let _ = pipeline.abort_request(&request.plugin, request.generation, at);
                    continue;
                }
                requests.push(request);
            }

            if requests.is_empty() {
                match pipeline.next_wakeup() {
                    Some(next) if next > at => {
                        at = next;
                        continue;
                    }
                    _ => break,
                }
            }

            let requested: BTreeSet<PluginId> =
                requests.iter().map(|request| request.plugin.clone()).collect();
            let (mut suggestions, newly_dead) = self.collect_suggestions(query, generation, &requested);
            for request in requests {
                if newly_dead.contains(&request.plugin) {
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
            dead_plugins.extend(newly_dead);

            match pipeline.next_wakeup() {
                Some(next) if next > at => at = next,
                _ => break,
            }
        }

        let frame = pipeline.present(at);
        frame.filter(|frame| frame.generation == generation)
    }

    /// Records which admitted query the provider is about to serve, so blocking
    /// work inside [`Self::drive_query`] can notice its own supersession.
    fn begin_serving(&mut self, intake: u64) {
        self.serving_intake = intake;
    }

    /// Whether a newer query has been admitted since the one being served.
    fn superseded(&self) -> bool {
        self.cancellation.intake() != self.serving_intake
    }

    /// Cooperative teardown of every modern worker (spec 24.3).
    pub fn shutdown(&mut self, _now: Millis) {
        self.cancellation.cancel_all();
        for result in self.catalog.shutdown(&mut self.pool) {
            if let CatalogBuildResult::Failed { plugin, reason, .. } = result {
                self.pool.record_dispatch_failure(plugin, reason);
            }
        }
        for (_, state) in std::mem::take(&mut self.pool.workers) {
            // Best effort: the child is reaped on drop even if orderly shutdown
            // reports an error, so no worker is leaked. A slot that never
            // started or already failed owns no child to tear down.
            if let WorkerLifecycle::Live(worker) = state {
                let _ = worker.shutdown();
            }
        }
    }

    /// Runs `suggest` in the child process of every plugin the scheduler
    /// dispatched this tick, and groups the resulting items by owning plugin.
    /// Every failure — a dead worker, a crash mid-callback, or a plugin-raised
    /// error — is contained as a recorded dispatch failure and contributes no
    /// items.
    ///
    /// `requested` is the gate. A plugin absent from it was found irrelevant by
    /// [`crikey_input_scheduler`], and starting its interpreter to ask anyway
    /// would spend a child process — and, for a `startup = "lazy"` plugin, the
    /// whole interpreter startup — on a query the pipeline will not publish.
    fn collect_suggestions(
        &mut self,
        query: &str,
        generation: Generation,
        requested: &BTreeSet<PluginId>,
    ) -> (BTreeMap<PluginId, Vec<Item>>, BTreeSet<PluginId>) {
        let request = SuggestRequest {
            generation: generation.get(),
            text: query.to_owned(),
            normalized: query.to_owned(),
            selected_item_id: None,
        };
        // Snapshot the loaded set so the pool can be mutated while iterating.
        let targets: Vec<(PluginId, WorkerKey, Duration)> = self
            .loaded
            .iter()
            .filter(|loaded| requested.contains(&loaded.plugin))
            .map(|loaded| (loaded.plugin.clone(), loaded.key.clone(), loaded.soft_timeout))
            .collect();

        let mut by_plugin: BTreeMap<PluginId, Vec<Item>> = BTreeMap::new();
        let mut dead_plugins = BTreeSet::new();

        for (plugin, key, soft_timeout) in targets {
            // An obsolete target list is abandoned rather than worked through.
            // Nothing this generation produces can be published now, and the
            // startup below is the one blocking call the provider makes with no
            // registered call for a newer query to cancel.
            if self.superseded() {
                break;
            }
            if let Some(reason) = self.pool.failure_reason(&key) {
                // A failed worker stays failed until a supervised restart, so a
                // plugin that crashes on every query pays interpreter startup
                // once rather than on every keystroke. It keeps being reported
                // as unavailable through `failed_workers` for as long as it
                // stays in this state.
                let reason = reason.to_owned();
                dead_plugins.insert(plugin.clone());
                self.pool.record_dispatch_failure(plugin, reason);
                continue;
            }
            if matches!(self.pool.workers.get(&key), Some(WorkerLifecycle::CatalogBusy)) {
                by_plugin.insert(plugin, Vec::new());
                continue;
            }
            if self.pool.never_started(&key) {
                if let Err(reason) = self.ensure_worker(&plugin, &key) {
                    dead_plugins.insert(plugin.clone());
                    self.pool.record_dispatch_failure(plugin, reason);
                    continue;
                }
                // Startup is bounded at 30 seconds per plugin and targets are
                // processed in order. A supersession that arrived during the
                // handshake has to stop the chain here, not after every
                // remaining plugin has also been started and asked.
                if self.superseded() {
                    break;
                }
            }
            // A worker that has died since its last call stays dead: retire the
            // slot, leave the plugin cleanly unavailable, and record the failure
            // at most once rather than re-dispatching to a corpse every
            // keystroke.
            let alive = self
                .pool
                .live_mut(&key)
                .map(|worker| worker.is_alive())
                .unwrap_or(false);
            if !alive {
                let reason = "the modern worker is no longer alive".to_owned();
                self.pool.fail(&key, reason.clone());
                self.pool.record_dispatch_failure(plugin.clone(), reason);
                self.action_items
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .retain(|(owner, _), _| owner != &plugin);
                continue;
            }

            let control = Arc::new(ModernCallControl::new());
            self.cancellation
                .register(generation.get(), plugin.clone(), Arc::clone(&control));
            let control_for_thread = Arc::clone(&control);
            let (answer, elapsed, incarnation) = {
                let worker = self
                    .pool
                    .live_mut(&key)
                    .expect("the worker was live a moment ago");
                let handle = worker.cancel_handle();
                // Clear any cancellation left by the prior call before
                // registering this one. The latched variant then preserves a
                // cancellation that races with call start instead of clearing
                // it with its normal pre-call reset.
                handle.reset();
                control.install(handle);
                let watcher = thread::Builder::new()
                    .name(format!("crikey-modern-cancel-{}", plugin.0))
                    .spawn(move || control_for_thread.watch_cancel());
                let started = Instant::now();
                let answer = catch_unwind(AssertUnwindSafe(|| worker.suggest_with_cancel_latched(&request)));
                let elapsed = started.elapsed();
                control.finish();
                if let Ok(watcher) = watcher {
                    let _ = watcher.join();
                }
                (answer, elapsed, worker.incarnation())
            };
            if elapsed > soft_timeout {
                self.pool.record_soft_timeout(plugin.clone());
            }
            self.cancellation.unregister(generation.get(), &plugin);
            let answer = match answer {
                Ok(answer) => answer,
                Err(_) => {
                    dead_plugins.insert(plugin.clone());
                    let reason = "modern worker panicked".to_owned();
                    self.pool.fail(&key, reason.clone());
                    self.pool.record_dispatch_failure(plugin.clone(), reason);
                    self.action_items
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .retain(|(owner, _), _| owner != &plugin);
                    continue;
                }
            };
            match answer {
                Ok(suggestions) if suggestions.state == WorkerBatchState::Failed => {
                    let reason = suggestions
                        .error
                        .map(|error| error.message)
                        .filter(|message| !message.is_empty())
                        .unwrap_or_else(|| "the modern plugin reported a failure".to_owned());
                    self.pool.record_dispatch_failure(plugin, reason);
                }
                Ok(suggestions) => {
                    let mut items = suggestions.items;
                    // The host owns identity (spec 10.2): stamp every item with
                    // the plugin's namespaced id regardless of what the child
                    // reported.
                    for item in &mut items {
                        item.plugin_id = plugin.clone();
                        self.action_items
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .insert(
                                (plugin.clone(), item.stable_id.clone()),
                                (item.clone(), incarnation),
                            );
                    }
                    by_plugin.insert(plugin, items);
                }
                Err(error) => {
                    // A crashed or unresponsive worker is contained: retire the
                    // slot so the plugin stays unavailable with this reason
                    // until a supervised restart, and record the failure once so
                    // a diagnostic can name the plugin.
                    dead_plugins.insert(plugin.clone());
                    let reason = error.to_string();
                    self.pool.fail(&key, reason.clone());
                    self.pool.record_dispatch_failure(plugin.clone(), reason);
                    self.action_items
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .retain(|(owner, _), _| owner != &plugin);
                }
            }
        }

        (by_plugin, dead_plugins)
    }
}

/// One query handed to the modern supervisor thread, tagged with the search
/// generation it belongs to.
#[derive(Debug)]
struct ModernJob {
    generation: Generation,
    query: String,
    /// Intake stamp minted when this job was admitted. The supervisor hands it
    /// to the provider so a startup handshake can notice its own supersession.
    intake: u64,
    now: Millis,
    /// The built-in provider's rows for this generation. Prepended to the modern
    /// rows so the merged frame keeps the built-in path's ordering.
    builtin_rows: Vec<ResultRow>,
    builtin_pending: bool,
    selected: usize,
}

/// The supervisor's request mailbox: a single slot with replace-oldest
/// overflow. A newer query overwrites an un-started one, so a slow modern plugin
/// never delays a fast keystroke (acceptance 31.8) and the channel is bounded by
/// construction.
struct ModernRequestSlot {
    job: Option<ModernJob>,
    /// The latest configuration state to publish, if one is waiting.
    ///
    /// Single-slot replace-oldest for the same reason the query slot is: only
    /// the newest complete state matters.
    configuration: Option<Box<crate::PluginConfiguration>>,
    stop: bool,
}

impl std::fmt::Debug for ModernRequestSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModernRequestSlot")
            .field("job", &self.job)
            .field(
                "configuration",
                &self.configuration.as_ref().map(|_| "<redacted>"),
            )
            .field("stop", &self.stop)
            .finish()
    }
}

const ACTION_QUEUE_CAPACITY: usize = 8;
const ACTION_COMPLETION_CAPACITY: usize = 64;
const ACTION_IN_FLIGHT_CAPACITY: usize = 32;
const ACTION_TIMEOUT_MS: u64 = CALL_BUDGET_MS;

/// One admitted action handed to the modern provider supervisor.
#[derive(Debug)]
struct ModernActionRequest {
    request_id: ActionRequestId,
    plugin: PluginId,
    item: Item,
    origin: u64,
    action_id: ActionId,
    argument: Option<String>,
    guard: ForegroundActionAdmission,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

enum ModernWork {
    Query(ModernJob),
    Action(Box<ModernActionRequest>),
    Preview(ModernPreviewJob),
    Refresh,
    /// Publish this complete configuration state to every loaded plugin.
    Configuration(Box<crate::PluginConfiguration>),
}

/// Bounded action endpoint retained by the live modern driver.
#[derive(Debug)]
struct ModernActionEndpoint {
    sender: SyncSender<ModernActionRequest>,
    completions: Arc<Mutex<VecDeque<PluginActionCompletion>>>,
    gates: BTreeMap<PluginId, Arc<PreviewActionGate>>,
    preview: Arc<ModernPreviewEndpoint>,
    items: ModernItemStore,
    pending: Arc<Mutex<BTreeMap<ActionRequestId, Arc<AtomicBool>>>>,
    in_flight: Arc<AtomicUsize>,
    next_id: Arc<AtomicU64>,
    mailbox: Arc<(Mutex<ModernRequestSlot>, Condvar)>,
}

impl crate::PluginActionExecutor for ModernActionEndpoint {
    fn submit_plugin_action(
        &self,
        plugin: &PluginId,
        item: &Item,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> crikey_core::Result<ActionRequestId> {
        if item.plugin_id != *plugin {
            return Err(crikey_core::CoreError::Invalid(
                "selected modern plugin result has stale ownership".to_owned(),
            ));
        }
        let item_id = item.stable_id.clone();
        let (item, origin) = self
            .items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&(plugin.clone(), item_id))
            .cloned()
            .ok_or_else(|| {
                crikey_core::CoreError::Invalid(
                    "selected modern plugin result is no longer current".to_owned(),
                )
            })?;
        if item.plugin_id != *plugin {
            return Err(crikey_core::CoreError::Invalid(
                "selected modern plugin result has stale ownership".to_owned(),
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
                "modern action mailbox is full".to_owned(),
            ));
        }
        let budget = match self.gates.get(plugin) {
            Some(budget) => budget,
            None => {
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                return Err(crikey_core::CoreError::Invalid(format!(
                    "no modern action runtime owns plugin `{}`",
                    plugin.0
                )));
            }
        };
        let guard = match budget.reserve_foreground() {
            Ok(guard) => guard,
            Err(error) => {
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                return Err(error);
            }
        };
        // The modern supervisor is serial across owners; retire every queued
        // speculative call only after reserving this foreground intent.
        for (_, token) in self
            .preview
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
        {
            token.store(true, Ordering::Release);
        }
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
        let request = ModernActionRequest {
            request_id: request_id.clone(),
            plugin: plugin.clone(),
            item,
            origin,
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
                    "modern plugin `{}` action queue is full",
                    plugin.0
                )),
                mpsc::TrySendError::Disconnected(_) => crikey_core::CoreError::Invalid(format!(
                    "modern plugin `{}` action runtime stopped",
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
            .map(|(_, (item, _))| item.clone())
            .ok_or_else(|| {
                crikey_core::CoreError::Invalid(
                    "selected modern plugin result is no longer current".to_owned(),
                )
            })?;
        if item.plugin_id != *plugin {
            return Err(crikey_core::CoreError::Invalid(
                "selected modern plugin result has stale ownership".to_owned(),
            ));
        }
        self.submit_plugin_action(plugin, &item, action_id, argument)
    }
}
fn enqueue_modern_completion(
    mailbox: &Mutex<VecDeque<PluginActionCompletion>>,
    completion: PluginActionCompletion,
) {
    let mut mailbox = mailbox.lock().unwrap_or_else(|error| error.into_inner());
    if mailbox.len() >= ACTION_COMPLETION_CAPACITY {
        eprintln!(
            "crikey: modern action completion mailbox overflow for {} / {}",
            completion.plugin.0, completion.action_id.0
        );
        return;
    }
    mailbox.push_back(completion);
}

/// Drives [`ModernProvider::drive_query`] on a dedicated supervisor thread so
/// the user-interface thread never blocks on a child interpreter (spec 6.5;
/// acceptance 31.1, 31.8).
///
/// The UI thread [`submit`](Self::submit)s a query and returns at once. The
/// supervisor drives the modern pipeline — where a superseded answer is refused
/// at the intake boundary because `drive_query` only ever delivers the current
/// generation — merges the resulting rows behind the built-in rows, and
/// publishes the frame two ways: through the `publish` callback (which the
/// composition root forwards straight to the renderer) and into a single-slot
/// outcome mailbox the UI thread folds into its retained view model with
/// [`take_outcome`](Self::take_outcome).
///
/// A late answer never appears under a newer generation: the frame is tagged
/// with the search generation it was submitted under (never relabelled) and the
/// supervisor drops it if the UI has already moved on (the `current` atomic).
///
/// Every failure stays contained exactly as in [`ModernProvider::drive_query`]:
/// a crash, timeout or missing worker degrades to a recorded diagnostic and an
/// empty answer, so the supervisor thread never panics and never aborts startup.
#[derive(Debug)]
pub struct ModernDriver {
    mailbox: Arc<(Mutex<ModernRequestSlot>, Condvar)>,
    action_endpoint: Arc<ModernActionEndpoint>,
    preview_endpoint: Arc<ModernPreviewEndpoint>,
    permissions: BTreeMap<PluginId, Permissions>,
    catalog_results: Arc<Mutex<Vec<CatalogBuildResult>>>,
    /// Per-plugin diagnostics refreshed by the supervisor thread after every
    /// unit of work, so the UI thread can report a throttled plugin without
    /// reaching into the pipeline it does not own.
    health: Arc<Mutex<Vec<(PluginId, crikey_plugin_supervisor::PluginHealth)>>>,
    outcome: Arc<Mutex<Option<ViewModel>>>,
    /// Search generation the UI last submitted. The supervisor re-reads it
    /// before publishing and drops any answer that is no longer current.
    current: Arc<AtomicU64>,
    cancellation: Arc<ModernCancellation>,
    has_plugins: bool,
    worker: Option<JoinHandle<()>>,
}

impl ModernDriver {
    /// Moves `provider` and its `pipeline` onto a supervisor thread and returns
    /// a handle the UI thread drives without ever blocking.
    ///
    /// `publish` runs on the supervisor thread with each merged frame. A thread
    /// that fails to spawn degrades to an inert driver rather than a panic:
    /// `provider` is dropped, reaping every child, and
    /// [`has_plugins`](Self::has_plugins) reports false.
    pub fn spawn<P>(mut provider: ModernProvider, mut pipeline: QueryPipeline, publish: P) -> Self
    where
        P: Fn(&ViewModel) + Send + 'static,
    {
        let permissions = provider.permissions();
        let has_plugins = !provider.plugins().is_empty();
        let (preview_sender, preview_receiver) = mpsc::sync_channel(2);
        let preview_endpoint = ModernPreviewEndpoint::new(&provider, preview_sender);
        let mailbox = Arc::new((
            Mutex::new(ModernRequestSlot {
                job: None,
                configuration: None,
                stop: false,
            }),
            Condvar::new(),
        ));
        let outcome = Arc::new(Mutex::new(None));
        let catalog_results = Arc::new(Mutex::new(Vec::new()));
        let health = Arc::new(Mutex::new(Vec::new()));
        let (action_sender, action_receiver) = mpsc::sync_channel(ACTION_QUEUE_CAPACITY);
        let completion_mailbox = Arc::new(Mutex::new(VecDeque::with_capacity(ACTION_COMPLETION_CAPACITY)));
        let action_endpoint = Arc::new(ModernActionEndpoint {
            sender: action_sender,
            completions: Arc::clone(&completion_mailbox),
            gates: preview_endpoint.gates.clone(),
            preview: Arc::clone(&preview_endpoint),
            items: Arc::clone(&provider.action_items),
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            in_flight: Arc::clone(&preview_endpoint.action_in_flight),
            next_id: Arc::new(AtomicU64::new(0)),
            mailbox: Arc::clone(&mailbox),
        });
        let cancellation = Arc::clone(&provider.cancellation);
        let current = Arc::new(AtomicU64::new(0));

        let thread_mailbox = Arc::clone(&mailbox);
        let thread_catalog_results = Arc::clone(&catalog_results);
        let thread_health = Arc::clone(&health);
        let thread_outcome = Arc::clone(&outcome);
        let thread_current = Arc::clone(&current);
        let thread_completion_mailbox = Arc::clone(&completion_mailbox);
        let thread_preview_endpoint = Arc::clone(&preview_endpoint);
        let spawned = std::thread::Builder::new()
            .name("crikey-modern".to_owned())
            .spawn(move || {
                let (lock, cvar) = &*thread_mailbox;
                let mut last_now: Millis = 0;
                let mut deferred_preview: Option<ModernPreviewJob> = None;
                loop {
                    let catalog_results = provider.take_catalog_results();
                    thread_preview_endpoint.refresh_liveness(&mut provider);
                    for result in catalog_results {
                        thread_catalog_results
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .push(result);
                        thread_preview_endpoint.wake();
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
                                    enqueue_modern_completion(&thread_completion_mailbox, completion);
                                }
                                while let Ok(request) = preview_receiver.try_recv() {
                                    thread_preview_endpoint.complete(request, PreviewState::Cancelled);
                                }
                                if let Some(request) = deferred_preview.take() {
                                    thread_preview_endpoint.complete(request, PreviewState::Cancelled);
                                }
                                drop(slot);
                                provider.shutdown(last_now);
                                return;
                            }
                            if let Ok(request) = action_receiver.try_recv() {
                                break ModernWork::Action(Box::new(request));
                            }
                            // Ahead of a query: a plugin about to answer should
                            // answer under the configuration the user just
                            // applied, not the one it is about to lose.
                            if let Some(configuration) = slot.configuration.take() {
                                break ModernWork::Configuration(configuration);
                            }
                            if let Some(job) = slot.job.take() {
                                break ModernWork::Query(job);
                            }
                            if deferred_preview.is_none() {
                                deferred_preview = preview_receiver.try_recv().ok();
                            }
                            if let Some(request) = &deferred_preview {
                                let catalog_busy = provider
                                    .loaded
                                    .iter()
                                    .find(|loaded| loaded.plugin == request.request.identity.plugin)
                                    .is_some_and(|loaded| {
                                        matches!(
                                            provider.pool.workers.get(&loaded.key),
                                            Some(WorkerLifecycle::CatalogBusy)
                                        )
                                    });
                                if request.cancelled.load(Ordering::Acquire) || !catalog_busy {
                                    break ModernWork::Preview(deferred_preview.take().unwrap());
                                }
                            }
                            let (next, timeout) = cvar
                                .wait_timeout(slot, Duration::from_millis(10))
                                .unwrap_or_else(|error| error.into_inner());
                            slot = next;
                            if timeout.timed_out() {
                                break ModernWork::Refresh;
                            }
                        }
                    };

                    let job = match work {
                        ModernWork::Refresh => continue,
                        ModernWork::Preview(request) => {
                            thread_preview_endpoint.execute(&mut provider, request);
                            continue;
                        }
                        ModernWork::Action(request) => {
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
                                    "modern plugin `{}` action timed out before execution",
                                    plugin.0
                                )))
                            } else {
                                match catch_unwind(AssertUnwindSafe(|| {
                                    provider.execute_action_request(*request)
                                })) {
                                    Ok(result) => result,
                                    Err(_) => Err(crikey_core::CoreError::Invalid(format!(
                                        "modern plugin `{}` action worker panicked",
                                        plugin.0
                                    ))),
                                }
                            };
                            let result = if cancelled.load(Ordering::Acquire) {
                                Err(crikey_core::CoreError::Cancelled)
                            } else if Instant::now() >= deadline {
                                Err(crikey_core::CoreError::Invalid(format!(
                                    "modern plugin `{}` action timed out",
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
                                outcome: result.map(|()| crate::ActionEffect::Completed),
                            };
                            enqueue_modern_completion(&thread_completion_mailbox, completion);
                            thread_preview_endpoint.refresh_liveness(&mut provider);
                            *thread_health.lock().unwrap_or_else(|error| error.into_inner()) =
                                pipeline.plugin_health_report();
                            continue;
                        }
                        ModernWork::Configuration(configuration) => {
                            for (plugin, reason) in provider.publish_configuration(&configuration) {
                                eprintln!(
                                    "crikey: modern configuration not delivered to {}: {reason}",
                                    plugin.0
                                );
                            }
                            continue;
                        }
                        ModernWork::Query(job) => job,
                    };
                    last_now = job.now;
                    // Tell the provider which admitted query it is serving, so
                    // a lazy plugin's blocking startup can abandon an obsolete
                    // target list instead of walking the whole chain.
                    provider.begin_serving(job.intake);

                    // The blocking child interpreter calls happen here, on this
                    // thread — never on the caller's. Stale answers are refused
                    // at the pipeline's intake boundary inside `drive_query`.
                    let modern = provider.drive_query(&mut pipeline, &job.query, job.now);
                    thread_preview_endpoint.refresh_liveness(&mut provider);
                    *thread_health.lock().unwrap_or_else(|error| error.into_inner()) =
                        pipeline.plugin_health_report();

                    let mut rows = job.builtin_rows;
                    let mut pending = job.builtin_pending;
                    if let Some(frame) = modern {
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
                        settings_open: false,
                        settings: Arc::default(),
                        settings_focus: None,
                        show_hints: true,
                        rounded_corners: true,
                        page: None,
                        preview: Default::default(),
                    };

                    // A late answer must never appear under a newer generation.
                    // Hold the mailbox lock across the whole check-store-publish
                    // so the staleness gate cannot race a `submit`: `submit`
                    // records the newer generation into `current` *before* it
                    // locks the mailbox, so while we hold the lock either we
                    // observe the newer generation (and drop this frame) or no
                    // supersession has happened yet. A queued newer job
                    // (`slot.job`) is likewise a supersession in flight.
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
                preview_endpoint,
                permissions,
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
                preview_endpoint,
                permissions,
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
    /// Bounded preview endpoint; all transport and decoding runs off-thread.
    pub(crate) fn preview_dispatcher(&self) -> Arc<dyn PreviewDispatcher> {
        self.preview_endpoint.clone()
    }

    /// Submits a query for asynchronous modern processing and returns at once;
    /// the UI thread never waits on a plugin (spec 6.5, acceptance 31.1).
    ///
    /// `builtin_rows` are the built-in provider's rows for `generation`, which
    /// the merged frame keeps ahead of the modern rows.
    pub fn submit(
        &self,
        generation: Generation,
        query: &str,
        now: Millis,
        builtin_rows: Vec<ResultRow>,
        builtin_pending: bool,
        selected: usize,
    ) {
        // Intake is monotonic even though `Generation` can be reconstructed
        // from an external value. A delayed caller must not rewind the live
        // generation and make its obsolete job eligible for publication.
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
        // Signal superseded in-flight callbacks before queueing the new job.
        self.cancellation.cancel_before(generation_value);
        // Stamped before the job is queued: any later submission raises the
        // registry's count past this stamp, so a provider already inside this
        // job's startup handshake observes the supersession.
        let intake = self.cancellation.admit();

        let (lock, cvar) = &*self.mailbox;
        let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
        if slot.stop {
            return;
        }
        slot.job = Some(ModernJob {
            generation,
            query: query.to_owned(),
            intake,
            now,
            builtin_rows,
            builtin_pending,
            selected,
        });
        drop(slot);
        cvar.notify_one();
    }

    /// Hands the supervisor thread one complete configuration state to publish
    /// to every modern plugin (spec 21.4).
    ///
    /// Returns at once and never blocks the caller: delivery happens on the
    /// supervisor thread, which is the only thread allowed to touch a modern
    /// worker. Replace-oldest, so a caller that publishes twice before the
    /// supervisor gets a turn delivers only the newer state — the same
    /// coalescing rule the host applied upstream, enforced again here because the
    /// two are separated by a thread boundary.
    pub fn publish_configuration(&self, configuration: crate::PluginConfiguration) {
        let (lock, cvar) = &*self.mailbox;
        let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
        if slot.stop {
            return;
        }
        slot.configuration = Some(Box::new(configuration));
        drop(slot);
        cvar.notify_one();
    }

    /// Takes the latest merged frame the supervisor produced, if any, for the
    /// UI thread to fold into its retained view model. Single slot,
    /// replace-oldest: only the newest matters.
    pub fn take_outcome(&self) -> Option<ViewModel> {
        self.outcome
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    /// Whether any modern plugin loaded and is being served by the supervisor.
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

    /// Returns the exact plugin ids owned by this driver's action endpoint.
    pub fn plugins(&self) -> Vec<PluginId> {
        self.action_endpoint.gates.keys().cloned().collect()
    }
    /// Manifest grants used by the host-mediated action boundary.
    pub fn permissions(&self) -> BTreeMap<PluginId, Permissions> {
        self.permissions.clone()
    }

    /// Returns the bounded action endpoint sharing this driver's per-plugin
    /// budget handles.
    pub fn action_executor(&self) -> Arc<dyn crate::PluginActionExecutor> {
        self.action_endpoint.clone()
    }

    /// Per-plugin diagnostics (spec 24.3) as of the supervisor thread's last
    /// unit of work, including the per-kind §13.5 refusal counters.
    pub fn health_report(&self) -> Vec<(PluginId, crikey_plugin_supervisor::PluginHealth)> {
        let mut report = self
            .health
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        for (plugin, gate) in &self.action_endpoint.gates {
            let budget = gate.budget();
            if let Some((_, health)) = report.iter_mut().find(|(id, _)| id == plugin) {
                health.concurrency_refusals = budget.refusals_snapshot();
            } else {
                let health = crikey_plugin_supervisor::PluginHealth {
                    concurrency_refusals: budget.refusals_snapshot(),
                    ..crikey_plugin_supervisor::PluginHealth::default()
                };
                report.push((plugin.clone(), health));
            }
        }
        report
    }
}
impl Drop for ModernDriver {
    fn drop(&mut self) {
        self.preview_endpoint.shutdown();
        // Cancel any callback before signalling shutdown, so a cooperative
        // child can return and let the supervisor join promptly.
        self.cancellation.cancel_all();
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

#[cfg(test)]
mod preview_origin_tests {
    use super::*;

    #[test]
    fn lookup_keeps_producing_origin_when_live_worker_changes() {
        let plugin = PluginId("modern.origin-fixture".into());
        let item_id = ItemId("selected".into());
        let item = Item {
            stable_id: item_id.clone(),
            plugin_id: plugin.clone(),
            category: crikey_core::Category::Application,
            label: "Selected".into(),
            description: String::new(),
            target: "selected".into(),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: crikey_core::HitPolicy::Recorded,
            score_hint: 0,
            metadata: BTreeMap::from([("crikey.preview".into(), "deferred".into())]),
            actions: Vec::new(),
        };
        let (sender, _receiver) = mpsc::sync_channel(2);
        let endpoint = ModernPreviewEndpoint {
            sender,
            gates: BTreeMap::new(),
            action_in_flight: Arc::new(AtomicUsize::new(0)),
            catalog_items: Arc::new(Mutex::new(BTreeMap::new())),
            incarnations: Mutex::new(BTreeMap::from([(plugin.clone(), 11)])),
            items: Arc::new(Mutex::new(BTreeMap::from([(
                (plugin.clone(), item_id.clone()),
                (item, 7),
            )]))),
            pending: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            wake: Mutex::new(None),
        };
        let (found, origin) = endpoint
            .lookup(&plugin, &item_id)
            .expect("the retained item is known");
        assert_eq!(found.stable_id, item_id);
        assert_eq!(
            origin, 7,
            "lookup must not rebind retained rows to live worker 11"
        );
        let identity = PreviewIdentity {
            view: 1,
            query: Generation::ZERO,
            plugin,
            incarnation: 11,
            item: item_id,
            selection: 1,
            request: 1,
        };
        let refusal = endpoint
            .submit(
                PreviewRequest { identity },
                Arc::new(|_| panic!("retired-origin work must not be admitted")),
            )
            .expect_err("a live replacement must not receive a retained item's preview");
        assert!(refusal.contains("retired worker"));
    }

    struct Fixture {
        root: PathBuf,
        witnesses: PathBuf,
    }

    impl Fixture {
        fn private_directory(path: &Path) {
            let mut directory = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                directory.mode(0o700);
            }
            directory.create(path).unwrap();
        }

        fn declare_witnesses(package: &Path, witnesses: &Path) {
            Self::private_directory(witnesses);
            fs::write(
                package.join("preview-witness-directory"),
                witnesses.to_str().unwrap(),
            )
            .unwrap();
        }

        fn load(class: &str) -> (Self, ModernProvider, QueryPipeline) {
            Self::load_owners(class, false)
        }

        fn load_owners(class: &str, second_owner: bool) -> (Self, ModernProvider, QueryPipeline) {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/preview-handoff-fixtures")
                .join(format!(
                    "crikey-modern-lifecycle-{}-{}",
                    std::process::id(),
                    sequence
                ));
            let witnesses = std::env::temp_dir().join(format!(
                "crikey-modern-preview-witness-{}-{sequence}",
                std::process::id()
            ));
            Self::private_directory(&witnesses);
            let package = root.join("plugins/fixture");
            fs::create_dir_all(&package).unwrap();
            Self::declare_witnesses(&package, &witnesses.join("fixture"));
            fs::write(
                package.join("preview_plugin.py"),
                include_str!("../../../compatibility/test-plugins/modern-preview/preview_plugin.py"),
            )
            .unwrap();
            fs::write(
                package.join("crikey.toml"),
                format!(
                    "manifest-version = 1\n[plugin]\nid = \"fixture\"\nname = \"Fixture\"\n\
                 version = \"1.0.0\"\nruntime = \"python\"\n\
                 entrypoint = \"preview_plugin:{class}\"\n\
                 [python]\nrequires-python = \">=3.12\"\n"
                ),
            )
            .unwrap();
            if second_owner {
                let second = root.join("plugins/second");
                fs::create_dir_all(&second).unwrap();
                Self::declare_witnesses(&second, &witnesses.join("second"));
                fs::copy(
                    package.join("preview_plugin.py"),
                    second.join("preview_plugin.py"),
                )
                .unwrap();
                let manifest = fs::read_to_string(package.join("crikey.toml")).unwrap();
                fs::write(
                    second.join("crikey.toml"),
                    manifest.replace("id = \"fixture\"", "id = \"second\""),
                )
                .unwrap();
            }
            let mut pipeline = QueryPipeline::new(crate::PipelineConfig::default());
            let provider = ModernProvider::load(
                &mut pipeline,
                &[root.join("plugins")],
                None,
                root.join("cache"),
                &DisabledPlugins::default(),
            );
            assert_eq!(
                provider.loaded.len(),
                if second_owner { 2 } else { 1 },
                "{:?}",
                provider.unavailable()
            );
            (Self { root, witnesses }, provider, pipeline)
        }

        fn catalog(provider: &mut ModernProvider) -> CatalogBuildResult {
            let plugin = provider.loaded[0].plugin.clone();
            provider
                .request_catalog_build(&plugin, 1, Generation::ZERO)
                .unwrap();
            assert!(matches!(
                provider.pool.workers.get(&provider.loaded[0].key),
                Some(WorkerLifecycle::CatalogBusy)
            ));
            assert!(
                provider
                    .request_catalog_build(&plugin, 1, Generation::ZERO)
                    .is_err(),
                "a transferred producer cannot be admitted twice"
            );
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if let Some(result) = provider.take_catalog_results().into_iter().next() {
                    return result;
                }
                assert!(Instant::now() < deadline, "catalog did not finish");
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
            let _ = fs::remove_dir_all(&self.witnesses);
        }
    }

    #[test]
    fn other_owner_foreground_cancels_preview_on_the_shared_supervisor() {
        fn wait_for(mut ready: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !ready() {
                assert!(
                    Instant::now() < deadline,
                    "cross-owner preview handoff did not settle"
                );
                thread::sleep(Duration::from_millis(1));
            }
        }
        let (fixture, mut provider, mut pipeline) = Fixture::load_owners("HandoffPreview", true);
        provider.drive_query(&mut pipeline, "handoff", 1).unwrap();
        let preview_owner = PluginId("modern.fixture".into());
        let action_owner = PluginId("modern.second".into());
        let item_id = ItemId("handoff".into());
        let (preview_item, preview_origin) =
            provider.action_items.lock().unwrap()[&(preview_owner.clone(), item_id.clone())].clone();
        let (action_item, action_origin) =
            provider.action_items.lock().unwrap()[&(action_owner.clone(), item_id)].clone();
        let preview_budget = pipeline.plugin_budget(&preview_owner).unwrap().clone();
        let action_budget = pipeline.plugin_budget(&action_owner).unwrap().clone();
        let driver = ModernDriver::spawn(provider, pipeline, |_| {});
        let actions = driver.action_executor();
        let preview_package = fixture.witnesses.join("fixture");
        let action_package = fixture.witnesses.join("second");
        let (send, receive) = mpsc::channel();
        driver
            .preview_endpoint
            .submit(
                PreviewRequest {
                    identity: PreviewIdentity {
                        view: 1,
                        query: Generation::from_raw(1),
                        plugin: preview_owner.clone(),
                        incarnation: preview_origin,
                        item: preview_item.stable_id,
                        selection: 1,
                        request: 1,
                    },
                },
                Arc::new(move |update| send.send(update).unwrap()),
            )
            .unwrap();
        wait_for(|| preview_package.join("preview-started").exists());
        fs::write(action_package.join("action-hold"), "").unwrap();
        let action = action_item.actions[0].action_id.clone();
        let before = Instant::now();
        let request = actions
            .submit_plugin_action(&action_owner, &action_item, &action, Some("other owner"))
            .unwrap();
        assert!(
            before.elapsed() < Duration::from_secs(1),
            "UI waited for another owner's preview"
        );
        wait_for(|| preview_package.join("preview-cancelled").exists());
        assert_eq!(preview_budget.in_flight(BudgetKind::Action), 1);
        assert_eq!(action_budget.in_flight(BudgetKind::Action), 1);
        assert!(!action_package.join("executions").exists());
        assert!(actions
            .submit_plugin_action(&action_owner, &action_item, &action, None)
            .is_err());
        fs::write(preview_package.join("preview-release"), "").unwrap();
        wait_for(|| action_package.join("executions").exists());
        assert!(preview_package.join("preview-terminal").exists());
        assert_eq!(preview_budget.in_flight(BudgetKind::Action), 0);
        assert_eq!(action_budget.in_flight(BudgetKind::Action), 1);
        assert!(matches!(
            receive.recv_timeout(Duration::from_secs(5)).unwrap().state,
            PreviewState::Cancelled
        ));
        fs::write(action_package.join("action-release"), "").unwrap();
        let mut completions = Vec::new();
        wait_for(|| {
            completions.extend(actions.poll_plugin_actions());
            !completions.is_empty()
        });
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].request_id, request);
        assert!(completions[0].outcome.is_ok(), "{:?}", completions[0].outcome);
        let executions = fs::read_to_string(action_package.join("executions")).unwrap();
        assert_eq!(executions.lines().count(), 1);
        let event: serde_json::Value = serde_json::from_str(executions.trim()).unwrap();
        assert_eq!(event["argument"], "other owner");
        assert!(!preview_package.join("executions").exists());
        assert_eq!(
            driver.preview_endpoint.incarnation(&preview_owner),
            Some(preview_origin)
        );
        assert_eq!(
            driver.preview_endpoint.incarnation(&action_owner),
            Some(action_origin)
        );
        assert_eq!(action_budget.in_flight(BudgetKind::Action), 0);
    }

    #[test]
    fn blocked_modern_preview_yields_once_without_releasing_quota_or_retagging_origin() {
        fn wait_for(mut ready: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !ready() {
                assert!(Instant::now() < deadline, "preview handoff did not settle");
                thread::sleep(Duration::from_millis(1));
            }
        }
        for mode in ["primary", "alternate", "cancel", "stale"] {
            let (fixture, mut provider, mut pipeline) = Fixture::load("HandoffPreview");
            provider.drive_query(&mut pipeline, "handoff", 1).unwrap();
            let owner = provider.loaded[0].plugin.clone();
            let key = (owner.clone(), ItemId("handoff".into()));
            let (item, origin) = provider.action_items.lock().unwrap()[&key].clone();
            let budget = pipeline.plugin_budget(&owner).unwrap().clone();
            let driver = ModernDriver::spawn(provider, pipeline, |_| {});
            let actions = driver.action_executor();
            let package = fixture.witnesses.join("fixture");
            let (send, receive) = mpsc::channel();
            driver
                .preview_endpoint
                .submit(
                    PreviewRequest {
                        identity: PreviewIdentity {
                            view: 1,
                            query: Generation::from_raw(1),
                            plugin: owner.clone(),
                            incarnation: origin,
                            item: item.stable_id.clone(),
                            selection: 1,
                            request: 1,
                        },
                    },
                    Arc::new(move |update| send.send(update).unwrap()),
                )
                .unwrap();
            wait_for(|| package.join("preview-started").exists());
            if mode == "stale" {
                driver
                    .action_endpoint
                    .items
                    .lock()
                    .unwrap()
                    .get_mut(&key)
                    .unwrap()
                    .1 = origin + 1;
            }
            let action = ActionId(
                if mode == "alternate" {
                    "alternate"
                } else {
                    "primary"
                }
                .into(),
            );
            let before = Instant::now();
            let request = actions
                .submit_plugin_action(&owner, &item, &action, Some("literal argument"))
                .unwrap();
            assert!(before.elapsed() < Duration::from_secs(1), "UI waited for preview");
            wait_for(|| package.join("preview-cancelled").exists());
            assert_eq!(budget.in_flight(BudgetKind::Action), 1);
            assert!(
                !package.join("executions").exists(),
                "cancel is not callback retirement"
            );
            assert!(actions
                .submit_plugin_action(&owner, &item, &action, None)
                .is_err());
            if mode == "cancel" {
                assert!(actions.cancel_plugin_action(&request));
            } else if mode != "stale" {
                fs::write(package.join("action-hold"), "").unwrap();
                let mut items = driver.action_endpoint.items.lock().unwrap();
                let replacement = &mut items.get_mut(&key).unwrap().0;
                replacement.actions.clear();
                replacement.target = "replacement target".into();
            }
            fs::write(package.join("preview-release"), "").unwrap();
            if mode == "primary" || mode == "alternate" {
                wait_for(|| package.join("executions").exists());
                assert!(package.join("preview-terminal").exists());
                assert_eq!(budget.in_flight(BudgetKind::Action), 1);
                assert!(actions
                    .submit_plugin_action(&owner, &item, &action, None)
                    .is_err());
                fs::write(package.join("action-release"), "").unwrap();
            }
            let mut completions = Vec::new();
            wait_for(|| {
                completions.extend(actions.poll_plugin_actions());
                !completions.is_empty()
            });
            assert_eq!(completions.len(), 1);
            assert_eq!(completions[0].request_id, request);
            assert_eq!(
                completions[0].outcome.is_ok(),
                mode == "primary" || mode == "alternate"
            );
            assert!(matches!(
                receive.recv_timeout(Duration::from_secs(5)).unwrap().state,
                PreviewState::Cancelled
            ));
            assert_eq!(budget.in_flight(BudgetKind::Action), 0);
            if mode == "primary" || mode == "alternate" {
                let executions = fs::read_to_string(package.join("executions")).unwrap();
                assert_eq!(executions.lines().count(), 1);
                let event: serde_json::Value = serde_json::from_str(executions.trim()).unwrap();
                assert_eq!(
                    event["pid"].to_string(),
                    fs::read_to_string(package.join("preview-started")).unwrap()
                );
                assert_eq!(event["target"], "frozen target");
                assert_eq!(event["action"], action.0);
                assert_eq!(event["argument"], "literal argument");
                assert_eq!(driver.preview_endpoint.incarnation(&owner), Some(origin));
            } else {
                assert!(!package.join("executions").exists());
            }
            assert!(actions.poll_plugin_actions().is_empty());
        }
    }

    #[test]
    fn catalog_callback_failure_preserves_transferred_provider_suggestions() {
        for already_live in [false, true] {
            let (_fixture, mut provider, mut pipeline) = Fixture::load("CatalogRaises");
            let key = provider.loaded[0].key.clone();
            assert!(provider.pool.never_started(&key));
            let original = if already_live {
                assert_eq!(provider.drive_query(&mut pipeline, "B", 1).unwrap().rows.len(), 1);
                Some(provider.pool.live_mut(&key).unwrap().incarnation())
            } else {
                None
            };
            let result = Fixture::catalog(&mut provider);
            assert!(
                matches!(result, CatalogBuildResult::Failed { ref reason, .. }
                if reason.contains("fixture catalog failure")),
                "{result:?}"
            );
            assert_eq!(
                pipeline
                    .plugin_budget(&provider.loaded[0].plugin)
                    .unwrap()
                    .in_flight(BudgetKind::Catalog),
                0
            );
            if let Some(original) = original {
                assert_eq!(
                    provider.pool.live_mut(&key).unwrap().incarnation(),
                    original,
                    "a pre-existing healthy producer must return to its slot"
                );
            } else {
                assert!(
                    provider.pool.never_started(&key),
                    "a deliberately released healthy producer leaves lazy queries available"
                );
            }
            assert_eq!(
                provider.drive_query(&mut pipeline, "B", 2).unwrap().rows.len(),
                1,
                "catalog callback failure must not permanently fail the query slot"
            );
            provider.shutdown(0);
        }
    }

    #[test]
    fn idle_catalog_exit_retires_current_preview_and_links_without_transport() {
        let (fixture, mut provider, pipeline) = Fixture::load("IdleExit");
        let CatalogBuildResult::Complete(build) = Fixture::catalog(&mut provider) else {
            panic!("idle-exit catalog must complete");
        };
        let item = build
            .items
            .into_iter()
            .find(|item| item.stable_id.0 == "B")
            .unwrap();
        let plugin = item.plugin_id.clone();
        let modern = ModernDriver::spawn(provider, pipeline, |_| {});
        let mut native_pipeline = QueryPipeline::new(crate::PipelineConfig::default());
        let native_provider =
            crate::NativeProvider::load(&mut native_pipeline, &[], &DisabledPlugins::default());
        let native = crate::NativeDriver::spawn(native_provider, native_pipeline, Box::new(|_| {}));
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake_count = Arc::clone(&wakes);
        let mut service = crate::PreviewService::new(&native, &modern, move || {
            wake_count.fetch_add(1, Ordering::AcqRel);
        });
        let search = crate::SearchService::new(crate::App::new());
        let mut model = crikey_ui::LauncherViewModel::new();
        model.activate();
        let _ = model.apply(crikey_ui::UiCommand::SetQuery("B".into()));
        let generation = Generation::from_raw(1);
        model.begin_generation(generation);
        model.publish(
            generation,
            vec![ResultRow {
                plugin: item.plugin_id,
                item: item.stable_id,
                label: item.label,
                description: item.description,
                icon_reference: None,
                icon: None,
                category: String::new(),
                plugin_name: String::new(),
                highlights: Vec::new(),
                argument_hint: None,
                status: None,
                default_action: None,
                alternate_actions: Vec::new(),
            }],
            false,
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        let content_frame = loop {
            service.synchronize(&mut model, &search);
            if let Some(frame) = model.frame() {
                if matches!(
                    frame.preview.status.as_ref(),
                    Some(crikey_ui::PreviewStatus::Content(_))
                ) {
                    break frame;
                }
            }
            assert!(Instant::now() < deadline, "preview did not publish");
            thread::sleep(Duration::from_millis(1));
        };
        let target = content_frame.preview.link_target.unwrap();
        let before = wakes.load(Ordering::Acquire);
        fs::write(fixture.root.join("plugins/fixture/exit-now"), "").unwrap();
        // No suggest, preview, action, configuration or shutdown call follows:
        // only the idle supervisor may discover this actual process exit.
        while modern.preview_endpoint.incarnation(&plugin).is_some() {
            assert!(Instant::now() < deadline, "idle worker retained its incarnation");
            thread::sleep(Duration::from_millis(1));
        }
        while wakes.load(Ordering::Acquire) == before {
            assert!(Instant::now() < deadline, "retirement did not wake the UI");
            thread::yield_now();
        }
        assert!(
            service
                .open_link(&target, "https://example.org/", &search)
                .is_err(),
            "retired producer links must fail even before presentation synchronizes"
        );
        service.synchronize(&mut model, &search);
        let retired_frame = model.frame().expect("retirement must publish a frame");
        assert!(matches!(
            retired_frame.preview.status,
            Some(crikey_ui::PreviewStatus::Unavailable(_))
        ));
        assert!(retired_frame.preview.link_target.is_none());
    }
}
