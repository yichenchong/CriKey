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
//!
//! # Typed root publications
//!
//! Complete catalog and suggestion sidecars enter the shared source ledger as
//! one validated transaction. Typed execution uses the existing foreground
//! mailbox with move-only values and never authorizes query history. Canonical
//! catalogs may reacquire a worker, whose transport confirms its current schema;
//! suggestions retain their exact execution generation and producing connection,
//! while a selected scope may outlive that generation on the same live producer.
//! Worker schema discovery precedes final source, cancellation, and deadline
//! admission; only confirmed execution may then move values into transport.
//! Owner-qualified scopes are captured at query admission, never broadened when
//! their declaration is unavailable.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::plugin_action::{
    RootExecutableBinding, RootPresentationSnapshot, RootSource, RootSourceLedger, SharedOriginItems,
};
use crate::preview_action_budget::{ForegroundActionAdmission, PreviewActionGate, PreviewCancellation};
use crate::query_pipeline::ProviderQueryContext;
use crikey_core::command::{
    validate_plugin_commands, CommandAvailability, CommandDeclaration, PluginCommands, MAX_COMMANDS,
    MAX_COMMAND_BYTES,
};
use crikey_core::result_presentation::ResultPresentation;
use crikey_core::{ActionId, ArgumentPolicy, ExecutionPolicy, Generation, Item, ItemId, PageInput, PluginId};
use crikey_input_scheduler::Millis;
use crikey_native_host::{
    CancelHandle, ExecuteOutcome, HostError, LaunchSpec, NativePageRequest, NativeSuggestRequest,
    NativeSupervisor, ResourceKind, ResourceOutcome, Suggestions, SupervisorConfig, WorkerOptions,
};
use crikey_plugin_model::{Manifest, Permissions, Runtime, Startup};
use crikey_plugin_supervisor::{BudgetKind, PluginBudgetHandle};
use crikey_ui::{ResultRow, ViewModel};

use crate::plugin_icons::{
    IconFetch, PluginIconResolver, PluginResourceSource, MAX_PLUGIN_ICON_BYTES, PLUGIN_ICON_DEADLINE,
};
use crate::plugin_page::{
    PageBackend, PageClosing, PageDeadlines, PageDraw, PageOpen, PageSession, PageUpdate, PageWake,
};
use crate::{
    ActionEffect, ActionRequestId, BatchState, CatalogBuildResult, CatalogDispatchError, DisabledPlugins,
    ObsoleteCatalogBuild, PluginActionCompletion, QueryPipeline, ResultBatch, DISABLED_BY_CONFIGURATION,
};

/// Identifies a native worker by its executable and the package source
/// directory that supplied it. The source directory is intentionally part of
/// the key: two packages sharing an entrypoint still own separate processes.
type WorkerKey = (String, String);

/// UI preview readers only inspect this snapshot. Background lifecycle polling
/// updates idle workers as well as workers completing a query/action/preview.
type NativeIncarnations = Arc<Mutex<NativeIncarnationState>>;

/// Preview liveness and declaration provenance are deliberately distinct:
/// healthy release clears `live` but must not forget a replacement was seen.
#[derive(Debug, Default)]
struct NativeIncarnationState {
    live: BTreeMap<PluginId, u64>,
    observed: BTreeMap<PluginId, u64>,
}

impl NativeIncarnationState {
    fn get(&self, plugin: &PluginId) -> Option<&u64> {
        self.live.get(plugin)
    }
    fn insert(&mut self, plugin: PluginId, incarnation: u64) {
        if self.observed.get(&plugin) != Some(&incarnation) {
            self.observed.insert(plugin.clone(), incarnation);
        }
        self.live.insert(plugin, incarnation);
    }
    fn remove(&mut self, plugin: &PluginId) -> Option<u64> {
        self.live.remove(plugin)
    }
    fn clear(&mut self) {
        self.live.clear();
        self.observed.clear();
    }
}

