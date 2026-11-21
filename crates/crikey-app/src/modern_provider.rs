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
//!
//! Typed root metadata is admitted with the complete catalog or exact suggestion
//! generation. A captured suggestion scope survives executable retirement only
//! while its declaration and physical producer remain current. Typed submissions
//! retain the frozen item/schema and confirm it on that physical worker.
//!
//! Global stop permanently fences bounded endpoint and wire admission. Its wait
//! joins the existing supervisor, catalog transfers, process trees and pipe
//! threads; failed settlement retains their canonical owners behind the control.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crikey_core::command::{
    validate_plugin_commands, CommandAvailability, CommandDeclaration, PluginCommands, MAX_COMMANDS,
    MAX_COMMAND_BYTES,
};
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

use crate::plugin_action::{
    RootExecutableBinding, RootPresentationSnapshot, RootScope, RootSource, RootSourceLedger,
    SharedOriginItems,
};
use crate::plugin_preview::{
    PendingPreviews, PreviewDispatcher, PreviewIdentity, PreviewPublish, PreviewRequest, PreviewState,
    PreviewUpdate,
};
use crate::preview_action_budget::{ForegroundActionAdmission, PreviewActionGate};
use crate::query_pipeline::ProviderQueryContext;
use crate::runtime_control::{AdmissionGate, DriverControl, StopError, StoppedRuntime};
use crate::{
    plugin_icons::PluginIconResolver, ActionRequestId, BatchState, CatalogBuildResult, CatalogDispatchError,
    DisabledPlugins, ObsoleteCatalogBuild, PluginActionCompletion, QueryPipeline, ResultBatch,
    DISABLED_BY_CONFIGURATION,
};
use crikey_core::result_presentation::ResultPresentation;
use crikey_core::structured_arguments::{ArgumentValues, ExecutableKey, OwnedItem};
use crikey_core::{PreviewContent, PreviewImage, PreviewText, PreviewValue, PreviewValueKind};

struct ModernPreviewJob {
    request: PreviewRequest,
    publish: PreviewPublish,
    cancelled: Arc<AtomicBool>,
}

