//! Process-scoped P7 package jobs. One worker serializes filesystem/config
//! mutations and bounds execution below the four-worker ceiling. Closing a
//! settings view does not cancel jobs. Drop/shutdown joins the worker, including
//! noninterruptible installer commit/rollback work.
//!
//! Resident installation requires an application-owned `PackageOwner` and the
//! already-held launcher lock. No default owner or boolean "stopped" token is
//! provided. The owner must close admission, drain pages/actions/queries, reap
//! worker process trees, and acknowledge that barrier before returning a lease.
//! The lease stays held through disk reconciliation; reopening admission after
//! a changed package is a next-launch decision, not implicit hot reload.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use crikey_config::ConfigStore;
use crikey_core::PluginId;
use crikey_legacy_compat::{PackageLoader, PACKAGE_ARCHIVE_EXTENSION};
use crikey_package_manager::{
    index_max_age, index_urls, search, InstallSource, InstalledPlugin, LauncherLock, PackageError,
    PluginIndexClient, PluginInstaller, SignaturePolicy, TrustStore, UnsignedPolicy,
    KEY_INDEX_MAX_AGE_SECONDS, KEY_INDEX_URLS, KEY_UNSIGNED_POLICY,
};
use crikey_platform::{PluginKind, StandardDirectories};
use crikey_plugin_model::{Manifest, Runtime};

static NEXT_JOB_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const QUEUED: usize = 32;
const RETAINED: usize = 128;
const RETAINED_BYTES: usize = 1024 * 1024;
// Queue/root descriptors and containers, all discovery-root path allocations,
// plus one maximum input per queued/working job. Admission bounds allocation
// capacities, not just the lengths of their current contents.
const ROOT_LIMIT: usize = 32;
const STORAGE_OVERHEAD_BYTES: usize = 16 * 1024
    + ROOT_LIMIT * (INPUT_BYTES + std::mem::size_of::<(PluginKind, PathBuf)>())
    + (QUEUED + 1) * INPUT_BYTES;
// Reserve a full terminal slot at admission; unused bytes are never lent to
// another job. 128 slots together consume at most the one aggregate ceiling.
const JOB_BYTES: usize = (RETAINED_BYTES - STORAGE_OVERHEAD_BYTES) / RETAINED;
const REPORT_BYTES: usize = 8192;
const INPUT_BYTES: usize = 4096;
const INVENTORY_LIMIT: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct JobId(u64);

/// Inventory identity or index identity, never a caller-selected filesystem path.
#[derive(Clone)]
pub enum PackageShowTarget {
    Index { id: String },
    Local { kind: PluginKind, id: String },
}

impl std::fmt::Debug for PackageShowTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PackageShowTarget")
            .field(
                "source",
                &match self {
                    Self::Index { .. } => "index",
                    Self::Local { .. } => "local",
                },
            )
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub enum PackageOperation {
    Search(String),
    List,
    Show(PackageShowTarget),
    Install(InstallSource),
    InstallIndexed(String),
    Remove { kind: PluginKind, id: String },
    Enable(String),
    Disable(String),
    UpdateIndex,
    Doctor(Option<String>),
}

impl PackageOperation {
    fn needs_owner(&self) -> bool {
        matches!(
            self,
            Self::Install(_) | Self::InstallIndexed(_) | Self::Remove { .. }
        )
    }
    fn target(&self) -> Option<Arc<str>> {
        match self {
            Self::Show(PackageShowTarget::Index { id })
            | Self::InstallIndexed(id)
            | Self::Enable(id)
            | Self::Disable(id)
            | Self::Doctor(Some(id)) => Some(Arc::from(id.as_str())),
            Self::Remove { kind, id } => Some(Arc::from(format!("{}.{}", kind.directory_name(), id))),
            Self::Show(PackageShowTarget::Local { kind, id }) => {
                Some(Arc::from(format!("{}.{}", kind.directory_name(), id)))
            }
            // Never retain a source URL (which may embed credentials), source
            // path or arbitrary search text in diagnostic/event metadata.
            _ => None,
        }
    }
    fn valid(&self) -> bool {
        let bounded = |s: &String| {
            !s.trim().is_empty() && s.capacity() <= INPUT_BYTES && !s.chars().any(char::is_control)
        };
        match self {
            Self::Search(text) => text.capacity() <= INPUT_BYTES,
            Self::Show(PackageShowTarget::Index { id })
            | Self::InstallIndexed(id)
            | Self::Enable(id)
            | Self::Disable(id) => bounded(id),
            Self::Show(PackageShowTarget::Local { id, .. }) | Self::Remove { id, .. } => {
                bounded(id) && id != "." && id != ".." && !id.contains(['/', '\\'])
            }
            Self::Doctor(Some(id)) => bounded(id),
            Self::Install(InstallSource::Url(url)) => bounded(url),
            Self::Install(
                InstallSource::Archive(path)
                | InstallSource::Directory(path)
                | InstallSource::LegacyPackage(path),
            ) => !path.as_os_str().is_empty() && path.capacity() <= INPUT_BYTES,
            _ => true,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::Search(_) => "search",
            Self::List => "list",
            Self::Show(_) => "show",
            Self::Install(_) | Self::InstallIndexed(_) => "install",
            Self::Remove { .. } => "remove",
            Self::Enable(_) => "enable",
            Self::Disable(_) => "disable",
            Self::UpdateIndex => "update-index",
            Self::Doctor(_) => "doctor",
        }
    }
}

impl std::fmt::Debug for PackageOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PackageOperation")
            .field("operation", &self.name())
            .finish_non_exhaustive()
    }
}

/// The resident configuration owner must reload through its current session
/// overrides and profile selection, not reconstruct a disk-only store. Called
/// on the package worker for admission policy and every effective-state reload.
/// Implementations must serialize with the host's configuration writer.
/// The owner can use `ConfigStore::load_with_overrides` with its current session
/// arguments and administrator-policy path, which applies layer 7 before
/// selecting a profile. A disk-only load or stale startup clone is not an
/// implementation of this authority.
pub trait PackageConfiguration: Send + Sync {
    fn load(&self) -> Result<ConfigStore, JobError>;
    /// Fresh read, enabled-state persistence and effective reload must be one
    /// serialized resident-authority transaction shared with settings writes.
    /// Return only that transaction's effective store, retaining session inputs.
    /// If persistence succeeded but reload/publication failed, return code
    /// `configuration-persisted-reload`; callers must not report an undone write.
    fn set_plugin_enabled(&self, plugin: &PluginId, enabled: bool) -> Result<ConfigStore, JobError>;
}

/// Only the composition/supervisor owner implements this interface. Acquiring
/// must synchronously acknowledge real shutdown, not enqueue a stop request.
/// The returned lease owns the admission barrier and may not reopen it on Drop
/// after a mutation; reconcile explicitly decides the safe post-operation state.
pub trait PackageOwner: Send + Sync {
    fn quiesce(&self) -> Result<Box<dyn QuiescentPlugins>, JobError>;
}

pub trait QuiescentPlugins: Send {
    /// Called with the installer-validated exact bare package id, before swap.
    /// Must refuse if any runtime with that id still has a live process/page.
    fn acknowledge_stopped(&mut self, id: &str) -> Result<(), PackageError>;
    /// Reconcile actual disk state even when install or rollback failed. Never
    /// restart an old worker against changed files. Failures stay visible and
    /// admission stays closed until the owner can recover safely.
    /// The fresh configuration comes from the same resident authority used for
    /// policy admission. An unavailable authority is an error, never permission
    /// to reconstruct disk-only state or reopen admission with lost overrides.
    fn reconcile(
        &mut self,
        installed: Result<&[InstalledPlugin], &PackageError>,
        configuration: Result<&ConfigStore, &JobError>,
    ) -> Result<(), JobError>;
}

/// Capability held by the application, not constructed from UI confirmation.
/// Construction alone makes no stopped-worker claim: every operation must
/// obtain a fresh acknowledged owner lease before it may enter the installer.
pub struct ResidentPackageAccess {
    lock: Arc<LauncherLock>,
    owner: Arc<dyn PackageOwner>,
}
impl std::fmt::Debug for ResidentPackageAccess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResidentPackageAccess")
            .finish_non_exhaustive()
    }
}
impl ResidentPackageAccess {
    pub fn new(lock: Arc<LauncherLock>, owner: Arc<dyn PackageOwner>) -> Self {
        Self { lock, owner }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobError {
    pub code: &'static str,
    pub detail: Arc<str>,
}
impl JobError {
    /// Owner errors must contain no secrets, source excerpts or credentials.
    pub fn new(code: &'static str, detail: &str) -> Self {
        let mut end = detail.len().min(REPORT_BYTES / 2);
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            code,
            detail: Arc::from(&detail[..end]),
        }
    }
}

/// Structured action targets for graphical management. Display prose is never
/// parsed to choose which runtime/plugin is enabled or removed.
#[derive(Debug)]
pub struct PackageRow {
    pub package_id: String,
    pub plugin: Option<PluginId>,
    pub kind: Option<PluginKind>,
    /// Empty for inventory rows whose label is their exact package id.
    pub name: String,
    pub version: String,
    pub summary: String,
    pub installed: bool,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone)]
