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

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crikey_core::{Generation, Item, PluginId};
use crikey_input_scheduler::Millis;
use crikey_package_manager::{
    resolve, EnvironmentInputs, EnvironmentStore, ImportPath, PackageError, PackageIndex,
};
use crikey_plugin_model::{Manifest, Runtime};
use crikey_python_host::{
    discover_interpreter, sdk_root, BatchState as WorkerBatchState, ModernWorker, RequiresPython,
    RuntimeProfile, SuggestRequest, WorkerOptions, WORKER_ENTRY_FILE,
};
use crikey_ui::{ResultRow, ViewModel};

use crate::{BatchState, QueryPipeline, ResultBatch};

/// Bound on the startup handshake with a child interpreter, in milliseconds.
/// A liveness guard: a worker that never answers becomes a recorded unavailable
/// plugin rather than a launcher that hangs on startup.
const STARTUP_BUDGET_MS: Millis = 30_000;

/// Bound on one modern `suggest` call, in milliseconds.
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

/// One loaded modern plugin: its host identity and the shared-worker key that
/// dispatches its `suggest` calls.
#[derive(Debug, Clone)]
struct LoadedPlugin {
    plugin: PluginId,
    key: WorkerKey,
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
    workers: BTreeMap<WorkerKey, ModernWorker>,
    failures: Vec<(PluginId, String)>,
    /// Plugins already recorded as a dispatch failure, so a worker that dies is
    /// recorded once rather than every keystroke (this bounds `failures`).
    recorded: std::collections::BTreeSet<PluginId>,
}

impl std::fmt::Debug for ModernWorkerPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModernWorkerPool")
            .field("workers", &self.workers.keys().collect::<Vec<_>>())
            .field("failures", &self.failures)
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
    plugins: Vec<PluginId>,
    unavailable: Vec<ModernUnavailable>,
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
    ) -> Self {
        let mut provider = Self {
            pool: ModernWorkerPool::default(),
            loaded: Vec::new(),
            plugins: Vec::new(),
            unavailable: Vec::new(),
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

            for dir in dirs {
                provider.register_plugin_dir(pipeline, &index, &store, &sdk, &dir);
            }
        }

        provider
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

    /// Loads one candidate `<dir>/crikey.toml`, resolves its environment, spawns
    /// (or reuses) its worker and registers it with the pipeline. Any failure is
    /// recorded and the function returns without disturbing other plugins.
    fn register_plugin_dir(
        &mut self,
        pipeline: &mut QueryPipeline,
        index: &PackageIndex,
        store: &EnvironmentStore,
        sdk: &Path,
        dir: &Path,
    ) {
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

        // The interpreter is discovered per plugin so its own requires-python
        // gates it; a version that does not satisfy the constraint is a recorded
        // failure, never a silent fall-through.
        let interpreter =
            match discover_interpreter(&RuntimeProfile::Bundled, &RequiresPython(requires_python.clone())) {
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

        // Spawn a worker for this (environment, entrypoint) if none is live yet;
        // a truly identical plugin already loaded reuses it.
        if !self.pool.workers.contains_key(&key) {
            let options = WorkerOptions::new(plugin.clone(), entrypoint, import_path)
                .with_startup_timeout_ms(STARTUP_BUDGET_MS)
                .with_call_timeout_ms(CALL_BUDGET_MS)
                .with_shutdown_timeout_ms(SHUTDOWN_BUDGET_MS);
            let worker = match ModernWorker::spawn(&interpreter, options) {
                Ok(worker) => worker,
                Err(error) => {
                    self.record_unavailable(
                        package,
                        Some(plugin),
                        format!("the modern worker did not start: {error}"),
                    );
                    return;
                }
            };
            self.pool.workers.insert(key.clone(), worker);
        }

        // Register with the pipeline under the manifest-derived modern policy so
        // host debouncing and gating are the pipeline's own, not a second path.
        let policy = crate::query_pipeline::plugin_policy_from_manifest(&manifest);
        if let Err(error) = pipeline.register_plugin(plugin.clone(), policy) {
            self.record_unavailable(
                package,
                Some(plugin),
                format!("the query pipeline refused the modern plugin: {error:?}"),
            );
            // Reap the worker rather than leak a child for a plugin the pipeline
            // will never dispatch — unless another loaded plugin shares it.
            if !self.loaded.iter().any(|loaded| loaded.key == key) {
                if let Some(worker) = self.pool.workers.remove(&key) {
                    let _ = worker.shutdown();
                }
            }
            return;
        }

        self.plugins.push(plugin.clone());
        self.loaded.push(LoadedPlugin { plugin, key });
    }

    fn record_unavailable(&mut self, package: String, plugin: Option<PluginId>, reason: String) {
        self.unavailable.push(ModernUnavailable {
            package,
            plugin,
            reason,
        });
    }

    /// The modern plugins that loaded and are being served through the pipeline.
    pub fn plugins(&self) -> &[PluginId] {
        &self.plugins
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
        let mut suggestions = self.collect_suggestions(query, generation);

        let mut tick_succeeded = true;
        let mut delivered = true;
        let mut at = now;
        // Advance the pipeline until its registered plugins have been dispatched
        // for this generation. Modern plugins debounce, so a query that is not a
        // leading edge dispatches on a later timer wake-up; the loop follows the
        // scheduler's own wake-ups and is bounded by construction.
        for _ in 0..64 {
            let tick = pipeline.tick(at);
            if !tick.errors.is_empty() {
                tick_succeeded = false;
            }
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
                let items = suggestions.remove(&request.plugin).unwrap_or_default();
                let admitted = pipeline
                    .deliver(
                        ResultBatch {
                            generation: request.generation,
                            plugin: request.plugin.clone(),
                            state: BatchState::Final,
                            items,
                        },
                        at,
                    )
                    .is_ok();
                delivered &= admitted;
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

        let frame = pipeline.present(at);
        let presentation_succeeded = pipeline.take_errors().is_empty();
        if !tick_succeeded || !delivered || !presentation_succeeded {
            return None;
        }
        frame.filter(|frame| frame.generation == generation)
    }

    /// Cooperative teardown of every modern worker (spec 24.3).
    pub fn shutdown(&mut self, _now: Millis) {
        for (_, worker) in std::mem::take(&mut self.pool.workers) {
            // Best effort: the child is reaped on drop even if orderly shutdown
            // reports an error, so no worker is leaked.
            let _ = worker.shutdown();
        }
    }

    /// Runs every loaded plugin's `suggest` in its child process and groups the
    /// resulting items by owning plugin. Every failure — a dead worker, a crash
    /// mid-callback, or a plugin-raised error — is contained as a recorded
    /// dispatch failure and contributes no items.
    fn collect_suggestions(&mut self, query: &str, generation: Generation) -> BTreeMap<PluginId, Vec<Item>> {
        let request = SuggestRequest {
            generation: generation.get(),
            text: query.to_owned(),
            normalized: query.to_owned(),
            selected_item_id: None,
        };

        // Snapshot the loaded set so the pool can be mutated while iterating.
        let targets: Vec<(PluginId, WorkerKey)> = self
            .loaded
            .iter()
            .map(|loaded| (loaded.plugin.clone(), loaded.key.clone()))
            .collect();

        let mut by_plugin: BTreeMap<PluginId, Vec<Item>> = BTreeMap::new();

        for (plugin, key) in targets {
            // A worker that has already died stays dead: skip it, leave the
            // plugin cleanly unavailable, and record the failure at most once
            // rather than re-dispatching to a corpse every keystroke.
            let alive = match self.pool.workers.get(&key) {
                Some(worker) => worker.is_alive(),
                None => continue,
            };
            if !alive {
                self.pool
                    .record_dispatch_failure(plugin, "the modern worker is no longer alive".to_owned());
                self.pool.workers.remove(&key);
                continue;
            }

            let answer = self
                .pool
                .workers
                .get_mut(&key)
                .expect("the worker was live a moment ago")
                .suggest(&request);
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
                    by_plugin.insert(plugin, items);
                }
                Err(error) => {
                    // A crashed or unresponsive worker is contained: record the
                    // failure once so a diagnostic can name the plugin, and drop
                    // the dead worker so the next query skips it rather than
                    // re-dispatching to a dead process.
                    self.pool.record_dispatch_failure(plugin, error.to_string());
                    self.pool.workers.remove(&key);
                }
            }
        }

        by_plugin
    }
}