struct ModernPreviewEndpoint {
    sender: SyncSender<ModernPreviewJob>,
    admission: AdmissionGate,
    gates: BTreeMap<PluginId, Arc<PreviewActionGate>>,
    action_in_flight: Arc<AtomicUsize>,
    incarnations: Arc<Mutex<BTreeMap<PluginId, u64>>>,
    items: SharedOriginItems,
    catalog_items: SharedOriginItems,
    static_catalog_items: Arc<Mutex<BTreeMap<(PluginId, ItemId), Item>>>,
    accepted_commands: ModernAcceptedCommands,
    pending: PendingPreviews<AtomicBool>,
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
            admission: provider.cancellation.admission.clone(),
            gates: provider
                .action_budgets()
                .into_iter()
                .map(|(owner, budget)| (owner, PreviewActionGate::new(budget)))
                .collect(),
            action_in_flight: Arc::new(AtomicUsize::new(0)),
            incarnations: Arc::new(Mutex::new(BTreeMap::new())),
            items: Arc::clone(&provider.action_items),
            catalog_items: Arc::clone(&provider.catalog.items),
            static_catalog_items: Arc::clone(&provider.catalog.static_items),
            accepted_commands: Arc::clone(&provider.catalog.accepted_commands),
            pending: Arc::new(Mutex::new(Vec::new())),
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
                job.cancelled.store(true, Ordering::Release);
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
            let cancellation: Arc<dyn crate::preview_action_budget::PreviewCancellation> =
                job.cancelled.clone();
            let Some(_guard) = gate.try_preview(cancellation) else {
                return Ok(PreviewState::Cancelled);
            };
            if self.action_in_flight.load(Ordering::Acquire) != 0 {
                job.cancelled.store(true, Ordering::Release);
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
                .lookup(&identity.plugin, &identity.item, identity.catalog_source.as_ref())
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
            let was_live = worker.is_alive();
            let reply = catch_unwind(AssertUnwindSafe(|| {
                worker.preview(&current, &wire_identity).inspect_err(|error| {
                    provider
                        .cancellation
                        .observe_transport_error(was_live, worker, error);
                })
            }));
            control.finish();
            provider
                .cancellation
                .failed
                .fetch_or(watcher.join().is_err(), Ordering::AcqRel);
            let reply = reply.map_err(|_| {
                provider.cancellation.failed.store(true, Ordering::Release);
                "modern preview callback failed".to_owned()
            })?;
            if job.cancelled.load(Ordering::Acquire) {
                return Ok(PreviewState::Cancelled);
            }
            decode_modern_preview(reply.map_err(|error| error.to_string())?)
        }));
        let state = match result {
            Ok(Ok(state)) => state,
            Ok(Err(reason)) => PreviewState::Error(reason),
            Err(_) => {
                provider.cancellation.failed.store(true, Ordering::Release);
                PreviewState::Error("modern preview supervisor panicked".into())
            }
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
    fn foreground_blocked(&self, _owner: &PluginId) -> bool {
        self.action_in_flight.load(Ordering::Acquire) != 0
    }

    fn foreground_conflicts(&self, preview_owner: &PluginId, action_owner: &PluginId) -> bool {
        self.gates.contains_key(preview_owner) && self.gates.contains_key(action_owner)
    }

    fn lookup(
        &self,
        plugin: &PluginId,
        item: &ItemId,
        source: Option<&crate::plugin_action::DeclaredCommandSource>,
    ) -> Option<(Item, u64)> {
        if self.admission.state() != crate::runtime_control::PluginAdmission::Open {
            return None;
        }
        let key = (plugin.clone(), item.clone());
        if let Some(source) = source {
            let publications = self
                .accepted_commands
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let current = publications.get(plugin)?;
            if current.status != ModernCommandStatus::Ready
                || current.request_id != source.publication
                || current.instance != source.instance
                || current.generation != source.generation
                || current.incarnation != source.producer
            {
                return None;
            }
            return self
                .catalog_items
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&key)
                .map(|(item, _)| (item.clone(), source.producer))
                .or_else(|| {
                    self.static_catalog_items
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .get(&key)
                        .map(|item| (item.clone(), source.producer))
                });
        }
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
    fn origin(
        &self,
        plugin: &PluginId,
        item: &ItemId,
        source: Option<&crate::plugin_action::DeclaredCommandSource>,
    ) -> Option<u64> {
        if self.admission.state() != crate::runtime_control::PluginAdmission::Open {
            return None;
        }
        if source.is_some() {
            return self.lookup(plugin, item, source).map(|(_, origin)| origin);
        }
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
        if self.admission.state() != crate::runtime_control::PluginAdmission::Open {
            return None;
        }
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
            .lookup(&id.plugin, &id.item, id.catalog_source.as_ref())
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
        let job = ModernPreviewJob {
            request,
            publish,
            cancelled,
        };
        let sent = match self.admission.admit() {
            Ok(_admission) => self.sender.try_send(job),
            Err(_) => Err(mpsc::TrySendError::Disconnected(job)),
        };
        if sent.is_err() {
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

/// Installed command data is independent of preview-worker liveness. The source
/// tags must also match an accepted catalog before composition into the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModernCommandStatus {
    Pending,
    Ready,
    Unsupported,
    Invalid(String),
    Failed(String),
    Cancelled,
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct ModernOwnerCommands {
    pub request_id: u64,
    pub instance: u64,
    pub generation: Generation,
    pub incarnation: u64,
    pub status: ModernCommandStatus,
    pub declarations: Vec<CommandDeclaration>,
}

#[derive(Debug, Clone, Default)]
pub struct ModernCommandSnapshot {
    pub revision: u64,
    pub owners: BTreeMap<PluginId, Arc<ModernOwnerCommands>>,
}

type ModernCommands = Arc<Mutex<Arc<ModernCommandSnapshot>>>;
/// Canonical source records share the catalog replacement lock. Pending
/// declaration discovery must not retire a still-current accepted catalog.
type ModernAcceptedCommands = Arc<Mutex<BTreeMap<PluginId, Arc<ModernOwnerCommands>>>>;

fn publish_modern_commands(
    commands: &ModernCommands,
    owner: PluginId,
    mut incoming: ModernOwnerCommands,
    admission: bool,
) -> bool {
    let mut current = commands.lock().unwrap_or_else(|error| error.into_inner());
    if !admission
        && !current.owners.get(&owner).is_some_and(|source| {
            source.request_id == incoming.request_id && source.status == ModernCommandStatus::Pending
        })
    {
        return false;
    }
    if !current.owners.contains_key(&owner) && current.owners.len() >= MAX_COMMANDS {
        return false;
    }
    let retained = current.owners.iter().filter(|(key, _)| *key != &owner);
    let mut count = incoming.declarations.len();
    let mut bytes = command_snapshot_bytes(&incoming.declarations);
    for (_, source) in retained {
        count += source.declarations.len();
        bytes += command_snapshot_bytes(&source.declarations);
    }
    if count > MAX_COMMANDS || bytes > MAX_COMMAND_BYTES {
        incoming.declarations = Vec::new();
        incoming.status = ModernCommandStatus::Invalid("command snapshot exceeds bounds".into());
    }
    if let ModernCommandStatus::Invalid(reason) | ModernCommandStatus::Failed(reason) = &mut incoming.status {
        const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;
        if reason.len() > MAX_DIAGNOSTIC_BYTES {
            let mut end = MAX_DIAGNOSTIC_BYTES;
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason.truncate(end);
        }
    }
    let Some(revision) = current.revision.checked_add(1) else {
        return false;
    };
    let next = Arc::make_mut(&mut *current);
    next.revision = revision;
    next.owners.insert(owner, Arc::new(incoming));
    true
}

fn command_snapshot_bytes(declarations: &[CommandDeclaration]) -> usize {
    declarations
        .iter()
        .map(|command| {
            let target = match &command.executable_target {
                crikey_core::command::ExecutableTarget::Item { item_id, action_id } => {
                    item_id.0.len() + action_id.as_ref().map_or(0, |id| id.0.len())
                }
                crikey_core::command::ExecutableTarget::Host(_) => 0,
            };
            command.reference.provider_id.0.len()
                + command.reference.command_id.len()
                + command.label.len()
                + target
        })
        .sum()
}

fn invalidate_modern_commands(
    commands: &ModernCommands,
    accepted_commands: &ModernAcceptedCommands,
    owner: &PluginId,
    request_id: u64,
) -> bool {
    let mut accepted = accepted_commands
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let removed = accepted
        .get(owner)
        .is_some_and(|source| source.request_id == request_id);
    if removed {
        accepted.remove(owner);
    }
    let mut current = commands.lock().unwrap_or_else(|error| error.into_inner());
    let Some(source) = current.owners.get(owner) else {
        return removed;
    };
    if source.request_id != request_id || source.status == ModernCommandStatus::Unavailable {
        return removed;
    }
    let Some(revision) = current.revision.checked_add(1) else {
        return false;
    };
    let retired = ModernOwnerCommands {
        request_id: source.request_id,
        instance: source.instance,
        generation: source.generation,
        incarnation: source.incarnation,
        status: ModernCommandStatus::Unavailable,
        declarations: Vec::new(),
    };
    let next = Arc::make_mut(&mut *current);
    next.owners.insert(owner.clone(), Arc::new(retired));
    next.revision = revision;
    true
}

fn bind_modern_commands(
    owner: &PluginId,
    items: &[Item],
    reply: PluginCommands,
) -> (ModernCommandStatus, Vec<CommandDeclaration>) {
    let declarations = match reply {
        PluginCommands::Unsupported => return (ModernCommandStatus::Unsupported, Vec::new()),
        PluginCommands::Cancelled => return (ModernCommandStatus::Cancelled, Vec::new()),
        PluginCommands::Declarations(declarations) => declarations,
    };
    let invalid = |reason: &str| (ModernCommandStatus::Invalid(reason.into()), Vec::new());
    if let Err(reason) = validate_plugin_commands(&declarations) {
        return invalid(reason);
    }
    let mut catalog = BTreeMap::new();
    for item in items {
        if &item.plugin_id != owner || catalog.insert(&item.stable_id, item).is_some() {
            return invalid("ambiguous catalog ownership or item identity");
        }
    }
    // Build each action-targeted item's index once. A missing index records
    // any duplicate action ID on that item, matching canonical admission.
    // IDs stay borrowed from this catalog; unreferenced items need no index.
    let mut action_indexes = BTreeMap::new();
    for declaration in &declarations {
        let Some(item) = catalog.get(&declaration.item_id) else {
            return invalid("command target is absent from the exact owner's catalog");
        };
        if declaration.argument_policy != item.argument_policy {
            return invalid("command argument policy differs from its catalog target");
        }
        // Ordinary default dispatch selects the first catalog action. Omission
        // must not bypass its policy, applicability or duplicate-ID checks.
        // An actionless item may still use the plugin's None-action callback.
        let action_id = declaration
            .action_id
            .as_ref()
            .or_else(|| item.actions.first().map(|action| &action.action_id));
        if let Some(action_id) = action_id {
            let actions = action_indexes.entry(&item.stable_id).or_insert_with(|| {
                let mut actions = BTreeMap::new();
                for action in &item.actions {
                    let valid = action.execution_policy == ExecutionPolicy::Plugin
                        && (action.applicable_categories.is_empty()
                            || action.applicable_categories.contains(&item.category));
                    if actions.insert(&action.action_id, valid).is_some() {
                        return None;
                    }
                }
                Some(actions)
            });
            let Some(actions) = actions.as_ref() else {
                return invalid("command target catalog item has duplicate action IDs");
            };
            let Some(valid) = actions.get(action_id) else {
                return invalid("command action is absent");
            };
            if !*valid {
                return invalid("command action is ambiguous or not plugin executable");
            }
        }
    }
    let bound: Vec<_> = declarations
        .into_iter()
        .map(|declaration| declaration.bind(owner.clone(), CommandAvailability::Available))
        .collect();
    if bound.iter().any(|declaration| declaration.validate().is_err()) {
        return invalid("invalid host-bound command");
    }
    (ModernCommandStatus::Ready, bound)
}

/// One catalog build running away from the query worker.
#[derive(Debug)]
struct ModernCatalogTask {
    join: Option<JoinHandle<()>>,
    key: WorkerKey,
    owner: Arc<Mutex<ModernCatalogOwner>>,
}

#[derive(Debug)]
struct ModernCatalogOwner {
    worker: Option<ModernWorker>,
    watcher: Option<JoinHandle<()>>,
}

/// Returning a healthy, deliberately released lazy producer is not a failure.
#[derive(Debug)]
enum CatalogWorkerReturn {
    Retained(ModernWorker),
    Unsettled(ModernWorker),
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
    result_tx: SyncSender<(
        u64,
        CatalogBuildResult,
        CatalogWorkerReturn,
        ModernOwnerCommands,
        ResultPresentation,
    )>,
    result_rx: Receiver<(
        u64,
        CatalogBuildResult,
        CatalogWorkerReturn,
        ModernOwnerCommands,
        ResultPresentation,
    )>,
    tasks: BTreeMap<u64, ModernCatalogTask>,
    latest: BTreeMap<PluginId, u64>,
    next_id: u64,
    failed: bool,
    items: SharedOriginItems,
    static_items: Arc<Mutex<BTreeMap<(PluginId, ItemId), Item>>>,
    commands: ModernCommands,
    accepted_commands: ModernAcceptedCommands,
    root: Arc<Mutex<RootSourceLedger>>,
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
            failed: false,
            items: Arc::new(Mutex::new(BTreeMap::new())),
            static_items: Arc::new(Mutex::new(BTreeMap::new())),
            commands: Arc::new(Mutex::new(Arc::new(ModernCommandSnapshot::default()))),
            accepted_commands: Arc::default(),
            root: Arc::default(),
        }
    }
}

impl ModernCatalogDispatcher {
    fn next_id(&mut self) -> Option<u64> {
        self.next_id = self.next_id.checked_add(1)?;
        Some(self.next_id)
    }

    fn insert(
        &mut self,
        id: u64,
        plugin: PluginId,
        key: WorkerKey,
        join: JoinHandle<()>,
        owner: Arc<Mutex<ModernCatalogOwner>>,
    ) {
        self.latest.insert(plugin.clone(), id);
        self.tasks.insert(
            id,
            ModernCatalogTask {
                join: Some(join),
                key,
                owner,
            },
        );
    }

    fn take(&mut self, pool: &mut ModernWorkerPool) -> Vec<CatalogBuildResult> {
        let mut ready = Vec::new();
        while let Ok((id, result, worker, mut commands, presentation)) = self.result_rx.try_recv() {
            let Some(mut task) = self.tasks.remove(&id) else {
                continue;
            };
            if let Some(join) = task.join.take() {
                self.failed |= join.join().is_err();
            }
            let incarnation = match worker {
                CatalogWorkerReturn::Retained(worker) => {
                    let incarnation = worker.incarnation();
                    pool.workers.insert(task.key, WorkerLifecycle::Live(worker));
                    incarnation
                }
                CatalogWorkerReturn::Unsettled(worker) => {
                    self.failed = true;
                    pool.workers.insert(
                        task.key.clone(),
                        WorkerLifecycle::Failed {
                            reason: "worker reap failed".into(),
                        },
                    );
                    pool.retired.insert(task.key, worker);
                    0
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
            let mut accepted = self
                .accepted_commands
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            // Declaration retirement revokes sidecar authority, not an
            // otherwise successful ordinary catalog. The accepted-source lock
            // above serializes this decision with source invalidation.
            let admitted = match &result {
                CatalogBuildResult::Complete(build) => self
                    .commands
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .owners
                    .get(&build.plugin)
                    .is_some_and(|source| {
                        source.request_id == id && source.status == ModernCommandStatus::Pending
                    }),
                _ => false,
            };
            if admitted {
                if let CatalogBuildResult::Complete(build) = &result {
                    let source = RootSource::Catalog(crate::plugin_action::DeclaredCommandSource {
                        publication: id,
                        instance: build.instance,
                        generation: build.generation,
                        producer: commands.incarnation,
                    });
                    let mut root = self.root.lock().unwrap_or_else(|error| error.into_inner());
                    if root
                        .publish(build.plugin.clone(), source, &build.items, presentation)
                        .is_err()
                    {
                        root.invalidate(&build.plugin);
                        // Presentation/executable rejection must not discard
                        // ordinary items or overwrite an existing declaration
                        // validation diagnostic (for example duplicate actions).
                        if matches!(
                            commands.status,
                            ModernCommandStatus::Ready | ModernCommandStatus::Unsupported
                        ) {
                            commands.status =
                                ModernCommandStatus::Invalid("invalid catalog presentation".into());
                        }
                        commands.declarations.clear();
                    }
                }
            }
            if let CatalogBuildResult::Complete(build) = &result {
                accepted.remove(&build.plugin);
                let mut items = self.items.lock().unwrap_or_else(|error| error.into_inner());
                items.retain(|(owner, _), _| owner != &build.plugin);
                let mut static_items = self
                    .static_items
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                static_items.retain(|(owner, _), _| owner != &build.plugin);
                if incarnation == 0 {
                    for item in &build.items {
                        static_items.insert((build.plugin.clone(), item.stable_id.clone()), item.clone());
                    }
                }
                if incarnation != 0 {
                    for item in &build.items {
                        items.insert(
                            (build.plugin.clone(), item.stable_id.clone()),
                            (item.clone(), incarnation),
                        );
                    }
                }
            }
            match &result {
                CatalogBuildResult::Complete(build) => {
                    if publish_modern_commands(&self.commands, build.plugin.clone(), commands, false) {
                        let publications = self.commands.lock().unwrap_or_else(|error| error.into_inner());
                        if let Some(source) = publications
                            .owners
                            .get(&build.plugin)
                            .filter(|source| source.status == ModernCommandStatus::Ready)
                        {
                            accepted.insert(build.plugin.clone(), Arc::clone(source));
                        }
                    }
                }
                CatalogBuildResult::Failed { plugin, .. } => {
                    if self.latest.get(plugin) == Some(&id) {
                        accepted.remove(plugin);
                    }
                    publish_modern_commands(&self.commands, plugin.clone(), commands, false);
                }
                CatalogBuildResult::Obsolete(_) => {}
            }
            ready.push(result);
        }
        ready
    }

    fn shutdown(&mut self, pool: &mut ModernWorkerPool) -> Vec<CatalogBuildResult> {
        for task in self.tasks.values_mut() {
            if let Some(join) = task.join.take() {
                self.failed |= join.join().is_err();
            }
        }
        let results = self.take(pool);
        self.failed |= !self.tasks.is_empty();
        for task in self.tasks.values() {
            let mut owner = task.owner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(watcher) = owner.watcher.take() {
                self.failed |= watcher.join().is_err();
            }
            if let Some(worker) = owner.worker.as_mut() {
                self.failed |= worker.shutdown_checked().is_err();
            }
        }
        self.latest.clear();
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
    startup_workers: Mutex<BTreeMap<PluginId, Arc<Mutex<ModernWorker>>>>,
    admission: AdmissionGate,
    failed: AtomicBool,
    calls: Mutex<BTreeMap<(u64, PluginId), Arc<ModernCallControl>>>,
    catalog_calls: Mutex<BTreeMap<u64, Arc<ModernCallControl>>>,
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
    /// Intake whose suggestion authority has completed retirement under `root`.
    suggestion_intake: AtomicU64,
    #[cfg(test)]
    publication_pause: Mutex<Option<(SyncSender<()>, Receiver<()>)>>,
    #[cfg(test)]
    spawn_pause: Mutex<Option<(SyncSender<()>, Receiver<()>)>>,
}

impl ModernCancellation {
    fn spawn_worker(
        &self,
        interpreter: &Interpreter,
        options: WorkerOptions,
    ) -> Result<ModernWorker, crikey_python_host::HostError> {
        let owner = options.plugin.clone();
        if self
            .startup_workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&owner)
        {
            return Err(crikey_python_host::HostError::TeardownFailed);
        }
        #[cfg(test)]
        if let Some((entered, resume)) = self.spawn_pause.lock().unwrap().take() {
            entered.send(()).unwrap();
            resume.recv().unwrap();
        }
        let gate = self.admission.clone();
        ModernWorker::spawn_with_admission(interpreter, options, move |send| {
            let Ok(_admission) = gate.admit() else {
                return false;
            };
            send()
        })
        .inspect_err(|error| {
            if let crikey_python_host::HostError::StartupCleanup { worker } = error {
                self.startup_workers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(owner.clone(), Arc::clone(worker));
                self.failed.store(true, Ordering::Release);
            }
            if matches!(error, crikey_python_host::HostError::TeardownFailed)
                || (self.admission.state() != crate::runtime_control::PluginAdmission::Open
                    && matches!(
                        error,
                        crikey_python_host::HostError::Crashed { .. }
                            | crikey_python_host::HostError::Protocol(_)
                            | crikey_python_host::HostError::Timeout
                    ))
            {
                self.failed.store(true, Ordering::Release);
            }
        })
    }

    fn observe_transport_error(
        &self,
        was_live: bool,
        worker: &ModernWorker,
        error: &crikey_python_host::HostError,
    ) {
        // A completed guest error or historical retired slot is not lost work.
        // AdmissionClosed proves the writer refused before handing work over.
        if was_live
            && !worker.is_alive()
            && matches!(
                error,
                crikey_python_host::HostError::Crashed { .. }
                    | crikey_python_host::HostError::Protocol(_)
                    | crikey_python_host::HostError::Timeout
            )
            && self.admission.state() != crate::runtime_control::PluginAdmission::Open
        {
            self.failed.store(true, Ordering::Release);
        }
    }

    fn register(&self, generation: u64, plugin: PluginId, control: Arc<ModernCallControl>) {
        let admission = self.admission.admit();
        if admission.is_err() {
            control.cancel();
        }
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

    fn source_current(&self, source: &RootSource) -> bool {
        self.admission.state() == crate::runtime_control::PluginAdmission::Open
            && (matches!(source, RootSource::Catalog(_))
                || self.suggestion_intake.load(Ordering::Acquire) == self.intake())
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
        let catalog_controls = self
            .catalog_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for control in catalog_controls {
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
    retired: BTreeMap<WorkerKey, ModernWorker>,
    teardown_failed: bool,
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
        if let Some(WorkerLifecycle::Live(mut worker)) = self
            .workers
            .insert(key.clone(), WorkerLifecycle::Failed { reason })
        {
            if worker.shutdown_checked().is_err() {
                self.teardown_failed = true;
                self.retired.insert(key.clone(), worker);
            }
        }
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
    icon_resolver: Option<Arc<PluginIconResolver>>,
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
    action_items: SharedOriginItems,
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
            icon_resolver: None,
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
    fn install_icon_resolver(&mut self, pipeline: &mut QueryPipeline) {
        let mut resolver = PluginIconResolver::default();
        resolver.set_admission(self.cancellation.admission.clone());
        for loaded in &self.loaded {
            resolver.insert_package(&loaded.plugin, loaded.package_dir.clone(), &loaded.permissions);
        }
        let resolver = Arc::new(resolver);
        pipeline.set_plugin_icons(Arc::clone(&resolver));
        self.icon_resolver = Some(resolver);
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
            let worker = match self
                .cancellation
                .spawn_worker(&interpreter, worker_options.clone())
            {
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
        match self.cancellation.spawn_worker(&interpreter, options) {
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
        if self.pool.retired.contains_key(&key)
            || self
                .cancellation
                .startup_workers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(plugin)
        {
            return Err("modern worker teardown is still owned".into());
        }
        if self.pool.failure_reason(&key).is_none() {
            return Err(format!("modern plugin `{}` has no failed worker", plugin.0));
        }
        // Retire suggestion/origin bindings before permitting a replacement
        // process to start, even when it later reuses the same stable item IDs.
        self.action_items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|(owner, _), _| owner != plugin);
        self.catalog
            .root
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retire_owner_suggestions(plugin);
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
            let was_live = worker.is_alive();
            let outcome = catch_unwind(AssertUnwindSafe(|| worker.send_configuration(&values, true)));
            if let Ok(Err(error)) = &outcome {
                self.cancellation.observe_transport_error(was_live, worker, error);
            }
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    let reason = format!("configuration delivery failed: {error}");
                    self.pool.record_dispatch_failure(plugin.clone(), reason.clone());
                    failures.push((plugin, reason));
                }
                Err(_) => {
                    self.cancellation.failed.store(true, Ordering::Release);
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
        let gate = self.cancellation.admission.clone();
        let _admission = gate.admit().map_err(|_| CatalogDispatchError::BudgetRefused {
            plugin: plugin.clone(),
        })?;
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
        {
            let snapshot = self.command_snapshot();
            if snapshot.revision == u64::MAX
                || (!snapshot.owners.contains_key(plugin) && snapshot.owners.len() >= MAX_COMMANDS)
                || snapshot.owners.get(plugin).is_some_and(|source| {
                    instance < source.instance
                        || (instance == source.instance && generation.get() < source.generation.get())
                })
            {
                return Err(CatalogDispatchError::BudgetRefused {
                    plugin: plugin.clone(),
                });
            }
        }
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
        let request_id = self
            .catalog
            .next_id()
            .ok_or_else(|| CatalogDispatchError::QueueFull {
                plugin: plugin.clone(),
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
        let sender = self.catalog.result_tx.clone();
        let plugin_for_thread = plugin.clone();
        let thread_plugin = plugin.clone();
        let cancellation = Arc::clone(&self.cancellation);
        let control = Arc::new(ModernCallControl::new());
        self.cancellation
            .catalog_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(request_id, Arc::clone(&control));
        let worker_owner = Arc::new(Mutex::new(ModernCatalogOwner {
            worker,
            watcher: None,
        }));
        let thread_owner = Arc::clone(&worker_owner);
        let join = thread::Builder::new()
            .name(format!("crikey-modern-catalog-{}", plugin.0))
            .spawn(move || {
                let mut ownership = thread_owner.lock().unwrap_or_else(|e| e.into_inner());
                let ModernCatalogOwner { worker, watcher } = &mut *ownership;
                let was_live = worker.is_some();
                let watcher_control = Arc::clone(&control);
                *watcher = match thread::Builder::new().name("crikey-modern-catalog-cancel".into())
                    .spawn(move || watcher_control.watch_cancel()) {
                        Ok(handle) => Some(handle),
                        Err(_) => { cancellation.failed.store(true, Ordering::Release); None }
                    };
                let result = catch_unwind(AssertUnwindSafe(|| {
                    if worker.is_none() {
                        *worker = Some(cancellation.spawn_worker(&interpreter, worker_options)
                            .map_err(|error| error.to_string())?);
                    }
                    let active = worker.as_mut().expect("worker installed");
                    active.cancel_handle().reset();
                    control.install(active.cancel_handle());
                    let was_live = active.is_alive();
                    active.build_catalog_with_timeout_ms(CALL_BUDGET_MS).inspect_err(|error| {
                        cancellation.observe_transport_error(was_live, active, error);
                    }).map_err(|error| error.to_string())
                }));
                let mut panicked = result.is_err();
                cancellation.failed.fetch_or(panicked, Ordering::AcqRel);
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
                let mut commands = ModernOwnerCommands {
                    request_id,
                    instance,
                    generation,
                    incarnation: worker.as_ref().map_or(0, ModernWorker::incarnation),
                    status: ModernCommandStatus::Failed("catalog source did not complete".into()),
                    declarations: Vec::new(),
                };
                if let CatalogBuildResult::Complete(build) = &completion {
                    let callback = catch_unwind(AssertUnwindSafe(|| {
                        let worker = worker.as_mut().expect("catalog producer");
                        if !worker.supports_commands() {
                            return Ok(PluginCommands::Unsupported);
                        }
                        let _action_guard = budget.try_acquire_owned(BudgetKind::Action)
                            .ok_or_else(|| "command action budget refused".to_owned())?;
                        worker.cancel_handle().reset();
                        let was_live = worker.is_alive();
                        let reply = worker.commands().inspect_err(|error| {
                            cancellation.observe_transport_error(was_live, worker, error);
                        }).map_err(|error| error.to_string())?;
                        if worker.incarnation() != commands.incarnation || !worker.is_alive() {
                            return Err("command catalog source is no longer current".into());
                        }
                        Ok(reply)
                    }));
                    let (status, declarations) = match callback {
                        Ok(Ok(reply)) => bind_modern_commands(&build.plugin, &build.items, reply),
                        Ok(Err(reason)) => (ModernCommandStatus::Failed(reason), Vec::new()),
                        Err(_) => {
                            panicked = true;
                            (ModernCommandStatus::Failed("modern commands callback panicked".into()), Vec::new())
                        }
                    };
                    commands.status = status;
                    commands.declarations = declarations;
                }
                let keep_origin = worker.as_ref().is_some_and(ModernWorker::supports_preview)
                    && matches!(&completion, CatalogBuildResult::Complete(build)
                        if build.items.iter().any(|item| item.metadata.get("crikey.preview").map(String::as_str) == Some("deferred")));
                let presentation = worker.as_ref().map(|worker| worker.presentation().clone())
                    .unwrap_or_default();
                control.finish();
                if let Some(watcher) = watcher.take() {
                    cancellation.failed.fetch_or(watcher.join().is_err(), Ordering::AcqRel);
                }
                cancellation.catalog_calls.lock().unwrap_or_else(|e| e.into_inner()).remove(&request_id);
                let returned = if !panicked && worker.as_ref().is_some_and(ModernWorker::is_alive) {
                    if !was_live && !keep_origin {
                        let mut producer = worker.take().expect("healthy producer");
                        if producer.shutdown_checked().is_ok() {
                            CatalogWorkerReturn::Released
                        } else {
                            CatalogWorkerReturn::Unsettled(producer)
                        }
                    } else {
                        CatalogWorkerReturn::Retained(worker.take().expect("healthy producer"))
                    }
                } else {
                    let reason = match &completion {
                        CatalogBuildResult::Failed { reason, .. } => reason.clone(),
                        _ => "modern catalog worker is no longer alive".to_owned(),
                    };
                    cancellation.failed.fetch_or(panicked, Ordering::AcqRel);
                    if let Some(mut producer) = worker.take() {
                        if producer.shutdown_checked().is_err() {
                            cancellation.failed.store(true, Ordering::Release);
                            CatalogWorkerReturn::Unsettled(producer)
                        } else {
                            CatalogWorkerReturn::Failed(reason)
                        }
                    } else {
                        CatalogWorkerReturn::Failed(reason)
                    }
                };
                if let Err(error) = sender.send((request_id, completion, returned, commands, presentation)) {
                    if let CatalogWorkerReturn::Retained(producer) | CatalogWorkerReturn::Unsettled(producer) = error.0.2 {
                        *worker = Some(producer);
                    }
                }
                drop(guard);
            })
            .map_err(|error| {
                self.cancellation.catalog_calls.lock().unwrap_or_else(|e| e.into_inner()).remove(&request_id);
                if let Some(worker) = worker_owner.lock().unwrap_or_else(|e| e.into_inner()).worker.take() {
                    self.pool.workers.insert(key.clone(), WorkerLifecycle::Live(worker));
                }
                self.pool.fail(&key, format!("catalog thread spawn failed: {error}"));
                CatalogDispatchError::ThreadSpawn { plugin: plugin.clone(), reason: error.to_string() }
            })?;
        self.catalog
            .insert(request_id, plugin.clone(), key, join, worker_owner);
        publish_modern_commands(
            &self.catalog.commands,
            plugin.clone(),
            ModernOwnerCommands {
                request_id,
                instance,
                generation,
                incarnation: 0,
                status: ModernCommandStatus::Pending,
                declarations: Vec::new(),
            },
            true,
        );
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

    /// Immutable, bounded installed declarations. Publication follows catalog
    /// drain, not child liveness. Match owner + request/instance/generation to
    /// the accepted catalog before calling `CommandRegistry::replace_owner`;
    /// use request_id as its monotonically increasing owner incarnation.
    pub fn command_snapshot(&self) -> Arc<ModernCommandSnapshot> {
        Arc::clone(
            &self
                .catalog
                .commands
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
    }

    pub fn command_changes_since(&self, revision: u64) -> Option<Arc<ModernCommandSnapshot>> {
        let snapshot = self.command_snapshot();
        (snapshot.revision != revision).then_some(snapshot)
    }

    /// Revoke an exact installed or in-flight source after catalog/package
    /// retirement. Late completion cannot revive it. Old source revocation
    /// cannot remove a newer catalog. Preview release/restart does not call this.
    pub fn invalidate_command_source(&self, owner: &PluginId, request_id: u64) -> bool {
        let invalidated = invalidate_modern_commands(
            &self.catalog.commands,
            &self.catalog.accepted_commands,
            owner,
            request_id,
        );
        if invalidated {
            let mut root = self
                .catalog
                .root
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if root.snapshots().iter().any(|snapshot| {
                snapshot.owner == *owner
                    && matches!(&snapshot.source, RootSource::Catalog(source) if source.publication == request_id)
            }) {
                root.invalidate(owner);
            }
        }
        invalidated
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

    fn execute_view_request(
        &mut self,
        queued: &mut crate::standard_view::QueuedView,
    ) -> crikey_core::Result<crate::ActionEffect> {
        let _guard = queued
            .admission
            .take()
            .ok_or(crikey_core::CoreError::Cancelled)?
            .acquire(queued.deadline, &queued.cancelled)?;
        if !queued.current() {
            return Err(crikey_core::CoreError::Cancelled);
        }
        let key = self
            .loaded
            .iter()
            .find(|loaded| loaded.plugin == queued.owner)
            .map(|loaded| loaded.key.clone())
            .ok_or_else(|| crikey_core::CoreError::Invalid("view owner unavailable".into()))?;
        // Never reacquire a different physical producer for a route.
        let worker = self
            .pool
            .live_mut(&key)
            .ok_or_else(|| crikey_core::CoreError::Invalid("view producer unavailable".into()))?;
        if worker.incarnation() != queued.producer || !worker.is_alive() {
            return Err(crikey_core::CoreError::Invalid(
                "view producer unavailable".into(),
            ));
        }
        let values = match &mut queued.operation {
            crate::standard_view::ViewOperation::Execute { values, .. } => values.take(),
            _ => None,
        };
        let cancel = worker.cancel_handle();
        cancel.reset();
        match &queued.operation {
            crate::standard_view::ViewOperation::Request(request) => {
                let reply = crate::standard_view::with_cancellation(
                    queued,
                    || cancel.cancel(),
                    || {
                        worker
                            .structured_view_with_admission(request, |send| queued.deliver(send))
                            .inspect_err(|error| {
                                self.cancellation.observe_transport_error(true, worker, error)
                            })
                    },
                )
                .map_err(|_| crikey_core::CoreError::Invalid("standard view request failed".into()))?;
                // Publication retires pending-request identity only after the
                // physical callback watcher has stopped and joined.
                queued
                    .authority
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .publish(queued, request, reply)
            }
            crate::standard_view::ViewOperation::Execute {
                context,
                executable,
                item,
                schema,
                argument,
                ..
            } => crate::standard_view::with_cancellation(
                queued,
                || cancel.cancel(),
                || {
                    if !queued.current() {
                        return Err(crikey_core::CoreError::Cancelled);
                    }
                    let action = executable.action.as_ref().map(|id| id.0.as_str());
                    if let Some(schema) = schema {
                        let outcome = worker
                            .typed_execute_with_admission(
                                item,
                                action,
                                schema,
                                values.ok_or(crikey_core::CoreError::Cancelled)?,
                                Some(context),
                                |send| queued.deliver(send),
                            )
                            .inspect_err(|error| {
                                self.cancellation.observe_transport_error(true, worker, error)
                            })
                            .map_err(|_| {
                                crikey_core::CoreError::Invalid("typed view execution failed".into())
                            })?;
                        Ok(crate::standard_view::typed_effect(outcome, queued.producer))
                    } else {
                        match worker
                            .execute_with_admission(
                                item,
                                action,
                                argument.as_deref(),
                                Some(context),
                                |send| queued.deliver(send),
                            )
                            .inspect_err(|error| {
                                self.cancellation.observe_transport_error(true, worker, error)
                            })
                            .map_err(|_| crikey_core::CoreError::Invalid("view execution failed".into()))?
                        {
                            ExecuteOutcome::Ok => Ok(crate::ActionEffect::Completed),
                            ExecuteOutcome::Navigate(outcome) => Ok(crate::ActionEffect::Navigate {
                                outcome,
                                producer: queued.producer,
                            }),
                            ExecuteOutcome::Failed(_) => {
                                Err(crikey_core::CoreError::Invalid("view execution refused".into()))
                            }
                        }
                    }
                },
            ),
        }
    }

    /// Validates and executes one admitted plugin action on its owning worker.
    ///
    /// The request owns the action guard. Keeping it in this function until
    /// the worker outcome is converted to `CoreResult` covers failures,
    /// cancellation, timeout and panic/unwind paths.
    fn execute_action_request(
        &mut self,
        mut request: ModernActionRequest,
    ) -> crikey_core::Result<crate::ActionEffect> {
        if let Some(binding) = request.work.typed_binding() {
            if !self.cancellation.source_current(&binding.source)
                || self
                    .catalog
                    .root
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .frozen_item(binding)
                    .is_none()
            {
                return Err(crikey_core::CoreError::Invalid(
                    "typed modern source is no longer current".into(),
                ));
            }
        }
        let _guard = request.admission.acquire(request.deadline, &request.cancelled)?;
        if request.cancelled.load(Ordering::Acquire) {
            return Err(crikey_core::CoreError::Cancelled);
        }
        if Instant::now() >= request.deadline {
            return Err(crikey_core::CoreError::Invalid(
                "modern action timed out before execution".into(),
            ));
        }
        let plugin = request.plugin;
        // Consume the item whose admission produced the history receipt.
        // Keep current owner/liveness checks, never substitute newer content.
        let item = &request.item;
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
        if let Some((declaration, source)) = request.work.command_source() {
            let publications = self
                .catalog
                .accepted_commands
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let current = publications
                .get(&plugin)
                .filter(|current| {
                    current.status == ModernCommandStatus::Ready
                        && current.request_id == source.publication
                        && current.instance == source.instance
                        && current.generation == source.generation
                        && current.incarnation == source.producer
                        && current.declarations.contains(declaration)
                })
                .ok_or_else(|| {
                    crikey_core::CoreError::Invalid(
                        "declared modern command source is no longer current".into(),
                    )
                })?;
            let key = (plugin.clone(), item.stable_id.clone());
            let canonical = self
                .catalog
                .items
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&key)
                .cloned();
            let current_item = if let Some((canonical, origin)) = canonical {
                if origin != current.incarnation || request.origin != Some(origin) {
                    return Err(crikey_core::CoreError::Invalid(
                        "declared modern command producer was replaced".into(),
                    ));
                }
                canonical
            } else {
                self.catalog
                    .static_items
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| {
                        crikey_core::CoreError::Invalid("canonical command item is unavailable".into())
                    })?
            };
            request
                .work
                .validate(&current_item, request.argument.as_deref())?;
        } else if request.origin.is_some()
            && !matches!(
                &request.work,
                crate::plugin_action::AdmittedPluginWork::Actionless { .. }
            )
        {
            // Static ordinary actions carry their accepted canonical item.
            // Healthy lazy release must not replace its frozen policy with a
            // later catalog row. Dynamic actions still require a live origin.
            let key = (plugin.clone(), item.stable_id.clone());
            let current = self
                .action_items
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&key)
                .cloned()
                .or_else(|| {
                    self.catalog
                        .items
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .get(&key)
                        .cloned()
                });
            if !current.as_ref().is_some_and(|(current, origin)| {
                current.plugin_id == plugin
                    && current.stable_id == item.stable_id
                    && current.target == item.target
                    && current.category == item.category
                    && current.argument_policy == item.argument_policy
                    && request.origin.is_none_or(|expected| expected == *origin)
            }) {
                return Err(crikey_core::CoreError::Invalid(
                    "selected modern action item changed after admission".into(),
                ));
            }
            if let Some((current, _)) = current {
                request.work.validate(&current, request.argument.as_deref())?;
            }
        }
        request.work.validate(item, request.argument.as_deref())?;

        let key = loaded.key.clone();
        if request.origin.is_none() {
            self.ensure_worker(&plugin, &key)
                .map_err(crikey_core::CoreError::Invalid)?;
        }
        let worker = self.pool.live_mut(&key).ok_or_else(|| {
            crikey_core::CoreError::Invalid(format!("modern worker for `{}` is unavailable", plugin.0))
        })?;
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
        if request
            .origin
            .is_some_and(|origin| origin != worker.incarnation())
        {
            return Err(crikey_core::CoreError::Invalid(
                "modern action originating worker was replaced".into(),
            ));
        }
        if request.cancelled.load(Ordering::Acquire) {
            return Err(crikey_core::CoreError::Cancelled);
        }
        if Instant::now() >= request.deadline {
            return Err(crikey_core::CoreError::Invalid(
                "modern action timed out before callback".into(),
            ));
        }
        // Lazy static startup may block after the handoff validation above.
        // A source retired during startup must not reach the callback.
        if let Some((declaration, source)) = request.work.command_source() {
            let publications = self
                .catalog
                .accepted_commands
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !publications.get(&plugin).is_some_and(|current| {
                current.status == ModernCommandStatus::Ready
                    && current.request_id == source.publication
                    && current.instance == source.instance
                    && current.generation == source.generation
                    && current.incarnation == source.producer
                    && current.declarations.contains(declaration)
            }) {
                return Err(crikey_core::CoreError::Invalid(
                    "declared modern command source is no longer current".into(),
                ));
            }
        }

        let physical_producer = worker.incarnation();
        if let Some(binding) = request.work.typed_binding().cloned() {
            let action = binding.executable.action.as_ref().map(|id| id.0.as_str());
            if !worker
                .confirm_typed_schema(item, action, &binding.schema)
                .inspect_err(|error| self.cancellation.observe_transport_error(true, worker, error))
                .map_err(|_| {
                    crikey_core::CoreError::Invalid("typed modern schema confirmation failed".into())
                })?
            {
                return Err(crikey_core::CoreError::Invalid(
                    "typed modern schema changed".into(),
                ));
            }
            if !self.cancellation.source_current(&binding.source)
                || self
                    .catalog
                    .root
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .frozen_item(&binding)
                    .is_none()
            {
                return Err(crikey_core::CoreError::Invalid(
                    "typed modern source is no longer current".into(),
                ));
            }
            if request.cancelled.load(Ordering::Acquire) || Instant::now() >= request.deadline {
                return Err(crikey_core::CoreError::Cancelled);
            }
            let values = request.work.take_typed_values().ok_or_else(|| {
                crikey_core::CoreError::Invalid("typed modern values are unavailable".into())
            })?;
            let handle = worker.cancel_handle();
            handle.reset();
            let finished = AtomicBool::new(false);
            let root = &self.catalog.root;
            let cancellation = &self.cancellation;
            let outcome = thread::scope(|scope| {
                let watcher = scope.spawn(|| {
                    while !finished.load(Ordering::Acquire) {
                        if request.cancelled.load(Ordering::Acquire) || Instant::now() >= request.deadline {
                            handle.cancel();
                            break;
                        }
                        thread::park_timeout(Duration::from_millis(5));
                    }
                });
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    worker
                        .typed_execute_with_admission(item, action, &binding.schema, values, None, |send| {
                            let root = root.lock().unwrap_or_else(|error| error.into_inner());
                            cancellation.source_current(&binding.source)
                                && root.frozen_item(&binding).is_some()
                                && !request.cancelled.load(Ordering::Acquire)
                                && Instant::now() < request.deadline
                                && send()
                        })
                        .inspect_err(|error| cancellation.observe_transport_error(true, worker, error))
                        .map_err(|_| crikey_core::CoreError::Invalid("typed modern execution failed".into()))
                }));
                finished.store(true, Ordering::Release);
                watcher.thread().unpark();
                cancellation
                    .failed
                    .fetch_or(watcher.join().is_err(), Ordering::AcqRel);
                outcome
            });
            if outcome.is_err() {
                self.cancellation.failed.store(true, Ordering::Release);
            }
            return match outcome {
                Ok(Ok(outcome)) => Ok(crate::standard_view::typed_effect(outcome, physical_producer)),
                Ok(Err(error)) => Err(error),
                _ => Err(crikey_core::CoreError::Invalid(
                    "typed modern execution failed".into(),
                )),
            };
        }
        // The previous speculative callback may have cooperatively cancelled
        // on this same physical worker. Its terminal has released the lease;
        // clear that call's Event before dispatching this admitted Root call.
        // The current request flag/deadline are still checked by the writer gate.
        worker.cancel_handle().reset();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            worker
                .execute_with_admission(
                    item,
                    request.work.action_id().map(|id| id.0.as_str()),
                    request.argument.as_deref(),
                    None,
                    |send| {
                        if request.cancelled.load(Ordering::Acquire) || Instant::now() >= request.deadline {
                            return false;
                        }
                        if let crate::plugin_action::AdmittedPluginWork::Actionless { source } = &request.work
                        {
                            let root = self.catalog.root.lock().unwrap_or_else(|e| e.into_inner());
                            self.cancellation.source_current(source)
                                && root.actionless_item(&plugin, &item.stable_id, source).is_some()
                                && send()
                        } else {
                            send()
                        }
                    },
                )
                .inspect_err(|error| self.cancellation.observe_transport_error(true, worker, error))
        }));
        match outcome {
            Ok(Ok(ExecuteOutcome::Ok)) => Ok(crate::ActionEffect::Completed),
            Ok(Ok(ExecuteOutcome::Navigate(outcome))) => Ok(crate::ActionEffect::Navigate {
                outcome,
                producer: physical_producer,
            }),
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
                self.cancellation.failed.store(true, Ordering::Release);
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
        self.drive_query_scoped(pipeline, query, now, None)
    }

    /// Admit an exact declared scope; an unavailable source never broadens to All.
    pub fn drive_query_scoped(
        &mut self,
        pipeline: &mut QueryPipeline,
        query: &str,
        now: Millis,
        scope: Option<RootScope>,
    ) -> Option<ViewModel> {
        let owned_scope = scope
            .as_ref()
            .filter(|scope| self.loaded.iter().any(|loaded| loaded.plugin == scope.owner));
        if let Some(scope) = owned_scope {
            if let RootSource::Suggestion { producer, .. } = &scope.source {
                let key = &self
                    .loaded
                    .iter()
                    .find(|loaded| loaded.plugin == scope.owner)?
                    .key;
                if !self
                    .pool
                    .live_mut(key)
                    .is_some_and(|worker| worker.incarnation() == *producer && worker.is_alive())
                {
                    return None;
                }
            }
        }
        if (scope.is_none() || owned_scope.is_some())
            && !self
                .catalog
                .root
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .capture_scope(owned_scope)
        {
            return None;
        }
        self.drive_query_admitted(pipeline, query, now, scope)
    }

    fn drive_query_admitted(
        &mut self,
        pipeline: &mut QueryPipeline,
        query: &str,
        now: Millis,
        scope: Option<RootScope>,
    ) -> Option<ViewModel> {
        self.catalog
            .root
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retire_suggestions();
        let generation = pipeline.keystroke_scoped(query, now, scope.clone());
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
                let _ = pipeline.cancel_request(&cancellation.plugin, cancellation.generation, at);
            }

            let mut requests = Vec::new();
            for request in tick.dispatches {
                if request.generation != generation {
                    let _ = pipeline.cancel_request(&request.plugin, request.generation, at);
                    continue;
                }
                if dead_plugins.contains(&request.plugin) {
                    let _ = pipeline.fail_request(&request.plugin, request.generation, at);
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
            let (mut suggestions, newly_dead) =
                self.collect_suggestions(query, generation, &requested, scope.as_ref());
            for request in requests {
                if newly_dead.contains(&request.plugin) {
                    let _ = pipeline.fail_request(&request.plugin, request.generation, at);
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
        let _ = self.shutdown_checked();
    }

    fn shutdown_checked(&mut self) -> Result<(), StopError> {
        self.cancellation.cancel_all();
        for result in self.catalog.shutdown(&mut self.pool) {
            if let CatalogBuildResult::Failed { plugin, reason, .. } = result {
                self.pool.record_dispatch_failure(plugin, reason);
            }
        }
        let mut failed = self.catalog.failed
            || self.pool.teardown_failed
            || self.cancellation.failed.load(Ordering::Acquire);
        for state in self.pool.workers.values_mut() {
            match state {
                WorkerLifecycle::Live(worker) => failed |= worker.shutdown_checked().is_err(),
                WorkerLifecycle::CatalogBusy => failed = true,
                WorkerLifecycle::NeverStarted | WorkerLifecycle::Failed { .. } => {}
            }
        }
        for worker in self.pool.retired.values_mut() {
            failed |= worker.shutdown_checked().is_err();
        }
        for worker in self
            .cancellation
            .startup_workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            failed |= worker
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .shutdown_checked()
                .is_err();
        }
        failed |= self
            .icon_resolver
            .as_ref()
            .is_some_and(|resolver| resolver.shutdown().is_err());
        if failed {
            Err(StopError::new("modern-shutdown-failed"))
        } else {
            Ok(())
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
        scope: Option<&RootScope>,
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

            if let Some(scope) = scope {
                let current = scope_declared(&self.catalog.root, scope)
                    && match &scope.source {
                        RootSource::Catalog(_) => true,
                        RootSource::Suggestion { producer, .. } => self
                            .pool
                            .live_mut(&key)
                            .is_some_and(|worker| worker.incarnation() == *producer),
                    };
                if plugin != scope.owner
                    || !current
                    || !self
                        .pool
                        .live_mut(&key)
                        .is_some_and(|worker| worker.supports_result_presentation())
                {
                    dead_plugins.insert(plugin.clone());
                    continue;
                }
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
                let answer = catch_unwind(AssertUnwindSafe(|| {
                    let was_live = worker.is_alive();
                    worker
                        .suggest_scoped(&request, scope.map(|scope| scope.id.0.as_str()))
                        .inspect_err(|error| {
                            self.cancellation.observe_transport_error(was_live, worker, error)
                        })
                }));
                let elapsed = started.elapsed();
                control.finish();
                if let Ok(watcher) = watcher {
                    self.cancellation
                        .failed
                        .fetch_or(watcher.join().is_err(), Ordering::AcqRel);
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
                    self.cancellation.failed.store(true, Ordering::Release);
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
                    }
                    if self.superseded() {
                        continue;
                    }
                    if scope.is_some_and(|scope| {
                        !scope_declared(&self.catalog.root, scope)
                            || matches!(&scope.source, RootSource::Suggestion { producer, .. } if *producer != incarnation)
                    }) {
                        dead_plugins.insert(plugin.clone());
                        continue;
                    }
                    #[cfg(test)]
                    if let Some((entered, resume)) = self
                        .cancellation
                        .publication_pause
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .take()
                    {
                        entered.send(()).unwrap();
                        resume.recv().unwrap();
                    }
                    // The intake check and publication share retirement's ledger
                    // lock: a completion cannot republish after a newer intake
                    // has retired its generation.
                    let mut root = self
                        .catalog
                        .root
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if self.superseded() {
                        continue;
                    }
                    if scope.is_some_and(|scope| !root.scope_current(scope)) {
                        dead_plugins.insert(plugin.clone());
                        continue;
                    }
                    let publication = root.publish(
                        plugin.clone(),
                        RootSource::Suggestion {
                            generation,
                            producer: incarnation,
                        },
                        &items,
                        suggestions.presentation,
                    );
                    drop(root);
                    if publication.is_err() {
                        dead_plugins.insert(plugin.clone());
                        self.pool
                            .record_dispatch_failure(plugin, "invalid modern suggestion presentation".into());
                        continue;
                    }
                    let mut action_items = self
                        .action_items
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    for item in &items {
                        action_items.insert(
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
fn scope_declared(root: &Mutex<RootSourceLedger>, scope: &RootScope) -> bool {
    root.lock()
        .unwrap_or_else(|error| error.into_inner())
        .scope_current(scope)
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
    scope: Option<RootScope>,
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
    origin: Option<u64>,
    work: crate::plugin_action::AdmittedPluginWork,
    argument: Option<String>,
    admission: ForegroundActionAdmission,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

enum ModernWork {
    Query(ModernJob),
    Action(Box<ModernActionRequest>),
    View(crate::standard_view::QueuedView),
    Preview(ModernPreviewJob),
    Refresh,
    /// Publish this complete configuration state to every loaded plugin.
    Configuration(Box<crate::PluginConfiguration>),
}

/// Bounded action endpoint retained by the live modern driver.
#[derive(Debug)]
struct ModernActionEndpoint {
    sender: SyncSender<crate::standard_view::ForegroundRequest<ModernActionRequest>>,
    completions: Arc<Mutex<VecDeque<PluginActionCompletion>>>,
    action_wake: crate::plugin_action::CompletionWake,
    gates: BTreeMap<PluginId, Arc<PreviewActionGate>>,
    preview_pending: PendingPreviews<AtomicBool>,
    items: SharedOriginItems,
    catalog_items: SharedOriginItems,
    static_catalog_items: Arc<Mutex<BTreeMap<(PluginId, ItemId), Item>>>,
    accepted_commands: ModernAcceptedCommands,
    root: Arc<Mutex<RootSourceLedger>>,
    views: Arc<Mutex<crate::standard_view::ViewAuthority>>,
    cancellation: Arc<ModernCancellation>,
    incarnations: Arc<Mutex<BTreeMap<PluginId, u64>>>,
    pending: Arc<Mutex<BTreeMap<ActionRequestId, Arc<AtomicBool>>>>,
    in_flight: Arc<AtomicUsize>,
    next_id: Arc<AtomicU64>,
    mailbox: Arc<(Mutex<ModernRequestSlot>, Condvar)>,
}

impl ModernActionEndpoint {
    fn view_producer(&self, owner: &PluginId) -> Option<u64> {
        if self.cancellation.admission.state() != crate::runtime_control::PluginAdmission::Open {
            return None;
        }
        self.incarnations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(owner)
            .copied()
    }
    fn enqueue_view(
        &self,
        owner: &PluginId,
        producer: u64,
        operation: crate::standard_view::ViewOperation,
    ) -> crikey_core::Result<ActionRequestId> {
        for (_, cancelled) in self
            .preview_pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            cancelled.store(true, Ordering::Release);
        }
        let request_id = crate::standard_view::enqueue(
            &self.sender,
            &self.cancellation.admission,
            &self.gates,
            &self.pending,
            &self.in_flight,
            &self.next_id,
            &self.views,
            owner,
            producer,
            operation,
            ACTION_IN_FLIGHT_CAPACITY,
            ACTION_TIMEOUT_MS,
        )?;
        self.mailbox.1.notify_one();
        Ok(request_id)
    }
    fn enqueue(
        &self,
        plugin: &PluginId,
        item: Item,
        origin: Option<u64>,
        work: crate::plugin_action::AdmittedPluginWork,
        argument: Option<&str>,
        query_history_allowed: bool,
    ) -> crikey_core::Result<crate::plugin_action::PluginActionReceipt> {
        let executable = ExecutableKey {
            target: OwnedItem {
                owner: plugin.clone(),
                item: item.stable_id.clone(),
            },
            action: work.action_id().cloned(),
        };
        if work.typed_binding().is_none()
            && !matches!(&work, crate::plugin_action::AdmittedPluginWork::Actionless { .. })
            && self
                .root
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .binding(&executable, work.command_source().map(|(_, source)| source))
                .is_some()
        {
            return Err(crikey_core::CoreError::Invalid(
                "typed arguments are required".into(),
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
        let gate = match self.gates.get(plugin) {
            Some(gate) => gate,
            None => {
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                return Err(crikey_core::CoreError::Invalid(format!(
                    "no modern action runtime owns plugin `{}`",
                    plugin.0
                )));
            }
        };
        let admission = match gate.reserve_foreground() {
            Ok(admission) => admission,
            Err(error) => {
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                return Err(error);
            }
        };
        // Every owner shares this supervisor; even a different owner's preview
        // must yield before this admitted foreground callback can run.
        for (_, cancelled) in self
            .preview_pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
        {
            cancelled.store(true, Ordering::Release);
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
            work,
            argument: argument.map(str::to_owned),
            admission,
            deadline: Instant::now() + Duration::from_millis(ACTION_TIMEOUT_MS),
            cancelled,
        };
        let sent = match self.cancellation.admission.admit() {
            Ok(_admission) => self
                .sender
                .try_send(crate::standard_view::ForegroundRequest::Action(request)),
            Err(_) => Err(mpsc::TrySendError::Disconnected(
                crate::standard_view::ForegroundRequest::Action(request),
            )),
        };
        if let Err(error) = sent {
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
        Ok(crate::plugin_action::PluginActionReceipt {
            request_id,
            query_history_allowed,
        })
    }
}

impl crate::PluginActionExecutor for ModernActionEndpoint {
    fn root_actionless_source(
        &self,
        owner: &PluginId,
        item: &ItemId,
        canonical: Option<&crate::plugin_action::DeclaredCommandSource>,
    ) -> Option<RootSource> {
        let source = self
            .root
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .actionless_source(owner, item, canonical)?;
        (self.cancellation.source_current(&source)
            && match &source {
                RootSource::Catalog(_) => true,
                RootSource::Suggestion { producer, .. } => {
                    self.incarnations
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(owner)
                        == Some(producer)
                }
            })
        .then_some(source)
    }
    fn submit_actionless(
        &self,
        owner: &PluginId,
        item: &ItemId,
        source: &RootSource,
    ) -> crikey_core::Result<ActionRequestId> {
        let canonical = match source {
            RootSource::Catalog(source) => Some(source),
            _ => None,
        };
        if self.root_actionless_source(owner, item, canonical).as_ref() != Some(source) {
            return Err(crikey_core::CoreError::Invalid(
                "actionless root source retired".into(),
            ));
        }
        let item = self
            .root
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .actionless_item(owner, item, source)
            .cloned()
            .ok_or_else(|| crikey_core::CoreError::Invalid("actionless root source retired".into()))?;
        let origin = match source {
            RootSource::Catalog(_) => None,
            RootSource::Suggestion { producer, .. } => Some(*producer),
        };
        self.enqueue(
            owner,
            item,
            origin,
            crate::plugin_action::AdmittedPluginWork::Actionless {
                source: source.clone(),
            },
            None,
            false,
        )
        .map(|receipt| receipt.request_id)
    }
    fn request_view(
        &self,
        owner: &PluginId,
        producer: u64,
        request: crikey_core::structured_view::ViewRequest,
    ) -> crikey_core::Result<ActionRequestId> {
        if self.view_producer(owner) != Some(producer) {
            return Err(crikey_core::CoreError::Invalid(
                "view producer unavailable".into(),
            ));
        }
        self.enqueue_view(
            owner,
            producer,
            crate::standard_view::ViewOperation::Request(request),
        )
    }
    fn submit_view_action(
        &self,
        owner: &PluginId,
        context: &crikey_core::structured_view::ViewExecutionContext,
        state: &crikey_core::structured_view::HostViewState,
        executable: &crikey_core::structured_arguments::ExecutableKey,
        argument: Option<&str>,
    ) -> crikey_core::Result<ActionRequestId> {
        let producer = self
            .view_producer(owner)
            .ok_or_else(|| crikey_core::CoreError::Invalid("view producer unavailable".into()))?;
        let operation = crate::standard_view::execution(
            &self.views,
            owner,
            producer,
            crate::standard_view::ViewEventContext { context, state },
            executable,
            None,
            argument,
        )?;
        self.enqueue_view(owner, producer, operation)
    }
    fn submit_view_typed(
        &self,
        owner: &PluginId,
        context: &crikey_core::structured_view::ViewExecutionContext,
        state: &crikey_core::structured_view::HostViewState,
        executable: &crikey_core::structured_arguments::ExecutableKey,
        values: crikey_core::structured_arguments::ArgumentValues,
    ) -> crikey_core::Result<ActionRequestId> {
        let producer = self
            .view_producer(owner)
            .ok_or_else(|| crikey_core::CoreError::Invalid("view producer unavailable".into()))?;
        let operation = crate::standard_view::execution(
            &self.views,
            owner,
            producer,
            crate::standard_view::ViewEventContext { context, state },
            executable,
            Some(values),
            None,
        )?;
        self.enqueue_view(owner, producer, operation)
    }
    fn retire_view(&self, owner: &PluginId, session: crikey_core::structured_view::ViewSessionIdentity) {
        self.views
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retire(owner, session);
    }
    fn view_current(
        &self,
        owner: &PluginId,
        context: &crikey_core::structured_view::ViewExecutionContext,
    ) -> bool {
        let producer = self.view_producer(owner);
        let mut authority = self.views.lock().unwrap_or_else(|e| e.into_inner());
        if producer.is_none() {
            authority.retire(owner, context.session);
        }
        producer.is_some_and(|producer| authority.current(owner, context, producer))
    }
    fn validate_view_link(
        &self,
        owner: &PluginId,
        context: &crikey_core::structured_view::ViewExecutionContext,
        state: &crikey_core::structured_view::HostViewState,
        url: &str,
    ) -> crikey_core::Result<()> {
        if self.cancellation.admission.admit().is_err() {
            return Err(crikey_core::CoreError::Cancelled);
        }
        let producer = self
            .view_producer(owner)
            .ok_or_else(|| crikey_core::CoreError::Invalid("view producer unavailable".into()))?;
        self.views
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .validate_link(owner, context, state, producer, url)
    }
    fn root_binding(
        &self,
        key: &ExecutableKey,
        canonical: Option<&crate::plugin_action::DeclaredCommandSource>,
    ) -> Option<RootExecutableBinding> {
        let root = self.root.lock().unwrap_or_else(|error| error.into_inner());
        root.binding(key, canonical)
            .filter(|binding| self.cancellation.source_current(&binding.source))
    }

    fn root_presentations(&self) -> Vec<RootPresentationSnapshot> {
        self.root
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshots()
            .into_iter()
            .filter(|snapshot| self.cancellation.source_current(&snapshot.source))
            .collect()
    }

    fn root_scope_current(&self, scope: &RootScope) -> bool {
        if self.cancellation.admission.state() != crate::runtime_control::PluginAdmission::Open {
            return false;
        }
        if !self.gates.contains_key(&scope.owner) || !scope_declared(&self.root, scope) {
            return false;
        }
        match &scope.source {
            RootSource::Catalog(_) => true,
            RootSource::Suggestion { producer, .. } => {
                self.incarnations
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(&scope.owner)
                    == Some(producer)
            }
        }
    }

    fn submit_typed(
        &self,
        binding: &RootExecutableBinding,
        values: ArgumentValues,
    ) -> crikey_core::Result<crate::plugin_action::TypedActionReceipt> {
        binding
            .schema
            .validate_values(&values)
            .map_err(|_| crikey_core::CoreError::Invalid("invalid typed modern arguments".into()))?;
        if !self.cancellation.source_current(&binding.source) {
            return Err(crikey_core::CoreError::Invalid(
                "typed modern source is no longer current".into(),
            ));
        }
        let item = {
            let root = self.root.lock().unwrap_or_else(|error| error.into_inner());
            if !self.cancellation.source_current(&binding.source) {
                return Err(crikey_core::CoreError::Invalid(
                    "typed modern source is no longer current".into(),
                ));
            }
            root.frozen_item(binding).ok_or_else(|| {
                crikey_core::CoreError::Invalid("typed modern source is no longer current".into())
            })?
        };
        let origin = match &binding.source {
            RootSource::Catalog(_) => None,
            RootSource::Suggestion { producer, .. } => Some(*producer),
        };
        if origin.is_some_and(|origin| {
            self.incarnations
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&binding.executable.target.owner)
                .copied()
                != Some(origin)
        }) {
            return Err(crikey_core::CoreError::Invalid(
                "typed modern producer is unavailable".into(),
            ));
        }
        let receipt = self.enqueue(
            &binding.executable.target.owner,
            item.clone(),
            origin,
            crate::plugin_action::AdmittedPluginWork::Typed {
                binding: binding.clone(),
                values,
            },
            None,
            false,
        )?;
        Ok(crate::plugin_action::TypedActionReceipt { receipt, item })
    }
    fn submit_declared_command_with_receipt(
        &self,
        declaration: &CommandDeclaration,
        source: &crate::plugin_action::DeclaredCommandSource,
        argument: Option<&str>,
    ) -> crikey_core::Result<crate::plugin_action::PluginActionReceipt> {
        let plugin = &declaration.reference.provider_id;
        let publications = self
            .accepted_commands
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let current = publications
            .get(plugin)
            .filter(|current| {
                current.status == ModernCommandStatus::Ready
                    && current.request_id == source.publication
                    && current.instance == source.instance
                    && current.generation == source.generation
                    && current.incarnation == source.producer
                    && current.declarations.contains(declaration)
            })
            .ok_or_else(|| {
                crikey_core::CoreError::Invalid("declared modern command source is no longer current".into())
            })?;
        let crikey_core::command::ExecutableTarget::Item { item_id, .. } = &declaration.executable_target
        else {
            return Err(crikey_core::CoreError::Invalid(
                "host command cannot execute in a plugin".into(),
            ));
        };
        let key = (plugin.clone(), item_id.clone());
        let canonical = self
            .catalog_items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .map(|(item, origin)| (item.clone(), Some(*origin)))
            .or_else(|| {
                self.static_catalog_items
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(&key)
                    .cloned()
                    .map(|item| (item, None))
            })
            .ok_or_else(|| crikey_core::CoreError::Invalid("canonical command item is unavailable".into()))?;
        let (item, origin) = canonical;
        let action_id = crate::plugin_action::validate_declared_command_item(declaration, &item, argument)?;
        let query_history_allowed = item.argument_policy == ArgumentPolicy::Forbidden
            && argument.is_none()
            && action_id.as_ref() == item.actions.first().map(|action| &action.action_id);
        let _ = current;
        self.enqueue(
            plugin,
            item,
            origin,
            crate::plugin_action::AdmittedPluginWork::Command {
                declaration: declaration.clone(),
                source: source.clone(),
                action_id,
            },
            argument,
            query_history_allowed,
        )
    }

    fn submit_plugin_action(
        &self,
        plugin: &PluginId,
        item: &Item,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> crikey_core::Result<ActionRequestId> {
        self.submit_plugin_action_with_receipt(plugin, item, action_id, argument)
            .map(|receipt| receipt.request_id)
    }

    fn submit_plugin_action_by_id_with_receipt(
        &self,
        plugin: &PluginId,
        item_id: &ItemId,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> crikey_core::Result<crate::plugin_action::PluginActionReceipt> {
        let live_origin = self
            .incarnations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(plugin)
            .copied();
        let (item, origin) = crate::plugin_action::resolve_action_item(
            plugin,
            item_id,
            live_origin,
            &self.items,
            &self.catalog_items,
            &self.static_catalog_items,
        )
        .ok_or_else(|| {
            crikey_core::CoreError::Invalid("selected modern plugin result is no longer current".into())
        })?;
        if item.plugin_id != *plugin {
            return Err(crikey_core::CoreError::Invalid(
                "selected modern plugin result has stale ownership".to_owned(),
            ));
        }
        let (action, query_history_allowed) =
            crate::plugin_action::validate_action_item(&item, action_id, argument)?;
        if action.execution_policy != crikey_core::ExecutionPolicy::Plugin {
            return Err(crikey_core::CoreError::Invalid(
                "selected action is not plugin-owned".into(),
            ));
        }
        self.enqueue(
            plugin,
            item,
            origin,
            crate::plugin_action::AdmittedPluginWork::Action(action_id.clone()),
            argument,
            query_history_allowed,
        )
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
        if self.cancellation.admission.state() != crate::runtime_control::PluginAdmission::Open {
            return false;
        }
        let key = (plugin.clone(), item_id.clone());
        self.items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(&key)
            || self
                .catalog_items
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains_key(&key)
            || self
                .static_catalog_items
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains_key(&key)
    }

    fn submit_plugin_action_by_id(
        &self,
        plugin: &PluginId,
        item_id: &ItemId,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> crikey_core::Result<ActionRequestId> {
        self.submit_plugin_action_by_id_with_receipt(plugin, item_id, action_id, argument)
            .map(|receipt| receipt.request_id)
    }

    fn submit_plugin_action_with_receipt(
        &self,
        plugin: &PluginId,
        item: &Item,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> crikey_core::Result<crate::plugin_action::PluginActionReceipt> {
        if item.plugin_id != *plugin {
            return Err(crikey_core::CoreError::Invalid(
                "selected modern plugin result has stale ownership".to_owned(),
            ));
        }
        self.submit_plugin_action_by_id_with_receipt(plugin, &item.stable_id, action_id, argument)
    }
}
fn enqueue_modern_completion(
    mailbox: &Mutex<VecDeque<PluginActionCompletion>>,
    action_wake: &crate::plugin_action::CompletionWake,
    completion: PluginActionCompletion,
) {
    let mut mailbox = mailbox.lock().unwrap_or_else(|error| error.into_inner());
    if mailbox.len() >= ACTION_COMPLETION_CAPACITY {
        eprintln!(
            "crikey: modern action completion mailbox overflow for {} / {}",
            completion.plugin.0,
            completion
                .action_id
                .as_ref()
                .map_or("<default>", |id| id.0.as_str())
        );
        return;
    }
    mailbox.push_back(completion);
    drop(mailbox);
    action_wake.ring();
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
    commands: ModernCommands,
    /// Per-plugin diagnostics refreshed by the supervisor thread after every
    /// unit of work, so the UI thread can report a throttled plugin without
    /// reaching into the pipeline it does not own.
    health: Arc<Mutex<Vec<(PluginId, crikey_plugin_supervisor::PluginHealth)>>>,
    outcome: Arc<Mutex<Option<ViewModel>>>,
    search_summary: Arc<Mutex<Option<crate::query_pipeline::OrdinarySearchSummary>>>,
    query_policies: BTreeMap<PluginId, crikey_input_scheduler::PluginPolicy>,
    /// Search generation the UI last submitted. The supervisor re-reads it
    /// before publishing and drops any answer that is no longer current.
    current: Arc<AtomicU64>,
    cancellation: Arc<ModernCancellation>,
    has_plugins: bool,
    control: DriverControl,
    worker_started: bool,
    #[cfg(test)]
    runtime_owner: Arc<Mutex<ModernProvider>>,
}

impl ModernDriver {
    /// Moves `provider` and its `pipeline` onto a supervisor thread and returns
    /// a handle the UI thread drives without ever blocking.
    ///
    /// `publish` runs on the supervisor thread with each merged frame. A thread
    /// that fails to spawn degrades to an inert driver rather than a panic:
    /// the canonical runtime remains owned for checked settlement, and
    /// [`has_plugins`](Self::has_plugins) reports false.
    pub fn spawn<P>(provider: ModernProvider, mut pipeline: QueryPipeline, publish: P) -> Self
    where
        P: Fn(&ViewModel) + Send + 'static,
    {
        let permissions = provider.permissions();
        let owners: Vec<PluginId> = provider
            .plugins()
            .iter()
            .cloned()
            .chain(provider.catalog_declarations.keys().cloned())
            .chain(provider.unavailable.iter().filter_map(|item| item.plugin.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let admission = provider.cancellation.admission.clone();
        let query_policies = permissions
            .keys()
            .filter_map(|plugin| {
                pipeline
                    .plugin_policy(plugin)
                    .cloned()
                    .map(|policy| (plugin.clone(), policy))
            })
            .collect();
        let commands = Arc::clone(&provider.catalog.commands);
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
        let search_summary = Arc::new(Mutex::new(None));
        let catalog_results = Arc::new(Mutex::new(Vec::new()));
        let health = Arc::new(Mutex::new(Vec::new()));
        let (action_sender, action_receiver) = mpsc::sync_channel(ACTION_QUEUE_CAPACITY);
        let completion_mailbox = Arc::new(Mutex::new(VecDeque::with_capacity(ACTION_COMPLETION_CAPACITY)));
        let action_endpoint = Arc::new(ModernActionEndpoint {
            sender: action_sender,
            completions: Arc::clone(&completion_mailbox),
            action_wake: crate::plugin_action::CompletionWake::default(),
            gates: preview_endpoint.gates.clone(),
            preview_pending: Arc::clone(&preview_endpoint.pending),
            items: Arc::clone(&provider.action_items),
            catalog_items: Arc::clone(&provider.catalog.items),
            static_catalog_items: Arc::clone(&provider.catalog.static_items),
            accepted_commands: Arc::clone(&provider.catalog.accepted_commands),
            root: Arc::clone(&provider.catalog.root),
            views: Arc::new(Mutex::new(crate::standard_view::ViewAuthority::default())),
            cancellation: Arc::clone(&provider.cancellation),
            incarnations: Arc::clone(&preview_endpoint.incarnations),
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
        let thread_search_summary = Arc::clone(&search_summary);
        let thread_current = Arc::clone(&current);
        let thread_completion_mailbox = Arc::clone(&completion_mailbox);
        let thread_action_wake = action_endpoint.action_wake.clone();
        let thread_views = Arc::clone(&action_endpoint.views);
        let thread_preview_endpoint = Arc::clone(&preview_endpoint);
        let thread_failed = Arc::clone(&cancellation);
        let runtime = Arc::new(Mutex::new(provider));
        let thread_runtime = Arc::clone(&runtime);
        let spawned = std::thread::Builder::new()
            .name("crikey-modern".to_owned())
            .spawn(move || {
                let mut runtime = thread_runtime.lock().unwrap_or_else(|e| e.into_inner());
                let provider = &mut *runtime;
                let (lock, cvar) = &*thread_mailbox;
                let mut deferred_preview: Option<ModernPreviewJob> = None;
                loop {
                    let catalog_results = provider.take_catalog_results();
                    thread_preview_endpoint.refresh_liveness(provider);
                    {
                        let live = thread_preview_endpoint
                            .incarnations
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        thread_views
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .retain_source(|owner| live.get(owner).copied());
                        for loaded in &provider.loaded {
                            if let Some(worker) = provider.pool.live_mut(&loaded.key) {
                                if !thread_views
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .owns_source(&loaded.plugin, worker.incarnation())
                                {
                                    worker.retire_view();
                                }
                            }
                        }
                    }
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
                                    let request = match request {
                                        crate::standard_view::ForegroundRequest::Action(request) => request,
                                        crate::standard_view::ForegroundRequest::View(request) => {
                                            enqueue_modern_completion(
                                                &thread_completion_mailbox,
                                                &thread_action_wake,
                                                request.completion(Err(crikey_core::CoreError::Cancelled)),
                                            );
                                            continue;
                                        }
                                    };
                                    let completion = PluginActionCompletion {
                                        request_id: request.request_id.clone(),
                                        plugin: request.plugin.clone(),
                                        item_id: request.item.stable_id.clone(),
                                        action_id: request.work.action_id().cloned(),
                                        outcome: Err(crikey_core::CoreError::Cancelled),
                                    };
                                    enqueue_modern_completion(
                                        &thread_completion_mailbox,
                                        &thread_action_wake,
                                        completion,
                                    );
                                }
                                while let Ok(request) = preview_receiver.try_recv() {
                                    thread_preview_endpoint.complete(request, PreviewState::Cancelled);
                                }
                                if let Some(request) = deferred_preview.take() {
                                    thread_preview_endpoint.complete(request, PreviewState::Cancelled);
                                }
                                drop(slot);
                                return;
                            }
                            if let Ok(request) = action_receiver.try_recv() {
                                break match request {
                                    crate::standard_view::ForegroundRequest::Action(request) => {
                                        ModernWork::Action(Box::new(request))
                                    }
                                    crate::standard_view::ForegroundRequest::View(request) => {
                                        ModernWork::View(request)
                                    }
                                };
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
                        ModernWork::View(mut request) => {
                            let result = catch_unwind(AssertUnwindSafe(|| {
                                provider.execute_view_request(&mut request)
                            }))
                            .unwrap_or_else(|_| {
                                thread_failed.failed.store(true, Ordering::Release);
                                Err(crikey_core::CoreError::Invalid(
                                    "standard view worker failed".into(),
                                ))
                            });
                            let result = request.finish(result);
                            thread_preview_endpoint.refresh_liveness(provider);
                            enqueue_modern_completion(
                                &thread_completion_mailbox,
                                &thread_action_wake,
                                request.completion(result),
                            );
                            continue;
                        }
                        ModernWork::Refresh => continue,
                        ModernWork::Preview(request) => {
                            thread_preview_endpoint.execute(provider, request);
                            continue;
                        }
                        ModernWork::Action(request) => {
                            let request_id = request.request_id.clone();
                            let plugin = request.plugin.clone();
                            let item_id = request.item.stable_id.clone();
                            let action_id = request.work.action_id().cloned();
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
                                    Err(_) => {
                                        thread_failed.failed.store(true, Ordering::Release);
                                        Err(crikey_core::CoreError::Invalid(
                                            "modern action worker failed".into(),
                                        ))
                                    }
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
                                outcome: result,
                            };
                            thread_preview_endpoint.refresh_liveness(provider);
                            enqueue_modern_completion(
                                &thread_completion_mailbox,
                                &thread_action_wake,
                                completion,
                            );
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
                    // Tell the provider which admitted query it is serving, so
                    // a lazy plugin's blocking startup can abandon an obsolete
                    // target list instead of walking the whole chain.
                    provider.begin_serving(job.intake);

                    // The blocking child interpreter calls happen here, on this
                    // thread — never on the caller's. Stale answers are refused
                    // at the pipeline's intake boundary inside `drive_query`.
                    let modern = provider.drive_query_admitted(&mut pipeline, &job.query, job.now, job.scope);
                    thread_preview_endpoint.refresh_liveness(provider);
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
                        action_panel: None,
                        settings_open: false,
                        settings: Arc::default(),
                        settings_focus: None,
                        command_management: Arc::default(),
                        setting_edits: Arc::default(),
                        show_hints: true,
                        rounded_corners: true,
                        page: None,
                        preview: Default::default(),
                        navigation: Default::default(),
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
                    let mut summary = pipeline.ordinary_search_summary(0);
                    summary.generation = job.generation;
                    *thread_search_summary
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = Some(summary);
                    *thread_outcome.lock().unwrap_or_else(|error| error.into_inner()) = Some(merged.clone());
                    publish(&merged);
                    drop(slot);
                }
            });

        let worker_started = spawned.is_ok();
        let signal_mailbox = Arc::clone(&mailbox);
        let signal_preview = Arc::clone(&preview_endpoint);
        let signal_actions = Arc::clone(&action_endpoint);
        let signal_cancellation = Arc::clone(&cancellation);
        let signal_runtime = Arc::clone(&runtime);
        #[cfg(test)]
        let test_runtime = Arc::clone(&runtime);
        let owners: Arc<[PluginId]> = owners.into();
        let control = DriverControl::new(
            "modern",
            Arc::clone(&owners),
            admission,
            move || {
                let _retained_runtime = &signal_runtime;
                signal_preview.shutdown();
                for token in signal_actions
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .values()
                {
                    token.store(true, Ordering::Release);
                }
                signal_cancellation.cancel_all();
                let mut slot = signal_mailbox.0.lock().unwrap_or_else(|e| e.into_inner());
                slot.stop = true;
                slot.job = None;
                slot.configuration = None;
                drop(slot);
                signal_mailbox.1.notify_all();
            },
            move |epoch| {
                let result = match spawned {
                    Ok(worker) => worker
                        .join()
                        .map_err(|_| StopError::new("modern-supervisor-panicked")),
                    Err(_) => Err(StopError::new("modern-supervisor-spawn-failed")),
                };
                let physical = runtime
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .shutdown_checked();
                result?;
                physical?;
                Ok(StoppedRuntime::validated("modern", epoch, owners.to_vec()))
            },
        );
        Self {
            mailbox,
            action_endpoint,
            preview_endpoint,
            permissions,
            catalog_results,
            commands,
            health,
            outcome,
            search_summary,
            query_policies,
            current,
            cancellation,
            has_plugins: has_plugins && worker_started,
            control,
            worker_started,
            #[cfg(test)]
            runtime_owner: test_runtime,
        }
    }

    /// Permanent admission fence and checked physical settlement.
    pub fn control(&self) -> DriverControl {
        self.control.clone()
    }

    /// Installs the terminal action notification independently of result and preview wakes.
    pub fn set_action_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        self.action_endpoint.action_wake.install(Arc::new(wake));
    }

    /// Bounded preview endpoint; all transport and decoding runs off-thread.
    pub(crate) fn preview_dispatcher(&self) -> Arc<dyn PreviewDispatcher> {
        self.preview_endpoint.clone()
    }

    /// Terminal evidence is independent of visible rows and optional frames.
    pub fn ordinary_search_summary(&self, view: u64) -> Option<crate::query_pipeline::OrdinarySearchSummary> {
        let mut summary = self
            .search_summary
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()?;
        if summary.generation.get() != self.current.load(Ordering::Acquire) {
            return None;
        }
        summary.view = view;
        Some(summary)
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
        let _ = self.submit_scoped(
            ProviderQueryContext {
                generation,
                text: query,
                now,
                selected,
            },
            builtin_rows,
            builtin_pending,
            None,
        );
    }

    /// Submit an exact owner-qualified scope, returning an explicit stale-source refusal.
    pub fn submit_scoped(
        &self,
        context: ProviderQueryContext<'_>,
        builtin_rows: Vec<ResultRow>,
        builtin_pending: bool,
        scope: Option<RootScope>,
    ) -> crikey_core::Result<()> {
        let admission = self.control.admission();
        // Acquire the fence only at bounded mailbox insertion, after source locks.
        let ProviderQueryContext {
            generation,
            text: query,
            now,
            selected,
        } = context;
        if scope.as_ref().is_some_and(|scope| {
            self.permissions.contains_key(&scope.owner)
                && !crate::PluginActionExecutor::root_scope_current(self.action_endpoint.as_ref(), scope)
        }) {
            return Err(crikey_core::CoreError::Invalid(
                "modern query scope is unavailable".into(),
            ));
        }
        // Intake is monotonic even though `Generation` can be reconstructed
        // from an external value. A delayed caller must not rewind the live
        // generation and make its obsolete job eligible for publication.
        let generation_value = generation.get();
        let mut observed = self.current.load(Ordering::Acquire);
        loop {
            if generation_value < observed {
                return Ok(());
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
        // Capture and retirement share publication's ledger lock. A scope token
        // is not executable authority and does not use the old intake stamp.
        self.cancellation.cancel_before(generation_value);
        let intake = self.cancellation.admit();

        let (lock, cvar) = &*self.mailbox;
        let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
        if slot.stop {
            return Err(crikey_core::CoreError::Cancelled);
        }
        {
            let mut root = self
                .action_endpoint
                .root
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let owned_scope = scope
                .as_ref()
                .filter(|scope| self.permissions.contains_key(&scope.owner));
            if (scope.is_none() || owned_scope.is_some()) && !root.capture_scope(owned_scope) {
                return Err(crikey_core::CoreError::Invalid(
                    "modern query scope is unavailable".into(),
                ));
            }
            root.retire_suggestions();
            self.cancellation
                .suggestion_intake
                .store(intake, Ordering::Release);
        }
        let _admission = admission.admit().map_err(|_| crikey_core::CoreError::Cancelled)?;
        let state = if self.worker_started {
            crate::command_preferences::OrdinaryProviderState::Pending
        } else {
            crate::command_preferences::OrdinaryProviderState::Failed
        };
        *self
            .search_summary
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(crate::query_pipeline::OrdinarySearchSummary {
                view: 0,
                generation,
                providers: self
                    .query_policies
                    .iter()
                    .filter(|(_, policy)| policy.query_applicable(query))
                    .filter(|(owner, _)| scope.as_ref().is_none_or(|scope| &scope.owner == *owner))
                    .map(|(plugin, _)| crate::query_pipeline::OrdinarySearchProvider {
                        plugin: plugin.clone(),
                        state,
                    })
                    .collect(),
            });
        slot.job = Some(ModernJob {
            generation,
            query: query.to_owned(),
            intake,
            now,
            builtin_rows,
            builtin_pending,
            selected,
            scope,
        });
        drop(slot);
        cvar.notify_one();
        Ok(())
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
        let admission = self.control.admission();
        let (lock, cvar) = &*self.mailbox;
        let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
        if slot.stop {
            return;
        }
        let Ok(_admission) = admission.admit() else {
            return;
        };
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

    /// See `ModernProvider::command_snapshot`; this never probes a worker.
    pub fn command_snapshot(&self) -> Arc<ModernCommandSnapshot> {
        Arc::clone(&self.commands.lock().unwrap_or_else(|error| error.into_inner()))
    }

    pub fn command_changes_since(&self, revision: u64) -> Option<Arc<ModernCommandSnapshot>> {
        let snapshot = self.command_snapshot();
        (snapshot.revision != revision).then_some(snapshot)
    }

    pub fn invalidate_command_source(&self, owner: &PluginId, request_id: u64) -> bool {
        let invalidated = invalidate_modern_commands(
            &self.commands,
            &self.action_endpoint.accepted_commands,
            owner,
            request_id,
        );
        if invalidated {
            let mut root = self
                .action_endpoint
                .root
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if root.snapshots().iter().any(|snapshot| {
                snapshot.owner == *owner
                    && matches!(&snapshot.source, RootSource::Catalog(source) if source.publication == request_id)
            }) {
                root.invalidate(owner);
            }
        }
        invalidated
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
        let gate = self.control.admission();
        let epoch = match gate.state() {
            crate::runtime_control::PluginAdmission::Open => std::num::NonZeroU64::MIN,
            crate::runtime_control::PluginAdmission::Closing { epoch }
            | crate::runtime_control::PluginAdmission::Offline { epoch }
            | crate::runtime_control::PluginAdmission::FailedClosed { epoch } => epoch,
        };
        if let Ok(ticket) = self.control.request_stop(epoch) {
            let _ = ticket.wait();
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
            admission: AdmissionGate::default(),
            gates: BTreeMap::new(),
            action_in_flight: Arc::default(),
            catalog_items: Arc::new(Mutex::new(BTreeMap::new())),
            static_catalog_items: Arc::default(),
            accepted_commands: Arc::default(),
            incarnations: Arc::new(Mutex::new(BTreeMap::from([(plugin.clone(), 11)]))),
            items: Arc::new(Mutex::new(BTreeMap::from([(
                (plugin.clone(), item_id.clone()),
                (item, 7),
            )]))),
            pending: Arc::new(Mutex::new(Vec::new())),
            stopped: AtomicBool::new(false),
            wake: Mutex::new(None),
        };
        let (found, origin) = endpoint
            .lookup(&plugin, &item_id, None)
            .expect("the retained item is known");
        assert_eq!(found.stable_id, item_id);
        assert_eq!(
            origin, 7,
            "lookup must not rebind retained rows to live worker 11"
        );
        let identity = PreviewIdentity {
            target: crikey_ui::ActionTarget::Result {
                owner: plugin.clone(),
                item: item_id.clone(),
            },
            catalog_source: None,
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

    #[test]
    fn declared_preview_ignores_same_id_suggestion_and_pins_publication() {
        use crate::plugin_action::DeclaredCommandSource;
        use crikey_core::command::CommandRef;
        let plugin = PluginId("modern.preview-command".into());
        let item = ItemId("shared".into());
        let canonical = Item {
            stable_id: item.clone(),
            plugin_id: plugin.clone(),
            category: crikey_core::Category::Command,
            label: "Canonical".into(),
            description: String::new(),
            target: String::new(),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: crikey_core::HitPolicy::Recorded,
            score_hint: 0,
            metadata: BTreeMap::from([("crikey.preview".into(), "deferred".into())]),
            actions: Vec::new(),
        };
        let mut suggestion = canonical.clone();
        suggestion.label = "Suggestion".into();
        let source = DeclaredCommandSource {
            instance: 2,
            generation: Generation::ZERO,
            producer: 7,
            publication: 3,
        };
        let (sender, receiver) = mpsc::sync_channel(2);
        let endpoint = ModernPreviewEndpoint {
            sender,
            admission: AdmissionGate::default(),
            gates: BTreeMap::new(),
            action_in_flight: Arc::default(),
            incarnations: Arc::new(Mutex::new(BTreeMap::from([(plugin.clone(), 7)]))),
            items: Arc::new(Mutex::new(BTreeMap::from([(
                (plugin.clone(), item.clone()),
                (suggestion, 7),
            )]))),
            catalog_items: Arc::new(Mutex::new(BTreeMap::from([(
                (plugin.clone(), item.clone()),
                (canonical, 7),
            )]))),
            static_catalog_items: Arc::default(),
            accepted_commands: Arc::default(),
            pending: Arc::new(Mutex::new(Vec::new())),
            stopped: AtomicBool::new(false),
            wake: Mutex::new(None),
        };
        endpoint.accepted_commands.lock().unwrap().insert(
            plugin.clone(),
            Arc::new(ModernOwnerCommands {
                request_id: source.publication,
                instance: source.instance,
                generation: source.generation,
                incarnation: source.producer,
                status: ModernCommandStatus::Ready,
                declarations: Vec::new(),
            }),
        );
        assert_eq!(
            endpoint.lookup(&plugin, &item, None).unwrap().0.label,
            "Suggestion"
        );
        assert_eq!(
            endpoint.lookup(&plugin, &item, Some(&source)).unwrap().0.label,
            "Canonical"
        );
        let mut stale = source.clone();
        stale.instance += 1;
        assert!(endpoint.lookup(&plugin, &item, Some(&stale)).is_none());
        let identity = PreviewIdentity {
            view: 1,
            query: Generation::ZERO,
            plugin: plugin.clone(),
            incarnation: 7,
            item,
            target: crikey_ui::ActionTarget::Command(CommandRef {
                provider_id: plugin,
                command_id: "preview".into(),
            }),
            catalog_source: Some(source.clone()),
            selection: 1,
            request: 1,
        };
        endpoint
            .submit(PreviewRequest { identity }, Arc::new(|_| {}))
            .unwrap();
        let admitted = receiver.try_recv().unwrap();
        assert_eq!(admitted.request.identity.catalog_source, Some(source));
        let origin = endpoint
            .incarnations
            .lock()
            .unwrap()
            .remove(&admitted.request.identity.plugin);
        assert_eq!(origin, Some(7));
        assert!(
            endpoint.submit(admitted.request, Arc::new(|_| {})).is_err(),
            "a canonical deferred preview cannot lazily replace its producing child"
        );
    }

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn load(class: &str) -> (Self, ModernProvider, QueryPipeline) {
            Self::load_source(
                class,
                include_str!("../../../compatibility/test-plugins/modern-preview/preview_plugin.py"),
            )
        }

        fn load_source(class: &str, source: &str) -> (Self, ModernProvider, QueryPipeline) {
            Self::load_sources(class, source, false)
        }

        fn load_sources(
            class: &str,
            source: &str,
            second_owner: bool,
        ) -> (Self, ModernProvider, QueryPipeline) {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "crikey-modern-lifecycle-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let package = root.join("plugins/fixture");
            fs::create_dir_all(&package).unwrap();
            fs::write(package.join("preview_plugin.py"), source).unwrap();
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
                fs::write(second.join("preview_plugin.py"), source).unwrap();
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
            (Self { root }, provider, pipeline)
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
        }
    }
    fn wait_for(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !condition() {
            assert!(Instant::now() < deadline, "real producer did not complete");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[allow(unsafe_code)]
    fn graceful_root_settles_redirected_descendant_and_retains_unfinished_reader() {
        const CHILD_MODE: &str = "CRIKEY_MODERN_TREE_TEST_HELPER";
        if std::env::var_os(CHILD_MODE).is_none() {
            let module = module_path!().split_once("::").unwrap().1;
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(format!(
                    "{module}::graceful_root_settles_redirected_descendant_and_retains_unfinished_reader"
                ))
                .env(CHILD_MODE, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        // This single-test subprocess owns and reaps only its known adopted
        // fixture descendants; the shared test runner is never a subreaper.
        unsafe extern "C" {
            fn prctl(option: i32, ...) -> i32;
            fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
            fn kill(pid: i32, signal: i32) -> i32;
        }
        assert_eq!(unsafe { prctl(36, 1i32, 0i32, 0i32, 0i32) }, 0);
        for escaped_pipe in [false, true] {
            let source = format!(
                r#"
import os, pathlib, time
from crikey_sdk import Item, Plugin, Preview
class TreeFixture(Plugin):
    def build_catalog(self):
        root = pathlib.Path(__file__).parent
        root.joinpath("root-pid").write_text(str(os.getpid()))
        child = os.fork()
        if child == 0:
            if {escaped}:
                os.setsid()
            else:
                null = os.open(os.devnull, os.O_RDWR)
                for fd in (0, 1, 2):
                    os.dup2(null, fd)
                os.close(null)
            root.joinpath("child-pid").write_text(str(os.getpid()))
            while True:
                time.sleep(1)
        return [Item("row", "Row", "row", preview_deferred=True)]
    def preview(self, item, context):
        return Preview(text="row")
"#,
                escaped = if escaped_pipe { "True" } else { "False" }
            );
            let (fixture, mut provider, pipeline) = Fixture::load_source("TreeFixture", &source);
            assert!(matches!(
                Fixture::catalog(&mut provider),
                CatalogBuildResult::Complete(_)
            ));
            let key = provider.loaded[0].key.clone();
            let producer = provider.pool.live_mut(&key).unwrap().incarnation();
            let root = fixture.root.join("plugins/fixture");
            wait_for(|| {
                fs::read_to_string(root.join("child-pid")).is_ok_and(|pid| pid.parse::<i32>().is_ok())
            });
            let descendant: i32 = fs::read_to_string(root.join("child-pid"))
                .unwrap()
                .parse()
                .unwrap();
            let root_pid: String = fs::read_to_string(root.join("root-pid")).unwrap();
            let reap = thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    let mut status = 0;
                    if unsafe { waitpid(descendant, &mut status, 1) } == descendant {
                        break;
                    }
                    assert!(Instant::now() < deadline, "known descendant was not reaped");
                    thread::sleep(Duration::from_millis(2));
                }
            });
            let driver = ModernDriver::spawn(provider, pipeline, |_| {});
            let control = driver.control();
            let epoch = std::num::NonZeroU64::new(47).unwrap();
            let result = control.request_stop(epoch).unwrap().wait();
            assert!(!Path::new("/proc").join(root_pid).exists());
            if escaped_pipe {
                assert!(result.is_err(), "an unfinished reader cannot support a proof");
                {
                    let mut runtime = driver.runtime_owner.lock().unwrap();
                    let retained = runtime
                        .pool
                        .live_mut(&key)
                        .expect("failed worker remains canonically owned");
                    assert_eq!(retained.incarnation(), producer);
                    assert!(retained.shutdown_checked().is_err());
                }
                assert_eq!(unsafe { kill(descendant, 9) }, 0);
            } else {
                result.unwrap();
            }
            reap.join().unwrap();
            assert!(!Path::new("/proc").join(descendant.to_string()).exists());
            if escaped_pipe {
                // The same owner can physically join the now-finished pipes,
                // but the original epoch remains permanently FailedClosed.
                let mut runtime = driver.runtime_owner.lock().unwrap();
                let _ = runtime.pool.live_mut(&key).unwrap().shutdown_checked();
                assert!(control.request_stop(epoch).unwrap().wait().is_err());
            }
        }
    }

    #[test]
    fn global_stop_before_lazy_process_insertion_is_clean_cancellation() {
        use std::num::NonZeroU64;
        let (fixture, provider, pipeline) = Fixture::load_source(
            "StopLazy",
            r#"
import pathlib
from crikey_sdk import Plugin
pathlib.Path(__file__).with_name("child-created").touch()
class StopLazy(Plugin):
    def suggest(self, query, context):
        return None
"#,
        );
        assert!(provider
            .pool
            .workers
            .values()
            .all(|slot| matches!(slot, WorkerLifecycle::NeverStarted)));
        let (entered, started) = mpsc::sync_channel(1);
        let (resume, resumed) = mpsc::sync_channel(1);
        *provider.cancellation.spawn_pause.lock().unwrap() = Some((entered, resumed));
        let owner = provider.loaded[0].plugin.clone();
        let driver = ModernDriver::spawn(provider, pipeline, |_| {});
        driver.submit(Generation::ZERO, "lazy", 0, Vec::new(), false, 0);
        started.recv_timeout(Duration::from_secs(30)).unwrap();
        let ticket = driver
            .control()
            .request_stop(NonZeroU64::new(37).unwrap())
            .unwrap();
        resume.send(()).unwrap();
        let proof = thread::spawn(move || ticket.wait()).join().unwrap().unwrap();
        assert_eq!(proof.owners(), &[owner]);
        assert!(!driver.cancellation.failed.load(Ordering::Acquire));
        assert!(!fixture.root.join("plugins/fixture/child-created").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn lost_real_callback_terminal_during_stop_fails_closed_but_settled_history_does_not() {
        use std::num::NonZeroU64;
        for during_stop in [false, true] {
            let (fixture, provider, pipeline) = Fixture::load_source(
                "StopCrash",
                r#"
import os, pathlib, time
from crikey_sdk import Plugin
class StopCrash(Plugin):
    def suggest(self, query, context):
        pathlib.Path(__file__).with_name("started").write_text(str(os.getpid()))
        deadline = time.monotonic() + 5
        while not pathlib.Path(__file__).with_name("release").exists():
            if time.monotonic() >= deadline:
                break
            time.sleep(0.002)
        os._exit(19)
"#,
            );
            let driver = ModernDriver::spawn(provider, pipeline, |_| {});
            driver.submit(Generation::ZERO, "crash", 0, Vec::new(), false, 0);
            let root = fixture.root.join("plugins/fixture");
            let marker = root.join("started");
            wait_for(|| fs::read_to_string(&marker).is_ok_and(|pid| pid.parse::<u32>().is_ok()));
            let pid = fs::read_to_string(marker).unwrap();
            if !during_stop {
                fs::write(root.join("release"), "").unwrap();
                wait_for(|| driver.cancellation.calls.lock().unwrap().is_empty());
                // Unregistration precedes checked pool retirement; the supervisor
                // publishes its outcome only after that physical cleanup.
                wait_for(|| driver.take_outcome().is_some());
            }
            let control = driver.control();
            let epoch = NonZeroU64::new(41).unwrap();
            let ticket = control.request_stop(epoch).unwrap();
            if during_stop {
                fs::write(root.join("release"), "").unwrap();
            }
            let result = thread::spawn(move || ticket.wait()).join().unwrap();
            assert!(!Path::new("/proc").join(pid).exists());
            if during_stop {
                assert!(result.is_err());
                assert_eq!(
                    result.unwrap_err(),
                    control.request_stop(epoch).unwrap().wait().unwrap_err()
                );
                assert!(matches!(control.admission().state(),
                    crate::runtime_control::PluginAdmission::FailedClosed { epoch: failed } if failed == epoch));
            } else {
                result.unwrap();
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn global_stop_holds_real_view_slot_until_physical_callback_terminal() {
        use std::num::NonZeroU64;
        let source = format!(
            "{}\n{}",
            include_str!("../../../compatibility/test-plugins/modern-views/views_plugin.py"),
            r#"
class StopViews(ViewsPlugin):
    async def structured_view(self, request, context):
        import time
        Path(__file__).with_name("started").write_text(str(os.getpid()))
        deadline = time.monotonic() + 5
        while not Path(__file__).with_name("release").exists():
            if time.monotonic() >= deadline:
                raise RuntimeError("view release barrier expired")
            time.sleep(0.002)
        return await super().structured_view(request, context)
"#
        );
        let (fixture, mut provider, pipeline) = Fixture::load_source("StopViews", &source);
        let owner = provider.loaded[0].plugin.clone();
        let key = provider.loaded[0].key.clone();
        let budget = Arc::clone(&provider.loaded[0].budget);
        provider.ensure_worker(&owner, &key).unwrap();
        let producer = provider.pool.live_mut(&key).unwrap().incarnation();
        let driver = ModernDriver::spawn(provider, pipeline, |_| {});
        let endpoint = driver.action_executor();
        let request = endpoint
            .request_view(
                &owner,
                producer,
                crikey_core::structured_view::ViewRequest {
                    mode: crikey_core::structured_view::ViewRequestMode::Open,
                    route: crikey_core::structured_view::RouteId("gallery".into()),
                    session: crikey_core::structured_view::ViewSessionIdentity {
                        logical_view: NonZeroU64::MIN,
                        surface: NonZeroU64::MIN,
                    },
                    previous_revision: None,
                    state: None,
                    query: String::new(),
                    scroll: None,
                    reason: crikey_core::structured_view::ViewRefreshReason::Explicit,
                },
            )
            .unwrap();
        let marker = fixture.root.join("plugins/fixture/started");
        wait_for(|| fs::read_to_string(&marker).is_ok_and(|pid| pid.parse::<u32>().is_ok()));
        let pid = fs::read_to_string(marker).unwrap();
        let ticket = driver
            .control()
            .request_stop(NonZeroU64::new(29).unwrap())
            .unwrap();
        assert!(
            budget.try_acquire_owned(BudgetKind::Action).is_none(),
            "logical cancellation released a physical view slot"
        );
        let wait = thread::spawn(move || ticket.wait());
        assert!(!wait.is_finished(), "view callback has not physically returned");
        fs::write(fixture.root.join("plugins/fixture/release"), "").unwrap();
        wait.join().unwrap().unwrap();
        assert!(!Path::new("/proc").join(pid).exists());
        let completions = endpoint.poll_plugin_actions();
        assert!(completions.iter().any(|result| result.request_id == request
            && matches!(result.outcome, Err(crikey_core::CoreError::Cancelled))));
        assert!(budget.try_acquire_owned(BudgetKind::Action).is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn global_stop_holds_real_preview_slot_until_physical_callback_terminal() {
        use std::num::NonZeroU64;
        let source = format!(
            "{}\n{}",
            include_str!("../../../compatibility/test-plugins/modern-preview/preview_plugin.py"),
            r#"
class StopPreview(PreviewPlugin):
    async def preview(self, item, context):
        import pathlib, time
        pathlib.Path(__file__).with_name("started").write_text(str(os.getpid()))
        deadline = time.monotonic() + 5
        while not pathlib.Path(__file__).with_name("release").exists():
            if time.monotonic() >= deadline:
                raise RuntimeError("preview release barrier expired")
            time.sleep(0.002)
        return Preview(text="terminal")
"#
        );
        let (fixture, mut provider, pipeline) = Fixture::load_source("StopPreview", &source);
        let CatalogBuildResult::Complete(build) = Fixture::catalog(&mut provider) else {
            panic!("real catalog must complete");
        };
        let item = build.items[0].stable_id.clone();
        let owner = provider.loaded[0].plugin.clone();
        let key = provider.loaded[0].key.clone();
        let budget = Arc::clone(&provider.loaded[0].budget);
        let producer = provider.pool.live_mut(&key).unwrap().incarnation();
        let driver = ModernDriver::spawn(provider, pipeline, |_| {});
        let endpoint = driver.preview_dispatcher();
        let (updates, terminal) = mpsc::sync_channel(1);
        endpoint
            .submit(
                PreviewRequest {
                    identity: PreviewIdentity {
                        target: crikey_ui::ActionTarget::Result {
                            owner: owner.clone(),
                            item: item.clone(),
                        },
                        catalog_source: None,
                        view: 1,
                        query: Generation::ZERO,
                        plugin: owner,
                        incarnation: producer,
                        item,
                        selection: 1,
                        request: 1,
                    },
                },
                Arc::new(move |update| {
                    updates.send(update).unwrap();
                }),
            )
            .unwrap();
        let marker = fixture.root.join("plugins/fixture/started");
        wait_for(|| fs::read_to_string(&marker).is_ok_and(|pid| pid.parse::<u32>().is_ok()));
        let pid = fs::read_to_string(marker).unwrap();
        let ticket = driver
            .control()
            .request_stop(NonZeroU64::new(31).unwrap())
            .unwrap();
        assert!(
            budget.try_acquire_owned(BudgetKind::Action).is_none(),
            "logical cancellation released a physical preview slot"
        );
        let wait = thread::spawn(move || ticket.wait());
        assert!(!wait.is_finished());
        fs::write(fixture.root.join("plugins/fixture/release"), "").unwrap();
        wait.join().unwrap().unwrap();
        assert!(matches!(terminal.recv().unwrap().state, PreviewState::Cancelled));
        assert!(!Path::new("/proc").join(pid).exists());
        assert!(budget.try_acquire_owned(BudgetKind::Action).is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn global_stop_joins_real_catalog_busy_owner_and_reuses_exact_terminal() {
        use std::num::NonZeroU64;
        for keep_preview in [false, true] {
            let source = format!(
                r#"
import os, pathlib, time
from crikey_sdk import Item, Plugin, Preview
class StopFixture(Plugin):
    def build_catalog(self):
        pathlib.Path(__file__).with_name("started").write_text(str(os.getpid()))
        time.sleep(0.2)
        return [Item(stable_id="row", label="Row", target="row", preview_deferred={})]
    def preview(self, item, context):
        return Preview(text="row")
"#,
                if keep_preview { "True" } else { "False" }
            );
            let (fixture, mut provider, pipeline) = Fixture::load_source("StopFixture", &source);
            let owner = provider.loaded[0].plugin.clone();
            let key = provider.loaded[0].key.clone();
            provider
                .request_catalog_build(&owner, 1, Generation::ZERO)
                .unwrap();
            assert!(matches!(
                provider.pool.workers.get(&key),
                Some(WorkerLifecycle::CatalogBusy)
            ));
            let marker = fixture.root.join("plugins/fixture/started");
            wait_for(|| fs::read_to_string(&marker).is_ok_and(|pid| pid.parse::<u32>().is_ok()));
            let pid = fs::read_to_string(&marker).unwrap();
            assert!(Path::new("/proc").join(&pid).exists());
            let driver = ModernDriver::spawn(provider, pipeline, |_| {});
            let endpoint = driver.action_executor();
            let control = driver.control();
            let epoch = NonZeroU64::new(17).unwrap();
            let first = control.request_stop(epoch).unwrap();
            let repeated = control.request_stop(epoch).unwrap();
            let proof = thread::spawn(move || first.wait()).join().unwrap().unwrap();
            let again = repeated.wait().unwrap();
            assert_eq!(proof.runtime(), "modern");
            assert_eq!(proof.epoch(), again.epoch());
            assert_eq!(proof.owners(), &[owner.clone()]);
            assert_eq!(proof.owners(), again.owners());
            assert!(
                !Path::new("/proc").join(pid).exists(),
                "receipt preceded child exit"
            );
            assert!(control.request_stop(NonZeroU64::new(18).unwrap()).is_err());
            assert!(endpoint
                .request_view(
                    &owner,
                    1,
                    crikey_core::structured_view::ViewRequest {
                        mode: crikey_core::structured_view::ViewRequestMode::Open,
                        route: crikey_core::structured_view::RouteId("retained".into()),
                        session: crikey_core::structured_view::ViewSessionIdentity {
                            logical_view: NonZeroU64::MIN,
                            surface: NonZeroU64::MIN,
                        },
                        previous_revision: None,
                        state: None,
                        query: String::new(),
                        scroll: None,
                        reason: crikey_core::structured_view::ViewRefreshReason::Explicit,
                    }
                )
                .is_err());
            assert!(driver
                .submit_scoped(
                    ProviderQueryContext {
                        generation: Generation::ZERO,
                        text: "retained",
                        now: 0,
                        selected: 0,
                    },
                    Vec::new(),
                    false,
                    None
                )
                .is_err());
            assert!(driver.preview_endpoint.incarnation(&owner).is_none());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn global_stop_fences_concurrent_queries_and_waits_for_real_callback_exit() {
        use std::num::NonZeroU64;
        let (fixture, provider, pipeline) = Fixture::load_source(
            "StopFixture",
            r#"
import os, pathlib, time
from crikey_sdk import Item, Plugin
class StopFixture(Plugin):
    def suggest(self, query, context):
        pathlib.Path(__file__).with_name("started").write_text(str(os.getpid()))
        time.sleep(0.2)
        context.emit(Item(stable_id="row", label="Row", target="row"))
"#,
        );
        let owner = provider.loaded[0].plugin.clone();
        let driver = ModernDriver::spawn(provider, pipeline, |_| {});
        driver.submit(Generation::ZERO, "start", 0, Vec::new(), false, 0);
        let marker = fixture.root.join("plugins/fixture/started");
        wait_for(|| fs::read_to_string(&marker).is_ok_and(|pid| pid.parse::<u32>().is_ok()));
        let pid = fs::read_to_string(marker).unwrap();
        let control = driver.control();
        thread::scope(|scope| {
            let barrier = Arc::new(std::sync::Barrier::new(3));
            let enqueue_barrier = Arc::clone(&barrier);
            let driver = &driver;
            let enqueue = scope.spawn(move || {
                enqueue_barrier.wait();
                for _ in 0..64 {
                    let _ = driver.submit_scoped(
                        ProviderQueryContext {
                            generation: Generation::ZERO,
                            text: "racing",
                            now: 0,
                            selected: 0,
                        },
                        Vec::new(),
                        false,
                        None,
                    );
                }
            });
            let configuration_barrier = Arc::clone(&barrier);
            let configuration = scope.spawn(move || {
                configuration_barrier.wait();
                for _ in 0..64 {
                    driver.publish_configuration(crate::PluginConfiguration::default());
                }
            });
            barrier.wait();
            let ticket = control.request_stop(NonZeroU64::new(23).unwrap()).unwrap();
            enqueue.join().unwrap();
            configuration.join().unwrap();
            let proof = scope.spawn(move || ticket.wait()).join().unwrap().unwrap();
            assert_eq!(proof.owners(), &[owner]);
        });
        assert!(!Path::new("/proc").join(pid).exists());
        assert!(driver
            .submit_scoped(
                ProviderQueryContext {
                    generation: Generation::ZERO,
                    text: "closed",
                    now: 0,
                    selected: 0,
                },
                Vec::new(),
                false,
                None
            )
            .is_err());
    }

    #[test]
    fn real_preview_yields_once_without_retiring_canonical_authority_or_foreground_quota() {
        for mode in ["execute", "cancel", "invalidate"] {
            let (fixture, mut provider, pipeline) = Fixture::load_source(
                "HandoffCommands",
                include_str!("../../../compatibility/test-plugins/modern-commands/commands_plugin.py"),
            );
            assert!(matches!(
                Fixture::catalog(&mut provider),
                CatalogBuildResult::Complete(_)
            ));
            let plugin = provider.loaded[0].plugin.clone();
            let snapshot = provider.command_snapshot();
            let current = &snapshot.owners[&plugin];
            let declaration = current.declarations[0].clone();
            let source = crate::plugin_action::DeclaredCommandSource {
                publication: current.request_id,
                instance: current.instance,
                generation: current.generation,
                producer: current.incarnation,
            };
            let driver = ModernDriver::spawn(provider, pipeline, |_| {});
            let endpoint = driver.action_executor();
            let root = fixture.root.join("plugins/fixture");
            let preview_state = Arc::new(Mutex::new(None));
            let sink = Arc::clone(&preview_state);
            driver
                .preview_endpoint
                .submit(
                    PreviewRequest {
                        identity: PreviewIdentity {
                            view: 1,
                            query: Generation::ZERO,
                            plugin: plugin.clone(),
                            incarnation: source.producer,
                            item: ItemId("handoff".into()),
                            target: crikey_ui::ActionTarget::Command(declaration.reference.clone()),
                            catalog_source: Some(source.clone()),
                            selection: 1,
                            request: 1,
                        },
                    },
                    Arc::new(move |update| *sink.lock().unwrap() = Some(update.state)),
                )
                .unwrap();
            wait_for(|| root.join("preview-entered").exists());
            let before = Instant::now();
            let receipt = endpoint
                .submit_declared_command_with_receipt(&declaration, &source, Some("frozen argument"))
                .unwrap();
            assert!(
                before.elapsed() < Duration::from_secs(1),
                "admission blocked on preview"
            );
            assert!(
                endpoint
                    .submit_declared_command_with_receipt(&declaration, &source, None)
                    .is_err(),
                "one bounded handoff only"
            );
            wait_for(|| root.join("preview-cancelled").exists());
            assert!(!root.join("executions").exists(), "cancellation is not terminal");
            match mode {
                "cancel" => assert!(endpoint.cancel_plugin_action(&receipt.request_id)),
                "invalidate" => assert!(driver.invalidate_command_source(&plugin, source.publication)),
                _ => fs::write(root.join("action-hold"), "").unwrap(),
            }
            fs::write(root.join("preview-release"), "").unwrap();
            if mode == "execute" {
                wait_for(|| root.join("executions").exists());
                assert!(root.join("preview-terminal").exists());
                assert!(
                    endpoint
                        .submit_declared_command_with_receipt(&declaration, &source, None)
                        .is_err(),
                    "real foreground saturation must refuse"
                );
                fs::write(root.join("action-release"), "").unwrap();
            }
            let mut completions = Vec::new();
            wait_for(|| {
                completions.extend(endpoint.poll_plugin_actions());
                !completions.is_empty()
            });
            assert_eq!(completions.len(), 1);
            assert_eq!(completions[0].request_id, receipt.request_id);
            assert_eq!(completions[0].outcome.is_ok(), mode == "execute");
            wait_for(|| preview_state.lock().unwrap().is_some());
            assert_eq!(*preview_state.lock().unwrap(), Some(PreviewState::Cancelled));
            assert_eq!(
                driver.action_endpoint.gates[&plugin]
                    .budget()
                    .in_flight(crikey_plugin_supervisor::BudgetKind::Action),
                0,
                "only the physical preview/action terminals release the shared slot"
            );
            assert!(!driver.action_endpoint.gates[&plugin].has_foreground());
            assert_eq!(
                driver.preview_endpoint.incarnations.lock().unwrap().get(&plugin),
                Some(&source.producer),
                "handoff must retain the same physical canonical producer"
            );
            if mode == "execute" {
                let executions = fs::read_to_string(root.join("executions")).unwrap();
                assert_eq!(executions.lines().count(), 1);
                let event: serde_json::Value = serde_json::from_str(executions.trim()).unwrap();
                assert_eq!(event["target"], "frozen canonical");
                assert_eq!(event["action"], "run");
                assert_eq!(event["argument"], "frozen argument");
            } else {
                assert!(!root.join("executions").exists());
            }
        }
    }

    #[test]
    fn other_owner_foreground_cancels_preview_on_the_shared_supervisor() {
        let (fixture, mut provider, pipeline) = Fixture::load_sources(
            "HandoffCommands",
            include_str!("../../../compatibility/test-plugins/modern-commands/commands_plugin.py"),
            true,
        );
        let preview_owner = PluginId("modern.fixture".into());
        let action_owner = PluginId("modern.second".into());
        for owner in [&preview_owner, &action_owner] {
            provider
                .request_catalog_build(owner, 1, Generation::ZERO)
                .unwrap();
            wait_for(|| !provider.take_catalog_results().is_empty());
        }
        let snapshot = provider.command_snapshot();
        let source_for = |owner: &PluginId| {
            let current = &snapshot.owners[owner];
            crate::plugin_action::DeclaredCommandSource {
                publication: current.request_id,
                instance: current.instance,
                generation: current.generation,
                producer: current.incarnation,
            }
        };
        let action_source = source_for(&action_owner);
        let declaration = &snapshot.owners[&action_owner].declarations[0];
        let driver = ModernDriver::spawn(provider, pipeline, |_| {});
        let endpoint = driver.action_executor();
        let root = fixture.root.join("plugins/fixture");
        let second = fixture.root.join("plugins/second");
        let mut native_pipeline = QueryPipeline::new(crate::PipelineConfig::default());
        let native_provider =
            crate::NativeProvider::load(&mut native_pipeline, &[], &DisabledPlugins::default());
        let native = crate::NativeDriver::spawn(native_provider, native_pipeline, Box::new(|_| {}));
        let mut service = crate::PreviewService::new(&native, &driver, || {});
        let search = crate::SearchService::new(crate::App::new());
        let mut model = crikey_ui::LauncherViewModel::new();
        model.activate();
        let _ = model.apply(crikey_ui::UiCommand::SetQuery("handoff".into()));
        let generation = Generation::from_raw(1);
        model.begin_generation(generation);
        let row = |id: &str| ResultRow {
            target: crikey_ui::ActionTarget::Result {
                owner: preview_owner.clone(),
                item: ItemId(id.into()),
            },
            source: Some(crikey_ui::ResultSource {
                owner: preview_owner.clone(),
                item: ItemId(id.into()),
                kind: crikey_ui::ResultSourceKind::Ordinary,
            }),
            command_state: None,
            label: id.into(),
            description: String::new(),
            icon_reference: None,
            icon: None,
            category: String::new(),
            plugin_name: String::new(),
            highlights: Vec::new(),
            argument_hint: None,
            status: None,
            default_action: None,
            alternate_actions: Vec::new(),
        };
        model.publish(generation, vec![row("handoff")], false);
        service.synchronize(&mut model, &search);
        wait_for(|| root.join("preview-entered").exists());
        let receipt = endpoint
            .submit_declared_command_with_receipt(declaration, &action_source, Some("other owner"))
            .unwrap();
        service.retire_for_foreground(&mut model, &action_owner);
        fs::write(second.join("action-hold"), "").unwrap();
        model.publish(generation, vec![row("latest")], false);
        service.synchronize(&mut model, &search);
        wait_for(|| root.join("preview-cancelled").exists());
        assert!(!second.join("executions").exists());
        fs::write(root.join("preview-release"), "").unwrap();
        wait_for(|| second.join("executions").exists());
        // Consume the old preview's actual terminal while foreground B still
        // owns its slot. Only the latest UI selection may remain pending.
        service.synchronize(&mut model, &search);
        assert!(!root.join("latest-preview").exists());
        assert!(!matches!(
            model.frame().unwrap().preview.status,
            Some(crikey_ui::PreviewStatus::Content(_))
        ));
        fs::write(second.join("action-release"), "").unwrap();
        let mut completions = Vec::new();
        wait_for(|| {
            completions.extend(endpoint.poll_plugin_actions());
            !completions.is_empty()
        });
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].request_id, receipt.request_id);
        assert!(completions[0].outcome.is_ok(), "{:?}", completions[0].outcome);
        wait_for(|| {
            service.synchronize(&mut model, &search);
            model.frame().is_some_and(|frame| {
                matches!(frame.preview.status, Some(crikey_ui::PreviewStatus::Content(_)))
            })
        });
        assert!(root.join("latest-preview").exists());
        assert_eq!(model.preview_selection().unwrap().item, ItemId("latest".into()));
        assert!(root.join("preview-terminal").exists());
        assert!(!root.join("executions").exists());
        let executions = fs::read_to_string(second.join("executions")).unwrap();
        assert_eq!(executions.lines().count(), 1);
        let event: serde_json::Value = serde_json::from_str(executions.trim()).unwrap();
        assert_eq!(event["argument"], "other owner");
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
                target: crikey_ui::ActionTarget::Result {
                    owner: item.plugin_id.clone(),
                    item: item.stable_id.clone(),
                },
                source: Some(crikey_ui::ResultSource {
                    owner: item.plugin_id,
                    item: item.stable_id,
                    kind: crikey_ui::ResultSourceKind::Ordinary,
                }),
                command_state: None,
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

#[cfg(test)]
mod query_history_receipt_tests {
    use super::*;
    use crate::PluginActionExecutor;

    fn item() -> Item {
        Item {
            stable_id: ItemId("shared".into()),
            plugin_id: PluginId("modern.receipt".into()),
            category: crikey_core::Category::Command,
            label: "Receipt".into(),
            description: String::new(),
            target: String::new(),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: crikey_core::ArgumentPolicy::Forbidden,
            hit_policy: crikey_core::HitPolicy::Recorded,
            score_hint: 0,
            metadata: BTreeMap::new(),
            actions: vec![crikey_core::Action {
                action_id: ActionId("run".into()),
                label: "Run".into(),
                description: String::new(),
                applicable_categories: Vec::new(),
                icon_reference: None,
                execution_policy: crikey_core::ExecutionPolicy::Plugin,
            }],
        }
    }

    #[test]
    fn declared_admission_pins_catalog_and_preserves_actionless_callback() {
        use crate::plugin_action::{AdmittedPluginWork, DeclaredCommandSource};
        use crikey_core::command::{CommandAvailability, CommandRef, ExecutableTarget};
        let suggestion = item();
        let (endpoint, receiver) = endpoint(suggestion.clone());
        let mut canonical = suggestion.clone();
        canonical.label = "Canonical default".into();
        canonical.actions.clear();
        let declaration = CommandDeclaration {
            reference: CommandRef {
                provider_id: canonical.plugin_id.clone(),
                command_id: "default".into(),
            },
            label: "Default callback".into(),
            executable_target: ExecutableTarget::Item {
                item_id: canonical.stable_id.clone(),
                action_id: None,
            },
            argument_policy: canonical.argument_policy,
            availability: CommandAvailability::Available,
        };
        let source = DeclaredCommandSource {
            instance: 2,
            generation: Generation::ZERO,
            producer: 7,
            publication: 3,
        };
        endpoint.accepted_commands.lock().unwrap().insert(
            canonical.plugin_id.clone(),
            Arc::new(ModernOwnerCommands {
                request_id: source.publication,
                instance: source.instance,
                generation: source.generation,
                incarnation: source.producer,
                status: ModernCommandStatus::Ready,
                declarations: vec![declaration.clone()],
            }),
        );
        endpoint.static_catalog_items.lock().unwrap().insert(
            (canonical.plugin_id.clone(), canonical.stable_id.clone()),
            canonical.clone(),
        );
        let receipt = endpoint
            .submit_declared_command_with_receipt(&declaration, &source, None)
            .unwrap();
        assert!(receipt.query_history_allowed);
        let admitted = receiver.try_recv().unwrap();
        assert_eq!(admitted.item.label, canonical.label);
        assert!(
            admitted.origin.is_none(),
            "released producers use existing lazy execution"
        );
        assert!(matches!(
            &admitted.work,
            AdmittedPluginWork::Command { action_id: None, .. }
        ));
        admitted
            .work
            .validate(&admitted.item, admitted.argument.as_deref())
            .unwrap();
        assert!(
            admitted.work.action_id().is_none(),
            "the worker receives None, not a fabricated ID"
        );
        let mut stale = source.clone();
        stale.publication += 1;
        assert!(endpoint
            .submit_declared_command_with_receipt(&declaration, &stale, None)
            .is_err());
        assert!(receiver.try_recv().is_err());
        assert!(endpoint
            .submit_declared_command_with_receipt(&declaration, &source, Some("secret"))
            .is_err());
        assert!(receiver.try_recv().is_err());
        drop(admitted);
        let publications = ModernCommands::default();
        publish_modern_commands(
            &publications,
            canonical.plugin_id.clone(),
            ModernOwnerCommands {
                request_id: source.publication + 1,
                instance: source.instance,
                generation: source.generation,
                incarnation: 0,
                status: ModernCommandStatus::Pending,
                declarations: Vec::new(),
            },
            true,
        );
        endpoint
            .submit_declared_command_with_receipt(&declaration, &source, None)
            .unwrap();
        drop(receiver.try_recv().unwrap());
        assert!(invalidate_modern_commands(
            &publications,
            &endpoint.accepted_commands,
            &canonical.plugin_id,
            source.publication
        ));
        assert!(
            endpoint
                .submit_declared_command_with_receipt(&declaration, &source, None)
                .is_err(),
            "retiring the accepted source also revokes it while a newer discovery is pending"
        );
    }

    fn endpoint(
        item: Item,
    ) -> (
        ModernActionEndpoint,
        crate::standard_view::ActionReceiver<ModernActionRequest>,
    ) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let owner = item.plugin_id.clone();
        (
            ModernActionEndpoint {
                sender,
                completions: Arc::default(),
                action_wake: crate::plugin_action::CompletionWake::default(),
                gates: BTreeMap::from([(
                    owner.clone(),
                    PreviewActionGate::new(crikey_plugin_supervisor::shared_budget_from_section(
                        &Default::default(),
                    )),
                )]),
                preview_pending: Arc::default(),
                incarnations: Arc::new(Mutex::new(BTreeMap::from([(owner.clone(), 1)]))),
                items: Arc::new(Mutex::new(BTreeMap::from([(
                    (owner, item.stable_id.clone()),
                    (item, 1),
                )]))),
                catalog_items: Arc::default(),
                static_catalog_items: Arc::default(),
                accepted_commands: Arc::default(),
                root: Arc::default(),
                views: Arc::default(),
                cancellation: Arc::default(),
                pending: Arc::default(),
                in_flight: Arc::default(),
                next_id: Arc::default(),
                mailbox: Arc::new((
                    Mutex::new(ModernRequestSlot {
                        job: None,
                        configuration: None,
                        stop: false,
                    }),
                    Condvar::new(),
                )),
            },
            crate::standard_view::ActionReceiver(receiver),
        )
    }

    #[test]
    fn receipt_uses_the_frozen_accepted_item_not_the_displayed_catalog_policy() {
        use crikey_core::ArgumentPolicy;
        for policy in [
            ArgumentPolicy::Forbidden,
            ArgumentPolicy::Optional,
            ArgumentPolicy::Required,
        ] {
            let displayed = item();
            let mut current = displayed.clone();
            current.argument_policy = policy;
            let (endpoint, receiver) = endpoint(current);
            let endpoint = Arc::new(endpoint);
            let mut router = crate::PluginActionRouter::default();
            router
                .register([displayed.plugin_id.clone()], endpoint.clone())
                .unwrap();
            let mut search = crate::SearchService::new(crate::App::new());
            for stage in [
                crate::StartupStage::WindowAndHotkey,
                crate::StartupStage::PersistedCatalog,
                crate::StartupStage::AcceptQueries,
            ] {
                search.complete_stage(stage).unwrap();
            }
            search.set_plugin_action_router(Arc::new(router));
            search
                .replace_catalog(&displayed.plugin_id, 1, vec![displayed.clone()])
                .unwrap();
            search.submit_query("Receipt").unwrap();
            let result = search.execute_owned(
                &displayed.plugin_id,
                &displayed.stable_id,
                &ActionId("run".into()),
            );
            let mut history = crate::QueryHistoryStore::session(Duration::MAX);
            if result.as_ref().is_ok_and(|receipt| receipt.query_history_allowed) {
                history.record_submitted("Receipt").unwrap();
            }
            assert_eq!(
                history.snapshot(),
                if policy == ArgumentPolicy::Forbidden {
                    vec!["Receipt".to_owned()]
                } else {
                    Vec::new()
                }
            );
            if policy == ArgumentPolicy::Required {
                assert!(result.is_err());
                assert!(
                    receiver.try_recv().is_err(),
                    "refused arguments have no admission receipt"
                );
            } else {
                let receipt = result.unwrap();
                endpoint
                    .items
                    .lock()
                    .unwrap()
                    .values_mut()
                    .next()
                    .unwrap()
                    .0
                    .argument_policy = ArgumentPolicy::Required;
                let accepted = receiver.try_recv().unwrap();
                assert_eq!(
                    receipt.submission,
                    crate::ActionSubmission::Pending(accepted.request_id.clone())
                );
                assert_eq!(accepted.item.argument_policy, policy);
                assert_eq!(receipt.query_history_allowed, policy == ArgumentPolicy::Forbidden);
                assert!(endpoint
                    .submit_plugin_action_with_receipt(
                        &displayed.plugin_id,
                        &displayed,
                        &ActionId("run".into()),
                        None
                    )
                    .is_err());
            }
        }
    }

    #[test]
    fn stale_primary_action_cannot_authorize_query_collection() {
        let displayed = item();
        let mut current = displayed.clone();
        current.actions.clear();
        let (endpoint, receiver) = endpoint(current);
        assert!(endpoint
            .submit_plugin_action_with_receipt(
                &displayed.plugin_id,
                &displayed,
                &ActionId("run".into()),
                None
            )
            .is_err());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn a_displayed_primary_that_is_now_an_alternate_is_not_a_root_submission() {
        let displayed = item();
        let mut current = displayed.clone();
        let mut replacement = current.actions[0].clone();
        replacement.action_id = ActionId("new-primary".into());
        current.actions.insert(0, replacement);
        let (endpoint, receiver) = endpoint(current);
        let receipt = endpoint
            .submit_plugin_action_with_receipt(
                &displayed.plugin_id,
                &displayed,
                &ActionId("run".into()),
                None,
            )
            .unwrap();
        assert!(!receipt.query_history_allowed);
        assert_eq!(
            receiver.try_recv().unwrap().item.actions[0].action_id.0,
            "new-primary"
        );
    }

    #[derive(Debug)]
    struct ProcessFixture(std::path::PathBuf);
    impl Drop for ProcessFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn real_modern_open_and_refresh_retirement_fences_the_final_writer_queue() {
        use crikey_core::structured_view::*;
        use std::num::NonZeroU64;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for refresh in [false, true] {
            let root = std::env::temp_dir().join(format!(
                "crikey-modern-route-writer-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            let fixture = ProcessFixture(root);
            let package = fixture.0.join("plugins/views");
            fs::create_dir_all(&package).unwrap();
            fs::write(package.join("crikey.toml"), "manifest-version = 1\n[plugin]\nid = \"views\"\nname = \"Views\"\nversion = \"1.0.0\"\nruntime = \"python\"\nentrypoint = \"views_plugin:ViewsPlugin\"\n[python]\nrequires-python = \">=3.12\"\n").unwrap();
            fs::write(
                package.join("views_plugin.py"),
                include_str!("../../../compatibility/test-plugins/modern-views/views_plugin.py"),
            )
            .unwrap();
            let mut pipeline = QueryPipeline::new(crate::PipelineConfig::default());
            let mut provider = ModernProvider::load(
                &mut pipeline,
                &[fixture.0.join("plugins")],
                None,
                fixture.0.join("cache"),
                &DisabledPlugins::default(),
            );
            assert_eq!(provider.loaded.len(), 1, "{:?}", provider.unavailable());
            let owner = provider.loaded[0].plugin.clone();
            let key = provider.loaded[0].key.clone();
            let gates = BTreeMap::from([(
                owner.clone(),
                PreviewActionGate::new(Arc::clone(&provider.loaded[0].budget)),
            )]);
            provider.ensure_worker(&owner, &key).unwrap();
            let producer = provider.pool.live_mut(&key).unwrap().incarnation();
            let authority = Arc::new(Mutex::new(crate::standard_view::ViewAuthority::default()));
            let (sender, receiver) = mpsc::sync_channel(1);
            let pending = Mutex::new(BTreeMap::new());
            let in_flight = AtomicUsize::new(0);
            let next_id = AtomicU64::new(0);
            let admission_gate = provider.cancellation.admission.clone();
            let enqueue = |request| {
                crate::standard_view::enqueue::<ModernActionRequest>(
                    &sender,
                    &admission_gate,
                    &gates,
                    &pending,
                    &in_flight,
                    &next_id,
                    &authority,
                    &owner,
                    producer,
                    crate::standard_view::ViewOperation::Request(request),
                    ACTION_IN_FLIGHT_CAPACITY,
                    ACTION_TIMEOUT_MS,
                )
                .unwrap();
                match receiver.recv().unwrap() {
                    crate::standard_view::ForegroundRequest::View(queued) => queued,
                    _ => panic!("the route must use the existing foreground queue"),
                }
            };
            let session = ViewSessionIdentity {
                logical_view: NonZeroU64::new(1).unwrap(),
                surface: NonZeroU64::new(1).unwrap(),
            };
            let request = ViewRequest {
                mode: ViewRequestMode::Open,
                route: RouteId("gallery".into()),
                session,
                previous_revision: None,
                state: None,
                query: String::new(),
                scroll: None,
                reason: ViewRefreshReason::Explicit,
            };
            let mut seed = enqueue(request);
            let effect = provider.execute_view_request(&mut seed);
            let crate::ActionEffect::View(reply) = seed.finish(effect).unwrap() else {
                panic!("seed Open must publish");
            };
            let crate::standard_view::ViewOperation::Request(mut request) = seed.operation else {
                panic!("seed owns its request");
            };
            if refresh {
                request.mode = ViewRequestMode::Refresh;
                request.previous_revision = Some(reply.context.revision);
                let admission = RouteAdmission::worker(
                    RouteIdentity {
                        owner: owner.clone(),
                        route: request.route.clone(),
                        incarnation: NonZeroU64::new(producer).unwrap(),
                    },
                    reply.context.surface(),
                )
                .unwrap();
                request.state = Some(reply.descriptor.initial_state(&admission).unwrap());
            } else {
                request.session.surface = NonZeroU64::new(2).unwrap();
            }
            request.query = "delay".into();
            let session = request.session;
            let mut queued = enqueue(request);
            let (ready, at_writer) = mpsc::sync_channel(1);
            let (release, released) = mpsc::sync_channel(1);
            queued.before_send = Some((ready, Mutex::new(released)));
            let result = thread::scope(|scope| {
                let callback = scope.spawn(|| provider.execute_view_request(&mut queued));
                at_writer
                    .recv_timeout(Duration::from_secs(60))
                    .expect("the real consumer must reach final writer admission");
                authority.lock().unwrap().retire(&owner, session);
                release.send(()).unwrap();
                callback.join().unwrap()
            });
            assert!(queued.finish(result).is_err());
            assert!(
                !package.join("route-running").exists(),
                "retired Open/Refresh must never invoke the real Python callback"
            );
        }
    }

    #[test]
    fn released_static_catalog_executes_frozen_admission_in_the_real_consumer() {
        use crikey_core::ArgumentPolicy;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "crikey-modern-receipt-consumer-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let fixture = ProcessFixture(root);
        let package = fixture.0.join("plugins/receipt");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("crikey.toml"),
            "manifest-version = 1\n[plugin]\nid = \"receipt\"\nname = \"Receipt\"\nversion = \"1.0.0\"\nruntime = \"python\"\nentrypoint = \"receipt:Receipt\"\n[python]\nrequires-python = \">=3.12\"\n[performance]\nstartup = \"lazy\"\n").unwrap();
        fs::write(package.join("receipt.py"), r#"
from pathlib import Path
from crikey_sdk import Action, Command, Item, Plugin
class Receipt(Plugin):
    def build_catalog(self):
        return [Item("shared", "Receipt", "original", actions=[Action("run", "Run", execution_policy="plugin")]),
                Item("actionless", "Actionless", "default")]
    def commands(self):
        return [Command("default", "Default callback", "actionless")]
    def execute(self, item, action_id, argument):
        with Path(__file__).with_name("executed").open("a", encoding="utf-8", newline="\n") as stream:
            stream.write(item.label + (" default" if action_id is None else "") + "\n")
"#).unwrap();
        let mut pipeline = QueryPipeline::new(crate::PipelineConfig::default());
        let mut provider = ModernProvider::load(
            &mut pipeline,
            &[fixture.0.join("plugins")],
            None,
            fixture.0.join("cache"),
            &DisabledPlugins::default(),
        );
        assert_eq!(provider.loaded.len(), 1, "{:?}", provider.unavailable());
        let owner = provider.loaded[0].plugin.clone();
        provider
            .request_catalog_build(&owner, 1, Generation::ZERO)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let build = loop {
            if let Some(result) = provider.take_catalog_results().into_iter().next() {
                match result {
                    CatalogBuildResult::Complete(build) => break build,
                    other => panic!("real static catalog failed: {other:?}"),
                }
            }
            assert!(Instant::now() < deadline, "modern catalog never completed");
            thread::sleep(Duration::from_millis(2));
        };
        let key = provider.loaded[0].key.clone();
        assert!(
            provider.pool.never_started(&key),
            "a healthy static producer was released"
        );
        let displayed = build
            .items
            .iter()
            .find(|item| item.stable_id.0 == "shared")
            .unwrap()
            .clone();
        let (mut endpoint, receiver) = endpoint(displayed.clone());
        endpoint.items = Arc::clone(&provider.action_items);
        endpoint.catalog_items = Arc::clone(&provider.catalog.items);
        endpoint.static_catalog_items = Arc::clone(&provider.catalog.static_items);
        endpoint.accepted_commands = Arc::clone(&provider.catalog.accepted_commands);
        endpoint.incarnations = Arc::default();
        endpoint.gates = provider
            .action_budgets()
            .into_iter()
            .map(|(owner, budget)| (owner, PreviewActionGate::new(budget)))
            .collect();
        let action = displayed.actions[0].action_id.clone();
        let receipt = endpoint
            .submit_plugin_action_with_receipt(&owner, &displayed, &action, None)
            .unwrap();
        assert!(receipt.query_history_allowed);
        let request = receiver.try_recv().unwrap();
        assert!(request.origin.is_none());
        {
            let mut catalog = provider.catalog.static_items.lock().unwrap();
            let replacement = catalog
                .get_mut(&(owner.clone(), displayed.stable_id.clone()))
                .unwrap();
            replacement.label = "Replacement".into();
            replacement.argument_policy = ArgumentPolicy::Required;
            replacement.actions.clear();
        }
        provider.execute_action_request(request).unwrap();
        let marker = package.join("executed");
        assert_eq!(
            fs::read_to_string(&marker).unwrap(),
            "Receipt\n",
            "the real callback received the admitted item, not replacement content"
        );
        let snapshot = provider.command_snapshot();
        let installed = &snapshot.owners[&owner];
        assert_eq!(installed.status, ModernCommandStatus::Ready);
        let source = crate::plugin_action::DeclaredCommandSource {
            instance: installed.instance,
            generation: installed.generation,
            producer: installed.incarnation,
            publication: installed.request_id,
        };
        endpoint
            .submit_declared_command_with_receipt(&installed.declarations[0], &source, None)
            .unwrap();
        let request = receiver.try_recv().unwrap();
        assert!(request.work.action_id().is_none());
        provider.execute_action_request(request).unwrap();
        assert_eq!(
            fs::read_to_string(&marker).unwrap(),
            "Receipt\nActionless default\n",
            "the real worker must receive an actionless callback with None"
        );
        assert!(endpoint
            .submit_plugin_action_with_receipt(&owner, &displayed, &action, None)
            .is_err());

        let origin = provider.pool.live_mut(&key).unwrap().incarnation();
        endpoint
            .incarnations
            .lock()
            .unwrap()
            .insert(owner.clone(), origin);
        provider.action_items.lock().unwrap().insert(
            (owner.clone(), displayed.stable_id.clone()),
            (displayed.clone(), origin),
        );
        endpoint
            .submit_plugin_action_with_receipt(&owner, &displayed, &action, None)
            .unwrap();
        let retired_request = receiver.try_recv().unwrap();
        provider
            .pool
            .fail(&key, "test retires the admitted dynamic origin".into());
        assert!(provider.execute_action_request(retired_request).is_err());
        assert_eq!(
            fs::read_to_string(marker).unwrap(),
            "Receipt\nActionless default\n"
        );
    }
    fn password_values() -> ArgumentValues {
        use crikey_core::structured_arguments::{FieldId, Password, StructuredValue};
        ArgumentValues(BTreeMap::from([(
            FieldId("secret".into()),
            StructuredValue::Password(Password::new("provider-private-value".into()).unwrap()),
        )]))
    }

    fn password_presentation(item: &Item) -> ResultPresentation {
        use crikey_core::result_presentation::ExecutableSchemaBinding;
        use crikey_core::structured_arguments::{ArgumentField, ArgumentSchema, FieldId, FieldKind};
        ResultPresentation {
            schemas: vec![ExecutableSchemaBinding {
                executable: ExecutableKey {
                    target: OwnedItem {
                        owner: item.plugin_id.clone(),
                        item: item.stable_id.clone(),
                    },
                    action: item.actions.first().map(|action| action.action_id.clone()),
                },
                schema: Arc::new(ArgumentSchema {
                    fields: vec![ArgumentField {
                        id: FieldId("secret".into()),
                        label: "Secret".into(),
                        required: true,
                        placeholder: None,
                        validation_message: None,
                        kind: FieldKind::Password {
                            min_bytes: 1,
                            max_bytes: 64,
                        },
                    }],
                }),
            }],
            ..ResultPresentation::default()
        }
    }

    #[test]
    fn typed_admission_moves_values_and_rejects_retired_generation_and_legacy_path() {
        let item = item();
        let (endpoint, receiver) = endpoint(item.clone());
        let source = RootSource::Suggestion {
            generation: Generation::from_raw(4),
            producer: 1,
        };
        let presentation = password_presentation(&item);
        let key = presentation.schemas[0].executable.clone();
        endpoint
            .root
            .lock()
            .unwrap()
            .publish(item.plugin_id.clone(), source, &[item.clone()], presentation)
            .unwrap();
        let binding = endpoint.root_binding(&key, None).unwrap();
        assert!(endpoint
            .submit_plugin_action_with_receipt(&item.plugin_id, &item, &item.actions[0].action_id, None,)
            .is_err());
        assert!(receiver.try_recv().is_err());
        let receipt = endpoint.submit_typed(&binding, password_values()).unwrap();
        assert!(!receipt.receipt.query_history_allowed);
        let request = receiver.try_recv().unwrap();
        assert!(request.argument.is_none());
        assert_eq!(request.origin, Some(1));
        assert!(!format!("{request:?}").contains("provider-private-value"));
        request.work.validate(&request.item, None).unwrap();
        drop(request);
        endpoint.root.lock().unwrap().retire_suggestions();
        assert!(endpoint.submit_typed(&binding, password_values()).is_err());
        assert!(receiver.try_recv().is_err());
        endpoint
            .root
            .lock()
            .unwrap()
            .publish(
                item.plugin_id.clone(),
                RootSource::Suggestion {
                    generation: Generation::from_raw(5),
                    producer: 1,
                },
                &[item.clone()],
                password_presentation(&item),
            )
            .unwrap();
        assert!(endpoint.submit_typed(&binding, password_values()).is_err());
        let current = endpoint.root_binding(&key, None).unwrap();
        endpoint
            .incarnations
            .lock()
            .unwrap()
            .insert(item.plugin_id.clone(), 2);
        assert!(endpoint.submit_typed(&current, password_values()).is_err());
    }

    #[test]
    fn invalid_complete_sidecar_grants_no_binding_or_scope_authority() {
        let item = item();
        let (endpoint, receiver) = endpoint(item.clone());
        let mut presentation = password_presentation(&item);
        let key = presentation.schemas[0].executable.clone();
        presentation.schemas[0].executable.target.owner = PluginId("modern.other".into());
        let source = RootSource::Suggestion {
            generation: Generation::from_raw(1),
            producer: 1,
        };
        assert!(endpoint
            .root
            .lock()
            .unwrap()
            .publish(
                item.plugin_id.clone(),
                source.clone(),
                &[item.clone()],
                presentation
            )
            .is_err());
        assert!(endpoint.root_binding(&key, None).is_none());
        assert!(endpoint.root_presentations().is_empty());
        assert!(!scope_declared(
            &endpoint.root,
            &RootScope {
                owner: item.plugin_id,
                id: crikey_core::result_presentation::ScopeId("private".into()),
                source,
            }
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn captured_dynamic_scope_survives_pending_intake_and_uses_fresh_membership() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "crikey-modern-scope-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&path).unwrap();
        let fixture = ProcessFixture(path);
        let package = fixture.0.join("plugins/scoped");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("crikey.toml"),
            "manifest-version = 1\n[plugin]\nid = \"scoped\"\nname = \"Scoped\"\nversion = \"1.0.0\"\nruntime = \"python\"\nentrypoint = \"scoped:Scoped\"\n[python]\nrequires-python = \">=3.12\"\n[performance]\nstartup = \"lazy\"\n").unwrap();
        fs::write(
            package.join("scoped.py"),
            r#"
from pathlib import Path
from crikey_sdk import Item, Plugin
from crikey_sdk.arguments import ResultPresentation, Scope, ItemPresentation
class Scoped(Plugin):
    def supports_result_presentation(self):
        return True
    def suggest(self, query, context):
        Path(__file__).with_name("scope").write_text(query.selected_scope_id or "all")
        context.emit(Item(query.text, query.text, query.text))
    def suggestion_presentation(self, query):
        if query.text == "missing":
            return ResultPresentation()
        return ResultPresentation(
            scopes=(Scope("private", "Private"),),
            items=(ItemPresentation(query.text, scopes=("private",)),),
        )
"#,
        )
        .unwrap();
        let mut pipeline = QueryPipeline::new(crate::PipelineConfig::default());
        let mut provider = ModernProvider::load(
            &mut pipeline,
            &[fixture.0.join("plugins")],
            None,
            fixture.0.join("cache"),
            &DisabledPlugins::default(),
        );
        assert_eq!(provider.loaded.len(), 1, "{:?}", provider.unavailable());
        provider.drive_query(&mut pipeline, "first", 1).unwrap();
        let snapshot = provider.catalog.root.lock().unwrap().snapshots().pop().unwrap();
        let scope = RootScope {
            owner: snapshot.owner,
            id: crikey_core::result_presentation::ScopeId("private".into()),
            source: snapshot.source,
        };
        assert!(provider
            .drive_query_scoped(&mut pipeline, "direct", 2, Some(scope.clone()),)
            .is_some());
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = mpsc::sync_channel(1);
        *provider.cancellation.publication_pause.lock().unwrap() = Some((entered_tx, resume_rx));
        let driver = ModernDriver::spawn(provider, pipeline, |_| {});
        let endpoint = driver.action_executor();
        driver
            .submit_scoped(
                ProviderQueryContext {
                    generation: Generation::from_raw(2),
                    text: "pending",
                    now: 2,
                    selected: 0,
                },
                Vec::new(),
                false,
                Some(scope.clone()),
            )
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(60)).unwrap();
        assert!(endpoint.root_presentations().is_empty());
        assert!(
            endpoint.root_scope_current(&scope),
            "retirement retains only the scope token"
        );
        driver
            .submit_scoped(
                ProviderQueryContext {
                    generation: Generation::from_raw(3),
                    text: "fresh",
                    now: 3,
                    selected: 0,
                },
                Vec::new(),
                false,
                Some(scope.clone()),
            )
            .unwrap();
        assert!(endpoint.root_scope_current(&scope));
        resume_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if driver
                .take_outcome()
                .is_some_and(|frame| frame.generation == Generation::from_raw(3))
            {
                break;
            }
            assert!(Instant::now() < deadline, "fresh scoped query did not complete");
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(fs::read_to_string(package.join("scope")).unwrap(), "private");
        let fresh = endpoint.root_presentations();
        assert_eq!(fresh.len(), 1);
        assert!(endpoint.root_scope_current(&scope));
        assert!(fresh[0]
            .presentation
            .items
            .iter()
            .any(|item| item.target.item == ItemId("fresh".into())));
        // A replacement or dead physical producer cannot inherit this token.
        let owner = scope.owner.clone();
        let producer = match &scope.source {
            RootSource::Suggestion { producer, .. } => *producer,
            _ => unreachable!(),
        };
        let (mut probe, _) = self::endpoint(item());
        probe.root = Arc::clone(&driver.action_endpoint.root);
        probe.gates = driver.action_endpoint.gates.clone();
        probe
            .incarnations
            .lock()
            .unwrap()
            .insert(owner.clone(), producer + 1);
        assert!(!probe.root_scope_current(&scope));
        probe.incarnations.lock().unwrap().remove(&owner);
        assert!(!probe.root_scope_current(&scope));
        driver
            .submit_scoped(
                ProviderQueryContext {
                    generation: Generation::from_raw(4),
                    text: "missing",
                    now: 4,
                    selected: 0,
                },
                Vec::new(),
                false,
                Some(scope.clone()),
            )
            .unwrap();
        loop {
            if driver
                .take_outcome()
                .is_some_and(|frame| frame.generation == Generation::from_raw(4))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "missing declaration query did not complete"
            );
            thread::sleep(Duration::from_millis(2));
        }
        assert!(!endpoint.root_scope_current(&scope));
        assert!(driver
            .submit_scoped(
                ProviderQueryContext {
                    generation: Generation::from_raw(5),
                    text: "refused",
                    now: 5,
                    selected: 0
                },
                Vec::new(),
                false,
                Some(scope),
            )
            .is_err());
    }

    #[test]
    fn real_typed_worker_confirms_fresh_schema_preserves_frozen_item_and_suppresses_diagnostics() {
        use crikey_core::structured_arguments::SubmissionOutcome;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "crikey-modern-typed-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&root).unwrap();
        let fixture = ProcessFixture(root);
        let package = fixture.0.join("plugins/typed");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("crikey.toml"),
            "manifest-version = 1\n[plugin]\nid = \"typed\"\nname = \"Typed\"\nversion = \"1.0.0\"\nruntime = \"python\"\nentrypoint = \"typed:Typed\"\n[python]\nrequires-python = \">=3.12\"\n[performance]\nstartup = \"lazy\"\n").unwrap();
        fs::write(package.join("typed.py"), r#"
from pathlib import Path
import sys
import time
from crikey_sdk import Item, Plugin
from crikey_sdk.arguments import ArgumentSchema, PasswordField, SchemaBinding, ResultPresentation, Scope
class Typed(Plugin):
    def supports_result_presentation(self):
        return True
    def build_catalog(self):
        if Path(__file__).with_name("bootstrap_wait").exists():
            Path(__file__).with_name("bootstrap_started").touch()
            while Path(__file__).with_name("bootstrap_wait").exists():
                time.sleep(0.005)
        return [Item("shared", "Frozen typed item", "original")]
    def catalog_presentation(self):
        maximum = 32 if Path(__file__).with_name("changed").exists() else 64
        return ResultPresentation(
            scopes=(Scope("private", "Private"),),
            schemas=(SchemaBinding("shared", ArgumentSchema((PasswordField(id="secret", label="Secret", required=True, min_bytes=1, max_bytes=maximum),))),),
        )
    def execute(self, item, action_id, argument):
        raise RuntimeError("legacy execution must never run")
    def execute_typed(self, item, action_id, values):
        assert action_id is None
        assert "provider-private-value" not in repr(values)
        with Path(__file__).with_name("executed").open("a", encoding="utf-8", newline="\n") as stream:
            stream.write(item.label + "\n")
        if Path(__file__).with_name("fail").exists():
            print("provider-private-value", file=sys.stderr)
            raise RuntimeError("provider-private-value")
    def suggest(self, query, context):
        if Path(__file__).with_name("fail").exists():
            context.log("provider-private-value")
            raise RuntimeError("provider-private-value")
        Path(__file__).with_name("scope").write_text(query.selected_scope_id or "all")
        context.emit(Item("dynamic", "Dynamic typed item", "dynamic"))
    def suggestion_presentation(self, query):
        return ResultPresentation(schemas=(SchemaBinding("dynamic", ArgumentSchema((PasswordField(id="secret", label="Secret", required=True, min_bytes=1, max_bytes=64),))),))
"#).unwrap();
        let mut pipeline = QueryPipeline::new(crate::PipelineConfig::default());
        let mut provider = ModernProvider::load(
            &mut pipeline,
            &[fixture.0.join("plugins")],
            None,
            fixture.0.join("cache"),
            &DisabledPlugins::default(),
        );
        assert_eq!(provider.loaded.len(), 1, "{:?}", provider.unavailable());
        let owner = provider.loaded[0].plugin.clone();
        provider
            .request_catalog_build(&owner, 1, Generation::ZERO)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let build = loop {
            if let Some(result) = provider.take_catalog_results().into_iter().next() {
                match result {
                    CatalogBuildResult::Complete(build) => break build,
                    other => panic!("typed catalog failed: {other:?}"),
                }
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        };
        let key = provider.loaded[0].key.clone();
        assert!(provider.pool.never_started(&key));
        let (mut endpoint, receiver) = endpoint(build.items[0].clone());
        endpoint.root = Arc::clone(&provider.catalog.root);
        endpoint.cancellation = Arc::clone(&provider.cancellation);
        endpoint.gates = provider
            .action_budgets()
            .into_iter()
            .map(|(owner, budget)| (owner, PreviewActionGate::new(budget)))
            .collect();
        let executable = ExecutableKey {
            target: OwnedItem {
                owner: owner.clone(),
                item: build.items[0].stable_id.clone(),
            },
            action: None,
        };
        let binding = endpoint.root_binding(&executable, None).unwrap();
        fs::write(package.join("changed"), "").unwrap();
        endpoint.submit_typed(&binding, password_values()).unwrap();
        assert!(provider
            .execute_action_request(receiver.try_recv().unwrap())
            .is_err());
        assert!(
            !package.join("executed").exists(),
            "fresh physical worker changed its schema"
        );
        assert!(provider
            .dispatch_failures()
            .iter()
            .all(|(_, reason)| !reason.contains("provider-private-value")));
        // A genuinely new physical worker bootstraps the original binding.
        fs::remove_file(package.join("changed")).unwrap();
        let worker = provider.pool.workers.remove(&key).unwrap();
        drop(worker);
        provider
            .pool
            .workers
            .insert(key.clone(), WorkerLifecycle::NeverStarted);
        provider
            .catalog
            .static_items
            .lock()
            .unwrap()
            .get_mut(&(owner.clone(), executable.target.item.clone()))
            .unwrap()
            .label = "replacement must not reach callback".into();
        let receipt = endpoint.submit_typed(&binding, password_values()).unwrap();
        assert!(!receipt.receipt.query_history_allowed);
        assert_eq!(receipt.item.label, "Frozen typed item");
        assert!(matches!(
            provider
                .execute_action_request(receiver.try_recv().unwrap())
                .unwrap(),
            crate::ActionEffect::Typed(SubmissionOutcome::Completed),
        ));
        assert_eq!(
            fs::read_to_string(package.join("executed")).unwrap(),
            "Frozen typed item\n"
        );
        let scope = RootScope {
            owner: owner.clone(),
            id: crikey_core::result_presentation::ScopeId("private".into()),
            source: binding.source.clone(),
        };
        let frame = provider
            .drive_query_scoped(&mut pipeline, "scoped", 1, Some(scope.clone()))
            .unwrap();
        assert_eq!(frame.rows.len(), 1);
        assert_eq!(fs::read_to_string(package.join("scope")).unwrap(), "private");
        let incarnation = provider.pool.live_mut(&key).unwrap().incarnation();
        endpoint
            .incarnations
            .lock()
            .unwrap()
            .insert(owner.clone(), incarnation);
        let dynamic = ExecutableKey {
            target: OwnedItem {
                owner: owner.clone(),
                item: ItemId("dynamic".into()),
            },
            action: None,
        };
        let dynamic_binding = endpoint.root_binding(&dynamic, None).unwrap();
        endpoint
            .submit_typed(&dynamic_binding, password_values())
            .unwrap();
        let retired = receiver.try_recv().unwrap();
        provider.drive_query(&mut pipeline, "new generation", 2).unwrap();
        assert!(
            provider.execute_action_request(retired).is_err(),
            "old generation cannot retarget an identical new result"
        );
        // Intake revokes execution even while ledger retirement is held.
        let held_binding = endpoint.root_binding(&dynamic, None).unwrap();
        endpoint.submit_typed(&held_binding, password_values()).unwrap();
        let held_request = receiver.try_recv().unwrap();
        let cancellation = Arc::clone(&provider.cancellation);
        let root = Arc::clone(&provider.catalog.root);
        let held_retirement = root.lock().unwrap();
        let intake = cancellation.admit();
        assert!(endpoint.submit_typed(&held_binding, password_values()).is_err());
        assert!(provider.execute_action_request(held_request).is_err());
        drop(held_retirement);
        assert!(
            endpoint.root_binding(&dynamic, None).is_none(),
            "intake rejects authority before retirement"
        );
        let mut held_retirement = root.lock().unwrap();
        held_retirement.retire_suggestions();
        cancellation.suggestion_intake.store(intake, Ordering::Release);
        drop(held_retirement);
        assert!(endpoint.root_binding(&dynamic, None).is_none());
        provider.begin_serving(intake);
        fs::write(package.join("fail"), "").unwrap();
        endpoint.submit_typed(&binding, password_values()).unwrap();
        let failure = provider.execute_action_request(receiver.try_recv().unwrap());
        assert!(!format!("{failure:?}").contains("provider-private-value"));
        provider
            .drive_query(&mut pipeline, "later diagnostics", 3)
            .unwrap();
        assert!(provider
            .dispatch_failures()
            .iter()
            .all(|(_, reason)| !reason.contains("provider-private-value")));
        // Pause a real result after its first supersession check, then retire
        // its intake before allowing it to acquire publication authority.
        fs::remove_file(package.join("fail")).unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = mpsc::sync_channel(1);
        *provider.cancellation.publication_pause.lock().unwrap() = Some((entered_tx, resume_rx));
        let cancellation = Arc::clone(&provider.cancellation);
        let root = Arc::clone(&provider.catalog.root);
        let publication = thread::spawn(move || {
            let _ = provider.drive_query(&mut pipeline, "held publication", 4);
            (provider, pipeline)
        });
        entered_rx.recv_timeout(Duration::from_secs(60)).unwrap();
        let intake = cancellation.admit();
        {
            let mut root = root.lock().unwrap();
            root.retire_suggestions();
            cancellation.suggestion_intake.store(intake, Ordering::Release);
        }
        resume_tx.send(()).unwrap();
        let (mut provider, mut pipeline) = publication.join().unwrap();
        provider.begin_serving(intake);
        assert!(endpoint.root_binding(&dynamic, None).is_none());
        assert!(endpoint
            .submit_typed(&dynamic_binding, password_values())
            .is_err());
        assert!(receiver.try_recv().is_err());
        // Retire while fresh-worker metadata discovery is blocked, then release
        // discovery. Confirmation must not turn the retired source executable.
        let before = fs::read_to_string(package.join("executed")).unwrap();
        drop(provider.pool.workers.remove(&key));
        provider.pool.workers.insert(key, WorkerLifecycle::NeverStarted);
        fs::write(package.join("bootstrap_wait"), "").unwrap();
        endpoint.submit_typed(&binding, password_values()).unwrap();
        let request = receiver.try_recv().unwrap();
        let root = Arc::clone(&provider.catalog.root);
        let execution = thread::spawn(move || {
            let outcome = provider.execute_action_request(request);
            (provider, outcome)
        });
        let deadline = Instant::now() + Duration::from_secs(60);
        while !package.join("bootstrap_started").exists() {
            assert!(
                Instant::now() < deadline,
                "fresh worker never entered schema discovery"
            );
            thread::sleep(Duration::from_millis(2));
        }
        root.lock().unwrap().invalidate(&owner);
        fs::remove_file(package.join("bootstrap_wait")).unwrap();
        let (mut provider, outcome) = execution.join().unwrap();
        assert!(outcome.is_err());
        assert_eq!(fs::read_to_string(package.join("executed")).unwrap(), before);
        assert!(provider
            .drive_query_scoped(&mut pipeline, "stale", 4, Some(scope))
            .is_none());
        assert!(endpoint.submit_typed(&binding, password_values()).is_err());
        // A revoked in-flight catalog cannot restore root sidecar authority.
        fs::remove_file(package.join("bootstrap_started")).unwrap();
        fs::write(package.join("bootstrap_wait"), "").unwrap();
        provider
            .request_catalog_build(&owner, 2, Generation::from_raw(8))
            .unwrap();
        let request_id = provider.command_snapshot().owners[&owner].request_id;
        let deadline = Instant::now() + Duration::from_secs(60);
        while !package.join("bootstrap_started").exists() {
            assert!(Instant::now() < deadline, "catalog never entered held build");
            thread::sleep(Duration::from_millis(2));
        }
        assert!(provider.invalidate_command_source(&owner, request_id));
        fs::remove_file(package.join("bootstrap_wait")).unwrap();
        let result = loop {
            if let Some(result) = provider.take_catalog_results().into_iter().next() {
                break result;
            }
            assert!(Instant::now() < deadline, "retired catalog never completed");
            thread::sleep(Duration::from_millis(2));
        };
        let CatalogBuildResult::Complete(retired_catalog) = result else {
            panic!("retiring command authority must preserve the ordinary catalog");
        };
        let retained_item = retired_catalog
            .items
            .iter()
            .find(|item| item.plugin_id == owner && item.stable_id == executable.target.item)
            .expect("the ordinary catalog still contains its exact owned item");
        assert_eq!(retained_item.label, "Frozen typed item");
        assert!(endpoint.root_binding(&executable, None).is_none());
        assert!(!endpoint
            .root_presentations()
            .iter()
            .any(|snapshot| snapshot.owner == owner));
        assert!(endpoint.submit_typed(&binding, password_values()).is_err());

        // Exercise real restart and binding consumers for sibling preservation.
        let mut sibling = build.items[0].clone();
        sibling.plugin_id = PluginId("modern.healthy-sibling".into());
        let presentation = password_presentation(&sibling);
        let sibling_key = presentation.schemas[0].executable.clone();
        endpoint
            .root
            .lock()
            .unwrap()
            .publish(
                sibling.plugin_id.clone(),
                RootSource::Suggestion {
                    generation: Generation::from_raw(99),
                    producer: 1,
                },
                &[sibling],
                presentation,
            )
            .unwrap();
        let sibling_binding = endpoint.root_binding(&sibling_key, None).unwrap();
        let key = provider.loaded[0].key.clone();
        provider.pool.fail(&key, "restart regression".into());
        provider.restart_worker(&owner).unwrap();
        let retained = endpoint.root_binding(&sibling_key, None).unwrap();
        assert_eq!(retained.executable, sibling_binding.executable);
        assert_eq!(retained.source, sibling_binding.source);
        assert_eq!(retained.fingerprint, sibling_binding.fingerprint);
    }
}

impl Drop for ModernProvider {
    fn drop(&mut self) {
        // In particular, a supervisor panic must not detach CatalogBusy owners.
        let _ = catch_unwind(AssertUnwindSafe(|| self.shutdown_checked()));
    }
}