fn refresh_incarnation(
    supervisor: &mut NativeSupervisor,
    plugin: &PluginId,
    incarnations: &NativeIncarnations,
) -> bool {
    let connection = supervisor.poll_live_connection(plugin);
    let mut incarnations = incarnations.lock().unwrap_or_else(|error| error.into_inner());
    match connection {
        Some(connection) if incarnations.get(plugin) == Some(&connection) => false,
        Some(connection) => {
            incarnations.insert(plugin.clone(), connection);
            true
        }
        None => incarnations.remove(plugin).is_some(),
    }
}
/// Process supervision uses one real monotonic epoch, independent of injected
/// query-pipeline scheduling time. Every native worker access uses this clock.
fn native_policy_now() -> Millis {
    static EPOCH: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
    EPOCH.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn with_native_worker<T>(
    supervisor: &mut NativeSupervisor,
    plugin: &PluginId,
    incarnations: &NativeIncarnations,
    call: impl FnOnce(&mut crikey_native_host::NativeWorker) -> Result<T, HostError>,
) -> Result<T, HostError> {
    refresh_incarnation(supervisor, plugin, incarnations);
    let result = match supervisor.worker(plugin, native_policy_now()) {
        Ok(worker) => {
            incarnations
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(plugin.clone(), worker.connection_id());
            call(worker)
        }
        Err(error) => Err(error),
    };
    refresh_incarnation(supervisor, plugin, incarnations);
    result
}

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

#[path = "native_preview.rs"]
mod native_preview;

/// One loaded native plugin and the shared query/catalog worker key that serves
/// it. The launch recipe is retained for the isolated resource supervisor.
#[derive(Debug, Clone)]
struct LoadedPlugin {
    plugin: PluginId,
    key: WorkerKey,
    launch: LaunchSpec,
    worker_options: WorkerOptions,
    budget: PluginBudgetHandle,
    action_gate: Arc<PreviewActionGate>,
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

/// Terminal declaration intake state, independent of preview-worker liveness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeCommandStatus {
    Ready,
    Unsupported,
    Cancelled,
    Invalid(String),
    Failed(String),
    Stale,
    Unavailable,
}

/// Immutable installed-owner declaration publication. `revision` is a host
/// publication identity, not a worker connection; use it as registry incarnation.
/// Ready declarations require matching catalog admission. Empty error or
/// invalidation publications revoke registry entries without a successful catalog.
#[derive(Debug, Clone)]
pub struct NativeCommandSnapshot {
    pub plugin: PluginId,
    pub revision: u64,
    pub instance: u64,
    pub generation: Generation,
    pub producer: u64,
    pub status: NativeCommandStatus,
    released: bool,
    pub declarations: Arc<[CommandDeclaration]>,
}

#[derive(Debug, Default)]
struct NativeCommandPublications {
    revision: u64,
    latest: BTreeMap<PluginId, u64>,
    owners: BTreeMap<PluginId, Arc<NativeCommandSnapshot>>,
}

impl NativeCommandPublications {
    fn publish(&mut self, mut snapshot: NativeCommandSnapshot) {
        let Some(revision) = self.revision.checked_add(1) else {
            // Exhaustion cannot revive an earlier executable publication.
            self.owners.clear();
            self.latest.clear();
            return;
        };
        let retained = self
            .owners
            .values()
            .filter(|owner| owner.plugin != snapshot.plugin);
        let (mut count, mut bytes) = (snapshot.declarations.len(), command_bytes(&snapshot.declarations));
        for owner in retained {
            count = count.saturating_add(owner.declarations.len());
            bytes = bytes.saturating_add(command_bytes(&owner.declarations));
        }
        if count > MAX_COMMANDS || bytes > MAX_COMMAND_BYTES {
            snapshot.status =
                NativeCommandStatus::Invalid("native declaration publications exceed bounds".into());
            snapshot.declarations = Arc::from([]);
        }
        self.revision = revision;
        snapshot.revision = revision;
        self.owners.insert(snapshot.plugin.clone(), Arc::new(snapshot));
    }

    fn invalidate(&mut self, plugin: &PluginId) {
        let pending = self.latest.remove(plugin).is_some();
        if let Some(previous) = self.owners.get(plugin) {
            let mut snapshot = (**previous).clone();
            snapshot.status = NativeCommandStatus::Unavailable;
            snapshot.declarations = Arc::from([]);
            self.publish(snapshot);
        } else if pending {
            // Invalidation is a revocation even before the first catalog has
            // succeeded. Empty terminal publications need no catalog admission.
            self.publish(NativeCommandSnapshot {
                plugin: plugin.clone(),
                revision: 0,
                instance: 0,
                generation: Generation::ZERO,
                producer: 0,
                released: false,
                status: NativeCommandStatus::Unavailable,
                declarations: Arc::from([]),
            });
        }
    }
}

fn command_bytes(commands: &[CommandDeclaration]) -> usize {
    commands
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

fn bounded_command_error(mut reason: String) -> String {
    if reason.len() > 8192 {
        let mut end = 8192;
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason.truncate(end);
    }
    reason
}

fn native_commands(
    owner: &PluginId,
    items: &[Item],
    outcome: Result<PluginCommands, HostError>,
) -> (NativeCommandStatus, Arc<[CommandDeclaration]>) {
    let commands = match outcome {
        Ok(PluginCommands::Unsupported) => return (NativeCommandStatus::Unsupported, Arc::from([])),
        Ok(PluginCommands::Cancelled) => return (NativeCommandStatus::Cancelled, Arc::from([])),
        Err(error) => {
            let reason = bounded_command_error(error.to_string());
            return (NativeCommandStatus::Failed(reason), Arc::from([]));
        }
        Ok(PluginCommands::Declarations(commands)) => commands,
    };
    let invalid = |reason: &str| -> (NativeCommandStatus, Arc<[CommandDeclaration]>) {
        (NativeCommandStatus::Invalid(reason.into()), Arc::from([]))
    };
    if let Err(reason) = validate_plugin_commands(&commands) {
        return invalid(reason);
    }
    // Catalog owner is host-bound before this function; no suggestion/cache
    // fallback is eligible. Duplicate item identities are ambiguous targets.
    let mut catalog = BTreeMap::new();
    for item in items {
        if &item.plugin_id != owner || catalog.insert(&item.stable_id, item).is_some() {
            return invalid("command catalog has ambiguous ownership or item identity");
        }
    }
    let mut declarations = Vec::with_capacity(commands.len());
    // Borrow catalog identities and cache action validity once per targeted
    // item. Repeated declarations must not rescan the same action/category set.
    let mut action_indexes: BTreeMap<&ItemId, Option<BTreeMap<&ActionId, bool>>> = BTreeMap::new();
    for command in commands {
        let Some(item) = catalog.get(&command.item_id) else {
            return invalid("command target is absent from the exact owner's catalog");
        };
        if command.argument_policy != item.argument_policy {
            return invalid("command argument policy differs from its catalog target");
        }
        // Omitted action IDs dispatch through the catalog's primary action,
        // unless the item is genuinely actionless. Preserve None on binding.
        let selected_action = command
            .action_id
            .as_ref()
            .or_else(|| item.actions.first().map(|action| &action.action_id));
        if let Some(id) = selected_action {
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
            if actions.get(id) != Some(&true) {
                return invalid("command action is absent, ambiguous or not plugin executable");
            }
        }
        let declaration = command.bind(owner.clone(), CommandAvailability::Available);
        if let Err(reason) = declaration.validate() {
            return invalid(reason);
        }
        declarations.push(declaration);
    }
    (NativeCommandStatus::Ready, declarations.into())
}
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
    result_tx: SyncSender<(
        u64,
        CatalogBuildResult,
        u64,
        NativeCommandSnapshot,
        ResultPresentation,
    )>,
    result_rx: Receiver<(
        u64,
        CatalogBuildResult,
        u64,
        NativeCommandSnapshot,
        ResultPresentation,
    )>,
    tasks: BTreeMap<u64, NativeCatalogTask>,
    commands: Arc<Mutex<NativeCommandPublications>>,
    incarnations: NativeIncarnations,
    next_id: u64,
    items: SharedOriginItems,
    /// Canonical targets whose healthy static catalog producer was released.
    static_items: Arc<Mutex<BTreeMap<(PluginId, ItemId), Item>>>,
    ledger: Arc<Mutex<RootSourceLedger>>,
}

impl Default for NativeCatalogDispatcher {
    fn default() -> Self {
        let (result_tx, result_rx) = mpsc::sync_channel(CATALOG_RESULT_CAPACITY);
        Self {
            result_tx,
            result_rx,
            tasks: BTreeMap::new(),
            commands: Arc::new(Mutex::new(NativeCommandPublications::default())),
            incarnations: Arc::new(Mutex::new(NativeIncarnationState::default())),
            next_id: 0,
            items: Arc::new(Mutex::new(BTreeMap::new())),
            static_items: Arc::new(Mutex::new(BTreeMap::new())),
            ledger: Arc::default(),
        }
    }
}

impl NativeCatalogDispatcher {
    fn next_id(&mut self) -> u64 {
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("catalog admission checks request identity exhaustion");
        self.next_id
    }

    fn insert(&mut self, id: u64, plugin: PluginId, join: JoinHandle<()>) {
        self.commands
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .latest
            .insert(plugin, id);
        self.tasks.insert(id, NativeCatalogTask { join: Some(join) });
    }

    fn take(&mut self) -> Vec<CatalogBuildResult> {
        let mut ready = Vec::new();
        while let Ok((id, result, incarnation, mut commands, presentation)) = self.result_rx.try_recv() {
            let Some(mut task) = self.tasks.remove(&id) else {
                continue;
            };
            if let Some(join) = task.join.take() {
                let _ = join.join();
            }
            let mut publications = self.commands.lock().unwrap_or_else(|error| error.into_inner());
            // `latest` is the newest request ever issued for this plugin,
            // not merely the newest still-running task. Keeping it after its
            // completion ensures an older task that retires later remains
            // explicitly obsolete.
            let result = match result {
                CatalogBuildResult::Complete(build)
                    if publications.latest.get(&build.plugin) != Some(&id) =>
                {
                    CatalogBuildResult::Obsolete(ObsoleteCatalogBuild {
                        plugin: build.plugin,
                        instance: build.instance,
                        generation: build.generation,
                    })
                }
                other => other,
            };
            if publications.latest.get(&commands.plugin) == Some(&id) {
                // A deliberate healthy release leaves no live connection and
                // remains installed. A replacement/dead retained origin cannot
                // lend its identity to an undrained declaration response.
                let incarnations = self
                    .incarnations
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let current = incarnations.get(&commands.plugin).copied();
                let observed = incarnations.observed.get(&commands.plugin).copied();
                if commands.status == NativeCommandStatus::Ready
                    && commands.producer != 0
                    && !commands.released
                    && (observed != Some(commands.producer) || current != Some(commands.producer))
                {
                    commands.status = NativeCommandStatus::Stale;
                    commands.declarations = Arc::from([]);
                }
                if let CatalogBuildResult::Complete(build) = &result {
                    let source = RootSource::Catalog(crate::plugin_action::DeclaredCommandSource {
                        instance: commands.instance,
                        generation: commands.generation,
                        producer: commands.producer,
                        publication: publications.revision.saturating_add(1),
                    });
                    let mut ledger = self.ledger.lock().unwrap_or_else(|error| error.into_inner());
                    if matches!(
                        commands.status,
                        NativeCommandStatus::Ready | NativeCommandStatus::Unsupported
                    ) {
                        if ledger
                            .publish(build.plugin.clone(), source, &build.items, presentation)
                            .is_err()
                        {
                            ledger.invalidate(&build.plugin);
                            commands.status =
                                NativeCommandStatus::Invalid("invalid native presentation".into());
                            commands.declarations = Arc::from([]);
                        }
                    } else {
                        ledger.invalidate(&build.plugin);
                    }
                } else {
                    self.ledger
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .invalidate(&commands.plugin);
                }
                publications.publish(commands);
            }
            if let CatalogBuildResult::Complete(build) = &result {
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
        self.commands
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .latest
            .clear();
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

impl PreviewCancellation for CallControl {
    fn cancel_preview(&self) {
        self.cancel();
    }
}

/// Cancellation registry shared with the non-blocking driver submission path.
#[derive(Debug, Default)]
struct NativeCancellation {
    calls: Mutex<BTreeMap<(u64, PluginId), Arc<CallControl>>>,
    newest_job: AtomicU64,
    #[cfg(test)]
    publication_pause: Mutex<Option<(SyncSender<()>, Receiver<()>)>>,
}

impl NativeCancellation {
    fn register(&self, job: u64, plugin: PluginId, control: Arc<CallControl>) {
        self.calls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert((job, plugin), Arc::clone(&control));
        // Submission can supersede a job before its dispatcher registers.
        if job < self.newest_job.load(Ordering::Acquire) {
            control.cancel();
        }
    }

    fn unregister(&self, generation: u64, plugin: &PluginId) {
        self.calls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&(generation, plugin.clone()));
    }

    fn cancel_before(&self, generation: u64) {
        self.newest_job.fetch_max(generation, Ordering::AcqRel);
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

/// Suggestions carry the identity of the child that actually produced them,
/// not whichever child happens to be current when the result is published.
#[derive(Debug)]
struct ProducedSuggestions {
    suggestions: Suggestions,
    incarnation: u64,
    generation: Generation,
}

#[derive(Debug)]
struct DispatchResult {
    generation: u64,
    plugin: PluginId,
    result: Result<ProducedSuggestions, HostError>,
    soft_timeout: bool,
}

#[derive(Debug)]
struct InFlightCall {
    job: u64,
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
    soft_timeouts: BTreeMap<PluginId, u32>,
    /// Plugins already reported as failed. A dead worker must not produce one
    /// diagnostic per later keystroke (contract §11.5).
    recorded: BTreeSet<PluginId>,
    /// Bounded with [`NATIVE_RESULT_CAPACITY`]; dispatchers apply backpressure
    /// rather than growing an unbounded queue.
    result_tx: SyncSender<DispatchResult>,
    result_rx: Receiver<DispatchResult>,
    in_flight: BTreeMap<(u64, PluginId), InFlightCall>,
    cancellation: Arc<NativeCancellation>,
    /// One lazily started child per plugin that serves resources, kept apart
    /// from `supervisors` on purpose: an icon is decoration and must never own
    /// the lock a query needs. Registration is lazy, so a plugin whose icons
    /// are never asked for pays for no second process.
    resource_supervisors: BTreeMap<WorkerKey, Arc<Mutex<NativeSupervisor>>>,
}

impl std::fmt::Debug for NativeWorkerPool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeWorkerPool")
            .field("supervisors", &self.supervisors.keys().collect::<Vec<_>>())
            .field("failures", &self.failures)
            .field("soft_timeouts", &self.soft_timeouts)
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
            soft_timeouts: BTreeMap::new(),
            recorded: BTreeSet::new(),
            result_tx,
            result_rx,
            in_flight: BTreeMap::new(),
            cancellation: Arc::new(NativeCancellation::default()),
            resource_supervisors: BTreeMap::new(),
        }
    }
}

impl NativeWorkerPool {
    /// Only one blocking call may own one plugin supervisor at a time.
    /// Cancelled calls retain ownership until their actual completion; newer
    /// admitted requests wait in the provider's bounded pending map.
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

    fn record_soft_timeout(&mut self, plugin: PluginId) {
        let count = self.soft_timeouts.entry(plugin).or_default();
        *count = count.saturating_add(1);
    }
    /// Removes one completed call and joins its short-lived dispatcher thread.
    fn finish_call(&mut self, generation: u64, plugin: &PluginId) {
        let key = (generation, plugin.clone());
        if let Some(mut call) = self.in_flight.remove(&key) {
            self.cancellation.unregister(call.job, &call.plugin);
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
            self.cancellation.unregister(call.job, &call.plugin);
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
        for supervisor in self
            .supervisors
            .values()
            .chain(self.resource_supervisors.values())
        {
            supervisor
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .shutdown_all();
        }
    }
}

// Why the resource child keeps the general startup budget.
//
// An earlier revision of this change capped it, on the reasoning that a fetch
// slot should not be held for the ten-second handshake allowance when
// `PLUGIN_ICON_DEADLINE` advertises 250 ms. The reasoning was wrong in a way
// worth recording so it is not repeated: startup time is per attempt, not
// additive across them. Each retry starts a fresh child and restarts the
// handshake, so retrying a four-second cap three times does not serve a child
// that needs seven seconds to come up — it fails three times and then
// memoizes the absence, losing icons on exactly the slow machines the old
// single ten-second attempt served.
//
// So the honest statement of the bound, which `MAX_CONCURRENT_FETCHES` is
// documented against, is: a fetch holds its slot for the worker startup
// budget plus `PLUGIN_ICON_DEADLINE`, and only the first fetch of a run pays
// the startup half. Making that genuinely 250 ms means taking the spawn off
// the fetch path — starting the resource child without occupying a slot and
// letting the retry find it warm — which is a change to the resolver seam
// rather than a constant, and is not attempted here.

/// Serves one native plugin's resources by asking a child of its own.
///
/// The supervisor here is deliberately NOT the one query dispatch uses. A
/// plugin has a whole [`PLUGIN_ICON_DEADLINE`] to answer a resource request
/// and a native child answers one request at a time, so sharing the query
/// worker would let a slow or silent icon hold the lock the next query needs
/// and leave the plugin contributing nothing inside that query's collection
/// window. Decoration therefore gets its own supervised process, exactly as a
/// catalog rebuild does. The blocking lock that remains only serializes this
/// plugin's own icon fetches against each other, which is what the concurrency
/// bound in `plugin_icons` already assumes.
#[derive(Debug)]
struct NativeResourceSource {
    plugin: PluginId,
    supervisor: Arc<Mutex<NativeSupervisor>>,
}

impl PluginResourceSource for NativeResourceSource {
    fn fetch(&self, reference: &str) -> IconFetch {
        let mut supervisor = self.supervisor.lock().unwrap_or_else(|error| error.into_inner());
        // Starting the child is part of the fetch, and it is the part
        // `PLUGIN_ICON_DEADLINE` does not bound — see the note above this
        // impl. A child that fails to come up is reported as unavailable
        // rather than absent, so the reference stays retryable instead of
        // being memoized away on one slow start.
        let Ok(worker) = supervisor.worker(&self.plugin, native_policy_now()) else {
            return IconFetch::Unavailable;
        };
        match worker.request_resource(
            ResourceKind::Icon,
            reference,
            PLUGIN_ICON_DEADLINE,
            MAX_PLUGIN_ICON_BYTES,
        ) {
            Ok(ResourceOutcome::Served(resource)) => IconFetch::Found(resource.content),
            // The plugin was reached and answered no. Asking again is waste.
            Ok(ResourceOutcome::Declined) => IconFetch::Absent,
            // No answer inside the deadline. Distinct from a decline on
            // purpose: a plugin busy with its first request, or a child that
            // has only just finished starting, answers the next attempt.
            Ok(ResourceOutcome::Silent) => IconFetch::Unavailable,
            // Transport fault. The supervisor will restart the child, and the
            // reference is worth asking again once it has.
            Err(_) => IconFetch::Unavailable,
        }
    }
}

/// Serves one plugin's page frames from the worker that opened the page.
///
/// Deliberately the query supervisor and not a child of its own, which is the
/// opposite of the choice [`NativeResourceSource`] makes. An icon is a pure
/// function of a reference and any child of that plugin can serve it; a page
/// is a conversation, and the state behind it — what the action opened, what
/// the user has typed into it since — lives in the process that ran the
/// action. Asking a second child would be asking a plugin that never opened
/// the page to draw it.
#[derive(Debug)]
struct NativePageSource {
    plugin: PluginId,
    supervisor: Arc<Mutex<NativeSupervisor>>,
    incarnations: NativeIncarnations,
    admission: Option<ForegroundActionAdmission>,
    gate: Arc<PreviewActionGate>,
}

impl PageBackend for NativePageSource {
    fn draw(&mut self, draw: &PageDraw) -> Result<crikey_core::PageFrame, String> {
        let admission = match self.admission.take() {
            Some(admission) => admission,
            None => self
                .gate
                .reserve_foreground()
                .map_err(|error| error.to_string())?,
        };
        let _guard = admission
            .acquire(
                Instant::now() + Duration::from_millis(NATIVE_CALL_TIMEOUT_MS),
                &AtomicBool::new(false),
            )
            .map_err(|error| error.to_string())?;
        let mut supervisor = self.supervisor.lock().unwrap_or_else(|error| error.into_inner());
        with_native_worker(&mut supervisor, &self.plugin, &self.incarnations, |worker| {
            worker.page(&NativePageRequest {
                page_id: draw.page_id.clone(),
                generation: draw.generation,
                width: draw.width,
                height: draw.height,
                events: draw.events.clone(),
                focused: true,
                // The palette is the launcher's, and the launcher's theme is
                // not visible from here. Zero states no colour rather than a
                // transparent one, so a plugin keeps its own defaults.
                colour_surface: draw.palette.surface.to_u32(),
                colour_text: draw.palette.text.to_u32(),
                colour_accent: draw.palette.accent.to_u32(),
                colour_muted: draw.palette.muted.to_u32(),
            })
        })
        .map_err(|error| error.to_string())
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
    /// Catalog-persistence declarations, keyed by plugin, recorded as soon as
    /// a manifest is parsed and independent of whether the plugin then loaded.
    catalog_declarations: BTreeMap<PluginId, bool>,
    /// Current suggestion snapshots and their producing worker connection,
    /// published together before their rows enter the query pipeline.
    action_items: SharedOriginItems,
    incarnations: NativeIncarnations,
    collection_window: Duration,
    /// Answers taken off the completion channel that have not been published
    /// yet, oldest first.
    ///
    /// A call that finishes after the collection window closes still owns an
    /// active pipeline request. Its answer waits here until a caller with a
    /// pipeline retires it. Bounded by the channel it comes from and emptied on
    /// every refresh tick, so it holds at most the calls of one generation.
    late: Vec<DispatchResult>,
    /// Admitted requests waiting for this plugin's physical callback to retire.
    pending: BTreeMap<PluginId, Generation>,
    /// UI submission identity and its independently minted pipeline generation.
    binding: Option<(Generation, Generation)>,
    scope: Option<crate::plugin_action::RootScope>,
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
    pub fn load(pipeline: &mut QueryPipeline, roots: &[PathBuf], disabled: &DisabledPlugins) -> Self {
        Self::load_with_collection_window(pipeline, roots, DEFAULT_COLLECTION_WINDOW, disabled)
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
        disabled: &DisabledPlugins,
    ) -> Self {
        let mut provider = Self {
            catalog_declarations: BTreeMap::new(),
            pool: NativeWorkerPool::default(),
            loaded: Vec::new(),
            catalog: NativeCatalogDispatcher::default(),
            plugins: Vec::new(),
            unavailable: Vec::new(),
            action_items: Arc::new(Mutex::new(BTreeMap::new())),
            incarnations: Arc::new(Mutex::new(NativeIncarnationState::default())),
            collection_window,
            late: Vec::new(),
            pending: BTreeMap::new(),
            binding: None,
            scope: None,
        };
        provider.catalog.incarnations = Arc::clone(&provider.incarnations);

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
                provider.register_plugin_dir(pipeline, &directory, disabled);
            }
        }

        provider.install_icon_resolver(pipeline);
        provider
    }

    /// Gives the pipeline a resolver that can reach every worker that started.
    ///
    /// Installed after discovery rather than per package: the resolver is
    /// shared immutably once the pipeline holds it, and a package that failed
    /// to load has no worker to ask for an icon.
    ///
    /// Each served origin is backed by a second supervisor built from the same
    /// immutable launch recipe, and the same worker options, that the catalog
    /// path uses. `register`
    /// starts no process, so this costs a plugin nothing until one of its
    /// icons is actually asked for; a registration that is refused leaves the
    /// plugin with no origin, which resolves to no icon rather than to a
    /// shared lock.
    fn install_icon_resolver(&mut self, pipeline: &mut QueryPipeline) {
        // Disjoint field borrows: the recipes are read while the pool that
        // retains the new supervisors is written.
        let Self { pool, loaded, .. } = self;
        let mut resolver = PluginIconResolver::default();
        for entry in loaded.iter() {
            if !pool.supervisors.contains_key(&entry.key) {
                continue;
            }
            let mut supervisor = NativeSupervisor::new(SupervisorConfig::default());
            if supervisor
                .register(entry.launch.clone(), entry.worker_options.clone())
                .is_err()
            {
                continue;
            }
            // One worker key belongs to one package directory, so this is
            // always a fresh entry and the provider is its only owner besides
            // the resolver it hands to the pipeline.
            let supervisor = Arc::new(Mutex::new(supervisor));
            pool.resource_supervisors
                .insert(entry.key.clone(), Arc::clone(&supervisor));
            resolver.insert_served(
                &entry.plugin,
                Arc::new(NativeResourceSource {
                    plugin: entry.plugin.clone(),
                    supervisor,
                }),
            );
        }
        pipeline.set_plugin_icons(Arc::new(resolver));
    }

    /// Loads one candidate package, starts its worker, and registers the
    /// namespaced plugin with the manifest-derived scheduling policy. No
    /// failure here can abort sibling discovery (spec 24.1).
    fn register_plugin_dir(
        &mut self,
        pipeline: &mut QueryPipeline,
        directory: &Path,
        disabled: &DisabledPlugins,
    ) {
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

        // This provider owns native packages and the `c-abi` and `wasm`
        // packages that `crikey-cabi-host` and `crikey-wasm-host` serve on
        // their behalf. All three are supervised executables speaking the
        // native protocol, so they share one worker, one supervisor and one
        // teardown; only the launch recipe differs (ADR-0014, ADR-0015).
        // Other runtimes remain for their respective providers.
        let runtime = manifest.plugin.runtime;
        if !matches!(runtime, Runtime::Native | Runtime::CAbi | Runtime::Wasm) {
            return;
        }

        // One namespace for all three: a `c-abi` or `wasm` package installs
        // under the same `native` plugin root and is addressed by the same id
        // the CLI prints.
        let plugin = PluginId(format!("native.{}", manifest.plugin.id));
        // Recorded the moment the manifest is understood, and deliberately not
        // from `loaded`: a refusal has to withdraw an earlier run's slice even
        // when this run never gets the plugin started (spec 22.1).
        self.catalog_declarations
            .insert(plugin.clone(), manifest.catalog.persist);
        // Held back before the worker process is spawned: an operator who
        // disabled a plugin must not pay for its process, and the only proof
        // that it did not run is that nothing started it (spec 21.2).
        if disabled.blocks(&plugin) {
            self.record_unavailable(package, Some(plugin), DISABLED_BY_CONFIGURATION.to_owned());
            return;
        }
        let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
        let suggest_timeout_ms = manifest
            .performance
            .suggest_hard_timeout_ms
            .min(NATIVE_CALL_TIMEOUT_MS);
        let (executable, arguments, environment) = match runtime {
            Runtime::CAbi => {
                // The launcher never resolves the shared library, let alone
                // loads it: it starts the host and hands over the package
                // directory.
                match crate::cabi_provider::launch_recipe(&manifest, directory, os, arch) {
                    Ok((executable, arguments)) => (executable, arguments, Vec::new()),
                    Err(reason) => {
                        self.record_unavailable(package, Some(plugin), reason);
                        return;
                    }
                }
            }
            Runtime::Wasm => {
                // The launcher never instantiates the module: it starts the
                // host, names the module and states what the manifest granted.
                // The enforced deadline travels with it, because fuel metering
                // inside the guest is what interrupts a spinning module.
                match crate::wasm_provider::launch_recipe(&manifest, directory, os, arch, suggest_timeout_ms)
                {
                    Ok(recipe) => recipe,
                    Err(reason) => {
                        self.record_unavailable(package, Some(plugin), reason);
                        return;
                    }
                }
            }
            _ => {
                let entrypoint = match manifest.entrypoint_for(os, arch) {
                    Ok(entrypoint) => entrypoint.to_owned(),
                    Err(error) => {
                        self.record_unavailable(
                            package,
                            Some(plugin),
                            format!("no usable entrypoint: {error}"),
                        );
                        return;
                    }
                };

                // Manifest entrypoints are paths, not shell command lines.
                // Resolve a relative path within the package and preserve
                // absolute paths exactly; never whitespace-split an entrypoint
                // (contract §11.1).
                let entrypoint_path = Path::new(&entrypoint);
                let executable = if entrypoint_path.is_absolute() {
                    entrypoint_path.to_path_buf()
                } else {
                    directory.join(entrypoint_path)
                };
                if !executable.is_file() {
                    self.record_unavailable(
                        package,
                        Some(plugin),
                        format!("native entrypoint is not a file: {}", executable.display()),
                    );
                    return;
                }
                (executable, Vec::new(), Vec::new())
            }
        };

        // Source directory identity is load-bearing even when entrypoint paths
        // are identical. One package gets one worker; no worker is shared
        // across distinct package directories. For `c-abi` and `wasm` packages
        // the executable is the same host for every one of them, so the
        // directory is the only thing that distinguishes their workers.
        let key: WorkerKey = (
            executable.to_string_lossy().into_owned(),
            directory.to_string_lossy().into_owned(),
        );
        let launch = LaunchSpec {
            plugin: plugin.clone(),
            executable,
            arguments,
            working_dir: Some(directory.to_path_buf()),
            environment,
            inherit_environment: manifest.permissions.environment,
        };
        let soft_timeout = Duration::from_millis(
            manifest
                .performance
                .suggest_soft_timeout_ms
                .min(suggest_timeout_ms),
        );
        let mut options = WorkerOptions::new().with_call_timeout_ms(suggest_timeout_ms);
        // A native, WASM or C-ABI plugin runs from a read-only package
        // directory and is given no writable location of its own, so the
        // policy is scratch space and the usual device files. An undeclared
        // network permission becomes a kernel refusal instead of a line in a
        // manifest nothing enforces (spec 20.2).
        options.sandbox =
            crikey_sandbox::plugin_policy(Vec::<std::path::PathBuf>::new(), !manifest.permissions.network);

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
        // `register` is intentionally lazy. Eager manifests pay the startup
        // handshake here; lazy manifests start on their first query.
        if manifest.performance.startup == Startup::Eager {
            if let Err(error) = supervisor.worker(&plugin, native_policy_now()) {
                self.record_unavailable(
                    package,
                    Some(plugin.clone()),
                    format!("native worker did not start: {error}"),
                );
                let _ = pipeline.unregister_plugin(&plugin);
                return;
            }
        }
        refresh_incarnation(&mut supervisor, &plugin, &self.incarnations);
        let supervisor = Arc::new(Mutex::new(supervisor));
        self.pool.supervisors.insert(key.clone(), supervisor);

        self.plugins.push(plugin.clone());
        self.loaded.push(LoadedPlugin {
            plugin,
            key,
            launch,
            worker_options: options,
            action_gate: PreviewActionGate::new(Arc::clone(&budget)),
            budget,
            soft_timeout,
            catalog_persist: manifest.catalog.persist,
            permissions: manifest.permissions,
        });
    }

    /// Delivers each loaded plugin its own complete configuration state (spec 21.4).
    ///
    /// Returns one `(plugin, reason)` per plugin that could not be reached, so a
    /// dead or refusing worker is a named diagnostic rather than a configuration
    /// change that silently did nothing. Delivery continues past a failure: one
    /// broken plugin must not stop the others from being configured.
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
            let values = configuration.get(&plugin).unwrap_or(&empty);
            let Some(supervisor) = self.pool.supervisors.get(&key).cloned() else {
                failures.push((plugin, "native worker is unavailable".to_owned()));
                continue;
            };
            // Caught rather than propagated for the same reason every other
            // dispatch seam here catches: a panic inside one plugin's transport
            // must not take down the supervisor thread publishing to the rest.
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                let mut supervisor = supervisor.lock().unwrap_or_else(|error| error.into_inner());
                with_native_worker(&mut supervisor, &plugin, &self.incarnations, |worker| {
                    worker.send_configuration(values, true)
                })
                .map_err(|error| error.to_string())
            }));
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(reason)) => {
                    let reason = format!("configuration delivery failed: {reason}");
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

    fn record_unavailable(&mut self, package: String, plugin: Option<PluginId>, reason: String) {
        self.unavailable.push(NativeUnavailable {
            package,
            plugin,
            reason,
        });
    }

    /// Manifest grants used by the host-mediated action boundary.
    pub fn permissions(&self) -> BTreeMap<PluginId, Permissions> {
        self.loaded
            .iter()
            .map(|loaded| (loaded.plugin.clone(), loaded.permissions.clone()))
            .collect()
    }
    /// The namespaced native plugins that loaded successfully.
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
    /// Saturating counts of suggestion callbacks that exceeded each manifest's
    /// soft deadline.
    pub fn soft_timeouts(&mut self) -> &BTreeMap<PluginId, u32> {
        self.drain_completed_results();
        &self.pool.soft_timeouts
    }

    /// Starts one bounded catalog rebuild for an exactly owned plugin.
    ///
    /// Catalog callbacks share the plugin's supervised child with suggestions
    /// and previews. The background task holds its supervisor for the callback.
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
        let supervisor = Arc::clone(self.pool.supervisors.get(&loaded.key).ok_or_else(|| {
            CatalogDispatchError::UnknownPlugin {
                plugin: plugin.clone(),
            }
        })?);
        let incarnations = Arc::clone(&self.incarnations);
        let publications = self
            .catalog
            .commands
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let full = publications.revision == u64::MAX
            || (!publications.latest.contains_key(plugin) && publications.latest.len() >= MAX_COMMANDS)
            || (!publications.owners.contains_key(plugin) && publications.owners.len() >= MAX_COMMANDS);
        drop(publications);
        if full || self.catalog.next_id == u64::MAX || self.catalog.tasks.len() >= CATALOG_RESULT_CAPACITY {
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
        let catalog_persist = loaded.catalog_persist;
        let plugin_for_thread = plugin.clone();
        let thread_plugin = plugin.clone();
        let join = thread::Builder::new()
            .name(format!("crikey-native-catalog-{}", plugin.0))
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let mut supervisor = supervisor.lock().unwrap_or_else(|error| error.into_inner());
                    let was_live = supervisor.poll_live_connection(&plugin_for_thread).is_some();
                    let result =
                        with_native_worker(&mut supervisor, &plugin_for_thread, &incarnations, |worker| {
                            let incarnation = worker.connection_id();
                            let supports_preview = worker.handshake().preview;
                            let mut items = worker.build_catalog_with_timeout_ms(NATIVE_CALL_TIMEOUT_MS)?;
                            let presentation = worker.catalog_presentation().cloned().unwrap_or_default();
                            for item in &mut items {
                                item.plugin_id = plugin_for_thread.clone();
                            }
                            // Still holding this exact supervisor/worker after
                            // its complete catalog; no other call can replace it.
                            let commands =
                                native_commands(&plugin_for_thread, &items, worker.commands(request_id));
                            Ok((items, incarnation, supports_preview, commands, presentation))
                        })
                        .map_err(|error| bounded_command_error(error.to_string()));
                    let keep_origin = result.as_ref().is_ok_and(|(items, _, supported, _, _)| {
                        *supported
                            && items.iter().any(|item| {
                                item.metadata.get("crikey.preview").map(String::as_str) == Some("deferred")
                            })
                    });
                    // Only opted-in deferred catalogs make a newly started
                    // catalog producer resident; static catalogs still release.
                    if !was_live && !keep_origin {
                        supervisor.release_worker(&plugin_for_thread, native_policy_now());
                        refresh_incarnation(&mut supervisor, &plugin_for_thread, &incarnations);
                    }
                    result.map(|(items, incarnation, _, commands, presentation)| {
                        (
                            items,
                            if keep_origin { incarnation } else { 0 },
                            NativeCommandSnapshot {
                                plugin: plugin_for_thread.clone(),
                                revision: 0,
                                instance,
                                generation,
                                producer: incarnation,
                                released: !was_live && !keep_origin,
                                status: commands.0,
                                declarations: commands.1,
                            },
                            presentation,
                        )
                    })
                }));
                let incarnation = result
                    .as_ref()
                    .ok()
                    .and_then(|result| result.as_ref().ok())
                    .map_or(0, |(_, incarnation, _, _)| *incarnation);
                let mut commands = NativeCommandSnapshot {
                    plugin: thread_plugin.clone(),
                    revision: 0,
                    instance,
                    generation,
                    producer: 0,
                    released: false,
                    status: NativeCommandStatus::Failed("native catalog worker panicked".into()),
                    declarations: Arc::from([]),
                };
                let mut presentation = ResultPresentation::default();
                let completion = match result {
                    Ok(Ok((items, _, declaration_snapshot, sidecar))) => {
                        presentation = sidecar;
                        commands = declaration_snapshot;
                        CatalogBuildResult::Complete(crate::CatalogBuild {
                            plugin: plugin_for_thread,
                            instance,
                            generation,
                            items,
                            persist: catalog_persist,
                        })
                    }
                    Ok(Err(reason)) => {
                        commands.status = NativeCommandStatus::Failed(reason.clone());
                        CatalogBuildResult::Failed {
                            plugin: thread_plugin.clone(),
                            instance,
                            generation,
                            reason: format!("native catalog build failed: {reason}"),
                        }
                    }
                    Err(_) => CatalogBuildResult::Failed {
                        plugin: thread_plugin,
                        instance,
                        generation,
                        reason: "native catalog worker panicked".to_owned(),
                    },
                };
                let _ = sender.send((request_id, completion, incarnation, commands, presentation));
                drop(guard);
            })
            .map_err(|error| CatalogDispatchError::ThreadSpawn {
                plugin: plugin.clone(),
                reason: error.to_string(),
            })?;
        self.catalog.insert(request_id, plugin.clone(), join);
        Ok(request_id)
    }

    /// Latest bounded per-owner publication, with no supervisor access or work.
    /// Drain catalog results first. Ready declarations require matching catalog
    /// admission; empty terminal states revoke even if the catalog failed.
    pub fn command_snapshot(&self, plugin: &PluginId) -> Option<Arc<NativeCommandSnapshot>> {
        self.catalog
            .commands
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .owners
            .get(plugin)
            .cloned()
    }

    /// Package disable/removal or host catalog invalidation retires executable
    /// declarations and fences already admitted catalog/declaration responses.
    /// A later explicit catalog build may establish a fresh publication.
    pub fn invalidate_commands(&mut self, plugin: &PluginId) {
        self.catalog
            .commands
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .invalidate(plugin);
        self.catalog
            .ledger
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .invalidate(plugin);
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

    fn action_gates(&self) -> BTreeMap<PluginId, Arc<PreviewActionGate>> {
        self.loaded
            .iter()
            .map(|loaded| (loaded.plugin.clone(), Arc::clone(&loaded.action_gate)))
            .collect()
    }

    /// Clones the query supervisor of every loaded plugin, keyed by plugin.
    ///
    /// A page is served by the worker that ran the action which opened it, so
    /// this is the same supervisor suggestion dispatch uses. The page session
    /// takes the lock per frame request, exactly as an action does, which is
    /// what keeps a page and a query from talking to one child at once.
    fn page_supervisors(&self) -> BTreeMap<PluginId, Arc<Mutex<NativeSupervisor>>> {
        self.loaded
            .iter()
            .filter_map(|loaded| {
                let supervisor = self.pool.supervisors.get(&loaded.key)?;
                Some((loaded.plugin.clone(), Arc::clone(supervisor)))
            })
            .collect()
    }

    /// Validates and executes one admitted plugin action on the exact native
    /// worker that owns the item's plugin id.
    fn execute_action_request(
        &mut self,
        mut request: NativeActionRequest,
    ) -> crikey_core::Result<ActionEffect> {
        let _guard = request.admission.acquire(request.deadline, &request.cancelled)?;
        let plugin = request.plugin;
        // Admission froze both validation and privacy evidence for this item.
        // Current ownership/liveness still come from the loaded runtime below;
        // a later result publication must not replace the admitted content.
        let item = &request.item;
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
        request.work.validate(item, request.argument.as_deref())?;
        if let Some(binding) = request.work.typed_binding().cloned() {
            if self
                .catalog
                .ledger
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .frozen_item(&binding)
                .is_none()
            {
                return Err(crikey_core::CoreError::Invalid(
                    "typed native source retired".into(),
                ));
            }
            let supervisor =
                self.pool.supervisors.get(&loaded.key).cloned().ok_or_else(|| {
                    crikey_core::CoreError::Invalid("typed native worker unavailable".into())
                })?;
            let mut supervisor = supervisor.lock().unwrap_or_else(|error| error.into_inner());
            let mut execute = |worker: &mut crikey_native_host::NativeWorker| {
                // Physical-worker schema discovery can block (including a
                // released static catalog's healthy reacquisition). Complete
                // it before the final provider-side admission checks so no
                // submitted values cross a retired authority boundary.
                let confirmed = match worker.prepare_typed_execution(
                    &item.stable_id,
                    binding.executable.action.as_ref(),
                    binding.fingerprint,
                )? {
                    crikey_native_host::TypedPreparation::Ready(confirmed) => confirmed,
                    crikey_native_host::TypedPreparation::Refused(code) => {
                        return Ok(crikey_core::structured_arguments::SubmissionOutcome::Failed(code));
                    }
                };
                if self
                    .catalog
                    .ledger
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .frozen_item(&binding)
                    .is_none()
                {
                    return Err(HostError::PluginFailed {
                        plugin: plugin.clone(),
                        detail: "typed source retired".into(),
                    });
                }
                if let RootSource::Suggestion { generation, .. } = &binding.source {
                    if request.current_generation.load(Ordering::Acquire) != generation.get() {
                        return Err(HostError::PluginFailed {
                            plugin: plugin.clone(),
                            detail: "typed generation retired".into(),
                        });
                    }
                }
                if request.cancelled.load(Ordering::Acquire) || Instant::now() >= request.deadline {
                    return Err(HostError::PluginFailed {
                        plugin: plugin.clone(),
                        detail: "typed execution cancelled".into(),
                    });
                }
                let values = request
                    .work
                    .take_typed_values()
                    .expect("typed work owns submitted values");
                worker.execute_typed_confirmed(confirmed, values)
            };
            let outcome = if let Some(origin) = request.origin {
                refresh_incarnation(&mut supervisor, &plugin, &self.incarnations);
                let worker = supervisor
                    .live_worker(&plugin)
                    .filter(|worker| worker.connection_id() == origin)
                    .ok_or_else(|| {
                        crikey_core::CoreError::Invalid("typed native producing worker retired".into())
                    })?;
                let outcome = execute(worker);
                refresh_incarnation(&mut supervisor, &plugin, &self.incarnations);
                outcome
            } else {
                with_native_worker(&mut supervisor, &plugin, &self.incarnations, execute)
            };
            return outcome
                .map(ActionEffect::Typed)
                .map_err(|_| crikey_core::CoreError::Invalid("typed native execution failed".into()));
        }
        let validate_command = || -> crikey_core::Result<()> {
            if let Some((declaration, source)) = request.work.command_source() {
                let publications = self
                    .catalog
                    .commands
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let current = publications
                    .owners
                    .get(&plugin)
                    .filter(|current| {
                        current.status == NativeCommandStatus::Ready
                            && current.revision == source.publication
                            && current.instance == source.instance
                            && current.generation == source.generation
                            && current.producer == source.producer
                            && current.declarations.contains(declaration)
                    })
                    .ok_or_else(|| {
                        crikey_core::CoreError::Invalid(
                            "declared native command source is no longer current".into(),
                        )
                    })?;
                let items = self
                    .catalog
                    .items
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let static_items = self
                    .catalog
                    .static_items
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let (canonical, _) = native_preview::canonical_preview_item(
                    &publications,
                    &items,
                    &static_items,
                    &plugin,
                    &item.stable_id,
                    source,
                )
                .ok_or_else(|| {
                    crikey_core::CoreError::Invalid("canonical command item is unavailable".into())
                })?;
                request.work.validate(canonical, request.argument.as_deref())?;
                if canonical.plugin_id != item.plugin_id
                    || canonical.stable_id != item.stable_id
                    || canonical.target != item.target
                    || canonical.argument_policy != item.argument_policy
                    || canonical.category != item.category
                    || current.plugin != plugin
                {
                    return Err(crikey_core::CoreError::Invalid(
                        "canonical native command target changed".into(),
                    ));
                }
            }
            Ok(())
        };

        let supervisor = self.pool.supervisors.get(&loaded.key).cloned().ok_or_else(|| {
            crikey_core::CoreError::Invalid(format!("native worker for `{}` is unavailable", plugin.0))
        })?;
        let mut supervisor = supervisor.lock().unwrap_or_else(|error| error.into_inner());
        if request.cancelled.load(Ordering::Acquire) {
            return Err(crikey_core::CoreError::Cancelled);
        }
        if Instant::now() >= request.deadline {
            return Err(crikey_core::CoreError::Invalid(
                "native action timed out before execution".into(),
            ));
        }
        let execute = |worker: &mut crikey_native_host::NativeWorker| {
            if request.cancelled.load(Ordering::Acquire) {
                return Err(HostError::PluginFailed {
                    plugin: plugin.clone(),
                    detail: "action cancelled before execution".into(),
                });
            }
            if Instant::now() >= request.deadline {
                return Err(HostError::Timeout {
                    plugin: plugin.clone(),
                    detail: "action timed out before execution".into(),
                });
            }
            validate_command().map_err(|error| HostError::PluginFailed {
                plugin: plugin.clone(),
                detail: error.to_string(),
            })?;
            worker.execute(
                &item.stable_id,
                request.work.action_id(),
                request.argument.as_deref(),
            )
        };
        let outcome = if let Some(origin) = request.origin {
            refresh_incarnation(&mut supervisor, &plugin, &self.incarnations);
            let worker = supervisor.live_worker(&plugin).ok_or_else(|| {
                crikey_core::CoreError::Invalid("native action originating worker is unavailable".into())
            })?;
            if worker.connection_id() != origin {
                return Err(crikey_core::CoreError::Invalid(
                    "native action originating worker was replaced".into(),
                ));
            }
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                execute(worker).map_err(|error| error.to_string())
            }));
            refresh_incarnation(&mut supervisor, &plugin, &self.incarnations);
            outcome
        } else {
            catch_unwind(AssertUnwindSafe(|| {
                with_native_worker(&mut supervisor, &plugin, &self.incarnations, execute)
                    .map_err(|error| error.to_string())
            }))
        };
        drop(supervisor);
        match outcome {
            Ok(Ok(ExecuteOutcome::Ok)) => Ok(ActionEffect::Completed),
            Ok(Ok(ExecuteOutcome::ShowPage { page_id })) => Ok(ActionEffect::ShowPage { page_id }),
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
        let started = Instant::now();
        let frame = self.begin_query(pipeline, query, now, None);
        // The synchronous entrypoint waits out an actual debounce deadline.
        // The asynchronous driver instead resumes this generation on refresh.
        let Some(next) = pipeline.next_wakeup().filter(|next| *next > now) else {
            return frame;
        };
        let deadline = Duration::from_millis(next.saturating_sub(now));
        thread::sleep(deadline.saturating_sub(started.elapsed()));
        let elapsed = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let generation = self.binding.expect("begin_query binds its generation").1;
        self.drive_current(pipeline, query, generation, now.saturating_add(elapsed))
            .or(frame)
    }

    fn begin_query(
        &mut self,
        pipeline: &mut QueryPipeline,
        query: &str,
        now: Millis,
        job: Option<Generation>,
    ) -> Option<ViewModel> {
        self.catalog
            .ledger
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retire_suggestions();
        let generation = pipeline.keystroke_scoped(query, now, self.scope.clone());
        self.action_items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        for (plugin, obsolete) in std::mem::take(&mut self.pending) {
            let _ = pipeline.cancel_request(&plugin, obsolete, now);
        }
        let job = job.unwrap_or(generation);
        self.binding = Some((job, generation));
        self.pool.cancel_before(job.get());
        self.drive_current(pipeline, query, generation, now)
    }

    /// Advances existing work, without minting a generation or advancing time.
    fn drive_current(
        &mut self,
        pipeline: &mut QueryPipeline,
        query: &str,
        generation: Generation,
        now: Millis,
    ) -> Option<ViewModel> {
        self.retire_late_answers(pipeline, now);
        let tick = pipeline.tick(now);
        for cancellation in tick.cancellations {
            let key = (cancellation.generation.get(), cancellation.plugin.clone());
            // Cancellation is a notification, not callback retirement.
            if !self.pool.in_flight.contains_key(&key) {
                if self.pending.get(&cancellation.plugin) == Some(&cancellation.generation) {
                    self.pending.remove(&cancellation.plugin);
                }
                let _ = pipeline.cancel_request(&cancellation.plugin, cancellation.generation, now);
            }
        }
        for request in tick.dispatches {
            if request.generation == generation {
                self.pending.insert(request.plugin, request.generation);
            } else {
                let _ = pipeline.cancel_request(&request.plugin, request.generation, now);
            }
        }
        let ready = self
            .pending
            .iter()
            .filter(|(plugin, pending)| **pending == generation && !self.pool.has_in_flight(plugin))
            .map(|(plugin, _)| plugin.clone())
            .collect::<BTreeSet<_>>();
        if !ready.is_empty() {
            let CollectedSuggestions {
                mut by_plugin,
                dead_plugins,
                outstanding,
            } = self.collect_suggestions(query, generation, &ready);
            for plugin in ready {
                self.pending.remove(&plugin);
                if dead_plugins.contains(&plugin) {
                    let _ = pipeline.fail_request(&plugin, generation, now);
                } else if !outstanding.contains(&plugin) {
                    let _ = pipeline.deliver(
                        ResultBatch {
                            generation,
                            plugin: plugin.clone(),
                            state: BatchState::Final,
                            items: by_plugin.remove(&plugin).unwrap_or_default(),
                        },
                        now,
                    );
                    let _ = pipeline.complete(&plugin, generation, now);
                }
            }
        }
        self.retire_late_answers(pipeline, now);
        pipeline
            .present(now)
            .filter(|frame| frame.generation == generation)
    }

    /// Cancels outstanding calls, joins their dispatch threads, and asks every
    /// registered supervisor to reap its child (spec 24.3).
    pub fn shutdown(&mut self, _now: Millis) {
        for result in self.catalog.shutdown() {
            if let CatalogBuildResult::Failed { plugin, reason, .. } = result {
                self.pool.record_dispatch_failure(plugin, reason);
            }
        }
        let mut publications = self
            .catalog
            .commands
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for plugin in &self.plugins {
            publications.invalidate(plugin);
            self.catalog
                .ledger
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .invalidate(plugin);
        }
        drop(publications);
        self.pool.shutdown();
        self.incarnations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        // Joining every dispatcher makes all of its outcomes observable on
        // the channel. Consume them before teardown returns so late successes
        // are retired and late failures cannot disappear with the provider.
        self.drain_completed_results();
        self.discard_buffered_answers();
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
        requested_plugins: &BTreeSet<PluginId>,
    ) -> CollectedSuggestions {
        let generation_value = generation.get();
        let job = self
            .binding
            .filter(|(_, pipeline)| *pipeline == generation)
            .map_or(generation_value, |(job, _)| job.get());
        self.drain_completed_results();
        let request = NativeSuggestRequest {
            generation: generation_value,
            text: query.to_owned(),
            normalized: query.to_owned(),
            selected_item_id: None,
            selected_scope_id: self.scope.as_ref().map(|scope| scope.id.0.clone()),
        };
        let targets: Vec<(PluginId, WorkerKey, Duration)> = self
            .loaded
            .iter()
            .filter(|loaded| requested_plugins.contains(&loaded.plugin))
            .map(|loaded| (loaded.plugin.clone(), loaded.key.clone(), loaded.soft_timeout))
            .collect();
        let mut pending = 0usize;

        let mut dead_plugins = BTreeSet::new();
        // Only physically available plugins are admitted by `drive_current`.
        let mut dispatched = BTreeSet::new();
        for (plugin, key, soft_timeout) in targets {
            dispatched.insert(plugin.clone());
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
            let incarnations = Arc::clone(&self.incarnations);
            let scope = self.scope.clone();
            let ledger = Arc::clone(&self.catalog.ledger);
            self.pool
                .cancellation
                .register(job, plugin.clone(), Arc::clone(&control));
            let join = thread::Builder::new()
                .name(format!("crikey-native-{}", plugin.0))
                .spawn(move || {
                    let mut soft_timeout_elapsed = false;
                    let result = {
                        let mut supervisor = supervisor.lock().unwrap_or_else(|error| error.into_inner());
                        let mut suggest = |worker: &mut crikey_native_host::NativeWorker| {
                            if let Some(scope) = &scope {
                                let current = ledger
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .scope_current(scope)
                                    && match &scope.source {
                                        RootSource::Suggestion { producer, .. } => {
                                            worker.connection_id() == *producer
                                        }
                                        RootSource::Catalog(_) => true,
                                    };
                                if !current {
                                    return Err(HostError::PluginFailed {
                                        plugin: plugin_for_thread.clone(),
                                        detail: "native scope retired".into(),
                                    });
                                }
                            }
                            if request_for_thread.selected_scope_id.is_some()
                                && !worker.handshake().result_presentation
                            {
                                return Err(HostError::PluginFailed {
                                    plugin: plugin_for_thread.clone(),
                                    detail: "native scoped queries unsupported".into(),
                                });
                            }
                            control_for_thread.install(worker.cancel_handle());
                            let watch_control = Arc::clone(&control_for_thread);
                            let watcher = thread::Builder::new()
                                .name(format!("crikey-native-cancel-{}", plugin_for_thread.0))
                                .spawn(move || watch_control.watch_cancel());
                            let started = Instant::now();
                            let incarnation = worker.connection_id();
                            let result =
                                worker
                                    .suggest(&request_for_thread)
                                    .map(|suggestions| ProducedSuggestions {
                                        suggestions,
                                        incarnation,
                                        generation: Generation::from_raw(job),
                                    });
                            soft_timeout_elapsed = started.elapsed() > soft_timeout;
                            control_for_thread.finish();
                            if let Ok(watcher) = watcher {
                                let _ = watcher.join();
                            }
                            result
                        };
                        if matches!(
                            scope.as_ref().map(|scope| &scope.source),
                            Some(RootSource::Suggestion { .. })
                        ) {
                            refresh_incarnation(&mut supervisor, &plugin_for_thread, &incarnations);
                            let result = match supervisor.live_worker(&plugin_for_thread) {
                                Some(worker) => suggest(worker),
                                None => Err(HostError::PluginFailed {
                                    plugin: plugin_for_thread.clone(),
                                    detail: "native scope producer retired".into(),
                                }),
                            };
                            refresh_incarnation(&mut supervisor, &plugin_for_thread, &incarnations);
                            result
                        } else {
                            with_native_worker(&mut supervisor, &plugin_for_thread, &incarnations, suggest)
                        }
                    };
                    let _ = sender.send(DispatchResult {
                        generation: generation_value,
                        plugin: plugin_for_thread,
                        result,
                        soft_timeout: soft_timeout_elapsed,
                    });
                });
            let join = match join {
                Ok(join) => join,
                Err(error) => {
                    self.pool.cancellation.unregister(job, &plugin);
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
                    job,
                    plugin,
                    join: Some(join),
                },
            );
            pending += 1;
        }

        let mut by_plugin = BTreeMap::new();
        let mut answered = BTreeSet::new();
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
            if completion.soft_timeout {
                self.pool.record_soft_timeout(plugin.clone());
            }
            if !is_current {
                self.late.push(completion);
                continue;
            }
            pending = pending.saturating_sub(1);
            answered.insert(plugin.clone());
            let failed =
                self.apply_dispatch_result(completion.plugin, completion.result, Some(&mut by_plugin));
            if failed {
                dead_plugins.insert(plugin);
            }
        }

        // Dispatched, not answered by the time the window closed, and not
        // already declared dead: those are the calls still genuinely running.
        let outstanding = dispatched
            .into_iter()
            .filter(|plugin| !answered.contains(plugin) && !dead_plugins.contains(plugin))
            .collect();
        CollectedSuggestions {
            by_plugin,
            dead_plugins,
            outstanding,
        }
    }
    /// Retires every completion that has reached the channel. Results that
    /// arrive outside the presentation window are intentionally not rendered,
    /// but failures are still recorded and every dispatcher is joined.
    fn drain_completed_results(&mut self) {
        while let Ok(completion) = self.pool.result_rx.try_recv() {
            self.pool.finish_call(completion.generation, &completion.plugin);
            if completion.soft_timeout {
                self.pool.record_soft_timeout(completion.plugin.clone());
            }
            // Buffered, not applied. This runs where there may be no pipeline to
            // publish to, and applying an answer with nowhere to put it drops
            // its rows while leaving its request active -- the exact shape of a
            // spinner that never stops. `retire_late_answers` empties this.
            self.late.push(completion);
        }
    }

    /// Retires answers that arrived after the collection window closed, handing
    /// their rows to `pipeline` and completing the requests they belong to.
    ///
    /// [`drive_query`](Self::drive_query) waits `DEFAULT_COLLECTION_WINDOW` and
    /// no longer, so a slower plugin's request is still active when the frame
    /// for that keystroke is published. The driver's refresh tick is the only
    /// thing still running for that generation; this is where the answer
    /// becomes rows the user can see.
    fn retire_late_answers(&mut self, pipeline: &mut QueryPipeline, now: Millis) {
        self.drain_completed_results();
        for completion in std::mem::take(&mut self.late) {
            let plugin = completion.plugin.clone();
            let generation = Generation::from_raw(completion.generation);
            let mut collected = BTreeMap::new();
            let output = (pipeline.visible_generation() == Some(generation)).then_some(&mut collected);
            let dead = self.apply_dispatch_result(plugin.clone(), completion.result, output);
            if dead {
                // The call failed outright, so the request is abandoned rather
                // than answered: what a dead plugin gets inside the window too.
                let _ = pipeline.fail_request(&plugin, generation, now);
                continue;
            }
            let items = collected.remove(&plugin).unwrap_or_default();
            let _ = pipeline.deliver(
                ResultBatch {
                    generation,
                    plugin: plugin.clone(),
                    state: BatchState::Final,
                    items,
                },
                now,
            );
            let _ = pipeline.complete(&plugin, generation, now);
        }
    }

    /// Applies buffered answers with nowhere to publish them, at teardown.
    ///
    /// Failure diagnostics are what survive; nobody can be shown rows from
    /// here, and there is no longer a generation to retire them under.
    fn discard_buffered_answers(&mut self) {
        for completion in std::mem::take(&mut self.late) {
            self.apply_dispatch_result(completion.plugin, completion.result, None);
        }
    }

    fn apply_dispatch_result(
        &mut self,
        plugin: PluginId,
        result: Result<ProducedSuggestions, HostError>,
        output: Option<&mut BTreeMap<PluginId, Vec<Item>>>,
    ) -> bool {
        match result {
            Ok(ProducedSuggestions { suggestions, .. })
                if matches!(suggestions.state, crikey_native_host::BatchState::Failed) =>
            {
                let reason = suggestions
                    .error
                    .map(|error| error.message)
                    .filter(|message| !message.is_empty())
                    .unwrap_or_else(|| "the native plugin reported a failure".to_owned());
                self.pool.record_dispatch_failure(plugin.clone(), reason);
                // A failure is an answer: the call is over and the plugin will
                // send nothing more for this generation. Recorded as a reply
                // carrying no rows so the caller retires the request. Left out,
                // the request stays outstanding for a call that has already
                // finished, nothing can ever retire it, and the launcher says
                // "Providers are still responding" until the next keystroke.
                if let Some(output) = output {
                    output.insert(plugin, Vec::new());
                }
                false
            }
            Ok(ProducedSuggestions {
                suggestions,
                incarnation,
                generation,
            }) => {
                if let Some(output) = output {
                    let mut items = suggestions.items;
                    // The loader owns identity: a plugin cannot forge a sibling
                    // namespace by changing the stable item's owner.
                    for item in &mut items {
                        item.plugin_id = plugin.clone();
                    }
                    let presentation = suggestions.presentation.unwrap_or_default();
                    #[cfg(test)]
                    let publication_pause = self
                        .pool
                        .cancellation
                        .publication_pause
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .take();
                    #[cfg(test)]
                    if let Some((entered, resume)) = publication_pause {
                        entered.send(()).unwrap();
                        resume.recv().unwrap();
                    }
                    let mut ledger = self
                        .catalog
                        .ledger
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    // Shared with intake retirement: an old physical callback
                    // cannot restore declarations after All cleared capture.
                    if generation.get() != self.pool.cancellation.newest_job.load(Ordering::Acquire) {
                        output.insert(plugin, Vec::new());
                        return false;
                    }
                    let publication = ledger.publish(
                        plugin.clone(),
                        RootSource::Suggestion {
                            generation,
                            producer: incarnation,
                        },
                        &items,
                        presentation,
                    );
                    drop(ledger);
                    if publication.is_err() {
                        output.insert(plugin, Vec::new());
                        return true;
                    }
                    for item in &items {
                        self.action_items
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .insert(
                                (plugin.clone(), item.stable_id.clone()),
                                (item.clone(), incarnation),
                            );
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

/// What one collection window observed.
struct CollectedSuggestions {
    /// Rows per plugin that answered in time.
    by_plugin: BTreeMap<PluginId, Vec<Item>>,
    /// Plugins whose call could not be made or failed outright.
    dead_plugins: BTreeSet<PluginId>,
    /// Plugins with a call for this generation still running when the window
    /// closed. Only these may keep a pipeline request open: every other
    /// request is retired now, because nothing is coming for it.
    outstanding: BTreeSet<PluginId>,
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
    scope: Option<crate::plugin_action::RootScope>,
}

/// Single-slot replace-oldest mailbox for native queries.
struct NativeRequestSlot {
    job: Option<NativeJob>,
    /// The latest configuration state to publish, if one is waiting.
    ///
    /// Single-slot replace-oldest: only the newest complete state matters.
    configuration: Option<Box<crate::PluginConfiguration>>,
    stop: bool,
}

impl std::fmt::Debug for NativeRequestSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeRequestSlot")
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
const ACTION_TIMEOUT_MS: u64 = NATIVE_CALL_TIMEOUT_MS;

/// One admitted action handed to the native provider supervisor.
#[derive(Debug)]
struct NativeActionRequest {
    request_id: ActionRequestId,
    plugin: PluginId,
    item: Item,
    /// Suggestions require the producing live incarnation; canonical targets do not.
    origin: Option<u64>,
    work: crate::plugin_action::AdmittedPluginWork,
    argument: Option<String>,
    admission: ForegroundActionAdmission,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    current_generation: Arc<AtomicU64>,
}

enum NativeWork {
    Query(NativeJob),
    Action(Box<NativeActionRequest>),
    /// Publish this complete configuration state to every loaded plugin.
    Configuration(Box<crate::PluginConfiguration>),
    /// Re-present the current generation while a native resource request is
    /// resolving, so an icon can arrive without another user query.
    Refresh,
}

/// The host's event-loop wake, installed once the renderer exists.
///
/// Shared rather than owned so a session can hold a handle that resolves the
/// callback when it fires: a page may be opened before the renderer has wired
/// itself up, and it must still ring the loop once it has. Hand-written
/// `Debug` because a callback has none and the endpoint around it is printed
/// in diagnostics.
#[derive(Clone, Default)]
struct WakeSlot(Arc<Mutex<Option<PageWake>>>);

impl WakeSlot {
    fn install(&self, wake: PageWake) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = Some(wake);
    }

    fn ring(&self) {
        let wake = self.0.lock().unwrap_or_else(|error| error.into_inner()).clone();
        if let Some(wake) = wake {
            wake();
        }
    }

    fn handle(&self) -> PageWake {
        let slot = self.clone();
        Arc::new(move || slot.ring())
    }
}

impl std::fmt::Debug for WakeSlot {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let installed = self.0.lock().unwrap_or_else(|error| error.into_inner()).is_some();
        out.write_str(if installed {
            "WakeSlot(installed)"
        } else {
            "WakeSlot(none)"
        })
    }
}

/// Bounded action endpoint retained by the live native driver.
#[derive(Debug)]
struct NativeActionEndpoint {
    sender: SyncSender<NativeActionRequest>,
    completions: Arc<Mutex<VecDeque<PluginActionCompletion>>>,
    gates: BTreeMap<PluginId, Arc<PreviewActionGate>>,
    items: SharedOriginItems,
    catalog_items: SharedOriginItems,
    static_catalog_items: Arc<Mutex<BTreeMap<(PluginId, ItemId), Item>>>,
    commands: Arc<Mutex<NativeCommandPublications>>,
    ledger: Arc<Mutex<RootSourceLedger>>,
    current_generation: Arc<AtomicU64>,
    pending: Arc<Mutex<BTreeMap<ActionRequestId, Arc<AtomicBool>>>>,
    in_flight: Arc<AtomicUsize>,
    next_id: Arc<AtomicU64>,
    mailbox: Arc<(Mutex<NativeRequestSlot>, Condvar)>,
    /// Query supervisors, so a page can be served by the child that opened it.
    page_supervisors: BTreeMap<PluginId, Arc<Mutex<NativeSupervisor>>>,
    incarnations: NativeIncarnations,
    /// The one open page, if any. The launcher shows one surface at a time,
    /// and a second page would be a second plugin drawing over the first.
    page: Mutex<Option<PageSession>>,
    /// The previous page's thread while it delivers its closing notification.
    /// Closing does not block the user's Escape key, so the notification is
    /// still in flight afterwards; opening the next page waits for it here, or
    /// a `Closed` for the old page could overtake the `Opened` for the new one
    /// on the same child.
    page_closing: Mutex<Option<PageClosing>>,
    /// Rings the host's event loop when a page frame lands.
    wake: WakeSlot,
    action_wake: crate::plugin_action::CompletionWake,
}
impl NativeActionEndpoint {
    fn enqueue(
        &self,
        plugin: &PluginId,
        item: Item,
        origin: Option<u64>,
        work: crate::plugin_action::AdmittedPluginWork,
        argument: Option<&str>,
        query_history_allowed: bool,
    ) -> crikey_core::Result<crate::plugin_action::PluginActionReceipt> {
        if work.typed_binding().is_none() {
            let key = crikey_core::structured_arguments::ExecutableKey {
                target: crikey_core::structured_arguments::OwnedItem {
                    owner: plugin.clone(),
                    item: item.stable_id.clone(),
                },
                action: work.action_id().cloned(),
            };
            let canonical = work.command_source().map(|(_, source)| source);
            if self
                .ledger
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .binding(&key, canonical)
                .is_some()
            {
                return Err(crikey_core::CoreError::Invalid(
                    "typed arguments are required".into(),
                ));
            }
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
        let gate = match self.gates.get(plugin) {
            Some(gate) => gate,
            None => {
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                return Err(crikey_core::CoreError::Invalid(format!(
                    "no native action runtime owns plugin `{}`",
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
            item,
            origin,
            work,
            argument: argument.map(str::to_owned),
            admission,
            deadline: Instant::now() + Duration::from_millis(ACTION_TIMEOUT_MS),
            cancelled,
            current_generation: Arc::clone(&self.current_generation),
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
        Ok(crate::plugin_action::PluginActionReceipt {
            request_id,
            query_history_allowed,
        })
    }
}

impl crate::PluginActionExecutor for NativeActionEndpoint {
    fn root_binding(
        &self,
        key: &crikey_core::structured_arguments::ExecutableKey,
        canonical: Option<&crate::plugin_action::DeclaredCommandSource>,
    ) -> Option<RootExecutableBinding> {
        let binding = self
            .ledger
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .binding(key, canonical)?;
        if let RootSource::Suggestion { generation, producer } = &binding.source {
            if self.current_generation.load(Ordering::Acquire) != generation.get() {
                return None;
            }
            if self
                .incarnations
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&key.target.owner)
                != Some(producer)
            {
                return None;
            }
        }
        Some(binding)
    }

    fn root_presentations(&self) -> Vec<RootPresentationSnapshot> {
        self.ledger
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshots()
            .into_iter()
            .filter(|snapshot| match &snapshot.source {
                RootSource::Catalog(_) => true,
                RootSource::Suggestion { generation, producer } => {
                    self.current_generation.load(Ordering::Acquire) == generation.get()
                        && self
                            .incarnations
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .get(&snapshot.owner)
                            == Some(producer)
                }
            })
            .collect()
    }

    fn root_scope_current(&self, scope: &crate::plugin_action::RootScope) -> bool {
        if !self.page_supervisors.contains_key(&scope.owner) {
            return false;
        }
        {
            let ledger = self.ledger.lock().unwrap_or_else(|error| error.into_inner());
            if !ledger.scope_current(scope) {
                return false;
            }
            if let RootSource::Suggestion { generation, .. } = &scope.source {
                if !ledger.scope_captured(scope)
                    && self.current_generation.load(Ordering::Acquire) != generation.get()
                {
                    return false;
                }
            }
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
        values: crikey_core::structured_arguments::ArgumentValues,
    ) -> crikey_core::Result<crate::plugin_action::TypedActionReceipt> {
        let item = self
            .ledger
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .frozen_item(binding)
            .ok_or_else(|| crikey_core::CoreError::Invalid("typed native source retired".into()))?;
        let origin = match &binding.source {
            RootSource::Catalog(_) => None,
            RootSource::Suggestion { generation, producer } => {
                if self.current_generation.load(Ordering::Acquire) != generation.get() {
                    return Err(crikey_core::CoreError::Invalid(
                        "typed native generation retired".into(),
                    ));
                }
                if self
                    .incarnations
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(&item.plugin_id)
                    != Some(producer)
                {
                    return Err(crikey_core::CoreError::Invalid(
                        "typed native producing worker retired".into(),
                    ));
                }
                Some(*producer)
            }
        };
        let work = crate::plugin_action::AdmittedPluginWork::Typed {
            binding: binding.clone(),
            values,
        };
        work.validate(&item, None)?;
        let queued = item.clone();
        let receipt = self.enqueue(&item.plugin_id, queued, origin, work, None, false)?;
        Ok(crate::plugin_action::TypedActionReceipt { receipt, item })
    }

    fn submit_declared_command_with_receipt(
        &self,
        declaration: &CommandDeclaration,
        source: &crate::plugin_action::DeclaredCommandSource,
        argument: Option<&str>,
    ) -> crikey_core::Result<crate::plugin_action::PluginActionReceipt> {
        let plugin = &declaration.reference.provider_id;
        // Publication and canonical item replacement share this lock. Never
        // release it between validating provenance and freezing the item.
        let publications = self.commands.lock().unwrap_or_else(|error| error.into_inner());
        let current = publications
            .owners
            .get(plugin)
            .filter(|current| {
                current.status == NativeCommandStatus::Ready
                    && current.revision == source.publication
                    && current.instance == source.instance
                    && current.generation == source.generation
                    && current.producer == source.producer
                    && current.declarations.contains(declaration)
            })
            .ok_or_else(|| {
                crikey_core::CoreError::Invalid("declared native command source is no longer current".into())
            })?;
        let crikey_core::command::ExecutableTarget::Item { item_id, .. } = &declaration.executable_target
        else {
            return Err(crikey_core::CoreError::Invalid(
                "host command cannot execute in a plugin".into(),
            ));
        };
        let key = (plugin.clone(), item_id.clone());
        let item = self
            .catalog_items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .map(|(item, _)| item.clone())
            .or_else(|| {
                self.static_catalog_items
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(&key)
                    .cloned()
            })
            .ok_or_else(|| crikey_core::CoreError::Invalid("canonical command item is unavailable".into()))?;
        let action_id = crate::plugin_action::validate_declared_command_item(declaration, &item, argument)?;
        let query_history_allowed = item.argument_policy == ArgumentPolicy::Forbidden
            && argument.is_none()
            && action_id.as_ref() == item.actions.first().map(|action| &action.action_id);
        // The producing worker may be healthily released; canonical commands
        // use the existing lazy worker path, unlike dynamic suggestions.
        let _ = current;
        self.enqueue(
            plugin,
            item,
            None,
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
            crikey_core::CoreError::Invalid("selected native plugin result is no longer current".into())
        })?;
        if item.plugin_id != *plugin {
            return Err(crikey_core::CoreError::Invalid(
                "selected native plugin result has stale ownership".to_owned(),
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
                "selected native plugin result has stale ownership".to_owned(),
            ));
        }
        self.submit_plugin_action_by_id_with_receipt(plugin, &item.stable_id, action_id, argument)
    }

    fn open_plugin_page(
        &self,
        plugin: &PluginId,
        page_id: &str,
        width: u32,
        height: u32,
        palette: crikey_core::PagePalette,
    ) -> crikey_core::Result<()> {
        let supervisor = self.page_supervisors.get(plugin).cloned().ok_or_else(|| {
            crikey_core::CoreError::Invalid(format!("no native worker owns plugin `{}`", plugin.0))
        })?;
        // Before anything else: the page that was just closed may still be
        // telling its plugin so. Letting a new page start first would let the
        // old page's `Closed` land after the new page's `Opened`.
        let settling = self
            .page_closing
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(settling) = settling {
            settling.settle();
        }
        let mut open = self.page.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(session) = open.as_ref() {
            return Err(crikey_core::CoreError::Invalid(format!(
                "native plugin `{}` already has page `{}` open",
                session.plugin().0,
                session.page_id()
            )));
        }
        let admission = self
            .gates
            .get(plugin)
            .ok_or_else(|| crikey_core::CoreError::Invalid("native page owner unavailable".into()))?
            .reserve_foreground()?;
        let session = PageSession::open(
            PageOpen {
                plugin: plugin.clone(),
                page_id: page_id.to_owned(),
                width,
                height,
                deadlines: PageDeadlines::default(),
                palette,
            },
            Box::new(NativePageSource {
                plugin: plugin.clone(),
                supervisor,
                incarnations: Arc::clone(&self.incarnations),
                admission: Some(admission),
                gate: Arc::clone(&self.gates[plugin]),
            }),
            self.wake.handle(),
        )
        .map_err(crikey_core::CoreError::Invalid)?;
        *open = Some(session);
        Ok(())
    }

    fn send_plugin_page_input(&self, input: PageInput) -> crikey_core::Result<()> {
        self.page
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .ok_or_else(|| crikey_core::CoreError::Invalid("no native page is open".to_owned()))?
            .send_input(input)
            .map_err(crikey_core::CoreError::Invalid)
    }

    fn resize_plugin_page(&self, width: u32, height: u32) {
        if let Some(session) = self
            .page
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            session.resize(width, height);
        }
    }

    fn close_plugin_page(&self) {
        let Some(session) = self.page.lock().unwrap_or_else(|error| error.into_inner()).take() else {
            return;
        };
        report_page_failure(&session);
        let (_, closing) = session.close();
        *self
            .page_closing
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(closing);
    }

    fn poll_plugin_page(&self) -> Option<PageUpdate> {
        let mut open = self.page.lock().unwrap_or_else(|error| error.into_inner());
        let update = open.as_ref()?.poll()?;
        // The session is finished the moment it reports a close, and holding
        // its thread past that would keep a plugin answering frame requests
        // for a surface that is already gone.
        if update.closed {
            if let Some(session) = open.take() {
                report_page_failure(&session);
                let (_, closing) = session.close();
                *self
                    .page_closing
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(closing);
            }
        }
        Some(update)
    }
}

/// Reports a page that ended in failure rather than letting it vanish.
///
/// A page that stops answering is a plugin defect the user just experienced,
/// and the surface disappearing is all they would otherwise see. The same
/// stream the rest of this provider's runtime faults go to carries it, named
/// by plugin and page so the report is attributable.
fn report_page_failure(session: &PageSession) {
    if let Some(reason) = session.failure() {
        eprintln!(
            "crikey: native page `{}` of {} failed: {reason}",
            session.page_id(),
            session.plugin().0
        );
    }
    let soft_timeouts = session.soft_timeouts();
    if soft_timeouts != 0 {
        eprintln!(
            "crikey: native page `{}` of {} missed its soft frame deadline {soft_timeouts} times",
            session.page_id(),
            session.plugin().0
        );
    }
}
fn enqueue_native_completion(
    mailbox: &Mutex<VecDeque<PluginActionCompletion>>,
    completion: PluginActionCompletion,
    action_wake: &crate::plugin_action::CompletionWake,
) {
    let mut mailbox = mailbox.lock().unwrap_or_else(|error| error.into_inner());
    if mailbox.len() >= ACTION_COMPLETION_CAPACITY {
        eprintln!(
            "crikey: native action completion mailbox overflow for {} / {}",
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
    // Publish before notifying: the callback may drain the mailbox immediately.
    action_wake.ring();
}

/// Drives [`NativeProvider::drive_query`] away from the UI thread (spec 6.5;
/// acceptance 31.1, 31.8). The mailbox is bounded to one pending job, and a
/// newer generation replaces an unstarted older job.
#[derive(Debug)]
pub struct NativeDriver {
    mailbox: Arc<(Mutex<NativeRequestSlot>, Condvar)>,
    action_endpoint: Arc<NativeActionEndpoint>,
    permissions: BTreeMap<PluginId, Permissions>,
    catalog_results: Arc<Mutex<Vec<CatalogBuildResult>>>,
    commands: Arc<Mutex<NativeCommandPublications>>,
    /// Per-plugin diagnostics refreshed by the supervisor thread after every
    /// unit of work, so the UI thread can report a throttled plugin without
    /// reaching into the pipeline it does not own.
    health: Arc<Mutex<Vec<(PluginId, crikey_plugin_supervisor::PluginHealth)>>>,
    outcome: Arc<Mutex<Option<ViewModel>>>,
    search_summary: Arc<Mutex<Option<crate::query_pipeline::OrdinarySearchSummary>>>,
    query_policies: BTreeMap<PluginId, crikey_input_scheduler::PluginPolicy>,
    current: Arc<AtomicU64>,
    cancellation: Arc<NativeCancellation>,
    replacements: Arc<AtomicU64>,
    busy: Arc<AtomicBool>,
    has_plugins: bool,
    worker: Option<JoinHandle<()>>,
    preview_endpoint: Arc<native_preview::NativePreviewEndpoint>,
}

impl NativeDriver {
    /// Installs the callback that wakes the host for page frames and lifecycle
    /// changes. Headless callers that poll can skip it.
    pub fn set_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        self.action_endpoint.wake.install(Arc::new(wake));
    }

    /// Installs the process-scoped callback for terminal action completions.
    /// The completion mailbox is published and unlocked before notification.
    pub fn set_action_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        self.action_endpoint.action_wake.install(Arc::new(wake));
    }

    /// Moves the provider and pipeline to a supervisor thread. A thread spawn
    /// refusal degrades to an inert driver while dropping the provider reaps
    /// all children (spec 24.1, 24.3).
    ///
    /// Query timestamps anchor a monotonic runtime clock; elapsed time keeps
    /// bounded result intake draining while no further queries are submitted.
    pub fn spawn(
        mut provider: NativeProvider,
        mut pipeline: QueryPipeline,
        publish: Box<dyn Fn(&ViewModel) + Send + 'static>,
    ) -> Self {
        let permissions = provider.permissions();
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
        let preview_endpoint = native_preview::NativePreviewEndpoint::spawn(&provider);
        let has_plugins = !provider.plugins().is_empty();
        let mailbox = Arc::new((
            Mutex::new(NativeRequestSlot {
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
        let current = Arc::new(AtomicU64::new(0));
        let cancellation = Arc::clone(&provider.pool.cancellation);
        let replacements = Arc::new(AtomicU64::new(0));
        let (action_sender, action_receiver) = mpsc::sync_channel(ACTION_QUEUE_CAPACITY);
        let completion_mailbox = Arc::new(Mutex::new(VecDeque::with_capacity(ACTION_COMPLETION_CAPACITY)));
        let action_endpoint = Arc::new(NativeActionEndpoint {
            sender: action_sender,
            completions: Arc::clone(&completion_mailbox),
            gates: provider.action_gates(),
            items: Arc::clone(&provider.action_items),
            catalog_items: Arc::clone(&provider.catalog.items),
            static_catalog_items: Arc::clone(&provider.catalog.static_items),
            commands: Arc::clone(&provider.catalog.commands),
            ledger: Arc::clone(&provider.catalog.ledger),
            current_generation: Arc::clone(&current),
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            in_flight: Arc::new(AtomicUsize::new(0)),
            next_id: Arc::new(AtomicU64::new(0)),
            mailbox: Arc::clone(&mailbox),
            page_supervisors: provider.page_supervisors(),
            incarnations: Arc::clone(&provider.incarnations),
            page: Mutex::new(None),
            page_closing: Mutex::new(None),
            wake: WakeSlot::default(),
            action_wake: crate::plugin_action::CompletionWake::default(),
        });
        let busy = Arc::new(AtomicBool::new(false));
        let thread_mailbox = Arc::clone(&mailbox);
        let thread_catalog_results = Arc::clone(&catalog_results);
        let thread_health = Arc::clone(&health);
        let thread_search_summary = Arc::clone(&search_summary);
        let thread_outcome = Arc::clone(&outcome);
        let thread_current = Arc::clone(&current);
        let thread_busy = Arc::clone(&busy);
        let thread_completion_mailbox = Arc::clone(&completion_mailbox);
        let thread_wake = Some(action_endpoint.wake.handle());
        let thread_action_wake = action_endpoint.action_wake.clone();
        let lifecycle_supervisors = provider.page_supervisors();
        let lifecycle_incarnations = Arc::clone(&provider.incarnations);
        let preview_lifecycle_wake = preview_endpoint.lifecycle_wake();
        let spawned = std::thread::Builder::new()
            .name("crikey-native".to_owned())
            .spawn(move || {
                let (lock, cvar) = &*thread_mailbox;
                let mut last_now: Millis = 0;
                // Anchor runtime elapsed time to the caller's logical query
                // clock. Idle refreshes must keep advancing that same clock:
                // intake permits only one bounded drain per timestamp.
                let mut clock_now: Millis = 0;
                let mut clock_origin = Instant::now();
                let mut last_context: Option<(Generation, String, Vec<ResultRow>, bool, usize)> = None;
                loop {
                    // Poll idle children here, never from the UI incarnation
                    // reader. A busy callback keeps ownership of its supervisor;
                    // it publishes its own lifecycle snapshot on completion.
                    let mut lifecycle_changed = false;
                    for (plugin, supervisor) in &lifecycle_supervisors {
                        match supervisor.try_lock() {
                            Ok(mut supervisor) => {
                                lifecycle_changed |=
                                    refresh_incarnation(&mut supervisor, plugin, &lifecycle_incarnations);
                            }
                            Err(std::sync::TryLockError::Poisoned(error)) => {
                                lifecycle_changed |= refresh_incarnation(
                                    &mut error.into_inner(),
                                    plugin,
                                    &lifecycle_incarnations,
                                );
                            }
                            Err(std::sync::TryLockError::WouldBlock) => {}
                        }
                    }
                    if lifecycle_changed {
                        if let Some(wake) = &thread_wake {
                            wake();
                        }
                        preview_lifecycle_wake();
                    }
                    for result in provider.take_catalog_results() {
                        thread_catalog_results
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .push(result);
                        preview_lifecycle_wake();
                        if let Some(wake) = &thread_wake {
                            wake();
                        }
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
                                        action_id: request.work.action_id().cloned(),
                                        outcome: Err(crikey_core::CoreError::Cancelled),
                                    };
                                    enqueue_native_completion(
                                        &thread_completion_mailbox,
                                        completion,
                                        &thread_action_wake,
                                    );
                                }
                                drop(slot);
                                provider.shutdown(last_now);
                                return;
                            }
                            if let Ok(request) = action_receiver.try_recv() {
                                break NativeWork::Action(Box::new(request));
                            }
                            // Ahead of a query: a plugin about to answer should
                            // answer under the configuration the user just
                            // applied, not the one it is about to lose.
                            if let Some(configuration) = slot.configuration.take() {
                                break NativeWork::Configuration(configuration);
                            }
                            if let Some(job) = slot.job.take() {
                                break NativeWork::Query(job);
                            }
                            let (next, wait) = cvar
                                .wait_timeout(slot, Duration::from_millis(10))
                                .unwrap_or_else(|error| error.into_inner());
                            slot = next;
                            if wait.timed_out() {
                                break NativeWork::Refresh;
                            }
                        }
                    };
                    let job = match work {
                        NativeWork::Action(request) => {
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
                            enqueue_native_completion(
                                &thread_completion_mailbox,
                                completion,
                                &thread_action_wake,
                            );
                            *thread_health.lock().unwrap_or_else(|error| error.into_inner()) =
                                pipeline.plugin_health_report();
                            continue;
                        }
                        NativeWork::Configuration(configuration) => {
                            for (plugin, reason) in provider.publish_configuration(&configuration) {
                                eprintln!(
                                    "crikey: native configuration not delivered to {}: {reason}",
                                    plugin.0
                                );
                            }
                            continue;
                        }
                        NativeWork::Refresh => {
                            last_now = clock_now.saturating_add(
                                clock_origin.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                            );
                            let Some((generation, query, builtin_rows, builtin_pending, selected)) =
                                last_context.as_ref()
                            else {
                                continue;
                            };
                            let slot = lock.lock().unwrap_or_else(|error| error.into_inner());
                            if slot.stop
                                || slot.job.is_some()
                                || thread_current.load(Ordering::Acquire) != generation.get()
                            {
                                continue;
                            }
                            // This tick is the only thing still running for
                            // that generation, so an answer that missed the
                            // collection window is published here or never.
                            drop(slot);
                            let inner = provider
                                .binding
                                .filter(|(job, _)| job == generation)
                                .map(|(_, inner)| inner);
                            let frame = inner.and_then(|inner| {
                                provider.drive_current(&mut pipeline, query, inner, last_now)
                            });
                            let mut summary = pipeline.ordinary_search_summary(0);
                            summary.generation = *generation;
                            let slot = lock.lock().unwrap_or_else(|error| error.into_inner());
                            if slot.stop
                                || slot.job.is_some()
                                || thread_current.load(Ordering::Acquire) != generation.get()
                            {
                                continue;
                            }
                            let mut evidence = thread_search_summary
                                .lock()
                                .unwrap_or_else(|error| error.into_inner());
                            let evidence_changed = evidence.as_ref() != Some(&summary);
                            *evidence = Some(summary);
                            drop(evidence);
                            drop(slot);
                            let Some(frame) = frame else {
                                if evidence_changed {
                                    if let Some(wake) = &thread_wake {
                                        wake();
                                    }
                                }
                                continue;
                            };
                            let mut rows = builtin_rows.clone();
                            rows.extend(frame.rows.iter().cloned());
                            let merged = ViewModel {
                                generation: *generation,
                                query: query.clone(),
                                rows: rows.into(),
                                selected: *selected,
                                pending_plugins: *builtin_pending || frame.pending_plugins,
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
                            let slot = lock.lock().unwrap_or_else(|error| error.into_inner());
                            if slot.stop
                                || slot.job.is_some()
                                || thread_current.load(Ordering::Acquire) != generation.get()
                            {
                                continue;
                            }
                            *thread_outcome.lock().unwrap_or_else(|error| error.into_inner()) =
                                Some(merged.clone());
                            publish(&merged);
                            continue;
                        }
                        NativeWork::Query(job) => job,
                    };
                    let elapsed = clock_origin.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                    last_now = job.now.max(clock_now.saturating_add(elapsed));
                    clock_now = last_now;
                    clock_origin = Instant::now();
                    provider.scope = job.scope.clone();

                    thread_busy.store(true, Ordering::Release);
                    let native =
                        provider.begin_query(&mut pipeline, &job.query, last_now, Some(job.generation));
                    let refresh_context = (
                        job.generation,
                        job.query.clone(),
                        job.builtin_rows.clone(),
                        job.builtin_pending,
                        job.selected,
                    );
                    *thread_health.lock().unwrap_or_else(|error| error.into_inner()) =
                        pipeline.plugin_health_report();
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
                    let mut summary = pipeline.ordinary_search_summary(0);
                    summary.generation = job.generation;
                    *thread_search_summary
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = Some(summary);
                    *thread_outcome.lock().unwrap_or_else(|error| error.into_inner()) = Some(merged.clone());
                    last_context = Some(refresh_context);
                    publish(&merged);
                    thread_busy.store(false, Ordering::Release);
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
                commands,
                health,
                outcome,
                search_summary,
                query_policies,
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
                replacements,
                busy,
                has_plugins: false,
                worker: None,
            },
        }
    }

    pub(crate) fn preview_dispatcher(&self) -> Arc<dyn crate::plugin_preview::PreviewDispatcher> {
        self.preview_endpoint.clone()
    }

    /// Ready declarations require matching catalog admission; empty terminal
    /// states revoke registry entries even without a successful catalog. Registry
    /// replacement uses `snapshot.revision`, never its preview producer ID.
    pub fn command_snapshot(&self, plugin: &PluginId) -> Option<Arc<NativeCommandSnapshot>> {
        self.commands
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .owners
            .get(plugin)
            .cloned()
    }

    /// Revoke on package/catalog invalidation, independent of preview liveness.
    pub fn invalidate_commands(&self, plugin: &PluginId) {
        self.commands
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .invalidate(plugin);
        self.action_endpoint
            .ledger
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .invalidate(plugin);
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
        let context = ProviderQueryContext {
            generation,
            text: query,
            now,
            selected,
        };
        let _ = self.submit_inner(context, builtin, builtin_pending, None);
    }

    /// Captures an exact declared owner scope; unavailable restoration never widens to All.
    pub fn submit_scoped(
        &self,
        context: ProviderQueryContext<'_>,
        builtin: Vec<ResultRow>,
        builtin_pending: bool,
        scope: Option<crate::plugin_action::RootScope>,
    ) -> crikey_core::Result<()> {
        self.submit_inner(context, builtin, builtin_pending, scope)
    }

    fn submit_inner(
        &self,
        context: ProviderQueryContext<'_>,
        builtin: Vec<ResultRow>,
        builtin_pending: bool,
        scope: Option<crate::plugin_action::RootScope>,
    ) -> crikey_core::Result<()> {
        let ProviderQueryContext {
            generation,
            text: query,
            now,
            selected,
        } = context;
        let generation_value = generation.get();
        let (lock, cvar) = &*self.mailbox;
        let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
        if slot.stop {
            return Err(crikey_core::CoreError::Cancelled);
        }
        let mut ledger = self
            .action_endpoint
            .ledger
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Admission, capture, and retirement share terminal publication's
        // ledger lock. A discarded older query cannot replace the chosen scope.
        let observed = self.current.load(Ordering::Acquire);
        if generation_value < observed {
            return Ok(());
        }
        let owned_scope = scope
            .as_ref()
            .filter(|scope| self.permissions.contains_key(&scope.owner));
        if let Some(scope) = owned_scope {
            let live = match &scope.source {
                RootSource::Catalog(_) => true,
                RootSource::Suggestion { generation, producer } => {
                    (ledger.scope_captured(scope) || generation.get() == observed)
                        && self
                            .action_endpoint
                            .incarnations
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .get(&scope.owner)
                            == Some(producer)
                }
            };
            if !live || !ledger.scope_current(scope) {
                return Err(crikey_core::CoreError::Invalid(
                    "native scope is unavailable".into(),
                ));
            }
        }
        // All submissions serialize on the ledger, including foreign-runtime
        // scopes, which remain nonapplicable but still retire prior native work.
        self.current.store(generation_value, Ordering::Release);
        ledger.capture_scope(owned_scope);
        self.cancellation.cancel_before(generation_value);
        ledger.retire_suggestions();
        drop(ledger);
        let state = if self.worker.is_some() {
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
                    .filter(|(plugin, _)| scope.as_ref().is_none_or(|scope| scope.owner == **plugin))
                    .map(|(plugin, _)| crate::query_pipeline::OrdinarySearchProvider {
                        plugin: plugin.clone(),
                        state,
                    })
                    .collect(),
            });
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
            scope,
        });
        drop(slot);
        cvar.notify_one();
        Ok(())
    }

    /// Hands the supervisor thread one complete configuration state to publish
    /// to every native plugin (spec 21.4).
    ///
    /// Returns at once and never blocks the caller: delivery happens on the
    /// supervisor thread, which is the only thread allowed to touch a native
    /// worker. Replace-oldest, so a caller that publishes twice before the
    /// supervisor gets a turn delivers only the newer state — the same
    /// coalescing rule the host applied upstream, enforced again here because
    /// the two are separated by a thread boundary.
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

    /// Takes the newest merged frame published by the supervisor, if any.
    pub fn take_outcome(&self) -> Option<ViewModel> {
        self.outcome
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
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
impl Drop for NativeDriver {
    fn drop(&mut self) {
        for cancelled in self
            .action_endpoint
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
        {
            cancelled.store(true, Ordering::Release);
        }
        self.preview_endpoint.shutdown();
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

#[cfg(test)]
mod query_history_receipt_tests {
    use super::*;
    use crate::PluginActionExecutor;
    use crikey_core::{ArgumentPolicy, ExecutionPolicy};

    #[test]
    fn typed_queue_preserves_source_privacy_and_foreground_handoff() {
        use crikey_core::result_presentation::ExecutableSchemaBinding;
        use crikey_core::structured_arguments::*;
        let item = item();
        let (endpoint, receiver) = endpoint(item.clone());
        endpoint.current_generation.store(9, Ordering::Release);
        let key = ExecutableKey {
            target: OwnedItem {
                owner: item.plugin_id.clone(),
                item: item.stable_id.clone(),
            },
            action: Some(item.actions[0].action_id.clone()),
        };
        let schema = Arc::new(ArgumentSchema {
            fields: vec![ArgumentField {
                id: FieldId("password".into()),
                label: "Password".into(),
                required: true,
                placeholder: None,
                validation_message: None,
                kind: FieldKind::Password {
                    min_bytes: 1,
                    max_bytes: 128,
                },
            }],
        });
        endpoint
            .ledger
            .lock()
            .unwrap()
            .publish(
                item.plugin_id.clone(),
                RootSource::Suggestion {
                    generation: Generation::from_raw(9),
                    producer: 1,
                },
                &[item.clone()],
                ResultPresentation {
                    schemas: vec![ExecutableSchemaBinding {
                        executable: key.clone(),
                        schema,
                    }],
                    ..Default::default()
                },
            )
            .unwrap();
        let binding = endpoint.root_binding(&key, None).unwrap();
        assert!(endpoint
            .submit_plugin_action_by_id_with_receipt(
                &item.plugin_id,
                &item.stable_id,
                &item.actions[0].action_id,
                None
            )
            .is_err());
        let control = Arc::new(CallControl::new());
        let gate = &endpoint.gates[&item.plugin_id];
        let preview = gate.try_preview(control.clone()).unwrap();
        let values = ArgumentValues(BTreeMap::from([(
            FieldId("password".into()),
            StructuredValue::Password(Password::new("private-native-value".into()).unwrap()),
        )]));
        let receipt = endpoint.submit_typed(&binding, values).unwrap();
        assert!(!receipt.receipt.query_history_allowed);
        assert!(control.requested.load(Ordering::Acquire));
        let request = receiver.try_recv().unwrap();
        assert_eq!(receipt.item.stable_id, request.item.stable_id);
        assert_eq!(receipt.item.label, request.item.label);
        assert!(!format!("{request:?}").contains("private-native-value"));
        assert_eq!(request.origin, Some(1));
        assert!(request.argument.is_none());
        assert!(request.work.typed_binding().is_some());
        endpoint.current_generation.store(10, Ordering::Release);
        assert!(endpoint.root_binding(&key, None).is_none());
        assert!(endpoint
            .submit_typed(&binding, ArgumentValues::default())
            .is_err());
        drop(preview);
        drop(request);
    }

    fn item() -> Item {
        Item {
            stable_id: ItemId("shared".into()),
            plugin_id: PluginId("native.receipt".into()),
            category: crikey_core::Category::Command,
            label: "Receipt".into(),
            description: String::new(),
            target: String::new(),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: crikey_core::HitPolicy::Recorded,
            score_hint: 0,
            metadata: BTreeMap::new(),
            actions: vec![crikey_core::Action {
                action_id: ActionId("run".into()),
                label: "Run".into(),
                description: String::new(),
                applicable_categories: Vec::new(),
                icon_reference: None,
                execution_policy: ExecutionPolicy::Plugin,
            }],
        }
    }

    #[test]
    fn declared_admission_pins_catalog_and_preserves_actionless_callback() {
        use crate::plugin_action::{AdmittedPluginWork, DeclaredCommandSource};
        use crikey_core::command::{CommandRef, ExecutableTarget};
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
        endpoint.commands.lock().unwrap().owners.insert(
            canonical.plugin_id.clone(),
            Arc::new(NativeCommandSnapshot {
                plugin: canonical.plugin_id.clone(),
                revision: source.publication,
                instance: source.instance,
                generation: source.generation,
                producer: source.producer,
                status: NativeCommandStatus::Ready,
                released: true,
                declarations: Arc::from([declaration.clone()]),
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
    }

    fn endpoint(item: Item) -> (NativeActionEndpoint, mpsc::Receiver<NativeActionRequest>) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let owner = item.plugin_id.clone();
        let incarnations = NativeIncarnations::default();
        incarnations.lock().unwrap().insert(owner.clone(), 1);
        (
            NativeActionEndpoint {
                sender,
                completions: Arc::default(),
                gates: BTreeMap::from([(
                    owner.clone(),
                    PreviewActionGate::new(crikey_plugin_supervisor::shared_budget_from_section(
                        &Default::default(),
                    )),
                )]),
                items: Arc::new(Mutex::new(BTreeMap::from([(
                    (owner, item.stable_id.clone()),
                    (item, 1),
                )]))),
                catalog_items: Arc::default(),
                static_catalog_items: Arc::default(),
                commands: Arc::default(),
                ledger: Arc::default(),
                current_generation: Arc::default(),
                pending: Arc::default(),
                in_flight: Arc::default(),
                next_id: Arc::default(),
                mailbox: Arc::new((
                    Mutex::new(NativeRequestSlot {
                        job: None,
                        configuration: None,
                        stop: false,
                    }),
                    Condvar::new(),
                )),
                page_supervisors: BTreeMap::new(),
                incarnations,
                page: Mutex::new(None),
                page_closing: Mutex::new(None),
                wake: WakeSlot::default(),
                action_wake: crate::plugin_action::CompletionWake::default(),
            },
            receiver,
        )
    }

    #[test]
    fn receipt_uses_the_frozen_accepted_item_not_the_displayed_catalog_policy() {
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
    fn real_fixture_binary() -> &'static std::path::Path {
        static BINARY: std::sync::LazyLock<std::path::PathBuf> = std::sync::LazyLock::new(|| {
            let app = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let manifest = app.join("../crikey-ui/tests/fixtures/navigation/Cargo.toml");
            let target = app.join("../../target/navigation-action-consumer");
            let output = std::process::Command::new("cargo")
                .arg("build")
                .arg("--manifest-path")
                .arg(&manifest)
                .arg("--target-dir")
                .arg(&target)
                .output()
                .expect("build real native fixture");
            assert!(
                output.status.success(),
                "native fixture build failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            target.join("debug").join(if cfg!(windows) {
                "crikey-navigation-fixture.exe"
            } else {
                "crikey-navigation-fixture"
            })
        });
        BINARY.as_path()
    }

    fn typed_fixture_binary() -> &'static Path {
        static BINARY: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
            let target = root.join("target/native-conformance");
            let output = std::process::Command::new("cargo")
                .arg("build")
                .arg("--bin")
                .arg("typed-admission-fixture")
                .arg("--manifest-path")
                .arg(root.join("compatibility/native-conformance/Cargo.toml"))
                .arg("--target-dir")
                .arg(&target)
                .output()
                .expect("build real typed fixture");
            assert!(
                output.status.success(),
                "typed fixture build failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            target.join("debug").join(if cfg!(windows) {
                "typed-admission-fixture.exe"
            } else {
                "typed-admission-fixture"
            })
        });
        BINARY.as_path()
    }

    #[test]
    fn real_scope_admission_rejects_old_capture_and_late_publication_after_all() {
        use crikey_core::result_presentation::ScopeId;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "crikey-native-scope-intake-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&root).unwrap();
        let fixture = ProcessFixture(root);
        let package = fixture.0.join("typed");
        fs::create_dir(&package).unwrap();
        let executable = if cfg!(windows) { "worker.exe" } else { "worker" };
        fs::copy(typed_fixture_binary(), package.join(executable)).unwrap();
        fs::write(package.join("crikey.toml"), format!(
            "manifest-version = 1\n[plugin]\nid = \"typed\"\nname = \"Typed\"\nversion = \"1.0.0\"\nruntime = \"native\"\nentrypoint = \"{executable}\"\n[performance]\nstartup = \"lazy\"\nsuggest-hard-timeout-ms = 20000\n"
        )).unwrap();
        let mut pipeline = QueryPipeline::new(crate::PipelineConfig::default());
        let provider = NativeProvider::load(&mut pipeline, &[fixture.0.clone()], &DisabledPlugins::default());
        assert_eq!(provider.loaded.len(), 1, "{:?}", provider.unavailable());
        let owner = provider.loaded[0].plugin.clone();
        let cancellation = Arc::clone(&provider.pool.cancellation);
        let ledger = Arc::clone(&provider.catalog.ledger);
        let driver = NativeDriver::spawn(provider, pipeline, Box::new(|_| {}));
        let executor = driver.action_executor();
        let deadline = Instant::now() + Duration::from_secs(60);
        let wait_publication = |expected: u64| loop {
            if let Some(snapshot) = executor.root_presentations().into_iter().find(|snapshot| {
                    matches!(&snapshot.source, RootSource::Suggestion { generation, .. } if generation.get() == expected)
                }) { break snapshot.source; }
            assert!(
                Instant::now() < deadline,
                "real scope publication {expected} did not arrive"
            );
            thread::sleep(Duration::from_millis(2));
        };
        driver.submit(Generation::from_raw(1), "scopes", 0, Vec::new(), false, 0);
        let first = wait_publication(1);
        let selected = crate::plugin_action::RootScope {
            owner: owner.clone(),
            id: ScopeId("private".into()),
            source: first,
        };
        driver
            .submit_scoped(
                ProviderQueryContext {
                    generation: Generation::from_raw(2),
                    text: "scopes",
                    now: 1,
                    selected: 0,
                },
                Vec::new(),
                false,
                Some(selected.clone()),
            )
            .unwrap();
        let fresh = wait_publication(2);
        let competing = crate::plugin_action::RootScope {
            owner: owner.clone(),
            id: ScopeId("other".into()),
            source: fresh.clone(),
        };
        assert!(executor.root_scope_current(&competing));
        // This declaration is real/current, but its enclosing delayed query
        // is not admitted and must not replace the already selected token.
        driver
            .submit_scoped(
                ProviderQueryContext {
                    generation: Generation::from_raw(1),
                    text: "discarded",
                    now: 2,
                    selected: 0,
                },
                Vec::new(),
                false,
                Some(competing.clone()),
            )
            .unwrap();
        assert!(executor.root_scope_current(&selected));
        assert!(ledger.lock().unwrap().scope_captured(&selected));
        assert!(!ledger.lock().unwrap().scope_captured(&competing));
        assert_eq!(driver.current.load(Ordering::Acquire), 2);

        let (entered_old_tx, entered_old) = mpsc::sync_channel(1);
        let (resume_old, resume_old_rx) = mpsc::sync_channel(1);
        *cancellation.publication_pause.lock().unwrap() = Some((entered_old_tx, resume_old_rx));
        driver
            .submit_scoped(
                ProviderQueryContext {
                    generation: Generation::from_raw(3),
                    text: "scopes",
                    now: 3,
                    selected: 0,
                },
                Vec::new(),
                false,
                Some(selected.clone()),
            )
            .unwrap();
        entered_old
            .recv_timeout(Duration::from_secs(60))
            .expect("real old callback reached publication");
        let (entered_new_tx, entered_new) = mpsc::sync_channel(1);
        let (resume_new, resume_new_rx) = mpsc::sync_channel(1);
        *cancellation.publication_pause.lock().unwrap() = Some((entered_new_tx, resume_new_rx));
        driver.submit(Generation::from_raw(4), "all", 4, Vec::new(), false, 0);
        assert!(!executor.root_scope_current(&selected));
        resume_old.send(()).unwrap();
        entered_new
            .recv_timeout(Duration::from_secs(60))
            .expect("real All callback reached publication");
        // Old callback has now drained, but the new result is still held.
        // There must be no resurrected executable/declaration sidecar.
        assert!(ledger
            .lock()
            .unwrap()
            .snapshots()
            .iter()
            .all(|snapshot| !matches!(&snapshot.source, RootSource::Suggestion { .. })));
        let producer = match fresh {
            RootSource::Suggestion { producer, .. } => producer,
            _ => unreachable!(),
        };
        let revoked = crate::plugin_action::RootScope {
            owner,
            id: ScopeId("private".into()),
            source: RootSource::Suggestion {
                generation: Generation::from_raw(3),
                producer,
            },
        };
        assert!(!executor.root_scope_current(&revoked));
        assert!(driver
            .submit_scoped(
                ProviderQueryContext {
                    generation: Generation::from_raw(5),
                    text: "revoked",
                    now: 5,
                    selected: 0
                },
                Vec::new(),
                false,
                Some(revoked)
            )
            .is_err());
        assert_eq!(driver.current.load(Ordering::Acquire), 4);
        resume_new.send(()).unwrap();
        wait_publication(4);
        assert!(!executor.root_scope_current(&selected));
    }

    #[test]
    fn released_catalog_survives_replacement_worker_before_publication() {
        use crikey_core::structured_arguments::{ExecutableKey, OwnedItem};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "crikey-native-static-publication-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&root).unwrap();
        let fixture = ProcessFixture(root);
        let package = fixture.0.join("typed");
        fs::create_dir(&package).unwrap();
        let executable = if cfg!(windows) { "worker.exe" } else { "worker" };
        fs::copy(typed_fixture_binary(), package.join(executable)).unwrap();
        fs::write(package.join("crikey.toml"), format!(
            "manifest-version = 1\n[plugin]\nid = \"typed\"\nname = \"Typed\"\nversion = \"1.0.0\"\nruntime = \"native\"\nentrypoint = \"{executable}\"\n[performance]\nstartup = \"lazy\"\n"
        )).unwrap();
        let mut pipeline = QueryPipeline::new(crate::PipelineConfig::default());
        let mut provider =
            NativeProvider::load(&mut pipeline, &[fixture.0.clone()], &DisabledPlugins::default());
        assert_eq!(provider.loaded.len(), 1, "{:?}", provider.unavailable());
        let owner = provider.loaded[0].plugin.clone();
        let key = provider.loaded[0].key.clone();
        provider
            .request_catalog_build(&owner, 1, Generation::ZERO)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        // Keep the completed catalog in its real mailbox while a later call
        // obtains a different physical worker. Do not drain publication early.
        while !provider
            .catalog
            .tasks
            .values()
            .all(|task| task.join.as_ref().unwrap().is_finished())
        {
            assert!(Instant::now() < deadline, "real catalog worker did not finish");
            thread::sleep(Duration::from_millis(2));
        }
        let producer = {
            let incarnations = provider.incarnations.lock().unwrap();
            assert!(
                incarnations.get(&owner).is_none(),
                "static producer was healthily released"
            );
            *incarnations.observed.get(&owner).unwrap()
        };
        let supervisor = Arc::clone(provider.pool.supervisors.get(&key).unwrap());
        let replacement = with_native_worker(
            &mut supervisor.lock().unwrap(),
            &owner,
            &provider.incarnations,
            |worker| Ok(worker.connection_id()),
        )
        .unwrap();
        assert_ne!(producer, replacement);
        let results = provider.take_catalog_results();
        assert!(matches!(results.as_slice(), [CatalogBuildResult::Complete(_)]));
        let commands = provider.command_snapshot(&owner).unwrap();
        assert_eq!(commands.status, NativeCommandStatus::Ready);
        assert!(commands.released);
        assert_eq!(commands.producer, producer);
        assert_eq!(commands.declarations.len(), 1);
        let ledger = provider.catalog.ledger.lock().unwrap();
        let binding = ledger
            .binding(
                &ExecutableKey {
                    target: OwnedItem {
                        owner: owner.clone(),
                        item: ItemId("typed-login".into()),
                    },
                    action: None,
                },
                None,
            )
            .expect("released catalog retains its typed schema");
        assert!(ledger.frozen_item(&binding).is_some());
        let snapshots = ledger.snapshots();
        assert!(snapshots.iter().any(|snapshot| snapshot.owner == owner
            && snapshot
                .presentation
                .sections
                .iter()
                .any(|section| section.id.0 == "accounts")
            && snapshot
                .presentation
                .scopes
                .iter()
                .any(|scope| scope.id.0 == "private")));
    }

    #[test]
    fn typed_discovery_rechecks_source_cancellation_and_deadline_before_delivery() {
        use crikey_core::structured_arguments::*;
        for revoke in ["source", "cancel", "deadline", "current"] {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "crikey-native-typed-discovery-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir(&root).unwrap();
            let fixture = ProcessFixture(root);
            let package = fixture.0.join("typed");
            fs::create_dir(&package).unwrap();
            let executable = if cfg!(windows) { "worker.exe" } else { "worker" };
            fs::copy(typed_fixture_binary(), package.join(executable)).unwrap();
            fs::write(package.join("crikey.toml"), format!(
                "manifest-version = 1\n[plugin]\nid = \"typed\"\nname = \"Typed\"\nversion = \"1.0.0\"\nruntime = \"native\"\nentrypoint = \"{executable}\"\n[performance]\nstartup = \"lazy\"\nsuggest-hard-timeout-ms = 20000\n"
            )).unwrap();
            let mut pipeline = QueryPipeline::new(crate::PipelineConfig::default());
            let mut provider =
                NativeProvider::load(&mut pipeline, &[fixture.0.clone()], &DisabledPlugins::default());
            assert_eq!(provider.loaded.len(), 1, "{:?}", provider.unavailable());
            let owner = provider.loaded[0].plugin.clone();
            provider
                .request_catalog_build(&owner, 1, Generation::ZERO)
                .unwrap();
            let end = Instant::now() + Duration::from_secs(60);
            let build = loop {
                if let Some(result) = provider.take_catalog_results().into_iter().next() {
                    match result {
                        CatalogBuildResult::Complete(build) => break build,
                        other => panic!("typed catalog failed: {other:?}"),
                    }
                }
                assert!(Instant::now() < end, "typed catalog never completed");
                thread::sleep(Duration::from_millis(2));
            };
            assert!(
                provider.incarnations.lock().unwrap().get(&owner).is_none(),
                "the replacement must perform its own schema discovery"
            );
            let item = build
                .items
                .iter()
                .find(|item| item.stable_id.0 == "typed-login")
                .unwrap()
                .clone();
            let (mut endpoint, receiver) = endpoint(item.clone());
            endpoint.ledger = Arc::clone(&provider.catalog.ledger);
            endpoint.incarnations = Arc::clone(&provider.incarnations);
            endpoint.gates = provider.action_gates();
            let key = ExecutableKey {
                target: OwnedItem {
                    owner: owner.clone(),
                    item: item.stable_id.clone(),
                },
                action: None,
            };
            let binding = endpoint.root_binding(&key, None).unwrap();
            let values = ArgumentValues(BTreeMap::from([(
                FieldId("password".into()),
                StructuredValue::Password(Password::new("private-discovery-secret".into()).unwrap()),
            )]));
            endpoint.submit_typed(&binding, values).unwrap();
            let mut request = receiver.try_recv().unwrap();
            // Keep startup outside the deadline interval: enter discovery on
            // an already acquired healthy replacement worker.
            {
                let supervisor = provider.pool.supervisors.get(&provider.loaded[0].key).unwrap();
                let mut supervisor = supervisor.lock().unwrap();
                with_native_worker(&mut supervisor, &owner, &provider.incarnations, |_| Ok(())).unwrap();
            }
            request.deadline = Instant::now() + Duration::from_secs(3);
            let deadline = request.deadline;
            let cancelled = Arc::clone(&request.cancelled);
            let ledger = Arc::clone(&provider.catalog.ledger);
            fs::write(package.join("typed-discovery-hold"), "").unwrap();
            thread::scope(|scope| {
                let execution = scope.spawn(|| provider.execute_action_request(request));
                while !package.join("typed-discovery-entered").exists() {
                    assert!(
                        Instant::now() < deadline,
                        "discovery must enter before revocation/deadline"
                    );
                    thread::sleep(Duration::from_millis(2));
                }
                match revoke {
                    "source" => {
                        ledger.lock().unwrap().invalidate(&owner);
                    }
                    "cancel" => cancelled.store(true, Ordering::Release),
                    "deadline" => thread::sleep(deadline.saturating_duration_since(Instant::now())),
                    "current" => {}
                    _ => unreachable!(),
                }
                fs::write(package.join("typed-discovery-release"), "").unwrap();
                let outcome = execution.join().unwrap();
                if revoke == "current" {
                    assert!(matches!(
                        outcome,
                        Ok(ActionEffect::Typed(SubmissionOutcome::Completed))
                    ));
                } else {
                    assert!(outcome.is_err(), "{revoke} must refuse delivery");
                }
            });
            // Reap the real transport before inspecting both independent
            // witnesses; SDK rejection alone cannot satisfy this assertion.
            provider.shutdown(0);
            assert_eq!(
                package.join("typed-received").exists(),
                revoke == "current",
                "{revoke}: raw physical transport receipt"
            );
            assert_eq!(
                package.join("typed-executed").exists(),
                revoke == "current",
                "{revoke}: typed callback admission"
            );
        }
    }

    #[test]
    fn released_static_catalog_executes_frozen_admission_in_the_real_consumer() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "crikey-native-receipt-consumer-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let fixture = ProcessFixture(root);
        let package = fixture.0.join("receipt");
        fs::create_dir(&package).unwrap();
        let executable = if cfg!(windows) { "worker.exe" } else { "worker" };
        fs::copy(real_fixture_binary(), package.join(executable)).unwrap();
        fs::write(package.join("crikey.toml"), format!(
            "manifest-version = 1\n[plugin]\nid = \"receipt\"\nname = \"Receipt\"\nversion = \"1.0.0\"\nruntime = \"native\"\nentrypoint = \"{executable}\"\n[performance]\nstartup = \"lazy\"\n"
        )).unwrap();
        let mut pipeline = QueryPipeline::new(crate::PipelineConfig::default());
        let mut provider =
            NativeProvider::load(&mut pipeline, &[fixture.0.clone()], &DisabledPlugins::default());
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
            assert!(Instant::now() < deadline, "native catalog never completed");
            thread::sleep(Duration::from_millis(2));
        };
        assert!(
            provider.incarnations.lock().unwrap().get(&owner).is_none(),
            "a healthy static producer is released"
        );
        let displayed = build
            .items
            .iter()
            .find(|item| item.stable_id.0 == "route-0")
            .unwrap()
            .clone();
        let (mut endpoint, receiver) = endpoint(displayed.clone());
        endpoint.items = Arc::clone(&provider.action_items);
        endpoint.catalog_items = Arc::clone(&provider.catalog.items);
        endpoint.static_catalog_items = Arc::clone(&provider.catalog.static_items);
        endpoint.incarnations = Arc::clone(&provider.incarnations);
        endpoint.gates = provider.action_gates();
        endpoint.commands = Arc::clone(&provider.catalog.commands);
        // Exercise revocation after admission, while the foreground lease is
        // still waiting for the speculative slot, against a real loaded owner.
        let declaration = CommandDeclaration {
            reference: crikey_core::command::CommandRef {
                provider_id: owner.clone(),
                command_id: "revoked".into(),
            },
            label: "Revoked".into(),
            executable_target: crikey_core::command::ExecutableTarget::Item {
                item_id: displayed.stable_id.clone(),
                action_id: Some(displayed.actions[0].action_id.clone()),
            },
            argument_policy: displayed.argument_policy,
            availability: CommandAvailability::Available,
        };
        let source = crate::plugin_action::DeclaredCommandSource {
            instance: 1,
            generation: Generation::ZERO,
            producer: 1,
            publication: 1,
        };
        endpoint.commands.lock().unwrap().owners.insert(
            owner.clone(),
            Arc::new(NativeCommandSnapshot {
                plugin: owner.clone(),
                revision: source.publication,
                instance: source.instance,
                generation: source.generation,
                producer: source.producer,
                status: NativeCommandStatus::Ready,
                released: true,
                declarations: Arc::from([declaration.clone()]),
            }),
        );
        let preview_cancelled = Arc::new(AtomicBool::new(false));
        let preview_guard = endpoint.gates[&owner]
            .try_preview(preview_cancelled.clone())
            .unwrap();
        let revoked = endpoint
            .submit_declared_command_with_receipt(&declaration, &source, None)
            .unwrap();
        assert!(preview_cancelled.load(Ordering::Acquire));
        let waiting = receiver.try_recv().unwrap();
        provider.invalidate_commands(&owner);
        drop(preview_guard);
        let refusal = provider.execute_action_request(waiting).unwrap_err();
        assert!(
            refusal.to_string().contains("source is no longer current"),
            "{refusal}"
        );
        endpoint.pending.lock().unwrap().remove(&revoked.request_id);
        endpoint.in_flight.fetch_sub(1, Ordering::AcqRel);
        let action = displayed.actions[0].action_id.clone();
        let receipt = endpoint
            .submit_plugin_action_with_receipt(&owner, &displayed, &action, None)
            .unwrap();
        assert!(receipt.query_history_allowed);
        let request = receiver.try_recv().unwrap();
        assert!(
            request.origin.is_none(),
            "canonical catalog execution does not borrow a retired producer"
        );
        {
            let mut catalog = provider.catalog.static_items.lock().unwrap();
            let replacement = catalog
                .get_mut(&(owner.clone(), displayed.stable_id.clone()))
                .unwrap();
            replacement.argument_policy = ArgumentPolicy::Required;
            replacement.actions.clear();
        }
        assert_eq!(
            provider.execute_action_request(request).unwrap(),
            ActionEffect::ShowPage {
                page_id: "route-0".into()
            }
        );
        let log = package.join("navigation-lifecycle.log");
        assert_eq!(
            fs::read_to_string(&log)
                .unwrap()
                .lines()
                .filter(|line| line.starts_with("execute "))
                .count(),
            1
        );
        assert!(
            endpoint
                .submit_plugin_action_with_receipt(&owner, &displayed, &action, None)
                .is_err(),
            "a later admission validates the changed canonical snapshot"
        );

        // A dynamic suggestion may not borrow a replacement process.
        let origin = provider
            .incarnations
            .lock()
            .unwrap()
            .get(&owner)
            .copied()
            .unwrap();
        provider.action_items.lock().unwrap().insert(
            (owner.clone(), displayed.stable_id.clone()),
            (displayed.clone(), origin),
        );
        endpoint
            .submit_plugin_action_with_receipt(&owner, &displayed, &action, None)
            .unwrap();
        let retired_request = receiver.try_recv().unwrap();
        {
            let supervisor = provider.pool.supervisors.get(&provider.loaded[0].key).unwrap();
            let mut supervisor = supervisor.lock().unwrap();
            supervisor.release_worker(&owner, native_policy_now());
            refresh_incarnation(&mut supervisor, &owner, &provider.incarnations);
        }
        assert!(provider.execute_action_request(retired_request).is_err());
        assert_eq!(
            fs::read_to_string(log)
                .unwrap()
                .lines()
                .filter(|line| line.starts_with("execute "))
                .count(),
            1
        );
    }
}

#[cfg(test)]
mod command_preview_handoff_tests {
    use super::*;
    use crate as crikey_app;
    use crate::PipelineConfig;
    use std::process::Command;
    use std::sync::LazyLock;

    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/native_command_fixture.rs"
    ));
    #[test]
    fn real_deferred_preview_yields_to_one_frozen_canonical_intent() {
        use crikey_app::{NativeDriver, PreviewIdentity, PreviewRequest, PreviewState};
        let (scratch, pipeline, mut provider, owner) = setup("handoff");
        provider
            .request_catalog_build(&owner, 7, Generation::from_raw(1))
            .unwrap();
        assert_catalog(complete(&mut provider));
        let commands = snapshot(&provider, &owner);
        let source = crikey_app::DeclaredCommandSource {
            instance: commands.instance,
            generation: commands.generation,
            producer: commands.producer,
            publication: commands.revision,
        };
        let budget = pipeline.plugin_budget(&owner).unwrap().clone();
        let driver = NativeDriver::spawn(provider, pipeline, Box::new(|_| {}));
        let actions = driver.action_executor();
        let previews = driver.preview_dispatcher();
        let package = scratch.0.join("commands");
        let declaration = &commands.declarations[0];
        for index in 0..4 {
            let _ = fs::remove_file(package.join("preview-started"));
            let _ = fs::remove_file(package.join("preview-cancelled"));
            let _ = fs::remove_file(package.join("preview-release"));
            let identity = PreviewIdentity {
                view: 1,
                query: Generation::from_raw(1),
                plugin: owner.clone(),
                incarnation: source.producer,
                item: crikey_core::ItemId("installed".into()),
                target: crikey_ui::ActionTarget::Command(declaration.reference.clone()),
                catalog_source: Some(source.clone()),
                selection: index + 1,
                request: index + 1,
            };
            let (send, receive) = std::sync::mpsc::channel();
            previews
                .submit(
                    PreviewRequest { identity },
                    Arc::new(move |update| {
                        send.send(update).unwrap();
                    }),
                )
                .unwrap();
            wait_for(|| package.join("preview-started").exists().then_some(()));
            let argument = format!("literal argument {index}");
            let receipt = actions
                .submit_declared_command_with_receipt(declaration, &source, Some(&argument))
                .unwrap();
            wait_for(|| package.join("preview-cancelled").exists().then_some(()));
            assert_eq!(budget.in_flight(BudgetKind::Action), 1);
            assert!(actions
                .submit_declared_command_with_receipt(declaration, &source, Some("duplicate"))
                .is_err());
            assert_eq!(
                fs::read_to_string(package.join("executions"))
                    .unwrap_or_default()
                    .lines()
                    .count(),
                index as usize
            );
            if index == 3 {
                assert!(actions.cancel_plugin_action(&receipt.request_id));
            }
            fs::write(package.join("preview-release"), "").unwrap();
            let completion = wait_for(|| actions.poll_plugin_actions().into_iter().next());
            assert_eq!(completion.request_id, receipt.request_id);
            if index == 3 {
                assert!(matches!(
                    completion.outcome,
                    Err(crikey_core::CoreError::Cancelled)
                ));
            } else {
                assert_eq!(completion.outcome.unwrap(), crikey_app::ActionEffect::Completed);
            }
            let update = receive.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(matches!(update.state, PreviewState::Cancelled));
        }
        assert_eq!(fs::read_to_string(package.join("executions")).unwrap(),
        "installed:alternate:literal argument 0\ninstalled:alternate:literal argument 1\ninstalled:alternate:literal argument 2\n");
        let occupied = budget.try_acquire_owned(BudgetKind::Action).unwrap();
        assert!(actions
            .submit_declared_command_with_receipt(declaration, &source, Some("saturated"))
            .is_err());
        drop(occupied);
        assert!(actions.poll_plugin_actions().is_empty());
    }
}