pub enum JobOutcome {
    /// UI must surface `details_truncated` explicitly. Capped reporting never
    /// changes a completed mutation into a failure or implies a rollback;
    /// structured search/list rows are refused rather than silently partial.
    Completed {
        report: Arc<str>,
        rows: Option<Arc<[PackageRow]>>,
        restart_required: bool,
        details_truncated: bool,
    },
    Failed {
        error: JobError,
        reconciliation: Option<JobError>,
        restart_required: bool,
    },
    Cancelled,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPhase {
    Queued,
    Working,
    Commit,
    Reconciling,
    Terminal,
}
#[derive(Debug, Clone)]
pub struct JobSnapshot {
    pub id: JobId,
    pub sequence: u64,
    pub operation: &'static str,
    pub target: Option<Arc<str>>,
    pub phase: JobPhase,
    /// None means indeterminate; no fabricated percentage is emitted.
    pub measured_progress: Option<(u64, u64)>,
    pub outcome: Option<JobOutcome>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobRefusal {
    InvalidInput,
    Saturated,
    Shutdown,
    IdExhausted,
    ResidentOwnerRequired,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancellationStatus {
    Requested,
    AlreadyTerminal,
    NotCancellable,
    Unknown,
}

struct Record {
    snapshot: JobSnapshot,
    cancel: bool,
    dirty: bool,
}
struct State {
    shutdown: bool,
    queue: VecDeque<(JobId, PackageOperation)>,
    records: BTreeMap<JobId, Record>,
}
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
    wake: Arc<dyn Fn() + Send + Sync>,
}
impl Shared {
    fn notify(&self) {
        // A host wake failure must not unwind the worker and strand accepted
        // terminal slots. Snapshots remain drainable even if its callback fails.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.wake)()));
    }
}

pub struct PackageJobs {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    resident: bool,
}
impl std::fmt::Debug for PackageJobs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PackageJobs").finish_non_exhaustive()
    }
}
impl PackageJobs {
    /// The supplied roots must be the same discovery roots used by providers.
    /// All I/O (including config/trust discovery) runs on the worker. Without
    /// resident access read/config jobs work; install/remove refuse admission.
    pub fn new(
        directories: StandardDirectories,
        roots: Vec<(PluginKind, PathBuf)>,
        resident: Option<ResidentPackageAccess>,
        configuration: Arc<dyn PackageConfiguration>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, std::io::Error> {
        if roots.capacity() > ROOT_LIMIT || roots.iter().any(|(_, root)| root.capacity() > INPUT_BYTES) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "package discovery roots exceed bounds",
            ));
        }
        let has_resident = resident.is_some();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                shutdown: false,
                queue: VecDeque::new(),
                records: BTreeMap::new(),
            }),
            ready: Condvar::new(),
            wake,
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("crikey-package-jobs".into())
            .spawn(move || {
                worker_loop(worker_shared, directories, roots, resident, configuration);
            })?;
        Ok(Self {
            shared,
            worker: Some(worker),
            resident: has_resident,
        })
    }

    pub fn submit(&self, operation: PackageOperation) -> Result<JobId, JobRefusal> {
        if !operation.valid() {
            return Err(JobRefusal::InvalidInput);
        }
        if operation.needs_owner() && !self.resident {
            return Err(JobRefusal::ResidentOwnerRequired);
        }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown {
            return Err(JobRefusal::Shutdown);
        }
        if state.queue.len() >= QUEUED || state.records.len() >= RETAINED {
            return Err(JobRefusal::Saturated);
        }
        let target = operation.target();
        if snapshot_metadata_bytes(operation.name(), target.as_deref()) + 512 > JOB_BYTES {
            return Err(JobRefusal::InvalidInput);
        }
        let previous = NEXT_JOB_ID
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |next| next.checked_add(1),
            )
            .map_err(|_| JobRefusal::IdExhausted)?;
        let id = JobId(previous + 1);
        state.records.insert(
            id,
            Record {
                snapshot: JobSnapshot {
                    id,
                    sequence: 0,
                    operation: operation.name(),
                    target,
                    phase: JobPhase::Queued,
                    measured_progress: None,
                    outcome: None,
                },
                cancel: false,
                dirty: true,
            },
        );
        state.queue.push_back((id, operation));
        drop(state);
        self.shared.ready.notify_one();
        self.shared.notify();
        Ok(id)
    }

    pub fn cancel(&self, id: JobId) -> CancellationStatus {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(record) = state.records.get_mut(&id) else {
            return CancellationStatus::Unknown;
        };
        match record.snapshot.phase {
            JobPhase::Terminal => CancellationStatus::AlreadyTerminal,
            JobPhase::Commit | JobPhase::Reconciling => CancellationStatus::NotCancellable,
            _ => {
                record.cancel = true;
                CancellationStatus::Requested
            }
        }
    }
    pub fn snapshot(&self, id: JobId) -> Option<JobSnapshot> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .records
            .get(&id)
            .map(|r| r.snapshot.clone())
    }
    /// Bytes charged to the one retained-snapshot budget, including targets,
    /// structured rows, reports, errors, and conservative allocation overhead.
    pub fn retained_bytes(&self) -> usize {
        STORAGE_OVERHEAD_BYTES
            + self
                .shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .records
                .values()
                .map(|record| retained_bytes(&record.snapshot))
                .sum::<usize>()
    }
    /// Coalesced snapshots are authoritative. A terminal remains pending until
    /// drained, and retained until explicit forget; no terminal eviction.
    pub fn drain_events(&self, maximum: usize) -> Vec<JobSnapshot> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .records
            .values_mut()
            .filter(|r| r.dirty)
            .take(maximum.min(RETAINED))
            .map(|r| {
                r.dirty = false;
                r.snapshot.clone()
            })
            .collect()
    }
    pub fn forget(&self, id: JobId) -> bool {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .records
            .get(&id)
            .is_some_and(|r| r.snapshot.phase == JobPhase::Terminal && !r.dirty)
        {
            state.records.remove(&id);
            true
        } else {
            false
        }
    }
    pub fn shutdown(&mut self) -> Result<(), JobError> {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.shutdown = true;
            for record in state.records.values_mut() {
                if matches!(record.snapshot.phase, JobPhase::Queued | JobPhase::Working) {
                    record.cancel = true;
                }
            }
        }
        self.shared.ready.notify_one();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                let error = JobError::new("worker-exit", "package worker exited unexpectedly; inspect and reconcile actual package/configuration state");
                let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
                for record in state
                    .records
                    .values_mut()
                    .filter(|record| record.snapshot.phase != JobPhase::Terminal)
                {
                    record.snapshot.phase = JobPhase::Terminal;
                    record.snapshot.sequence = record
                        .snapshot
                        .sequence
                        .checked_add(1)
                        .expect("terminal sequence reserved");
                    let outcome = JobOutcome::Failed {
                        error: error.clone(),
                        reconciliation: Some(error.clone()),
                        restart_required: true,
                    };
                    record.snapshot.outcome = Some(fit_outcome(&record.snapshot, outcome));
                    record.dirty = true;
                }
                drop(state);
                self.shared.notify();
                return Err(error);
            }
        }
        Ok(())
    }
}
impl Drop for PackageJobs {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

struct Context {
    shared: Arc<Shared>,
    id: JobId,
}
impl Context {
    /// The check and transition share cancel's mutex: cancellation either wins
    /// before the safe point or observes NotCancellable, never both.
    fn phase(&self, phase: JobPhase) -> Result<(), JobError> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let record = state.records.get_mut(&self.id).expect("admitted job retained");
        if record.cancel {
            return Err(JobError::new("cancelled", "cancelled at safe point"));
        }
        if record.snapshot.phase == phase {
            return Ok(());
        }
        record.snapshot.phase = phase;
        record.snapshot.sequence = record
            .snapshot
            .sequence
            .checked_add(1)
            .filter(|next| *next < u64::MAX)
            .ok_or_else(|| JobError::new("sequence-exhausted", "job event sequence exhausted"))?;
        let notify = !record.dirty;
        record.dirty = true;
        drop(state);
        if notify {
            self.shared.notify();
        }
        Ok(())
    }
    fn finish(&self, outcome: JobOutcome) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let record = state.records.get_mut(&self.id).expect("admitted job retained");
        let outcome = fit_outcome(&record.snapshot, outcome);
        record.snapshot.phase = JobPhase::Terminal;
        record.snapshot.sequence = record
            .snapshot
            .sequence
            .checked_add(1)
            .expect("terminal sequence reserved");
        record.snapshot.outcome = Some(outcome);
        let notify = !record.dirty;
        record.dirty = true;
        drop(state);
        if notify {
            self.shared.notify();
        }
    }
}

fn worker_loop(
    shared: Arc<Shared>,
    directories: StandardDirectories,
    roots: Vec<(PluginKind, PathBuf)>,
    resident: Option<ResidentPackageAccess>,
    configuration: Arc<dyn PackageConfiguration>,
) {
    loop {
        let next = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.queue.is_empty() && !state.shutdown {
                state = shared.ready.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            state.queue.pop_front()
        };
        let Some((id, operation)) = next else {
            return;
        };
        let context = Context {
            shared: Arc::clone(&shared),
            id,
        };
        // Contain a worker panic so all admitted jobs still receive a terminal.
        // A panic during mutation is not reported as an undone operation.
        let mutation = operation.needs_owner()
            || matches!(
                &operation,
                PackageOperation::Enable(_) | PackageOperation::Disable(_)
            );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            context.phase(JobPhase::Working)?;
            execute(
                &context,
                &directories,
                &roots,
                resident.as_ref(),
                configuration.as_ref(),
                operation,
            )
        }));
        let outcome = match result {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) if error.code == "cancelled" => JobOutcome::Cancelled,
            Ok(Err(error)) => JobOutcome::Failed {
                error,
                reconciliation: None,
                restart_required: mutation,
            },
            Err(_) => JobOutcome::Failed {
                error: JobError::new(
                    "worker-panic",
                    "package worker failed; inspect actual disk state before retrying",
                ),
                reconciliation: Some(JobError::new(
                    "reconciliation-required",
                    "owner must keep package admission closed and reconcile",
                )),
                restart_required: mutation,
            },
        };
        context.finish(outcome);
    }
}

fn config(configuration: &dyn PackageConfiguration) -> Result<ConfigStore, JobError> {
    // The authority's errors must already omit TOML source excerpts/secrets.
    // There is deliberately no disk/default fallback if resident loading fails.
    configuration.load()
}
fn package_error(error: PackageError) -> JobError {
    // Package manifest/parser prose may contain secret defaults. Preserve the
    // actionable error class without exporting untrusted raw source excerpts.
    match error {
        PackageError::Io(error) => JobError::new("io", &format!("filesystem/transport operation failed ({:?}): retry after checking access and available space", error.kind())),
        PackageError::Signature(_) => JobError::new("signature", "package signature refused: verify signer trust, signature integrity and configured unsigned policy"),
        PackageError::IndexSignature(_) => JobError::new("index-signature", "index signature refused: verify the configured publisher trust and detached signature"),
        PackageError::HashMismatch(_) => JobError::new("digest", "package digest mismatch; do not install these bytes; refresh the publisher index"),
        PackageError::LauncherRunning { .. } => JobError::new("launcher-lock", "another launcher owns this installation; no files were authorized for replacement"),
        PackageError::IncompatiblePlatform | PackageError::MissingEntrypoint { .. } => JobError::new("compatibility", "package has no usable entrypoint for this platform and architecture"),
        PackageError::Manifest(_) => JobError::new("manifest", "package manifest is invalid; repair the package declaration"),
        PackageError::MalformedArchive(_) => JobError::new("archive", "package archive is malformed or violates package bounds/path policy"),
        PackageError::SourceUnavailable(_) => JobError::new("source", "package source is unavailable; verify the source exists and is accessible"),
        PackageError::MalformedIndex(_) => JobError::new("index", "index document is malformed or exceeds supported bounds"),
        PackageError::Install(detail) => JobError::new("installation", &detail),
        _ => JobError::new("package-operation", "package operation failed; inspect installation and retained previous version before retrying"),
    }
}
fn index(directories: &StandardDirectories, config: &ConfigStore) -> Result<PluginIndexClient, JobError> {
    let urls = index_urls(config.get(KEY_INDEX_URLS));
    if urls.is_empty() {
        return Err(JobError::new("no-index", "no plugin indexes configured"));
    }
    if urls.len() > 32 {
        return Err(JobError::new("index-limit", "more than 32 configured indexes"));
    }
    PluginIndexClient::new(
        directories,
        urls,
        index_max_age(config.get(KEY_INDEX_MAX_AGE_SECONDS)),
    )
    .map_err(package_error)
}
fn append(report: &mut String, line: &str) -> Result<(), JobError> {
    if line.len().saturating_add(1) > REPORT_BYTES.saturating_sub(report.len()) {
        return Err(JobError::new(
            "report-limit",
            "result exceeds 8 KiB; narrow the search or select one plugin",
        ));
    }
    report.push_str(line);
    report.push('\n');
    Ok(())
}
fn completed(mut report: String, restart_required: bool) -> JobOutcome {
    let details_truncated = report.len() > REPORT_BYTES;
    if details_truncated {
        report.truncate(boundary(&report, REPORT_BYTES));
    }
    JobOutcome::Completed {
        report: report.into(),
        rows: None,
        restart_required,
        details_truncated,
    }
}

fn completed_rows(report: String, rows: Vec<PackageRow>) -> JobOutcome {
    // Metadata/targets are charged by fit_outcome before terminal publication.
    // This early bound also prevents two independent 8 KiB payload allowances.
    if report
        .len()
        .saturating_add(rows_bytes(&rows))
        .saturating_add(2 * ARC_BYTES)
        > JOB_BYTES
    {
        return JobOutcome::Failed {
            error: JobError::new(
                "result-budget",
                "combined report and rows exceed retained capacity; narrow the query",
            ),
            reconciliation: None,
            restart_required: false,
        };
    }
    JobOutcome::Completed {
        report: report.into(),
        rows: Some(rows.into()),
        restart_required: false,
        details_truncated: false,
    }
}