/// One query handed to the modern supervisor thread, tagged with the search
/// generation it belongs to.
#[derive(Debug)]
struct ModernJob {
    generation: Generation,
    query: String,
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
#[derive(Debug)]
struct ModernRequestSlot {
    job: Option<ModernJob>,
    stop: bool,
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
    outcome: Arc<Mutex<Option<ViewModel>>>,
    /// Search generation the UI last submitted. The supervisor re-reads it
    /// before publishing and drops any answer that is no longer current.
    current: Arc<AtomicU64>,
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
        let has_plugins = !provider.plugins().is_empty();
        let mailbox = Arc::new((
            Mutex::new(ModernRequestSlot {
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
            .name("crikey-modern".to_owned())
            .spawn(move || {
                let (lock, cvar) = &*thread_mailbox;
                let mut last_now: Millis = 0;
                loop {
                    let job = {
                        let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
                        loop {
                            if slot.stop {
                                // Cooperative teardown of every child before the
                                // thread exits, so no worker is leaked (spec
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

                    // The blocking child interpreter calls happen here, on this
                    // thread — never on the caller's. Stale answers are refused
                    // at the pipeline's intake boundary inside `drive_query`.
                    let modern = provider.drive_query(&mut pipeline, &job.query, job.now);

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
        // Record the live generation first, so an answer for a query this call
        // supersedes is dropped rather than shown even if it finishes late.
        self.current.store(generation.get(), Ordering::Release);
        let (lock, cvar) = &*self.mailbox;
        let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
        if slot.stop {
            return;
        }
        slot.job = Some(ModernJob {
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
}

impl Drop for ModernDriver {
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