const ARC_BYTES: usize = 2 * std::mem::size_of::<usize>();

fn boundary(text: &str, maximum: usize) -> usize {
    let mut end = text.len().min(maximum);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn snapshot_metadata_bytes(operation: &str, target: Option<&str>) -> usize {
    // Conservative record/B-tree node allowance, including inline enum/string
    // descriptors; heap text and shared allocation headers are charged below.
    3 * std::mem::size_of::<Record>()
        + 256
        + operation.len()
        + target.map_or(0, |target| target.len() + ARC_BYTES)
}

fn rows_bytes(rows: &[PackageRow]) -> usize {
    rows.iter().fold(
        rows.len().saturating_mul(std::mem::size_of::<PackageRow>()),
        |bytes, row| {
            bytes
                .saturating_add(row.package_id.capacity())
                .saturating_add(row.plugin.as_ref().map_or(0, |plugin| plugin.0.capacity()))
                .saturating_add(row.name.capacity())
                .saturating_add(row.version.capacity())
                .saturating_add(row.summary.capacity())
        },
    )
}

fn outcome_bytes(outcome: &JobOutcome) -> usize {
    match outcome {
        JobOutcome::Completed { report, rows, .. } => {
            report.len() + ARC_BYTES + rows.as_ref().map_or(0, |rows| rows_bytes(rows) + ARC_BYTES)
        }
        JobOutcome::Failed {
            error,
            reconciliation,
            ..
        } => {
            error.code.len()
                + error.detail.len()
                + ARC_BYTES
                + reconciliation
                    .as_ref()
                    .map_or(0, |error| error.code.len() + error.detail.len() + ARC_BYTES)
        }
        JobOutcome::Cancelled => 0,
    }
}

fn retained_bytes(snapshot: &JobSnapshot) -> usize {
    snapshot_metadata_bytes(snapshot.operation, snapshot.target.as_deref())
        + snapshot.outcome.as_ref().map_or(0, outcome_bytes)
}

fn fit_error(error: JobError, available: usize) -> JobError {
    let code = if error.code.len() <= 64 {
        error.code
    } else {
        "owner-error"
    };
    let detail_bytes = available.saturating_sub(code.len() + ARC_BYTES);
    let end = boundary(&error.detail, detail_bytes);
    JobError {
        code,
        detail: if end == error.detail.len() {
            error.detail
        } else {
            Arc::from(&error.detail[..end])
        },
    }
}

fn fit_outcome(snapshot: &JobSnapshot, outcome: JobOutcome) -> JobOutcome {
    let available = JOB_BYTES.saturating_sub(snapshot_metadata_bytes(
        snapshot.operation,
        snapshot.target.as_deref(),
    ));
    match outcome {
        JobOutcome::Completed {
            mut report,
            mut rows,
            restart_required,
            mut details_truncated,
        } => {
            if report
                .len()
                .saturating_add(ARC_BYTES)
                .saturating_add(rows.as_ref().map_or(0, |rows| rows_bytes(rows) + ARC_BYTES))
                > available
            {
                if rows.is_some() && !restart_required {
                    return fit_outcome(
                        snapshot,
                        JobOutcome::Failed {
                            error: JobError::new(
                                "result-budget",
                                "results exceed this job's reserved retained capacity; narrow the query",
                            ),
                            reconciliation: None,
                            restart_required: false,
                        },
                    );
                }
                // A successful mutation remains completed. Reporting capacity
                // cannot fabricate an installation failure or imply rollback.
                rows = None;
                let end = boundary(&report, available.saturating_sub(ARC_BYTES));
                report = Arc::from(&report[..end]);
                details_truncated = true;
            }
            JobOutcome::Completed {
                report,
                rows,
                restart_required,
                details_truncated,
            }
        }
        JobOutcome::Failed {
            error,
            reconciliation,
            restart_required,
        } => {
            let primary_bytes = if reconciliation.is_some() {
                available / 2
            } else {
                available
            };
            JobOutcome::Failed {
                error: fit_error(error, primary_bytes),
                reconciliation: reconciliation.map(|error| fit_error(error, available - primary_bytes)),
                restart_required,
            }
        }
        JobOutcome::Cancelled => JobOutcome::Cancelled,
    }
}

fn execute(
    context: &Context,
    directories: &StandardDirectories,
    roots: &[(PluginKind, PathBuf)],
    resident: Option<&ResidentPackageAccess>,
    configuration: &dyn PackageConfiguration,
    operation: PackageOperation,
) -> Result<JobOutcome, JobError> {
    let store = config(configuration)?;
    match operation {
        PackageOperation::Search(query) => {
            let snapshots = load_indexes(context, &index(directories, &store)?, false)?;
            let mut report = String::new();
            let mut rows = Vec::new();
            for hit in search(&snapshots, &query) {
                context.phase(JobPhase::Working)?;
                append(
                    &mut report,
                    &format!(
                        "{} {} {} {}",
                        hit.entry.id, hit.entry.version, hit.entry.name, hit.entry.summary
                    ),
                )?;
                let kind = hit.entry.parsed_runtime().and_then(runtime_kind);
                rows.push(PackageRow {
                    package_id: hit.entry.id,
                    plugin: None,
                    kind,
                    name: hit.entry.name,
                    version: hit.entry.version,
                    summary: hit.entry.summary,
                    installed: false,
                    enabled: None,
                });
            }
            context.phase(JobPhase::Working)?;
            Ok(completed_rows(report, rows))
        }
        PackageOperation::Show(PackageShowTarget::Local { kind, id }) => {
            local_details(context, directories, roots, kind, &id)
        }
        PackageOperation::Show(PackageShowTarget::Index { id: wanted }) => {
            let snapshots = load_indexes(context, &index(directories, &store)?, false)?;
            let mut report = String::new();
            for snapshot in &snapshots {
                context.phase(JobPhase::Working)?;
                let Some(entry) = snapshot.document.entry(&wanted) else {
                    continue;
                };
                append(
                    &mut report,
                    &format!("{} {} {} {}", entry.id, entry.version, entry.name, entry.summary),
                )?;
                append(
                    &mut report,
                    &format!(
                        "runtime={} homepage={} licence={} digest={} signer={}",
                        entry.runtime,
                        entry.homepage.as_deref().unwrap_or("-"),
                        entry.licence.as_deref().unwrap_or("-"),
                        entry.package_digest,
                        entry.signer_fingerprint
                    ),
                )?;
                append(
                    &mut report,
                    if matches!(
                        snapshot.freshness,
                        crikey_package_manager::Freshness::Stale { .. }
                    ) {
                        "index=stale-cache"
                    } else {
                        "index=current"
                    },
                )?;
            }
            if report.is_empty() {
                return Err(JobError::new(
                    "not-found",
                    "plugin is not listed by configured indexes",
                ));
            }
            context.phase(JobPhase::Working)?;
            Ok(completed(report, false))
        }
        PackageOperation::UpdateIndex => {
            let snapshots = load_indexes(context, &index(directories, &store)?, true)?;
            let mut report = String::new();
            for snapshot in snapshots {
                let freshness = if matches!(
                    snapshot.freshness,
                    crikey_package_manager::Freshness::Stale { .. }
                ) {
                    "stale-cache"
                } else {
                    "current"
                };
                append(
                    &mut report,
                    &format!(
                        "entries={} freshness={freshness}",
                        snapshot.document.plugins.len()
                    ),
                )?;
            }
            Ok(completed(report, false))
        }
        PackageOperation::Install(source) => {
            let unsigned = store
                .get(KEY_UNSIGNED_POLICY)
                .map(UnsignedPolicy::parse)
                .transpose()
                .map_err(|_| JobError::new("unsigned-policy", "invalid packages.unsigned-policy"))?
                .unwrap_or_default();
            let trust = TrustStore::load(directories)
                .map_err(|_| JobError::new("trust", "cannot load package trust store"))?;
            mutate(
                context,
                directories,
                resident,
                configuration,
                Some(source),
                None,
                (SignaturePolicy::enforced(unsigned, trust), None),
            )
        }
        PackageOperation::InstallIndexed(wanted) => {
            let client = index(directories, &store)?;
            let snapshots = load_indexes(context, &client, false)?;
            let entries: Vec<_> = snapshots
                .iter()
                .filter_map(|s| s.document.entry(&wanted))
                .collect();
            let entry = match entries.as_slice() {
                [entry] => *entry,
                [] => {
                    return Err(JobError::new(
                        "not-found",
                        "plugin is not listed by configured indexes",
                    ))
                }
                _ => {
                    return Err(JobError::new(
                        "ambiguous",
                        "multiple indexes list this plugin; choose an explicit package source",
                    ))
                }
            };
            let runtime = entry
                .parsed_runtime()
                .ok_or_else(|| JobError::new("runtime", "index declares an unsupported runtime"))?;
            let download_root = directories.cache_dir().join("plugin-index").join("job-downloads");
            std::fs::create_dir_all(&download_root).map_err(|e| package_error(e.into()))?;
            // Exclusive directory ownership prevents concurrent sessions from
            // reusing or overwriting a package awaiting validation.
            let scratch = download_root.join(format!("{}-{}", std::process::id(), context.id.0));
            std::fs::create_dir(&scratch).map_err(|e| package_error(e.into()))?;
            let downloaded = scratch.join(if runtime == Runtime::LegacyPython {
                format!("{}.{}", entry.id, PACKAGE_ARCHIVE_EXTENSION)
            } else {
                "package".to_owned()
            });
            let result = (|| {
                client
                    .download_package(entry, &downloaded)
                    .map_err(package_error)?;
                context.phase(JobPhase::Working)?;
                let source = match runtime {
                    Runtime::LegacyPython => InstallSource::LegacyPackage(downloaded),
                    Runtime::Python | Runtime::Native | Runtime::CAbi | Runtime::Wasm => {
                        InstallSource::Archive(downloaded)
                    }
                    Runtime::Builtin => {
                        return Err(JobError::new("runtime", "builtins are not installable packages"))
                    }
                };
                // Signed index + verified pinned digest are the same existing
                // provenance path as CLI indexed installs, not a policy bypass.
                mutate(
                    context,
                    directories,
                    resident,
                    configuration,
                    Some(source),
                    None,
                    (SignaturePolicy::unchecked(), Some((entry.id.clone(), runtime))),
                )
            })();
            let _ = std::fs::remove_dir_all(scratch);
            result
        }
        PackageOperation::Remove { kind, id } => mutate(
            context,
            directories,
            resident,
            configuration,
            None,
            Some((kind, id)),
            (SignaturePolicy::unchecked(), None),
        ),
        PackageOperation::Enable(wanted) | PackageOperation::Disable(wanted) => {
            let inventory = inventory(context, directories, roots)?;
            let entry = resolve(&inventory, &wanted)?;
            let enabled = context
                .shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .records[&context.id]
                .snapshot
                .operation
                == "enable";
            context.phase(JobPhase::Commit)?;
            // No independent load/mutate/save: the resident authority owns the
            // entire writer transaction and its effective-state publication.
            // In particular configuration-persisted-reload remains a failure
            // requiring restart, never a completed or cancelled/no-write result.
            let actual = configuration.set_plugin_enabled(&entry.plugin, enabled)?;
            let effective = actual.plugin_enabled(&entry.plugin);
            Ok(completed(
                format!(
                    "plugin={} requested={} effective={} applies=next-launch",
                    entry.plugin.0, enabled, effective
                ),
                true,
            ))
        }
        PackageOperation::List => {
            let mut report = String::new();
            let mut rows = Vec::new();
            for entry in inventory(context, directories, roots)? {
                append(
                    &mut report,
                    &format!(
                        "{} {} enabled={} installed={}",
                        entry.plugin.0,
                        entry.version,
                        store.plugin_enabled(&entry.plugin),
                        entry.installed
                    ),
                )?;
                let enabled = store.plugin_enabled(&entry.plugin);
                rows.push(PackageRow {
                    package_id: entry.id,
                    plugin: Some(entry.plugin),
                    kind: Some(entry.kind),
                    name: String::new(),
                    version: entry.version,
                    summary: String::new(),
                    installed: entry.installed,
                    enabled: Some(enabled),
                });
            }
            Ok(completed_rows(report, rows))
        }
        PackageOperation::Doctor(wanted) => doctor(context, directories, roots, &store, wanted.as_deref()),
    }
}

fn load_indexes(
    context: &Context,
    client: &PluginIndexClient,
    refresh: bool,
) -> Result<Vec<crikey_package_manager::IndexSnapshot>, JobError> {
    context.phase(JobPhase::Working)?;
    let mut snapshots = Vec::new();
    for outcome in client.load_with_checkpoint(refresh, &mut || context.phase(JobPhase::Working))? {
        context.phase(JobPhase::Working)?;
        snapshots.push(outcome.snapshot.map_err(package_error)?);
    }
    Ok(snapshots)
}

fn mutate(
    context: &Context,
    directories: &StandardDirectories,
    resident: Option<&ResidentPackageAccess>,
    configuration: &dyn PackageConfiguration,
    source: Option<InstallSource>,
    removal: Option<(PluginKind, String)>,
    validation: (SignaturePolicy, Option<(String, Runtime)>),
) -> Result<JobOutcome, JobError> {
    let access = resident.ok_or_else(|| {
        JobError::new(
            "resident-owner-required",
            "resident launcher lock and acknowledged quiescence owner required",
        )
    })?;
    context.phase(JobPhase::Working)?;
    let mut lease = access.owner.quiesce()?;
    // Once quiescence is acquired, cancellation must still reconcile the owner
    // before releasing its barrier. Stop callbacks are additional real safe
    // points after validation and before the installer's atomic swap.
    let (policy, expected_identity) = validation;
    let mut installer = PluginInstaller::new(directories).with_signature_policy(policy);
    if let Some((id, runtime)) = expected_identity {
        installer = installer.with_expected_identity(id, runtime);
    }
    let operation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(source) = source {
            installer.install_under_owned_lock(&access.lock, &source, &mut |id| {
                context
                    .phase(JobPhase::Working)
                    .map_err(|_| PackageError::Install("cancelled before commit".into()))?;
                lease.acknowledge_stopped(id)?;
                context
                    .phase(JobPhase::Commit)
                    .map_err(|_| PackageError::Install("cancelled before commit".into()))
            })
        } else if let Some((kind, id)) = removal {
            match lease.acknowledge_stopped(&id).and_then(|()| {
                context
                    .phase(JobPhase::Commit)
                    .map_err(|_| PackageError::Install("cancelled before commit".into()))
            }) {
                Ok(()) => installer.remove_under_owned_lock(&access.lock, kind, &id),
                Err(error) => Err(error),
            }
        } else {
            unreachable!("mutation has source or removal")
        }
    }))
    .unwrap_or_else(|_| {
        Err(PackageError::Install(
            "installer panicked; reconcile actual disk state and retained rollback before retrying".into(),
        ))
    });
    let (cancelled, notify) = {
        let mut state = context.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let record = state.records.get_mut(&context.id).expect("job retained");
        if let Ok(plugin) = &operation {
            record.snapshot.target = Some(Arc::from(format!(
                "{}.{}",
                plugin.kind.directory_name(),
                plugin.id
            )));
        }
        let cancelled = record.cancel && record.snapshot.phase != JobPhase::Commit;
        record.snapshot.phase = JobPhase::Reconciling;
        record.snapshot.sequence = record
            .snapshot
            .sequence
            .checked_add(1)
            .expect("bounded mutation phases");
        let notify = !record.dirty;
        record.dirty = true;
        (cancelled, notify)
    };
    if notify {
        context.shared.notify();
    }
    let actual = installer.list_bounded(INVENTORY_LIMIT);
    let effective = config(configuration);
    let reconciliation = lease.reconcile(actual.as_deref(), effective.as_ref());
    let disk_error = actual.err().map(package_error);
    let reconciliation = reconciliation.err().or(disk_error).or_else(|| effective.err());
    let changed = operation.is_ok();
    if let Some(reconciliation) = reconciliation {
        return Ok(JobOutcome::Failed {
            error: operation.err().map(package_error).unwrap_or_else(|| {
                JobError::new(
                    "reconciliation",
                    "disk operation completed but owner reconciliation failed",
                )
            }),
            reconciliation: Some(reconciliation),
            restart_required: true,
        });
    }
    if cancelled && !changed {
        return Ok(JobOutcome::Cancelled);
    }
    match operation {
        Ok(plugin) => Ok(completed(
            format!(
                "plugin={}.{} version={} applies=next-launch",
                plugin.kind.directory_name(),
                plugin.id,
                plugin.version
            ),
            true,
        )),
        Err(error) => Ok(JobOutcome::Failed {
            error: package_error(error),
            reconciliation: None,
            restart_required: true,
        }),
    }
}

struct InventoryEntry {
    plugin: PluginId,
    id: String,
    kind: PluginKind,
    root: PathBuf,
    version: String,
    manifest: Option<Manifest>,
    installed: bool,
}
fn read_manifest(path: &Path, remaining: &mut usize) -> Result<Manifest, JobError> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).map_err(|e| package_error(e.into()))?;
    let mut text = String::new();
    let cap = (*remaining).min(1024 * 1024);
    file.take(cap as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|e| package_error(e.into()))?;
    if text.len() > cap {
        return Err(JobError::new(
            "manifest-limit",
            "manifest exceeds 1 MiB or aggregate inventory metadata exceeds 8 MiB",
        ));
    }
    *remaining -= text.len();
    let mut manifest = Manifest::parse(&text).map_err(|_| {
        JobError::new(
            "manifest",
            "invalid plugin manifest (source excerpt withheld to protect secrets)",
        )
    })?;
    // Package management never needs configuration values or secret defaults.
    // Keep only the manifest portions used for diagnosis/permission reporting.
    manifest.configuration = Default::default();
    Ok(manifest)
}
fn runtime_kind(runtime: Runtime) -> Option<PluginKind> {
    match runtime {
        Runtime::LegacyPython => Some(PluginKind::Legacy),
        Runtime::Python => Some(PluginKind::Modern),
        Runtime::Native | Runtime::CAbi | Runtime::Wasm => Some(PluginKind::Native),
        Runtime::Builtin => None,
    }
}
fn inventory(
    context: &Context,
    directories: &StandardDirectories,
    roots: &[(PluginKind, PathBuf)],
) -> Result<Vec<InventoryEntry>, JobError> {
    let mut entries = Vec::new();
    let mut metadata_remaining = 8 * 1024 * 1024;
    for plugin in PluginInstaller::new(directories)
        .list_bounded(INVENTORY_LIMIT)
        .map_err(package_error)?
    {
        context.phase(JobPhase::Working)?;
        let manifest = if plugin.kind == PluginKind::Legacy {
            None
        } else {
            Some(read_manifest(
                &plugin.root.join("crikey.toml"),
                &mut metadata_remaining,
            )?)
        };
        entries.push(InventoryEntry {
            plugin: PluginId(format!("{}.{}", plugin.kind.directory_name(), plugin.id)),
            id: plugin.id,
            kind: plugin.kind,
            root: plugin.root,
            version: plugin.version,
            manifest,
            installed: true,
        });
        if entries.len() > INVENTORY_LIMIT {
            return Err(JobError::new("inventory-limit", "more than 4096 plugin copies"));
        }
    }
    let loader = PackageLoader::new(directories.cache_dir().join("legacy-packages"));
    for (kind, root) in roots {
        let children = match std::fs::read_dir(root) {
            Ok(children) => children,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(package_error(error.into())),
        };
        for (scanned, child) in children.enumerate() {
            if scanned >= INVENTORY_LIMIT {
                return Err(JobError::new(
                    "inventory-limit",
                    "discovery root exceeds 4096 entries",
                ));
            }
            context.phase(JobPhase::Working)?;
            let path = child.map_err(|e| package_error(e.into()))?.path();
            if entries.iter().any(|entry| entry.root == path) {
                continue;
            }
            let (id, version, manifest) = if *kind == PluginKind::Legacy {
                if !path.is_dir()
                    && !path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case(PACKAGE_ARCHIVE_EXTENSION))
                {
                    continue;
                }
                let package = loader.load(&path).map_err(|_| {
                    JobError::new(
                        "legacy-package",
                        "legacy package failed the existing loader's safety checks",
                    )
                })?;
                (package.id.as_str().to_owned(), String::new(), None)
            } else {
                if !path.join("crikey.toml").is_file() {
                    continue;
                }
                let manifest = read_manifest(&path.join("crikey.toml"), &mut metadata_remaining)?;
                if runtime_kind(manifest.plugin.runtime) != Some(*kind) {
                    continue;
                }
                (
                    manifest.plugin.id.clone(),
                    manifest.plugin.version.clone(),
                    Some(manifest),
                )
            };
            if entries.len() >= INVENTORY_LIMIT {
                return Err(JobError::new("inventory-limit", "more than 4096 plugin copies"));
            }
            entries.push(InventoryEntry {
                plugin: PluginId(format!("{}.{id}", kind.directory_name())),
                id,
                kind: *kind,
                root: path,
                version,
                manifest,
                installed: false,
            });
        }
    }
    entries.sort_by(|a, b| {
        a.plugin
            .0
            .cmp(&b.plugin.0)
            .then_with(|| b.installed.cmp(&a.installed))
            .then_with(|| a.root.cmp(&b.root))
    });
    Ok(entries)
}
fn resolve<'a>(entries: &'a [InventoryEntry], wanted: &str) -> Result<&'a InventoryEntry, JobError> {
    if let Some(exact) = entries.iter().find(|entry| entry.plugin.0 == wanted) {
        return Ok(exact);
    }
    let mut matching = entries.iter().filter(|entry| entry.id == wanted);
    let first = matching.next().ok_or_else(|| {
        JobError::new(
            "not-found",
            "plugin is neither installed nor on the configured discovery roots",
        )
    })?;
    if matching.any(|entry| entry.plugin != first.plugin) {
        return Err(JobError::new(
            "ambiguous",
            "bare plugin id matches multiple runtimes; use the namespaced id",
        ));
    }
    Ok(first)
}

fn detail_field(report: &mut String, key: &str, value: &str) -> Result<(), JobError> {
    if key.len().saturating_add(value.len()).saturating_add(2) > REPORT_BYTES.saturating_sub(report.len()) {
        return Err(JobError::new(
            "report-limit",
            "package metadata exceeds the bounded details report",
        ));
    }
    report.push_str(key);
    report.push('=');
    report.push_str(value);
    report.push('\n');
    Ok(())
}

fn local_details(
    context: &Context,
    directories: &StandardDirectories,
    roots: &[(PluginKind, PathBuf)],
    kind: PluginKind,
    id: &str,
) -> Result<JobOutcome, JobError> {
    let entries = inventory(context, directories, roots)?;
    let mut matching = entries
        .iter()
        .filter(|entry| entry.kind == kind && entry.id == id);
    let entry = matching.next().ok_or_else(|| {
        JobError::new(
            "not-found",
            "local package is no longer present in the captured discovery inventory",
        )
    })?;
    if matching.next().is_some() {
        return Err(JobError::new(
            "ambiguous",
            "multiple local copies have this exact runtime and package identity; no arbitrary copy was selected",
        ));
    }
    context.phase(JobPhase::Working)?;
    let mut report = String::new();
    detail_field(&mut report, "plugin", &entry.plugin.0)?;
    detail_field(&mut report, "package", &entry.id)?;
    detail_field(
        &mut report,
        "source",
        if entry.installed {
            "managed"
        } else {
            "operator-root"
        },
    )?;
    detail_field(&mut report, "location", &entry.root.to_string_lossy())?;
    if let Some(manifest) = &entry.manifest {
        detail_field(&mut report, "name", &manifest.plugin.name)?;
        detail_field(&mut report, "version", &manifest.plugin.version)?;
        detail_field(
            &mut report,
            "runtime",
            match manifest.plugin.runtime {
                Runtime::LegacyPython => "legacy-python",
                Runtime::Python => "python",
                Runtime::Native => "native",
                Runtime::CAbi => "c-abi",
                Runtime::Wasm => "wasm",
                Runtime::Builtin => "builtin",
            },
        )?;
        for (platform, path) in &manifest.plugin.entrypoint {
            detail_field(&mut report, "entrypoint-platform", platform)?;
            detail_field(&mut report, "entrypoint", path)?;
        }
        if let Some(api) = &manifest.plugin.api {
            detail_field(&mut report, "api", api)?;
        }
        append(&mut report, &format!(
            "declared-permissions network={} network-listener={} process={} window-enumeration={} window-control={} notifications={} secrets={} environment={} native-library-loading={} web-storage={} background-execution={} clipboard={:?}",
            manifest.permissions.network, manifest.permissions.network_listener,
            manifest.permissions.process, manifest.permissions.window_enumeration,
            manifest.permissions.window_control, manifest.permissions.notifications,
            manifest.permissions.secrets, manifest.permissions.environment,
            manifest.permissions.native_library_loading, manifest.permissions.web_storage,
            manifest.permissions.background_execution, manifest.permissions.clipboard,
        ))?;
        for permission in &manifest.permissions.filesystem {
            append(&mut report, &format!("declared-filesystem={permission:?}"))?;
        }
    } else {
        let package = PackageLoader::new(directories.cache_dir().join("legacy-packages"))
            .load(&entry.root)
            .map_err(|_| {
                JobError::new(
                    "legacy-package",
                    "legacy package metadata could not be loaded safely",
                )
            })?;
        detail_field(&mut report, "runtime", "legacy-python")?;
        detail_field(&mut report, "version", "not-declared-by-format")?;
        detail_field(&mut report, "entrypoint-module", &package.main_module)?;
        append(
            &mut report,
            &format!(
                "modules={} resources={} configuration-files={}",
                package.modules.len(),
                package.resources.len(),
                package
                    .resources
                    .iter()
                    .filter(|path| path
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("ini")))
                    .count(),
            ),
        )?;
        detail_field(&mut report, "configuration", "legacy-ini; values not inspected")?;
    }
    context.phase(JobPhase::Working)?;
    Ok(completed(report, false))
}

fn doctor(
    context: &Context,
    directories: &StandardDirectories,
    roots: &[(PluginKind, PathBuf)],
    store: &ConfigStore,
    wanted: Option<&str>,
) -> Result<JobOutcome, JobError> {
    use crikey_plugin_supervisor::{shared_budget_from_section, BudgetKind};
    let entries = inventory(context, directories, roots)?;
    let target = wanted
        .map(|wanted| resolve(&entries, wanted).map(|entry| entry.plugin.clone()))
        .transpose()?;
    let interpreter =
        crikey_legacy_compat::discover_interpreter(&crikey_python_host::RuntimeProfile::LegacyCompatibility)
            .ok();
    let mut report = String::new();
    let mut degraded = false;
    for entry in &entries {
        if target.as_ref().is_some_and(|target| target != &entry.plugin) {
            continue;
        }
        context.phase(JobPhase::Working)?;
        append(
            &mut report,
            &format!(
                "plugin={} enabled={} copies={}",
                entry.plugin.0,
                store.plugin_enabled(&entry.plugin),
                entries.iter().filter(|copy| copy.plugin == entry.plugin).count()
            ),
        )?;
        let concurrency = entry
            .manifest
            .as_ref()
            .map(|manifest| manifest.concurrency.clone())
            .unwrap_or_default();
        let budget = shared_budget_from_section(&concurrency);
        for kind in BudgetKind::ALL {
            let admitted = budget.try_acquire(kind).is_some();
            degraded |= budget.limit(kind) > 0 && !admitted;
            append(
                &mut report,
                &format!(
                    "probe=this-process work={kind:?} limit={} admitted={admitted} refusals={}",
                    budget.limit(kind),
                    budget.refusals(kind)
                ),
            )?;
        }
        let sandbox = crikey_sandbox::plugin_policy(
            Vec::<PathBuf>::new(),
            entry
                .manifest
                .as_ref()
                .is_some_and(|manifest| !manifest.permissions.network),
        )
        .prepare();
        let sandbox_report = sandbox.report();
        append(
            &mut report,
            &format!(
                "probe=this-process filesystem-write={} tcp-network={}",
                sandbox_report.filesystem_write, sandbox_report.tcp_network
            ),
        )?;
        if let Some(manifest) = &entry.manifest {
            append(&mut report, &format!("permissions={:?}", manifest.permissions))?;
            for declaration in manifest.unhonoured_declarations() {
                append(
                    &mut report,
                    &format!("unhonoured={} reason={}", declaration.field, declaration.reason),
                )?;
            }
            if entry.kind == PluginKind::Native
                && manifest
                    .entrypoint_for(std::env::consts::OS, std::env::consts::ARCH)
                    .is_err()
            {
                degraded = true;
                append(&mut report, "blocking=no-compatible-entrypoint")?;
            }
        } else {
            let package = PackageLoader::new(directories.cache_dir().join("legacy-packages"))
                .load(&entry.root)
                .map_err(|_| JobError::new("legacy-package", "legacy package could not be loaded safely"))?;
            let profile = store
                .scheduling_profile(&entry.plugin)
                .unwrap_or(crikey_plugin_model::SchedulingProfile::LegacyStrict);
            let profile = match profile {
                crikey_plugin_model::SchedulingProfile::LegacyStrict => {
                    crikey_input_scheduler::SchedulingProfile::LegacyStrict
                }
                crikey_plugin_model::SchedulingProfile::LegacyOptimized => {
                    crikey_input_scheduler::SchedulingProfile::LegacyOptimized
                }
                crikey_plugin_model::SchedulingProfile::Modern => {
                    crikey_input_scheduler::SchedulingProfile::Modern
                }
            };
            let mut diagnostics = crikey_legacy_compat::LegacyDiagnostics::default();
            diagnostics.observe_scheduling_profile(&entry.plugin, profile);
            if let Some(interpreter) = &interpreter {
                diagnostics.observe_python_requirement(
                    &entry.plugin,
                    crikey_legacy_compat::MINIMUM_SUPPORTED_PYTHON,
                    interpreter.version(),
                );
            } else {
                degraded = true;
                append(&mut report, "blocking=no-supported-python-interpreter")?;
            }
            observe_legacy_imports(context, &package, &entry.plugin, &mut diagnostics)?;
            append(
                &mut report,
                "legacy-permissions=compatibility-baseline host-mediated=process,filesystem-package-read",
            )?;
            for finding in diagnostics.warnings_for(&entry.plugin) {
                degraded |= finding.warning.severity() >= crikey_legacy_compat::Severity::Blocking;
                append(
                    &mut report,
                    &format!(
                        "finding={} severity={} {}",
                        finding.warning.code(),
                        finding.warning.severity().as_str(),
                        finding.warning.message()
                    ),
                )?;
            }
        }
    }
    append(
        &mut report,
        if degraded {
            "verdict=degraded"
        } else {
            "verdict=healthy"
        },
    )?;
    Ok(completed(report, false))
}

/// Uses the existing loader's module inventory and the diagnostics store's
/// import finding, with the same 512-module/1 MiB-per-module static scan bounds
/// as the command-line diagnosis. Source text is never retained in reports.
fn observe_legacy_imports(
    context: &Context,
    package: &crikey_legacy_compat::LegacyPackage,
    plugin: &PluginId,
    diagnostics: &mut crikey_legacy_compat::LegacyDiagnostics,
) -> Result<(), JobError> {
    use std::io::Read as _;
    const WINDOWS_MODULE: &str = "keypirinha_wintypes";
    if package.modules.len() > 512 {
        return Err(JobError::new(
            "legacy-scan-limit",
            "legacy package exceeds 512 modules; portability is unknown",
        ));
    }
    let root = package.root.content_root();
    for module in &package.modules {
        context.phase(JobPhase::Working)?;
        let file =
            std::fs::File::open(root.join(&module.relative_path)).map_err(|e| package_error(e.into()))?;
        let mut source = String::new();
        file.take(1024 * 1024 + 1)
            .read_to_string(&mut source)
            .map_err(|_| JobError::new("legacy-scan", "legacy source is unreadable or not UTF-8"))?;
        if source.len() > 1024 * 1024 {
            return Err(JobError::new(
                "legacy-scan-limit",
                "legacy source exceeds 1 MiB; portability is unknown",
            ));
        }
        if source.lines().any(|line| {
            let mut words = line.split_whitespace();
            matches!(words.next(), Some("from") | Some("import")) && words.next() == Some(WINDOWS_MODULE)
        }) {
            diagnostics.observe_import(
                plugin,
                WINDOWS_MODULE,
                crikey_legacy_compat::ImportOutcome::WindowsOnly {
                    entry_point: "Win32 compatibility import".to_owned(),
                    detail: format!(
                        "Windows-only compatibility module imported by {}",
                        module.import_name
                    ),
                },
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // State-only harness: installer behavior is covered by owned_lock_jobs.
    fn harness() -> (PackageJobs, Arc<AtomicUsize>) {
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake_count = Arc::clone(&wakes);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                shutdown: false,
                queue: VecDeque::new(),
                records: BTreeMap::new(),
            }),
            ready: Condvar::new(),
            wake: Arc::new(move || {
                wake_count.fetch_add(1, Ordering::Relaxed);
            }),
        });
        (
            PackageJobs {
                shared,
                worker: None,
                resident: false,
            },
            wakes,
        )
    }
    fn context(jobs: &PackageJobs, id: JobId) -> Context {
        jobs.shared
            .state
            .lock()
            .expect("state")
            .queue
            .retain(|(queued, _)| *queued != id);
        Context {
            shared: Arc::clone(&jobs.shared),
            id,
        }
    }

    #[test]
    fn admissions_are_bounded_and_resident_mutations_need_a_real_owner() {
        let (jobs, _) = harness();
        assert_eq!(
            jobs.submit(PackageOperation::Install(InstallSource::Directory(
                "source".into()
            ))),
            Err(JobRefusal::ResidentOwnerRequired)
        );
        for _ in 0..QUEUED {
            jobs.submit(PackageOperation::List).expect("bounded admission");
        }
        assert_eq!(jobs.submit(PackageOperation::List), Err(JobRefusal::Saturated));
        assert_eq!(
            jobs.submit(PackageOperation::Remove {
                kind: PluginKind::Modern,
                id: "../escape".into()
            }),
            Err(JobRefusal::InvalidInput)
        );
    }

    #[test]
    fn admission_bounds_allocated_input_capacity_not_only_text_length() {
        let (jobs, _) = harness();
        let mut text = String::with_capacity(RETAINED_BYTES);
        text.push_str("fixture");
        assert_eq!(
            jobs.submit(PackageOperation::Search(text)),
            Err(JobRefusal::InvalidInput)
        );
        let mut path = PathBuf::with_capacity(RETAINED_BYTES);
        path.push("fixture");
        assert_eq!(
            jobs.submit(PackageOperation::Install(InstallSource::Archive(path))),
            Err(JobRefusal::InvalidInput)
        );
    }

    #[test]
    fn constructor_refuses_oversized_root_path_and_vector_allocations() {
        let fixture = Fixture::new();
        let configuration = Arc::new(SessionConfiguration {
            directories: fixture.directories.clone(),
            overrides: Mutex::new(Vec::new()),
            loads: AtomicUsize::new(0),
        });
        let mut path = PathBuf::with_capacity(2 * RETAINED_BYTES);
        path.push("short");
        let mut excess_slots = Vec::with_capacity(ROOT_LIMIT + 1);
        excess_slots.push((PluginKind::Native, PathBuf::from("short")));
        for roots in [vec![(PluginKind::Native, path)], excess_slots] {
            let (wake, _wakes) = std::sync::mpsc::channel();
            let result = PackageJobs::new(
                fixture.directories.clone(),
                roots,
                None,
                configuration.clone(),
                Arc::new(move || {
                    let _ = wake.send(());
                }),
            );
            assert!(matches!(&result, Err(error) if error.kind() == std::io::ErrorKind::InvalidInput));
        }
        assert_eq!(configuration.loads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn cancellation_wins_only_before_commit_and_terminal_is_never_evicted() {
        let (jobs, _) = harness();
        let id = jobs.submit(PackageOperation::List).expect("job");
        let context = context(&jobs, id);
        context.phase(JobPhase::Working).expect("start");
        assert_eq!(jobs.cancel(id), CancellationStatus::Requested);
        assert_eq!(
            context.phase(JobPhase::Commit).expect_err("cancel wins").code,
            "cancelled"
        );
        context.finish(JobOutcome::Cancelled);
        assert!(!jobs.forget(id), "undrained terminal retained");
        assert_eq!(jobs.cancel(id), CancellationStatus::AlreadyTerminal);
        let events = jobs.drain_events(1);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].outcome, Some(JobOutcome::Cancelled)));
        assert!(jobs.forget(id));
        assert_eq!(jobs.cancel(id), CancellationStatus::Unknown);
    }

    #[test]
    fn commit_refuses_cancellation_and_progress_wakes_are_coalesced() {
        let (jobs, wakes) = harness();
        let id = jobs.submit(PackageOperation::List).expect("job");
        let context = context(&jobs, id);
        context.phase(JobPhase::Working).expect("start");
        for _ in 0..100 {
            context.phase(JobPhase::Working).expect("safe point");
        }
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        let first = jobs.drain_events(1).pop().expect("working snapshot");
        context.phase(JobPhase::Commit).expect("commit");
        assert_eq!(jobs.cancel(id), CancellationStatus::NotCancellable);
        context.finish(completed("done".into(), true));
        let terminal = jobs.drain_events(1).pop().expect("terminal snapshot");
        assert!(terminal.sequence > first.sequence);
        assert_eq!(terminal.phase, JobPhase::Terminal);
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn job_identity_is_not_reused_when_a_service_is_recreated() {
        let (first, _) = harness();
        let first_id = first.submit(PackageOperation::List).expect("first job");
        let (second, _) = harness();
        let second_id = second.submit(PackageOperation::List).expect("second job");
        assert_ne!(first_id, second_id);
        assert!(second.snapshot(first_id).is_none());
    }

    #[test]
    fn reports_refuse_overflow_and_error_bounds_preserve_utf8() {
        assert!(matches!(
            completed("x".repeat(REPORT_BYTES + 1), true),
            JobOutcome::Completed {
                restart_required: true,
                details_truncated: true,
                ..
            }
        ));
        let error = JobError::new("bounded", &"界".repeat(REPORT_BYTES));
        assert!(error.detail.len() <= REPORT_BYTES / 2);
        let operation = PackageOperation::Install(InstallSource::Url(
            "https://user:password@example.invalid/package".into(),
        ));
        assert!(
            operation.target().is_none(),
            "credentials never enter event metadata"
        );
    }

    #[test]
    fn empty_row_result_explicitly_replaces_previous_management_targets() {
        let (jobs, _) = harness();
        let id = jobs.submit(PackageOperation::List).expect("inventory");
        context(&jobs, id).finish(completed_rows(String::new(), Vec::new()));
        let terminal = jobs.drain_events(1).pop().expect("inventory result");
        assert!(matches!(
            &terminal.outcome,
            Some(JobOutcome::Completed { rows: Some(rows), .. }) if rows.is_empty()
        ));
        assert!(matches!(
            completed(String::new(), false),
            JobOutcome::Completed { rows: None, .. }
        ));
    }

    #[test]
    fn report_rows_targets_and_errors_share_one_aggregate_reservation() {
        let (jobs, _) = harness();
        for index in 0..RETAINED {
            let id = jobs
                .submit(PackageOperation::Show(PackageShowTarget::Index {
                    id: "t".repeat(INPUT_BYTES),
                }))
                .expect("reserved terminal");
            let context = context(&jobs, id);
            let outcome = if index % 2 == 0 {
                completed_rows(
                    "r".repeat(4000),
                    vec![PackageRow {
                        package_id: "package".into(),
                        plugin: None,
                        kind: None,
                        name: String::new(),
                        version: "1".into(),
                        summary: "s".repeat(4000),
                        installed: false,
                        enabled: None,
                    }],
                )
            } else {
                JobOutcome::Failed {
                    error: JobError::new("primary", &"e".repeat(REPORT_BYTES)),
                    reconciliation: Some(JobError::new("reconcile", &"f".repeat(REPORT_BYTES))),
                    restart_required: true,
                }
            };
            context.finish(outcome);
            let snapshot = jobs.snapshot(id).expect("consumer snapshot");
            assert!(retained_bytes(&snapshot) <= JOB_BYTES);
            if index % 2 == 0 {
                assert!(
                    matches!(snapshot.outcome, Some(JobOutcome::Failed { ref error, .. }) if error.code == "result-budget")
                );
            }
        }
        assert_eq!(jobs.submit(PackageOperation::List), Err(JobRefusal::Saturated));
        assert!(jobs.retained_bytes() <= RETAINED_BYTES);
        let terminals = jobs.drain_events(RETAINED);
        assert_eq!(
            terminals.len(),
            RETAINED,
            "capacity never drops an accepted terminal"
        );
        assert!(terminals
            .iter()
            .all(|snapshot| snapshot.phase == JobPhase::Terminal));
    }

    #[test]
    fn queued_working_and_terminal_jobs_share_bounded_retention() {
        let (jobs, _) = harness();
        for _ in 0..RETAINED - QUEUED - 1 {
            let id = jobs
                .submit(PackageOperation::Show(PackageShowTarget::Index {
                    id: "t".repeat(INPUT_BYTES),
                }))
                .expect("terminal slot");
            context(&jobs, id).finish(completed("r".repeat(REPORT_BYTES), true));
        }
        let working_id = jobs
            .submit(PackageOperation::Search("w".repeat(INPUT_BYTES)))
            .expect("working slot");
        // Retain the actual working input, as worker_loop does, rather than
        // dropping it with the state-only harness's normal dequeue helper.
        let working = jobs
            .shared
            .state
            .lock()
            .expect("state")
            .queue
            .pop_front()
            .expect("working input");
        let working_context = Context {
            shared: Arc::clone(&jobs.shared),
            id: working_id,
        };
        working_context.phase(JobPhase::Working).expect("working");
        for index in 0..QUEUED {
            let id = "q".repeat(INPUT_BYTES);
            let target = if index % 2 == 0 {
                PackageShowTarget::Index { id }
            } else {
                PackageShowTarget::Local {
                    kind: PluginKind::Native,
                    id,
                }
            };
            jobs.submit(PackageOperation::Show(target)).expect("queued slot");
        }
        assert_eq!(jobs.submit(PackageOperation::List), Err(JobRefusal::Saturated));
        assert!(jobs.retained_bytes() <= RETAINED_BYTES);
        {
            let state = jobs.shared.state.lock().expect("state");
            let input_capacity = match &working.1 {
                PackageOperation::Search(text) => text.capacity(),
                _ => unreachable!("working search"),
            } + state
                .queue
                .iter()
                .map(|(_, operation)| match operation {
                    PackageOperation::Show(
                        PackageShowTarget::Index { id } | PackageShowTarget::Local { id, .. },
                    ) => id.capacity(),
                    _ => unreachable!("queued show"),
                })
                .sum::<usize>();
            let snapshots = state
                .records
                .values()
                .map(|record| retained_bytes(&record.snapshot))
                .sum::<usize>();
            let queue_storage = state.queue.capacity() * std::mem::size_of::<(JobId, PackageOperation)>();
            assert!(snapshots + input_capacity + queue_storage + 16 * 1024 <= RETAINED_BYTES);
            assert_eq!(state.queue.len(), QUEUED);
        }
        let terminal_count = RETAINED - QUEUED - 1;
        let events = jobs.drain_events(RETAINED);
        assert_eq!(events.len(), RETAINED);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.phase == JobPhase::Terminal)
                .count(),
            terminal_count
        );
        assert_eq!(jobs.cancel(working_id), CancellationStatus::Requested);
        working_context.finish(JobOutcome::Cancelled);
        assert!(jobs
            .snapshot(working_id)
            .is_some_and(|snapshot| matches!(snapshot.outcome, Some(JobOutcome::Cancelled))));
        assert!(jobs.retained_bytes() <= RETAINED_BYTES);
    }

    #[test]
    fn successful_mutation_stays_completed_when_its_report_exceeds_capacity() {
        let (jobs, _) = harness();
        let id = jobs
            .submit(PackageOperation::Show(PackageShowTarget::Index {
                id: "t".repeat(INPUT_BYTES),
            }))
            .expect("reservation");
        context(&jobs, id).finish(completed("界".repeat(REPORT_BYTES), true));
        let event = jobs.drain_events(1).pop().expect("terminal event");
        assert!(retained_bytes(&event) <= JOB_BYTES);
        assert!(
            matches!(
                event.outcome,
                Some(JobOutcome::Completed {
                    restart_required: true,
                    details_truncated: true,
                    ..
                })
            ),
            "report capacity never pretends a committed mutation was rolled back"
        );
    }

    struct Fixture {
        root: PathBuf,
        directories: StandardDirectories,
    }
    impl Fixture {
        fn new() -> Self {
            use crikey_platform::{DirectoryConvention, DirectoryEnvironment};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "crikey-package-job-policy-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).expect("unique fixture directory");
            let environment = DirectoryEnvironment::new()
                .set("HOME", &root)
                .set("APPDATA", &root)
                .set("LOCALAPPDATA", &root)
                .set("CRIKEY_CONFIG_DIR", root.join("config"))
                .set("CRIKEY_DATA_DIR", root.join("data"))
                .set("CRIKEY_CACHE_DIR", root.join("cache"))
                .set("CRIKEY_STATE_DIR", root.join("state"));
            let directories = StandardDirectories::resolve(DirectoryConvention::current(), &environment)
                .expect("directories");
            Self { root, directories }
        }
        fn archive(&self, label: &str, id: &str, version: &str) -> PathBuf {
            let source = self.root.join(label);
            std::fs::create_dir_all(source.join("bin")).expect("source");
            let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
            std::fs::write(source.join("crikey.toml"), format!(
                "manifest-version = 1\n[plugin]\nid = \"{id}\"\nname = \"Fixture\"\nversion = \"{version}\"\nruntime = \"native\"\nentrypoint.{os}-{arch} = \"bin/plugin\"\n[platform]\nos = [\"{os}\"]\narch = [\"{arch}\"]\n"
            )).expect("manifest");
            std::fs::write(source.join("bin/plugin"), version).expect("binary fixture");
            let archive = self.root.join(format!("{label}.crikey"));
            crikey_package_manager::build_package(&source, &archive).expect("real native archive");
            archive
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn local_details_use_exact_fresh_inventory_without_indexes_or_plugin_execution() {
        let fixture = Fixture::new();
        let native_root = fixture.root.join("native-root");
        let legacy_root = fixture.root.join("legacy-root");
        std::fs::create_dir_all(&native_root).expect("native discovery root");
        std::fs::create_dir_all(legacy_root.join("same")).expect("legacy package");
        fixture.archive("native-source", "same", "1.2.3");
        std::fs::rename(fixture.root.join("native-source"), native_root.join("same"))
            .expect("native operator package");
        let manifest_path = native_root.join("same/crikey.toml");
        let manifest = std::fs::read_to_string(&manifest_path).expect("native declaration");
        std::fs::write(&manifest_path, format!(
            "{manifest}\n[[configuration.field]]\nname = \"token\"\ntype = \"string\"\nsecret = true\ndefault = \"DETAILS-SECRET-CANARY\"\n",
        )).expect("secret schema default");
        std::fs::write(
            legacy_root.join("same/same.py"),
            "raise AssertionError('inspection must not execute plugins')\n",
        )
        .expect("legacy entrypoint");
        std::fs::write(
            legacy_root.join("same/same.ini"),
            "[main]\npassword = DETAILS-SECRET-CANARY\n",
        )
        .expect("legacy settings");
        let managed = fixture.archive("managed-source", "managed", "2.0.0");
        PluginInstaller::new(&fixture.directories)
            .install(&InstallSource::Archive(managed), &mut |_| Ok(()))
            .expect("managed package");
        let configuration = Arc::new(SessionConfiguration {
            directories: fixture.directories.clone(),
            overrides: Mutex::new(Vec::new()),
            loads: AtomicUsize::new(0),
        });
        let (wake, wakes) = std::sync::mpsc::channel();
        let mut jobs = PackageJobs::new(
            fixture.directories.clone(),
            vec![
                (PluginKind::Native, native_root.clone()),
                (PluginKind::Legacy, legacy_root.clone()),
            ],
            None,
            configuration,
            Arc::new(move || {
                let _ = wake.send(());
            }),
        )
        .expect("inspection worker");
        for (kind, id, runtime, source, version) in [
            (PluginKind::Native, "same", "native", "operator-root", "1.2.3"),
            (
                PluginKind::Legacy,
                "same",
                "legacy-python",
                "operator-root",
                "not-declared-by-format",
            ),
            (PluginKind::Native, "managed", "native", "managed", "2.0.0"),
        ] {
            let job = jobs
                .submit(PackageOperation::Show(PackageShowTarget::Local {
                    kind,
                    id: id.into(),
                }))
                .expect("local details");
            let outcome = terminal(&jobs, job, &wakes).outcome.expect("details terminal");
            let JobOutcome::Completed {
                report,
                restart_required,
                ..
            } = outcome
            else {
                panic!("local details must work without indexes: {outcome:?}")
            };
            assert!(!restart_required);
            assert!(report.contains(&format!("plugin={}.{id}\n", kind.directory_name())));
            assert!(report.contains(&format!("runtime={runtime}\n")));
            assert!(report.contains(&format!("source={source}\n")));
            assert!(report.contains(&format!("version={version}\n")));
            assert!(report.contains("location="));
            assert!(!report.contains("DETAILS-SECRET-CANARY"));
        }
        // A second copy with the same exact kind/id cannot silently redirect a
        // details action to a guessed winning path.
        std::fs::create_dir(native_root.join("duplicate")).expect("duplicate candidate");
        std::fs::copy(&manifest_path, native_root.join("duplicate/crikey.toml")).expect("duplicate manifest");
        let duplicate = jobs
            .submit(PackageOperation::Show(PackageShowTarget::Local {
                kind: PluginKind::Native,
                id: "same".into(),
            }))
            .expect("duplicate details");
        assert!(matches!(
            terminal(&jobs, duplicate, &wakes).outcome,
            Some(JobOutcome::Failed { error, restart_required: false, .. }) if error.code == "ambiguous"
        ));
        std::fs::remove_dir_all(legacy_root.join("same")).expect("package disappears after row publication");
        let stale = jobs
            .submit(PackageOperation::Show(PackageShowTarget::Local {
                kind: PluginKind::Legacy,
                id: "same".into(),
            }))
            .expect("stale row details");
        assert!(matches!(
            terminal(&jobs, stale, &wakes).outcome,
            Some(JobOutcome::Failed { error, restart_required: false, .. }) if error.code == "not-found"
        ));
        // Local details must not silently reinterpret an explicit index target.
        let indexed = jobs
            .submit(PackageOperation::Show(PackageShowTarget::Index {
                id: "same".into(),
            }))
            .expect("index details");
        assert!(matches!(
            terminal(&jobs, indexed, &wakes).outcome,
            Some(JobOutcome::Failed { .. })
        ));
        jobs.shutdown().expect("inspection shutdown");
    }

    #[test]
    fn local_details_refuse_oversized_metadata_and_allocated_target_capacity() {
        let fixture = Fixture::new();
        fixture.archive("oversized", "large", "1.0.0");
        let path = fixture.root.join("oversized/crikey.toml");
        let declaration = std::fs::read_to_string(&path).expect("declaration");
        std::fs::write(
            &path,
            declaration.replace(
                "name = \"Fixture\"",
                &format!("name = \"{}\"", "x".repeat(REPORT_BYTES)),
            ),
        )
        .expect("oversized display name");
        let configuration = SessionConfiguration {
            directories: fixture.directories.clone(),
            overrides: Mutex::new(Vec::new()),
            loads: AtomicUsize::new(0),
        };
        let (jobs, _) = harness();
        let operation = PackageOperation::Show(PackageShowTarget::Local {
            kind: PluginKind::Native,
            id: "large".into(),
        });
        let id = jobs.submit(operation.clone()).expect("bounded identity");
        let outcome = execute(
            &context(&jobs, id),
            &fixture.directories,
            &[(PluginKind::Native, fixture.root.clone())],
            None,
            &configuration,
            operation,
        );
        assert!(matches!(outcome, Err(error) if error.code == "report-limit"));
        let mut oversized = String::with_capacity(RETAINED_BYTES);
        oversized.push_str("large");
        assert_eq!(
            jobs.submit(PackageOperation::Show(PackageShowTarget::Local {
                kind: PluginKind::Native,
                id: oversized
            })),
            Err(JobRefusal::InvalidInput)
        );
    }

    struct SessionConfiguration {
        directories: StandardDirectories,
        overrides: Mutex<Vec<(String, String)>>,
        loads: AtomicUsize,
    }
    impl PackageConfiguration for SessionConfiguration {
        fn load(&self) -> Result<ConfigStore, JobError> {
            self.loads.fetch_add(1, Ordering::Relaxed);
            ConfigStore::load_with_overrides(
                &self.directories,
                None,
                &self.overrides.lock().expect("session overrides"),
            )
            .map_err(|_| JobError::new("configuration", "fixture configuration failed to load"))
        }
        fn set_plugin_enabled(&self, plugin: &PluginId, enabled: bool) -> Result<ConfigStore, JobError> {
            // This fixture's single writer serializes settings/session changes
            // with enabled-state edits, just as the resident worker must do.
            let overrides = self.overrides.lock().expect("session writer");
            let mut store = ConfigStore::load_with_overrides(&self.directories, None, &overrides)
                .map_err(|_| JobError::new("configuration", "fixture configuration failed to load"))?;
            store.set_plugin_enabled(plugin, enabled);
            store
                .save()
                .map_err(|_| JobError::new("configuration-write", "fixture persistence failed"))?;
            ConfigStore::load_with_overrides(&self.directories, None, &overrides).map_err(|_| {
                JobError::new(
                    "configuration-persisted-reload",
                    "fixture effective reload failed",
                )
            })
        }
    }

    fn enabled_transaction_preserves_interleaved_settings(persisted_failure: bool) {
        struct Transaction {
            inner: SessionConfiguration,
            entered: std::sync::mpsc::Sender<()>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
            persisted_failure: bool,
        }
        impl PackageConfiguration for Transaction {
            fn load(&self) -> Result<ConfigStore, JobError> {
                let store = self.inner.load()?;
                assert_eq!(
                    self.inner.loads.load(Ordering::Relaxed),
                    1,
                    "enabled edits must use the setter's effective result, not a separate reload"
                );
                Ok(store)
            }
            fn set_plugin_enabled(&self, plugin: &PluginId, enabled: bool) -> Result<ConfigStore, JobError> {
                self.entered.send(()).expect("transaction admission");
                self.release
                    .lock()
                    .expect("transaction release")
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("interleaved setting saved");
                let effective = self.inner.set_plugin_enabled(plugin, enabled)?;
                if self.persisted_failure {
                    Err(JobError::new(
                        "configuration-persisted-reload",
                        "enabled state persisted but effective publication failed",
                    ))
                } else {
                    Ok(effective)
                }
            }
        }
        let fixture = Fixture::new();
        let archive = fixture.archive("enabled-transaction", "fixture", "1.0.0");
        PluginInstaller::new(&fixture.directories)
            .install(&InstallSource::Archive(archive), &mut |_| Ok(()))
            .expect("inventory fixture");
        let (entered, started) = std::sync::mpsc::channel();
        let (resume, release) = std::sync::mpsc::channel();
        let (wake, wakes) = std::sync::mpsc::channel();
        let authority = Arc::new(Transaction {
            inner: SessionConfiguration {
                directories: fixture.directories.clone(),
                overrides: Mutex::new(vec![("plugins.native.fixture.enabled".into(), "true".into())]),
                loads: AtomicUsize::new(0),
            },
            entered,
            release: Mutex::new(release),
            persisted_failure,
        });
        let mut jobs = PackageJobs::new(
            fixture.directories.clone(),
            Vec::new(),
            None,
            authority,
            Arc::new(move || {
                let _ = wake.send(());
            }),
        )
        .expect("worker");
        let id = jobs
            .submit(PackageOperation::Disable("native.fixture".into()))
            .expect("disable");
        started
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("setter entered");
        assert_eq!(jobs.cancel(id), CancellationStatus::NotCancellable);
        // A settings write lands after the job's initial policy read but before
        // the authority's fresh enabled transaction. It must not be overwritten.
        let mut setting = ConfigStore::load_with_policy(&fixture.directories, None).expect("settings read");
        setting.set_user_global("launcher.show-hints", "false");
        setting.save().expect("interleaved settings write");
        resume.send(()).expect("resume transaction");
        let snapshot = terminal(&jobs, id, &wakes);
        if persisted_failure {
            assert!(matches!(
                &snapshot.outcome,
                Some(JobOutcome::Failed { error, restart_required: true, .. })
                    if error.code == "configuration-persisted-reload"
            ));
        } else {
            assert!(matches!(
                &snapshot.outcome,
                Some(JobOutcome::Completed { report, restart_required: true, .. })
                    if report.contains("requested=false effective=true")
            ));
        }
        let disk = ConfigStore::load_with_policy(&fixture.directories, None).expect("persisted state");
        assert_eq!(disk.get("launcher.show-hints"), Some("false"));
        assert!(!disk.plugin_enabled(&PluginId("native.fixture".into())));
        jobs.shutdown().expect("shutdown");
    }

    #[test]
    fn enabled_edit_uses_transaction_effective_state_and_preserves_interleaved_settings() {
        enabled_transaction_preserves_interleaved_settings(false);
    }

    #[test]
    fn persisted_enabled_edit_failure_stays_failed_restart_required_and_not_cancelled() {
        enabled_transaction_preserves_interleaved_settings(true);
    }
    // No process is started by this fixture, so the actual owner set is empty.
    // Its acknowledgments are still observed: a refused archive must not reach
    // the pre-commit stop callback merely because disk policy permits it.
    struct EmptyOwner {
        stops: Arc<AtomicUsize>,
    }
    struct EmptyLease {
        stops: Arc<AtomicUsize>,
    }
    impl PackageOwner for EmptyOwner {
        fn quiesce(&self) -> Result<Box<dyn QuiescentPlugins>, JobError> {
            Ok(Box::new(EmptyLease {
                stops: Arc::clone(&self.stops),
            }))
        }
    }
    impl QuiescentPlugins for EmptyLease {
        fn acknowledge_stopped(&mut self, _id: &str) -> Result<(), PackageError> {
            self.stops.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn reconcile(
            &mut self,
            installed: Result<&[InstalledPlugin], &PackageError>,
            configuration: Result<&ConfigStore, &JobError>,
        ) -> Result<(), JobError> {
            configuration.map_err(Clone::clone)?;
            installed
                .map(|_| ())
                .map_err(|_| JobError::new("reconciliation", "fixture disk inventory failed"))
        }
    }
    fn terminal(jobs: &PackageJobs, id: JobId, wakes: &std::sync::mpsc::Receiver<()>) -> JobSnapshot {
        loop {
            if let Some(snapshot) = jobs
                .drain_events(RETAINED)
                .into_iter()
                .find(|snapshot| snapshot.id == id && snapshot.phase == JobPhase::Terminal)
            {
                return snapshot;
            }
            wakes
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("package worker wake");
        }
    }

    #[test]
    fn resident_session_policy_profile_and_indexes_survive_job_reloads() {
        let fixture = Fixture::new();
        let directories = &fixture.directories;
        std::fs::create_dir_all(directories.config_dir().join("profiles")).expect("profiles");
        std::fs::write(directories.config_dir().join("config.toml"),
            "[packages]\nunsigned-policy = \"allow\"\n[launcher]\nprofile = \"disk\"\nplugin-index-urls = \"https://disk.invalid/index\"\n").expect("disk policy");
        std::fs::write(
            directories.config_dir().join("profiles/disk.toml"),
            "[launcher]\nshow-hints = true\n",
        )
        .expect("disk profile");
        std::fs::write(
            directories.config_dir().join("profiles/resident.toml"),
            "[launcher]\nshow-hints = false\n",
        )
        .expect("session profile");
        let configuration = Arc::new(SessionConfiguration {
            directories: directories.clone(),
            overrides: Mutex::new(vec![
                (KEY_UNSIGNED_POLICY.into(), "refuse".into()),
                ("launcher.profile".into(), "resident".into()),
                (KEY_INDEX_URLS.into(), "https://session.invalid/index".into()),
                ("plugins.native.fixture.enabled".into(), "false".into()),
            ]),
            loads: AtomicUsize::new(0),
        });
        let effective = config(configuration.as_ref()).expect("resident state");
        assert_eq!(effective.get(KEY_UNSIGNED_POLICY), Some("refuse"));
        assert_eq!(
            effective.get(KEY_INDEX_URLS),
            Some("https://session.invalid/index")
        );
        assert_eq!(effective.get("launcher.profile"), Some("resident"));
        assert_eq!(effective.get("launcher.show-hints"), Some("false"));
        let stops = Arc::new(AtomicUsize::new(0));
        let access = ResidentPackageAccess::new(
            Arc::new(LauncherLock::acquire(directories).expect("resident lock")),
            Arc::new(EmptyOwner {
                stops: Arc::clone(&stops),
            }),
        );
        let (wake, wakes) = std::sync::mpsc::channel();
        let authority: Arc<dyn PackageConfiguration> = configuration.clone();
        let mut jobs = PackageJobs::new(
            directories.clone(),
            Vec::new(),
            Some(access),
            authority,
            Arc::new(move || {
                let _ = wake.send(());
            }),
        )
        .expect("worker");
        let archive = fixture.archive("unsigned", "fixture", "1.0.0");
        let id = jobs
            .submit(PackageOperation::Install(InstallSource::Archive(archive.clone())))
            .expect("admission");
        let refused = terminal(&jobs, id, &wakes);
        assert!(
            matches!(refused.outcome, Some(JobOutcome::Failed { ref error, .. }) if error.code == "signature")
        );
        assert_eq!(stops.load(Ordering::Relaxed), 0);
        assert!(!directories
            .plugin_dir(PluginKind::Native)
            .join("fixture")
            .exists());

        configuration
            .overrides
            .lock()
            .expect("session")
            .iter_mut()
            .find(|(key, _)| key == KEY_UNSIGNED_POLICY)
            .expect("policy override")
            .1 = "allow".into();
        let id = jobs
            .submit(PackageOperation::Install(InstallSource::Archive(archive)))
            .expect("admission");
        assert!(matches!(
            terminal(&jobs, id, &wakes).outcome,
            Some(JobOutcome::Completed {
                restart_required: true,
                ..
            })
        ));
        assert_eq!(stops.load(Ordering::Relaxed), 1);
        assert!(directories
            .plugin_dir(PluginKind::Native)
            .join("fixture/bin/plugin")
            .is_file());

        let id = jobs
            .submit(PackageOperation::Enable("native.fixture".into()))
            .expect("enable");
        assert!(matches!(terminal(&jobs, id, &wakes).outcome,
            Some(JobOutcome::Completed { ref report, .. }) if report.contains("requested=true effective=false")));
        let effective = config(configuration.as_ref()).expect("effective reload");
        assert_eq!(
            effective.get(KEY_INDEX_URLS),
            Some("https://session.invalid/index")
        );
        assert_eq!(effective.get("launcher.show-hints"), Some("false"));
        assert!(!effective.plugin_enabled(&PluginId("native.fixture".into())));
        assert!(ConfigStore::load_with_policy(directories, None)
            .expect("disk")
            .plugin_enabled(&PluginId("native.fixture".into())));
        assert!(configuration.loads.load(Ordering::Relaxed) >= 6);
        jobs.shutdown().expect("worker shutdown");
    }

    #[test]
    fn unavailable_resident_configuration_does_not_fall_back_to_disk() {
        struct Unavailable;
        impl PackageConfiguration for Unavailable {
            fn load(&self) -> Result<ConfigStore, JobError> {
                Err(JobError::new(
                    "owner-unavailable",
                    "resident configuration authority unavailable",
                ))
            }
            fn set_plugin_enabled(
                &self,
                _plugin: &PluginId,
                _enabled: bool,
            ) -> Result<ConfigStore, JobError> {
                self.load()
            }
        }
        let fixture = Fixture::new();
        let (wake, wakes) = std::sync::mpsc::channel();
        let mut jobs = PackageJobs::new(
            fixture.directories.clone(),
            Vec::new(),
            None,
            Arc::new(Unavailable),
            Arc::new(move || {
                let _ = wake.send(());
            }),
        )
        .expect("worker");
        let id = jobs.submit(PackageOperation::List).expect("job");
        assert!(matches!(terminal(&jobs, id, &wakes).outcome,
            Some(JobOutcome::Failed { ref error, .. }) if error.code == "owner-unavailable"));
        jobs.shutdown().expect("shutdown");
    }

    #[test]
    fn indexed_identity_mismatch_is_a_consumer_failure_and_leaves_other_package_untouched() {
        let fixture = Fixture::new();
        let directories = &fixture.directories;
        let original = fixture.archive("original", "other", "1.0.0");
        PluginInstaller::new(directories)
            .install(&InstallSource::Archive(original), &mut |_| Ok(()))
            .expect("existing other package");
        let replacement = fixture.archive("replacement", "other", "2.0.0");
        struct ArchiveTransport(PathBuf);
        impl crikey_package_manager::PackageFetcher for ArchiveTransport {
            fn fetch(&self, _url: &str, destination: &Path) -> Result<(), PackageError> {
                std::fs::copy(&self.0, destination)?;
                Ok(())
            }
        }
        let entry = crikey_package_manager::IndexEntry {
            id: "selected".into(),
            name: "Selected".into(),
            version: "2.0.0".into(),
            runtime: "native".into(),
            summary: String::new(),
            homepage: None,
            licence: None,
            download_url: "https://fixture.invalid/package".into(),
            package_digest: crikey_package_manager::package_digest(&replacement)
                .expect("real archive digest"),
            signer_fingerprint: "0".repeat(32),
            extra: Default::default(),
        };
        let client = PluginIndexClient::with_parts(
            directories.cache_dir(),
            Vec::new(),
            std::time::Duration::from_secs(60),
            Box::new(ArchiveTransport(replacement.clone())),
            Box::new(ArchiveTransport(replacement)),
            TrustStore::load(directories).expect("fixture trust store"),
        );
        let downloaded = fixture.root.join("verified-index-download.crikey");
        client
            .download_package(&entry, &downloaded)
            .expect("selected entry's pinned digest verifies");
        let stops = Arc::new(AtomicUsize::new(0));
        let access = ResidentPackageAccess::new(
            Arc::new(LauncherLock::acquire(directories).expect("resident lock")),
            Arc::new(EmptyOwner {
                stops: Arc::clone(&stops),
            }),
        );
        let (jobs, _) = harness();
        let id = jobs
            .submit(PackageOperation::Show(PackageShowTarget::Index {
                id: "selected".into(),
            }))
            .expect("terminal reservation");
        let context = context(&jobs, id);
        // Exercise the same mutation path after a real digest-verified download:
        // authenticated bytes still cannot authorize a different manifest owner.
        let configuration = SessionConfiguration {
            directories: directories.clone(),
            overrides: Mutex::new(Vec::new()),
            loads: AtomicUsize::new(0),
        };
        let runtime = entry.parsed_runtime().expect("selected runtime");
        let outcome = mutate(
            &context,
            directories,
            Some(&access),
            &configuration,
            Some(InstallSource::Archive(downloaded)),
            None,
            (SignaturePolicy::unchecked(), Some((entry.id, runtime))),
        )
        .expect("job outcome");
        context.finish(outcome);
        let consumer = jobs.drain_events(1).pop().expect("consumer terminal");
        assert!(
            matches!(consumer.outcome, Some(JobOutcome::Failed { ref error, .. }) if error.detail.contains("indexed identity mismatch"))
        );
        assert_eq!(
            stops.load(Ordering::Relaxed),
            0,
            "identity refusal precedes stop/commit"
        );
        assert_eq!(
            std::fs::read_to_string(
                directories
                    .plugin_dir(PluginKind::Native)
                    .join("other/bin/plugin")
            )
            .expect("original bytes"),
            "1.0.0"
        );
        assert!(!directories
            .plugin_dir(PluginKind::Native)
            .join("selected")
            .exists());
    }
}
