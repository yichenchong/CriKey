//! Supervised native worker runtime (spec 16.3-16.6, 24.1-24.4).

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io::Read;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

use crikey_core::result_presentation::ResultPresentation;
use crikey_core::structured_arguments::{ArgumentValues, SubmissionFailureCode, SubmissionOutcome};
use crikey_core::structured_arguments::{ExecutableKey, OwnedItem};
use crikey_core::structured_view::{TypedExecutionOutcome, ViewExecutionContext, ViewReply};
use crikey_core::{ActionId, Item, ItemId, PageFrame, PluginId};

use crikey_native_protocol::convert::{from_proto_item, from_proto_page_frame, to_proto_page_input};
use crikey_native_protocol::frame::{read_frame, write_frame};
use crikey_native_protocol::message::{self, Envelope, Payload};
use crikey_native_protocol::transport::{Listener, Transport};
use crikey_native_protocol::{Capabilities, Endpoint, Message, ProtocolError, PROTOCOL_VERSION};

use crate::launch::{configure_command, LaunchSpec, TransportKind, WorkerOptions};
use crate::stream::{
    error_detail, BatchState, EchoMismatch, ExecuteOutcome, HealthSnapshot, NativePageRequest,
    NativeSuggestRequest, PluginError, ProtocolObservation, StreamDiagnostics, Suggestions, MAX_LOG_RECORDS,
    OBSERVATION_CAPACITY, READER_QUEUE_CAPACITY, READER_QUEUE_MAX_BYTES,
};
use crate::supervisor::NativeAdmission;

const SOCKET_POLL_MS: u64 = 20;
const CONTROL_WRITE_TIMEOUT_MS: u64 = 200;
const STDERR_TAIL_BYTES: usize = 8 * 1024;
const CHILD_REAP_TIMEOUT_MS: u64 = 2_000;
/// `CREATE_SUSPENDED`: the child is created with its primary thread suspended
/// so containment can be established before it executes any of its own code.
#[cfg(windows)]
const CREATE_SUSPENDED: u32 = 0x0000_0004;
static CONNECTION_COUNTER: AtomicU64 = AtomicU64::new(1);
static ENDPOINT_COUNTER: AtomicU64 = AtomicU64::new(1);
static INCARNATION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Discovery either refuses safely or yields a move-only incarnation-bound token.
#[derive(Debug)]
pub enum TypedPreparation {
    Ready(ConfirmedTypedExecution),
    Refused(SubmissionFailureCode),
}

/// Opaque confirmation; consuming it never performs discovery or reacquisition.
#[derive(Debug)]
pub struct ConfirmedTypedExecution {
    incarnation: u64,
    executable: ExecutableKey,
    fingerprint: [u8; 32],
    view: Option<ViewExecutionContext>,
}

/// How a native child process ended (spec 24.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    Clean,
    Crashed,
    Killed,
    ProtocolViolation,
}

/// Process status and bounded diagnostics retained after exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitRecord {
    pub kind: ExitKind,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stderr_tail: String,
    /// Internal supervisor classification for the operation that ended it.
    pub(crate) failure_kind: Option<crikey_plugin_supervisor::FailureKind>,
}

/// Failures at the host/process boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    Spawn(String),
    Handshake(String),
    Protocol(String),
    Timeout { plugin: PluginId, detail: String },
    Crashed { plugin: PluginId, detail: String },
    PluginFailed { plugin: PluginId, detail: String },
    ResourceLimit { plugin: PluginId, detail: String },
    Closed,
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(detail) => write!(formatter, "failed to spawn native plugin: {detail}"),
            Self::Handshake(detail) => write!(formatter, "native plugin handshake failed: {detail}"),
            Self::Protocol(detail) => write!(formatter, "native plugin protocol failure: {detail}"),
            Self::Timeout { plugin, detail } => {
                write!(formatter, "plugin `{}` timed out: {detail}", plugin.0)
            }
            Self::Crashed { plugin, detail } => {
                write!(formatter, "plugin `{}` crashed: {detail}", plugin.0)
            }
            Self::PluginFailed { plugin, detail } => {
                write!(formatter, "plugin `{}` failed: {detail}", plugin.0)
            }
            Self::ResourceLimit { plugin, detail } => {
                write!(formatter, "plugin `{}` resource limit: {detail}", plugin.0)
            }
            Self::Closed => formatter.write_str("native plugin worker is closed"),
        }
    }
}

impl std::error::Error for HostError {}

/// Startup failure retaining the exact child and pipe owners, if one was
/// created. Diagnostic access borrows the cause rather than extracting it and
/// silently dropping unfinished cleanup.
pub struct NativeSpawnFailure {
    error: HostError,
    startup: Option<NativeStartupOwner>,
}

impl NativeSpawnFailure {
    pub fn error(&self) -> &HostError {
        &self.error
    }

    /// Retries bounded physical cleanup without consuming a failed owner.
    pub fn retry_cleanup(&mut self) -> Result<(), HostError> {
        match self.startup.as_mut() {
            Some(owner) => owner.settle().map(|_| ()),
            None => Ok(()),
        }
    }
}

impl fmt::Debug for NativeSpawnFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeSpawnFailure")
    }
}

impl fmt::Display for NativeSpawnFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, formatter)
    }
}

impl std::error::Error for NativeSpawnFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Identity and capabilities reported by the plugin during startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginHandshake {
    pub plugin_id: String,
    pub plugin_name: String,
    pub plugin_version: String,
    pub sdk_version: String,
    pub protocol_version: u32,
    pub capabilities: Capabilities,
    /// Optional preview capability intersected with host support.
    pub preview: bool,
    /// Explicit executable declarations, independent of preview/process liveness.
    pub commands: bool,
    pub typed_arguments: bool,
    pub result_presentation: bool,
    pub structured_view: bool,
}

/// Bytes a plugin served in answer to one [`NativeWorker::request_resource`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginResource {
    /// The reference the plugin echoed back.
    pub reference: String,
    /// The payload, already checked against the caller's byte ceiling.
    pub content: Vec<u8>,
    /// The plugin's media-type hint, empty when it gave none.
    pub media_type: String,
}

/// What a plugin did when asked for one resource.
///
/// Three outcomes rather than two because a caller that memoizes has to tell
/// "the plugin answered no" from "the plugin has not answered": the first is
/// final, the second is worth retrying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceOutcome {
    /// The plugin served these bytes.
    Served(PluginResource),
    /// The plugin was reached and declined: it has no such reference, it
    /// answered with an error, it served nothing, or it served more than the
    /// caller's `max_bytes`. Asking again gets the same answer.
    Declined,
    /// The plugin said nothing before the caller's timeout. The request may
    /// still be in flight inside the plugin; a later attempt may be answered.
    Silent,
}

/// What a host resource request is asking a plugin for (spec 16.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// Icon pixels for an item the plugin published.
    Icon,
    /// An opaque file shipped inside the plugin package.
    File,
    /// Configuration data owned by the plugin.
    Configuration,
}

impl ResourceKind {
    fn to_proto(self) -> message::ResourceKind {
        message::ResourceKind::from_i32(match self {
            Self::Icon => 1,
            Self::File => 2,
            Self::Configuration => 3,
        })
    }
}

#[derive(Debug)]
enum ReaderEvent {
    Envelope(Box<Envelope>),
    Failure(ReaderFailure),
}

#[derive(Debug)]
struct ReaderContext {
    sender: SyncSender<ReaderEvent>,
    credits: Arc<AtomicI64>,
    queue: Arc<AtomicUsize>,
    queued_bytes: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
    protocol_violation: Arc<AtomicBool>,
    resource_limit: Arc<AtomicBool>,
    connection_id: u64,
    handshake_seen: Arc<AtomicBool>,
    observations: Arc<Mutex<VecDeque<ProtocolObservation>>>,
    mismatch: Arc<Mutex<Option<EchoMismatch>>>,
    privacy: Arc<Mutex<StderrTail>>,
    sensitive: Arc<AtomicBool>,
    view_request: Arc<AtomicU64>,
    /// Request id of the page frame the host is currently waiting for, or
    /// zero when it is waiting for none. A page frame is a reply and nothing
    /// else; without this the reader could not tell the answer the host asked
    /// for from a plugin repainting the screen whenever it pleased.
    page_request: Arc<AtomicU64>,
}

#[derive(Debug)]
struct ReaderFailure {
    error: ProtocolError,
    protocol_violation: bool,
    resource_limit: bool,
}

#[derive(Debug)]
enum WriterKind {
    Queued {
        sender: SyncSender<Envelope>,
        pending: Arc<AtomicUsize>,
    },
}

#[derive(Debug)]
struct WorkerLink {
    plugin: PluginId,
    writer: WriterKind,
    write_mutex: Mutex<()>,
    events: Mutex<Receiver<ReaderEvent>>,
    closed: Arc<AtomicBool>,
    protocol_violation: Arc<AtomicBool>,
    resource_limit: Arc<AtomicBool>,
    cancel_latched: AtomicBool,
    current_request: AtomicU64,
    current_generation: AtomicU64,
    connection_id: u64,
    credits: Arc<AtomicI64>,
    queued_events: Arc<AtomicUsize>,
    queued_bytes: Arc<AtomicUsize>,
    peak_queue_depth: Arc<AtomicUsize>,
    observations: Arc<Mutex<VecDeque<ProtocolObservation>>>,
    mismatch: Arc<Mutex<Option<EchoMismatch>>>,
    privacy: Arc<Mutex<StderrTail>>,
    sensitive: Arc<AtomicBool>,
    /// Shared with the reader; see [`ReaderContext::page_request`].
    page_request: Arc<AtomicU64>,
    view_request: Arc<AtomicU64>,
    reader: Mutex<Option<JoinHandle<()>>>,
    writer_thread: Mutex<Option<JoinHandle<()>>>,
    admission: NativeAdmission,
    revoked_request: Arc<AtomicU64>,
}

impl WorkerLink {
    fn observations(&self) -> Vec<ProtocolObservation> {
        lock_unpoisoned(&self.observations).iter().cloned().collect()
    }

    fn echo_mismatch(&self) -> Option<EchoMismatch> {
        lock_unpoisoned(&self.mismatch).clone()
    }

    fn send(&self, envelope: Envelope) -> Result<(), HostError> {
        let end = Instant::now()
            .checked_add(Duration::from_millis(CONTROL_WRITE_TIMEOUT_MS))
            .unwrap_or_else(Instant::now);
        self.send_until(envelope, end)
    }

    fn send_until(&self, envelope: Envelope, end: Instant) -> Result<(), HostError> {
        if is_callback(&envelope) && self.admission.is_closed() {
            return Err(HostError::Closed);
        }
        if self.closed.load(Ordering::Acquire) {
            return Err(HostError::Closed);
        }
        if message::validate_outbound(&envelope).is_err() {
            self.resource_limit.store(true, Ordering::Release);
            return Err(HostError::ResourceLimit {
                plugin: self.plugin.clone(),
                detail: "outbound envelope exceeds protocol bounds".into(),
            });
        }
        let callback = is_callback(&envelope);
        let _guard = if callback {
            match self.write_mutex.try_lock() {
                Ok(guard) => guard,
                Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    return Err(HostError::ResourceLimit {
                        plugin: self.plugin.clone(),
                        detail: "native callback writer is busy".into(),
                    })
                }
            }
        } else {
            loop {
                match self.write_mutex.try_lock() {
                    Ok(guard) => break guard,
                    Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
                    Err(std::sync::TryLockError::WouldBlock) if Instant::now() < end => thread::yield_now(),
                    Err(_) => return Err(self.write_error(envelope_kind(&envelope), ProtocolError::Timeout)),
                }
            }
        };
        match &self.writer {
            WriterKind::Queued { sender, pending } => {
                let kind = envelope_kind(&envelope);
                let mut value = envelope;
                pending.fetch_add(1, Ordering::AcqRel);
                loop {
                    if self.closed.load(Ordering::Acquire) {
                        pending.fetch_sub(1, Ordering::AcqRel);
                        return Err(HostError::Closed);
                    }
                    match sender.try_send(value) {
                        Ok(()) => return Ok(()),
                        Err(TrySendError::Disconnected(_)) => {
                            pending.fetch_sub(1, Ordering::AcqRel);
                            return Err(self.write_error(kind, ProtocolError::Closed));
                        }
                        Err(TrySendError::Full(next)) => {
                            if callback {
                                pending.fetch_sub(1, Ordering::AcqRel);
                                return Err(HostError::ResourceLimit {
                                    plugin: self.plugin.clone(),
                                    detail: "native callback writer queue is full".into(),
                                });
                            }
                            value = next;
                            if Instant::now() >= end {
                                pending.fetch_sub(1, Ordering::AcqRel);
                                return Err(self.write_error(kind, ProtocolError::Timeout));
                            }
                            thread::yield_now();
                        }
                    }
                }
            }
        }
    }

    fn write_error(&self, kind: &'static str, error: ProtocolError) -> HostError {
        let detail = format!("failed to write {kind} frame: {error:?}");
        if self.protocol_violation() {
            HostError::Protocol(detail)
        } else {
            HostError::Crashed {
                plugin: self.plugin.clone(),
                detail,
            }
        }
    }

    fn recv(&self, timeout: Duration) -> Result<ReaderEvent, RecvTimeoutError> {
        let end = Instant::now() + timeout;
        loop {
            let revoked = self.revoked_request.load(Ordering::Acquire);
            if revoked != 0 && revoked == self.current_request.load(Ordering::Acquire) {
                return Err(RecvTimeoutError::Disconnected);
            }
            let remaining = end.saturating_duration_since(Instant::now());
            let result = lock_unpoisoned(&self.events)
                .recv_timeout(remaining.min(Duration::from_millis(SOCKET_POLL_MS)));
            match result {
                Ok(event) => {
                    let _ = self
                        .queued_events
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |depth| {
                            Some(depth.saturating_sub(1))
                        });
                    let bytes = reader_event_size(&event);
                    let _ = self
                        .queued_bytes
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |depth| {
                            Some(depth.saturating_sub(bytes))
                        });
                    let mut event = event;
                    sanitize_event(&mut event, &self.privacy);
                    return Ok(event);
                }
                Err(RecvTimeoutError::Timeout) if Instant::now() < end => (),
                Err(error) => return Err(error),
            }
        }
    }

    fn set_current(&self, request_id: u64, generation: u64) {
        self.current_request.store(request_id, Ordering::Release);
        self.current_generation.store(generation, Ordering::Release);
    }

    fn send_cancel(&self) {
        self.cancel_latched.store(true, Ordering::Release);
        let envelope = Envelope {
            connection_id: self.connection_id,
            request_id: self.current_request.load(Ordering::Acquire),
            generation: self.current_generation.load(Ordering::Acquire),
            deadline_ms: 0,
            payload: Some(Payload::Cancel(message::Cancel {
                reason: "cancelled".to_owned(),
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        };
        let _ = self.send(envelope);
    }

    fn send_flow(&self) -> Result<(), HostError> {
        self.credits.fetch_add(1, Ordering::AcqRel);
        let envelope = Envelope {
            connection_id: self.connection_id,
            request_id: self.current_request.load(Ordering::Acquire),
            generation: self.current_generation.load(Ordering::Acquire),
            deadline_ms: 0,
            payload: Some(Payload::Flow(message::FlowControl {
                credits: 1,
                paused: false,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        };
        match self.send(envelope) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = self
                    .credits
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                        Some(value.saturating_sub(1))
                    });
                Err(error)
            }
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn protocol_violation(&self) -> bool {
        self.protocol_violation.load(Ordering::Acquire)
    }

    fn wait_writer_idle(&self, end: Instant) {
        let WriterKind::Queued { pending, .. } = &self.writer;
        let pending = Arc::clone(pending);
        while pending.load(Ordering::Acquire) != 0 && Instant::now() < end {
            thread::yield_now();
        }
    }
}

/// A clonable out-of-band cancellation handle.
#[derive(Debug, Clone)]
pub struct CancelHandle {
    link: Arc<WorkerLink>,
}

impl CancelHandle {
    /// Returns the same bounded protocol observation ring as the worker, even
    /// while another thread owns its blocking call.
    pub fn observations(&self) -> Vec<ProtocolObservation> {
        self.link.observations()
    }

    /// Requests cooperative cancellation of the current call.
    pub fn cancel(&self) {
        self.link.send_cancel();
    }

    /// Clears the cancellation latch.
    pub fn reset(&self) {
        self.link.cancel_latched.store(false, Ordering::Release);
    }
}
// observation API is implemented below
/// A live native plugin process and framed protocol channel.
#[derive(Debug)]
pub struct NativeWorker {
    spec: LaunchSpec,
    incarnation: u64,
    options: WorkerOptions,
    handshake: PluginHandshake,
    link: Arc<WorkerLink>,
    child: Option<Child>,
    stderr_tail: Arc<Mutex<StderrTail>>,
    stderr_reader: Option<JoinHandle<()>>,
    exit: Option<ExitRecord>,
    // Exit observation is separate from pipe/process cleanup so snapshot
    // refreshers never wait for descendants that retain a standard stream.
    observed_exit: Option<Result<(), ()>>,
    process_status: Option<std::process::ExitStatus>,
    containment_reap_ready: bool,
    thread_failure: Option<&'static str>,
    retirement_kind: Option<ExitKind>,
    closing_failure: Option<&'static str>,
    diagnostics: StreamDiagnostics,
    sandbox_report: crikey_sandbox::SandboxReport,
    next_request_id: u64,
    call_succeeded: bool,
    catalog_initialized: bool,
    failure_kind: Option<crikey_plugin_supervisor::FailureKind>,
    catalog_presentation: Option<ResultPresentation>,
    suggestion_presentation: Option<ResultPresentation>,
    view: Option<ViewReply>,
    #[cfg(windows)]
    job: Option<OwnedJob>,
}

impl NativeWorker {
    /// Host-created physical process identity; never taken from plugin bytes.
    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    /// Returns bounded observations of envelopes actually sent or received.
    pub fn observations(&self) -> Vec<ProtocolObservation> {
        self.link.observations()
    }

    /// Returns observed request/generation echo mismatches, if any.
    pub fn echo_mismatch(&self) -> Option<EchoMismatch> {
        self.link.echo_mismatch()
    }

    /// Returns the non-zero identifier assigned to this connection.
    pub fn connection_id(&self) -> u64 {
        self.link.connection_id
    }

    /// Shuts down and returns both the exit record and the bounded observation ring.
    pub fn shutdown_with_observations(mut self) -> (ExitRecord, Vec<ProtocolObservation>) {
        let exit = self.shutdown_inner();
        let observations = self.link.observations();
        (exit, observations)
    }
    pub(crate) fn take_failure_kind(&mut self) -> Option<crikey_plugin_supervisor::FailureKind> {
        if self.failure_kind.is_none() && self.link.resource_limit.swap(false, Ordering::AcqRel) {
            self.failure_kind = Some(crikey_plugin_supervisor::FailureKind::ResourceLimit);
        }
        self.failure_kind.take()
    }

    pub(crate) fn take_call_success(&mut self) -> bool {
        std::mem::take(&mut self.call_succeeded)
    }

    /// Launches a child and completes the authenticated handshake (spec 16.3, 16.6).
    ///
    /// A failure retains any created child and unfinished pipes. Keep that
    /// [`NativeSpawnFailure`] through the invocation boundary; bounded cleanup
    /// can be retried without giving up ownership.
    pub fn spawn(spec: LaunchSpec, options: WorkerOptions) -> Result<Self, NativeSpawnFailure> {
        let mut startup = None;
        match Self::spawn_with_admission(spec, options, NativeAdmission::default(), &mut startup) {
            Ok(worker) => Ok(worker),
            Err(error) => Err(NativeSpawnFailure { error, startup }),
        }
    }

    pub(crate) fn spawn_with_admission(
        spec: LaunchSpec,
        options: WorkerOptions,
        admission: NativeAdmission,
        retained: &mut Option<NativeStartupOwner>,
    ) -> Result<Self, HostError> {
        let plugin = spec.plugin.clone();
        Self::spawn_inner(spec, options, admission, retained).map_err(|error| match error {
            HostError::Spawn(detail) => HostError::Spawn(format!("plugin `{}`: {detail}", plugin.0)),
            HostError::Handshake(detail) => HostError::Handshake(format!("plugin `{}`: {detail}", plugin.0)),
            other => other,
        })
    }

    fn spawn_inner(
        spec: LaunchSpec,
        options: WorkerOptions,
        admission: NativeAdmission,
        retained: &mut Option<NativeStartupOwner>,
    ) -> Result<Self, HostError> {
        if retained.is_some() {
            return Err(HostError::Closed);
        }
        if options.limits.initial_credits == 0 {
            return Err(HostError::ResourceLimit {
                plugin: spec.plugin.clone(),
                detail: "initial_credits must be greater than zero".to_owned(),
            });
        }
        let incarnation = INCARNATION_COUNTER
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| HostError::Spawn("native incarnation space exhausted".into()))?;
        let token = session_token().map_err(HostError::Spawn)?;
        let connection_id = next_connection_id();
        let (endpoint, listener) = make_endpoint(options.transport, &token)?;
        // Resolve the executable against the CURRENT directory before any
        // `current_dir` is applied. `Command` resolves a relative program name
        // against the child's working directory, so a caller-supplied
        // `./plugin` would otherwise fail with a bare "No such file or
        // directory" the moment a plugin's package dir is set as its cwd.
        let executable = std::fs::canonicalize(&spec.executable).unwrap_or_else(|_| spec.executable.clone());
        let mut command = Command::new(&executable);
        command.args(&spec.arguments);
        let stdio = matches!(options.transport, TransportKind::Stdio);
        command.stdin(if stdio { Stdio::piped() } else { Stdio::null() });
        command.stdout(if stdio { Stdio::piped() } else { Stdio::null() });
        command.stderr(Stdio::piped());
        if let Some(dir) = &spec.working_dir {
            command.current_dir(dir);
        }
        // Stripped unless the manifest bought the ambient environment. A
        // plugin that never declared `permissions.environment` must not learn
        // the user's tokens, proxies and paths just by being spawned by us.
        if !spec.inherit_environment {
            command.env_clear();
        }
        add_restricted_environment(&mut command, &spec, &endpoint, &token);
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(target_os = "linux")]
        configure_parent_death(&mut command);
        configure_command(&mut command, &options).map_err(HostError::Spawn)?;
        // Prepared here, in the parent, because building the rule set opens
        // descriptors and allocates; the child runs two syscalls on the result
        // between fork and exec. An unavailable sandbox is reported, not
        // fatal (spec 20.2).
        let sandbox = options.sandbox.prepare();
        sandbox.install(&mut command);
        let sandbox_report = sandbox.report().clone();
        // Windows children start suspended so their job object is already in
        // force when they run their first instruction. `AssignProcessToJobObject`
        // does not examine memory the process allocated before assignment, and a
        // descendant created before assignment is never pulled into the job
        // afterwards - so a plugin spawned running could pass the memory cap and
        // leave a survivor that `TerminateJobObject` cannot reach, while
        // diagnostics still reported the limits as enforced. Unix is unchanged:
        // `process_group(0)` above already covers the whole tree from creation.
        #[cfg(windows)]
        command.creation_flags(CREATE_SUSPENDED);
        let _start = admission.commit(true).ok_or(HostError::Closed)?;
        let mut child = spawn_child(command).map_err(|error| HostError::Spawn(error.to_string()))?;
        // Held as an owner, not a raw handle: every early return below this
        // point drops it, which closes the job and - through
        // `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` - kills descendants that the
        // direct `child.kill()` cannot see.
        #[cfg(windows)]
        let job = match create_job_for_child(&child, &options.limits) {
            Ok(value) => {
                if let Err(error) = resume_primary_thread(child.id()) {
                    retain_failed_startup(&spec.plugin, child, None, None, Some(value), retained);
                    return Err(HostError::Spawn(error));
                }
                Some(value)
            }
            Err(error) => {
                retain_failed_startup(&spec.plugin, child, None, None, None, retained);
                return Err(HostError::Spawn(error));
            }
        };
        #[cfg(not(windows))]
        let job = ();
        let stderr = match child.stderr.take() {
            Some(value) => value,
            None => {
                retain_failed_startup(&spec.plugin, child, None, None, job, retained);
                return Err(HostError::Spawn("child stderr was not piped".to_owned()));
            }
        };
        let stderr_tail = Arc::new(Mutex::new(StderrTail::default()));
        let stderr_reader = match spawn_stderr_drain(stderr, Arc::clone(&stderr_tail)) {
            Ok(handle) => handle,
            Err(error) => {
                retain_failed_startup(&spec.plugin, child, None, None, job, retained);
                return Err(HostError::Spawn(format!("stderr drain thread: {error}")));
            }
        };
        let credits = Arc::new(AtomicI64::new(0));
        let sensitive = Arc::new(AtomicBool::new(false));
        let (event_sender, event_receiver) = mpsc::sync_channel(READER_QUEUE_CAPACITY);
        let queued = Arc::new(AtomicUsize::new(0));
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let protocol_violation = Arc::new(AtomicBool::new(false));
        let resource_limit = Arc::new(AtomicBool::new(false));
        let handshake_seen = Arc::new(AtomicBool::new(false));
        let observations = Arc::new(Mutex::new(VecDeque::with_capacity(OBSERVATION_CAPACITY)));
        let mismatch = Arc::new(Mutex::new(None));
        let page_request = Arc::new(AtomicU64::new(0));
        let view_request = Arc::new(AtomicU64::new(0));
        let writer_pending = Arc::new(AtomicUsize::new(0));
        let revoked_request = Arc::new(AtomicU64::new(0));
        let (transport, writer, outgoing, writer_thread) = match options.transport {
            TransportKind::Stdio => {
                let stdin = match child.stdin.take() {
                    Some(value) => value,
                    None => {
                        retain_failed_startup(&spec.plugin, child, Some(stderr_reader), None, job, retained);
                        return Err(HostError::Spawn("child stdin was not piped".to_owned()));
                    }
                };
                let stdout = match child.stdout.take() {
                    Some(value) => value,
                    None => {
                        retain_failed_startup(&spec.plugin, child, Some(stderr_reader), None, job, retained);
                        return Err(HostError::Spawn("child stdout was not piped".to_owned()));
                    }
                };
                let (sender, receiver) = mpsc::sync_channel(READER_QUEUE_CAPACITY);
                let writer = WriterKind::Queued {
                    sender,
                    pending: Arc::clone(&writer_pending),
                };
                let writer_thread = match spawn_stdio_writer(
                    stdin,
                    receiver,
                    Arc::clone(&closed),
                    Arc::clone(&writer_pending),
                    admission.clone(),
                    Arc::clone(&revoked_request),
                    Arc::clone(&observations),
                ) {
                    Ok(worker) => worker,
                    Err(error) => {
                        retain_failed_startup(&spec.plugin, child, Some(stderr_reader), None, job, retained);
                        return Err(HostError::Spawn(format!("native writer thread: {error}")));
                    }
                };
                (
                    Box::new(ChildStdioTransport { stdout }) as Box<dyn Transport>,
                    writer,
                    None,
                    Some(writer_thread),
                )
            }
            TransportKind::UnixSocket | TransportKind::NamedPipe => {
                let _ = child.stdin.take();
                let _ = child.stdout.take();
                let listener = match listener {
                    Some(value) => value,
                    None => {
                        retain_failed_startup(&spec.plugin, child, Some(stderr_reader), None, job, retained);
                        return Err(HostError::Spawn("native listener was not created".to_owned()));
                    }
                };
                let transport = match listener.accept(Some(Duration::from_millis(options.startup_timeout_ms)))
                {
                    Ok(value) => value,
                    Err(error) => {
                        retain_failed_startup(&spec.plugin, child, Some(stderr_reader), None, job, retained);
                        return Err(HostError::Handshake(format!("plugin did not connect: {error:?}")));
                    }
                };
                let (sender, receiver) = mpsc::sync_channel(READER_QUEUE_CAPACITY);
                let writer = WriterKind::Queued {
                    sender,
                    pending: Arc::clone(&writer_pending),
                };
                (transport, writer, Some(receiver), None)
            }
        };
        let reader_context = ReaderContext {
            sender: event_sender,
            credits: Arc::clone(&credits),
            queue: Arc::clone(&queued),
            queued_bytes: Arc::clone(&queued_bytes),
            peak: Arc::clone(&peak),
            closed: Arc::clone(&closed),
            protocol_violation: Arc::clone(&protocol_violation),
            resource_limit: Arc::clone(&resource_limit),
            connection_id,
            handshake_seen: Arc::clone(&handshake_seen),
            observations: Arc::clone(&observations),
            mismatch: Arc::clone(&mismatch),
            page_request: Arc::clone(&page_request),
            view_request: Arc::clone(&view_request),
            privacy: Arc::clone(&stderr_tail),
            sensitive: Arc::clone(&sensitive),
        };
        let link = Arc::new(WorkerLink {
            plugin: spec.plugin.clone(),
            writer,
            write_mutex: Mutex::new(()),
            events: Mutex::new(event_receiver),
            closed,
            protocol_violation,
            resource_limit,
            cancel_latched: AtomicBool::new(false),
            current_request: AtomicU64::new(0),
            current_generation: AtomicU64::new(0),
            connection_id,
            credits,
            queued_events: Arc::clone(&queued),
            queued_bytes,
            peak_queue_depth: Arc::clone(&peak),
            observations,
            mismatch,
            page_request,
            view_request,
            reader: Mutex::new(None),
            writer_thread: Mutex::new(writer_thread),
            privacy: Arc::clone(&stderr_tail),
            sensitive,
            admission,
            revoked_request,
        });
        let reader = if let Some(outgoing_receiver) = outgoing {
            spawn_multiplex_reader(
                transport,
                outgoing_receiver,
                reader_context,
                Arc::clone(&writer_pending),
                link.admission.clone(),
                Arc::clone(&link.revoked_request),
            )
        } else {
            spawn_blocking_reader(transport, reader_context)
        };
        match reader {
            Ok(reader) => *lock_unpoisoned(&link.reader) = Some(reader),
            Err(error) => {
                retain_failed_startup(
                    &spec.plugin,
                    child,
                    Some(stderr_reader),
                    Some(Arc::clone(&link)),
                    job,
                    retained,
                );
                return Err(HostError::Spawn(format!("native reader thread: {error}")));
            }
        }
        let handshake = match receive_handshake(&link, &token, &options) {
            Ok(value) => value,
            Err(error) => {
                link.close();
                retain_failed_startup(
                    &spec.plugin,
                    child,
                    Some(stderr_reader),
                    Some(Arc::clone(&link)),
                    job,
                    retained,
                );
                let stderr = lock_unpoisoned(&stderr_tail).text();
                if stderr.is_empty() {
                    return Err(error);
                }
                let detail = format!("{error}; child stderr: {stderr}");
                return Err(match error {
                    HostError::Spawn(_) => HostError::Spawn(detail),
                    HostError::Handshake(_) => HostError::Handshake(detail),
                    HostError::Protocol(_) => HostError::Protocol(detail),
                    HostError::Timeout { plugin, .. } => HostError::Timeout { plugin, detail },
                    HostError::Crashed { plugin, .. } => HostError::Crashed { plugin, detail },
                    HostError::PluginFailed { plugin, .. } => HostError::PluginFailed { plugin, detail },
                    HostError::ResourceLimit { plugin, .. } => HostError::ResourceLimit { plugin, detail },
                    HostError::Closed => HostError::Closed,
                });
            }
        };
        link.credits
            .store(i64::from(options.limits.initial_credits), Ordering::Release);
        Ok(Self {
            spec,
            incarnation,
            options,
            handshake,
            link,
            child: Some(child),
            stderr_tail,
            stderr_reader: Some(stderr_reader),
            exit: None,
            observed_exit: None,
            process_status: None,
            containment_reap_ready: false,
            thread_failure: None,
            retirement_kind: None,
            closing_failure: None,
            diagnostics: StreamDiagnostics::default(),
            sandbox_report,
            next_request_id: 1,
            call_succeeded: false,
            catalog_initialized: false,
            catalog_presentation: None,
            suggestion_presentation: None,
            view: None,
            failure_kind: None,
            #[cfg(windows)]
            job,
        })
    }

    /// Returns self-reported handshake diagnostics.
    pub fn handshake(&self) -> &PluginHandshake {
        &self.handshake
    }

    /// What the kernel actually enforces on this plugin's process (spec 20.2).
    ///
    /// Read from the sandbox that was installed on the child, so a caller
    /// reporting it is reporting what happened rather than what was asked for.
    pub fn sandbox_report(&self) -> &crikey_sandbox::SandboxReport {
        &self.sandbox_report
    }

    /// Returns host-authoritative plugin identity.
    pub fn plugin(&self) -> &PluginId {
        &self.spec.plugin
    }

    /// Delivers the latest complete configuration state to the plugin (spec 21.4).
    ///
    /// Host-initiated and unsolicited: there is no request to correlate, so the
    /// envelope carries `request_id = 0`, which no real request ever uses
    /// (`next_request_id` starts at 1). That matters for the failure path — the
    /// SDK answers a raising `on_configuration` with an `Error` envelope, and
    /// tagging this publication with a live request id would let that error be
    /// mistaken for the reply to a call the host is waiting on. Against
    /// `request_id = 0` it is simply an envelope no call claims, which the reader
    /// already discards.
    ///
    /// Nothing is awaited. Configuration delivery is not a request: a host that
    /// blocked here would let one slow plugin delay publication to every other
    /// plugin, and there is no answer worth waiting for.
    ///
    /// `complete` says whether `values` is the whole state rather than a delta.
    /// The host only ever publishes whole states — that IS the coalescing rule of
    /// spec 21.4 — so a caller passing `false` is describing a delta path that
    /// does not exist yet.
    pub fn send_configuration(
        &mut self,
        values: &BTreeMap<String, String>,
        complete: bool,
    ) -> Result<(), HostError> {
        self.ensure_alive()?;
        let envelope = Envelope {
            connection_id: self.link.connection_id,
            request_id: 0,
            generation: 0,
            deadline_ms: 0,
            payload: Some(Payload::Configuration(message::ConfigurationChange {
                values: values.clone(),
                complete,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        };
        self.send_control(envelope)
    }

    /// Streams and folds the plugin catalog under aggregate bounds.
    pub fn build_catalog(&mut self) -> Result<Vec<Item>, HostError> {
        self.build_catalog_with_timeout_ms(self.options.call_timeout_ms)
    }

    /// Streams the catalog with a per-call deadline without changing the
    /// configured timeout used by subsequent catalog, query, or preview calls.
    pub fn build_catalog_with_timeout_ms(&mut self, timeout_ms: u64) -> Result<Vec<Item>, HostError> {
        self.ensure_alive()?;
        self.catalog_initialized = false;
        self.catalog_presentation = None;
        let request_id = self.begin_request(0);
        let envelope = Envelope {
            connection_id: self.link.connection_id,
            request_id,
            generation: 0,
            deadline_ms: timeout_ms,
            payload: Some(Payload::CatalogRequest(message::CatalogRequest {
                max_items: self.options.limits.max_catalog_items as u64,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        };
        self.send_control(envelope)?;
        let end = deadline(timeout_ms);
        let mut items = Vec::new();
        let mut batches = 0usize;
        let mut bytes = 0usize;
        let mut next_sequence = 0_u64;
        let mut presentation = None;
        loop {
            let event = self.next_event(end, request_id, 0)?;
            let envelope = match event {
                ReaderEvent::Envelope(value) => value,
                ReaderEvent::Failure(failure) => return Err(self.transport_error(failure)),
            };
            match envelope.payload {
                Some(Payload::CatalogBatch(mut batch)) => {
                    self.validate_batch_sequence(batch.sequence, &mut next_sequence, "catalog")?;
                    if let Some(error) = batch.error.as_ref() {
                        self.validate_nested_error(error, request_id, "catalog")?;
                    }
                    batches = batches.saturating_add(1);
                    let batch_bytes = batch.encode().len();
                    self.accumulate_presentation(&mut presentation, batch.presentation.take())?;
                    bytes = bytes.saturating_add(batch_bytes);
                    self.record_batch(batch_bytes, batch.items.len());
                    if batches > self.options.limits.max_batches_per_query
                        || bytes > self.options.limits.max_bytes_per_query
                    {
                        if batch.done {
                            self.mark_call_succeeded();
                            return Ok(items);
                        }
                        self.truncate(request_id, 0)?;
                        self.mark_call_succeeded();
                        return Ok(items);
                    }
                    let remaining = self.options.limits.max_catalog_items.saturating_sub(items.len());
                    if batch.items.len() > remaining {
                        items.extend(
                            batch
                                .items
                                .iter()
                                .take(remaining)
                                .map(|value| from_proto_item(&self.spec.plugin, value)),
                        );
                        if batch.done {
                            self.mark_call_succeeded();
                            return Ok(items);
                        }
                        self.truncate(request_id, 0)?;
                        self.mark_call_succeeded();
                        return Ok(items);
                    }
                    items.extend(
                        batch
                            .items
                            .iter()
                            .map(|value| from_proto_item(&self.spec.plugin, value)),
                    );
                    self.replenish_credit()?;
                    if let Some(error) = batch.error.as_ref() {
                        let error = plugin_error(error);
                        return Err(HostError::PluginFailed {
                            plugin: self.spec.plugin.clone(),
                            detail: error_detail(&error),
                        });
                    }
                    if batch.done {
                        self.catalog_presentation = self.admit_presentation(presentation, &items)?;
                        self.catalog_initialized = true;
                        self.mark_call_succeeded();
                        return Ok(items);
                    }
                }
                Some(Payload::Log(log)) => self.record_log(log.message),
                Some(Payload::Error(error)) => return Err(self.error_payload(error, request_id)),
                Some(_) | None => {
                    return Err(self.protocol_failure("unexpected catalog response payload".to_owned()));
                }
            }
        }
    }

    /// Streams one suggestion request under aggregate bounds.
    pub fn suggest(&mut self, request: &NativeSuggestRequest) -> Result<Suggestions, HostError> {
        self.suggest_inner(request, true)
    }

    /// Runs a request while preserving a caller-latched cancellation flag.
    pub fn suggest_with_cancel_latched(
        &mut self,
        request: &NativeSuggestRequest,
    ) -> Result<Suggestions, HostError> {
        self.suggest_inner(request, false)
    }

    fn suggest_inner(
        &mut self,
        request: &NativeSuggestRequest,
        clear_cancel: bool,
    ) -> Result<Suggestions, HostError> {
        self.ensure_alive()?;
        self.suggestion_presentation = None;
        if clear_cancel {
            self.link.cancel_latched.store(false, Ordering::Release);
        }
        let request_id = self.begin_request(request.generation);
        let envelope = Envelope {
            connection_id: self.link.connection_id,
            request_id,
            generation: request.generation,
            deadline_ms: self.options.call_timeout_ms,
            payload: Some(Payload::Suggest(message::SuggestRequest {
                text: request.text.clone(),
                normalized_text: request.normalized.clone(),
                selected_item_id: request.selected_item_id.clone().unwrap_or_default(),
                max_items: self.options.limits.max_items_per_query as u64,
                selected_scope_id: if self.handshake.result_presentation {
                    request.selected_scope_id.clone()
                } else {
                    None
                },
                max_batches: self.options.limits.max_batches_per_query as u64,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        };
        self.send_control(envelope)?;
        if self.link.cancel_latched.load(Ordering::Acquire) {
            self.link.send_cancel();
        }
        let end = deadline(self.options.call_timeout_ms);
        let mut items = Vec::new();
        let mut logs = Vec::new();
        let mut log_bytes = 0usize;
        let mut log_records = 0usize;
        let mut batches = 0usize;
        let mut bytes = 0usize;
        let mut next_sequence = 0_u64;
        let mut presentation = None;
        loop {
            let event = self.next_event(end, request_id, request.generation)?;
            let envelope = match event {
                ReaderEvent::Envelope(value) => value,
                ReaderEvent::Failure(failure) => return Err(self.transport_error(failure)),
            };
            match envelope.payload {
                Some(Payload::Results(mut batch)) => {
                    self.validate_batch_sequence(batch.sequence, &mut next_sequence, "result")?;
                    if let Some(error) = batch.error.as_ref() {
                        self.validate_nested_error(error, request_id, "result")?;
                    }
                    let state = batch.state.as_i32();
                    if !matches!(state, 1..=4) {
                        return Err(self.protocol_failure("unknown result batch state".to_owned()));
                    }
                    batches = batches.saturating_add(1);
                    let batch_bytes = batch.encode().len();
                    bytes = bytes.saturating_add(batch_bytes);
                    self.record_batch(batch_bytes, batch.items.len());
                    self.accumulate_presentation(&mut presentation, batch.presentation.take())?;
                    let limit_exceeded = batches > self.options.limits.max_batches_per_query
                        || bytes > self.options.limits.max_bytes_per_query;
                    if limit_exceeded && state == 1 {
                        self.truncate(request_id, request.generation)?;
                        let terminal_batches =
                            self.await_terminal(request_id, request.generation, end, &mut next_sequence)?;
                        self.mark_call_succeeded();
                        return Ok(Suggestions {
                            presentation: None,
                            items,
                            state: BatchState::Final,
                            log: logs,
                            error: None,
                            batches: batches.saturating_add(terminal_batches),
                            truncated: true,
                        });
                    }
                    let remaining = self
                        .options
                        .limits
                        .max_items_per_query
                        .saturating_sub(items.len());
                    if batch.items.len() > remaining {
                        items.extend(
                            batch
                                .items
                                .iter()
                                .take(remaining)
                                .map(|value| from_proto_item(&self.spec.plugin, value)),
                        );
                        if state != 1 {
                            let error = if state == 4 {
                                batch.error.as_ref().map(plugin_error).or_else(|| {
                                    Some(PluginError {
                                        message: "plugin returned FAILED without an error".to_owned(),
                                        detail: String::new(),
                                    })
                                })
                            } else {
                                None
                            };
                            self.mark_call_succeeded();
                            return Ok(Suggestions {
                                presentation: None,
                                items,
                                state: match state {
                                    2 => BatchState::Final,
                                    3 => BatchState::Cancelled,
                                    _ => BatchState::Failed,
                                },
                                log: logs,
                                error,
                                batches,
                                truncated: true,
                            });
                        }
                        self.truncate(request_id, request.generation)?;
                        let terminal_batches =
                            self.await_terminal(request_id, request.generation, end, &mut next_sequence)?;
                        self.mark_call_succeeded();
                        return Ok(Suggestions {
                            presentation: None,
                            items,
                            state: BatchState::Final,
                            log: logs,
                            error: None,
                            batches: batches.saturating_add(terminal_batches),
                            truncated: true,
                        });
                    }
                    items.extend(
                        batch
                            .items
                            .iter()
                            .map(|value| from_proto_item(&self.spec.plugin, value)),
                    );
                    self.replenish_credit()?;
                    if let Some(error) = batch.error.as_ref() {
                        self.validate_nested_error(error, request_id, "result")?;
                        self.mark_call_succeeded();
                        return Ok(Suggestions {
                            presentation: None,
                            items,
                            state: BatchState::Failed,
                            log: logs,
                            error: Some(plugin_error(error)),
                            batches,
                            truncated: limit_exceeded,
                        });
                    }
                    match state {
                        1 => {}
                        2 => {
                            let admitted = if limit_exceeded {
                                None
                            } else {
                                self.admit_presentation(presentation, &items)?
                            };
                            self.suggestion_presentation = admitted.clone();
                            self.mark_call_succeeded();
                            return Ok(Suggestions {
                                presentation: admitted,
                                items,
                                state: BatchState::Final,
                                log: logs,
                                error: None,
                                batches,
                                truncated: limit_exceeded,
                            });
                        }
                        3 => {
                            self.link.cancel_latched.store(false, Ordering::Release);
                            self.mark_call_succeeded();
                            return Ok(Suggestions {
                                presentation: None,
                                items,
                                state: BatchState::Cancelled,
                                log: logs,
                                error: None,
                                batches,
                                truncated: limit_exceeded,
                            });
                        }
                        4 => {
                            let error = batch.error.as_ref().map(plugin_error).or_else(|| {
                                Some(PluginError {
                                    message: "plugin returned FAILED without an error".to_owned(),
                                    detail: String::new(),
                                })
                            });
                            self.mark_call_succeeded();
                            return Ok(Suggestions {
                                presentation: None,
                                items,
                                state: BatchState::Failed,
                                log: logs,
                                error,
                                batches,
                                truncated: limit_exceeded,
                            });
                        }
                        _ => unreachable!("result batch state validated above"),
                    }
                }
                Some(Payload::Log(log)) => {
                    if !self.is_sensitive() && log_records < MAX_LOG_RECORDS {
                        log_records = log_records.saturating_add(1);
                        let remaining = self.options.limits.max_log_bytes.saturating_sub(log_bytes);
                        if remaining > 0 {
                            let mut value = log.message;
                            if value.len() > remaining {
                                value.truncate(valid_char_boundary(&value, remaining));
                            }
                            log_bytes = log_bytes.saturating_add(value.len());
                            logs.push(value);
                        }
                    }
                }
                Some(Payload::Error(error)) => return Err(self.error_payload(error, request_id)),
                Some(_) | None => {
                    return Err(self.protocol_failure("unexpected suggestion response payload".to_owned()));
                }
            }
        }
    }

    /// Metadata from this incarnation's last complete accepted catalog.
    pub fn catalog_presentation(&self) -> Option<&ResultPresentation> {
        self.catalog_presentation.as_ref()
    }

    fn accumulate_presentation(
        &mut self,
        aggregate: &mut Option<message::ResultPresentation>,
        fragment: Option<message::ResultPresentation>,
    ) -> Result<(), HostError> {
        let Some(mut fragment) = fragment else {
            return Ok(());
        };
        if !self.handshake.typed_arguments && !self.handshake.result_presentation {
            return Err(self.protocol_failure("unnegotiated result metadata"));
        }
        if !self.handshake.typed_arguments && !fragment.schemas.is_empty() {
            return Err(self.protocol_failure("unnegotiated argument schemas"));
        }
        if !self.handshake.result_presentation
            && (!fragment.sections.is_empty() || !fragment.scopes.is_empty() || !fragment.items.is_empty())
        {
            return Err(self.protocol_failure("unnegotiated visual presentation"));
        }
        let combined = aggregate.get_or_insert_with(Default::default);
        // Keep a conservative cumulative retained-allocation charge, including
        // discarded fragment wrappers/unknowns, rather than allowing fragment
        // boundaries to reset the decoder's allocation allowance.
        let fragment_charge = fragment
            .decoded_allocation
            .max(crikey_native_protocol::typed::presentation_charge(&fragment));
        let growth = presentation_growth(&combined.sections, fragment.sections.len())
            .saturating_add(presentation_growth(&combined.scopes, fragment.scopes.len()))
            .saturating_add(presentation_growth(&combined.items, fragment.items.len()))
            .saturating_add(presentation_growth(&combined.schemas, fragment.schemas.len()));
        let allocation = combined
            .decoded_allocation
            .saturating_add(fragment_charge)
            .saturating_add(growth);
        if allocation > 4 * 1024 * 1024 {
            return Err(self.protocol_failure("result metadata exceeds aggregate allocation bounds"));
        }
        combined.decoded_allocation = allocation;
        combined.decoded_bytes = combined.decoded_bytes.saturating_add(fragment.decoded_bytes);
        if combined.decoded_bytes > 4 * 1024 * 1024 {
            return Err(self.protocol_failure("result metadata exceeds aggregate byte bounds"));
        }
        if combined.sections.len().saturating_add(fragment.sections.len()) > 128
            || combined.scopes.len().saturating_add(fragment.scopes.len()) > 128
            || combined.items.len().saturating_add(fragment.items.len()) > 4096
            || combined.schemas.len().saturating_add(fragment.schemas.len()) > 4096
        {
            return Err(self.protocol_failure("result metadata exceeds aggregate bounds"));
        }
        // Exact reservations make growth predictable before any append allocates.
        reserve_presentation(&mut combined.sections, fragment.sections.len());
        reserve_presentation(&mut combined.scopes, fragment.scopes.len());
        reserve_presentation(&mut combined.items, fragment.items.len());
        reserve_presentation(&mut combined.schemas, fragment.schemas.len());
        combined.sections.append(&mut fragment.sections);
        combined.scopes.append(&mut fragment.scopes);
        combined.items.append(&mut fragment.items);
        combined.schemas.append(&mut fragment.schemas);
        Ok(())
    }

    fn admit_presentation(
        &mut self,
        presentation: Option<message::ResultPresentation>,
        items: &[Item],
    ) -> Result<Option<ResultPresentation>, HostError> {
        let Some(presentation) = presentation else {
            return Ok(None);
        };
        let presentation = crikey_native_protocol::typed::from_presentation(presentation, &self.spec.plugin)
            .map_err(|_| self.protocol_failure("invalid result metadata"))?;
        let mut targets = std::collections::BTreeSet::new();
        let mut executables = std::collections::BTreeSet::new();
        for item in items {
            let target = OwnedItem {
                owner: self.spec.plugin.clone(),
                item: item.stable_id.clone(),
            };
            if !targets.insert(target.clone()) {
                return Err(self.protocol_failure("duplicate result item identity"));
            }
            if item.actions.is_empty() {
                executables.insert(ExecutableKey { target, action: None });
            } else {
                for action in &item.actions {
                    executables.insert(ExecutableKey {
                        target: target.clone(),
                        action: Some(action.action_id.clone()),
                    });
                }
            }
        }
        presentation
            .validate(&self.spec.plugin, &targets, &executables)
            .map_err(|_| self.protocol_failure("invalid result metadata references"))?;
        Ok(Some(presentation))
    }

    fn is_sensitive(&self) -> bool {
        self.link.sensitive.load(Ordering::Acquire)
    }

    fn mark_sensitive(&mut self) -> Result<(), HostError> {
        {
            let mut privacy = lock_unpoisoned(&self.stderr_tail);
            privacy.sensitive = true;
            self.link.sensitive.store(true, Ordering::Release);
            privacy.bytes.clear();
            lock_unpoisoned(&self.link.observations).clear();
            *lock_unpoisoned(&self.link.mismatch) = None;
        }
        // No earlier request remains active at submission. Retire queued old
        // diagnostics before delivery, preserving fixed transport failures and
        // returning stream credits instead of stranding the next callback.
        for _ in 0..READER_QUEUE_CAPACITY {
            let event = match self.link.recv(Duration::ZERO) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return Err(HostError::Closed),
            };
            match event {
                ReaderEvent::Failure(failure) => return Err(self.transport_error(failure)),
                ReaderEvent::Envelope(envelope) => match envelope.payload.as_ref() {
                    Some(Payload::Results(_)) | Some(Payload::CatalogBatch(_)) => self.replenish_credit()?,
                    Some(Payload::ResourceRequest(request)) => {
                        self.answer_resource_request(&envelope, request)?
                    }
                    _ => {}
                },
            }
        }
        Ok(())
    }

    /// Confirms the exact schema on this physical worker before moving values
    /// into its writer queue. Dynamic authority is never regenerated.
    pub fn execute_typed(
        &mut self,
        item: &ItemId,
        action: Option<&ActionId>,
        expected_fingerprint: [u8; 32],
        values: ArgumentValues,
    ) -> Result<TypedExecutionOutcome, HostError> {
        match self.prepare_typed_execution(item, action, expected_fingerprint)? {
            TypedPreparation::Ready(confirmed) => self.execute_typed_confirmed(confirmed, values),
            TypedPreparation::Refused(code) => Ok(SubmissionOutcome::Failed(code).into()),
        }
    }

    /// Performs discovery without accepting or queueing live values. Missing
    /// bindings may bootstrap the static catalog, never dynamic suggestions.
    pub fn prepare_typed_execution(
        &mut self,
        item: &ItemId,
        action: Option<&ActionId>,
        expected_fingerprint: [u8; 32],
    ) -> Result<TypedPreparation, HostError> {
        self.ensure_alive()?;
        if !self.handshake.typed_arguments {
            return Ok(TypedPreparation::Refused(SubmissionFailureCode::Unsupported));
        }
        let executable = ExecutableKey {
            target: OwnedItem {
                owner: self.spec.plugin.clone(),
                item: item.clone(),
            },
            action: action.cloned(),
        };
        if !self.catalog_initialized
            && self
                .suggestion_presentation
                .as_ref()
                .and_then(|p| p.schema_for(&executable))
                .is_none()
        {
            self.build_catalog()?;
        }
        let schema = self
            .suggestion_presentation
            .as_ref()
            .and_then(|p| p.schema_for(&executable))
            .or_else(|| {
                self.catalog_presentation
                    .as_ref()
                    .and_then(|p| p.schema_for(&executable))
            })
            .cloned();
        let Some(schema) = schema else {
            return Ok(TypedPreparation::Refused(SubmissionFailureCode::SchemaMismatch));
        };
        if schema.fingerprint().ok() != Some(expected_fingerprint) {
            return Ok(TypedPreparation::Refused(SubmissionFailureCode::SchemaMismatch));
        }
        Ok(TypedPreparation::Ready(ConfirmedTypedExecution {
            incarnation: self.incarnation,
            executable,
            fingerprint: expected_fingerprint,
            view: None,
        }))
    }

    /// Rechecks this incarnation's registry and sends without discovery.
    pub fn execute_typed_confirmed(
        &mut self,
        confirmed: ConfirmedTypedExecution,
        values: ArgumentValues,
    ) -> Result<TypedExecutionOutcome, HostError> {
        self.execute_typed_confirmed_with_admission(confirmed, values, |send| send())
    }

    pub fn execute_typed_confirmed_with_admission<F>(
        &mut self,
        confirmed: ConfirmedTypedExecution,
        values: ArgumentValues,
        admission: F,
    ) -> Result<TypedExecutionOutcome, HostError>
    where
        F: FnOnce(&mut dyn FnMut() -> bool) -> bool,
    {
        self.ensure_alive()?;
        if confirmed.incarnation != self.incarnation {
            return Ok(SubmissionOutcome::Failed(SubmissionFailureCode::SchemaMismatch).into());
        }
        let schema = if let Some(view) = &confirmed.view {
            self.view_schema(view, &confirmed.executable)
        } else {
            self.suggestion_presentation
                .as_ref()
                .and_then(|p| p.schema_for(&confirmed.executable))
                .or_else(|| {
                    self.catalog_presentation
                        .as_ref()
                        .and_then(|p| p.schema_for(&confirmed.executable))
                })
                .cloned()
        };
        let Some(schema) = schema else {
            return Ok(SubmissionOutcome::Failed(SubmissionFailureCode::SchemaMismatch).into());
        };
        if schema.fingerprint().ok() != Some(confirmed.fingerprint) {
            return Ok(SubmissionOutcome::Failed(SubmissionFailureCode::SchemaMismatch).into());
        }
        if schema.validate_values(&values).is_err() {
            return Ok(SubmissionOutcome::Failed(SubmissionFailureCode::Denied).into());
        }
        self.mark_sensitive()?;
        let mut request = crikey_native_protocol::typed::to_request(
            &confirmed.executable.target.item,
            confirmed.executable.action.as_ref(),
            confirmed.fingerprint,
            values,
        )
        .map_err(|_| HostError::Protocol("invalid typed execute request".into()))?;
        request.view = confirmed
            .view
            .as_ref()
            .map(crikey_native_protocol::structured_view::to_context);
        let request_id = self.begin_request(0);
        let admitted = self.send_admitted(
            Envelope {
                connection_id: self.link.connection_id,
                request_id,
                generation: 0,
                deadline_ms: self.options.call_timeout_ms,
                payload: Some(Payload::TypedExecuteRequest(request)),
                unknown: Default::default(),
            },
            admission,
        )?;
        if !admitted {
            return Ok(SubmissionOutcome::Failed(SubmissionFailureCode::Cancelled).into());
        }
        let end = deadline(self.options.call_timeout_ms);
        loop {
            let envelope = match self.next_event(end, request_id, 0)? {
                ReaderEvent::Envelope(value) => value,
                ReaderEvent::Failure(failure) => return Err(self.transport_error(failure)),
            };
            match envelope.payload {
                Some(Payload::TypedExecuteResponse(response)) => {
                    let outcome = crikey_native_protocol::typed::from_response(response, &schema)
                        .map_err(|_| self.protocol_failure("invalid typed execute response"))?;
                    if matches!(outcome, TypedExecutionOutcome::Navigate(_))
                        && !self.handshake.structured_view
                    {
                        return Ok(SubmissionOutcome::Failed(SubmissionFailureCode::Unsupported).into());
                    }
                    self.mark_call_succeeded();
                    return Ok(outcome);
                }
                Some(Payload::Log(_)) => {}
                Some(Payload::Error(error)) => return Err(self.error_payload(error, request_id)),
                _ => return Err(self.protocol_failure("unexpected typed execute response")),
            }
        }
    }

    /// Executes an action request; plugin failures stay on the `Ok` path.
    pub fn execute(
        &mut self,
        item: &ItemId,
        action: Option<&ActionId>,
        argument: Option<&str>,
    ) -> Result<ExecuteOutcome, HostError> {
        self.execute_inner(None, item, action, argument, |send| send())
    }

    pub fn execute_with_admission<F>(
        &mut self,
        item: &ItemId,
        action: Option<&ActionId>,
        argument: Option<&str>,
        admission: F,
    ) -> Result<ExecuteOutcome, HostError>
    where
        F: FnOnce(&mut dyn FnMut() -> bool) -> bool,
    {
        self.execute_inner(None, item, action, argument, admission)
    }

    fn execute_inner<F>(
        &mut self,
        view: Option<&ViewExecutionContext>,
        item: &ItemId,
        action: Option<&ActionId>,
        argument: Option<&str>,
        admission: F,
    ) -> Result<ExecuteOutcome, HostError>
    where
        F: FnOnce(&mut dyn FnMut() -> bool) -> bool,
    {
        self.ensure_alive()?;
        let request_id = self.begin_request(0);
        let envelope = Envelope {
            connection_id: self.link.connection_id,
            request_id,
            generation: 0,
            deadline_ms: self.options.call_timeout_ms,
            payload: Some(Payload::Execute(message::ExecuteRequest {
                item_id: item.0.clone(),
                action_id: action.map(|value| value.0.clone()).unwrap_or_default(),
                argument: argument.unwrap_or_default().to_owned(),
                view: view.map(crikey_native_protocol::structured_view::to_context),
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        };
        if !self.send_admitted(envelope, admission)? {
            return Ok(ExecuteOutcome::Unsupported);
        }
        let event = self.next_event(deadline(self.options.call_timeout_ms), request_id, 0)?;
        let envelope = match event {
            ReaderEvent::Envelope(value) => value,
            ReaderEvent::Failure(failure) => return Err(self.transport_error(failure)),
        };
        match envelope.payload {
            Some(Payload::ExecuteResult(result)) => {
                if result.outcome.as_i32() == 2 {
                    if let Some(error) = result.error.as_ref() {
                        self.validate_nested_error(error, request_id, "execute")?;
                    }
                }
                match execute_outcome(&result) {
                    Ok(outcome) => {
                        if matches!(outcome, ExecuteOutcome::Navigate(_)) && !self.handshake.structured_view {
                            return Ok(ExecuteOutcome::Unsupported);
                        }
                        self.mark_call_succeeded();
                        Ok(outcome)
                    }
                    Err(detail) => Err(self.protocol_failure(detail)),
                }
            }
            Some(Payload::Error(error)) => Err(self.error_payload(error, request_id)),
            _ => Err(self.protocol_failure("unexpected execute response".to_owned())),
        }
    }

    /// Produces only the selected item's preview using the ordinary worker
    /// request/deadline/cancellation channel. Call from a provider endpoint.
    pub fn preview(
        &mut self,
        item_id: &str,
        generation: u64,
    ) -> Result<crikey_core::PreviewOutcome, HostError> {
        self.preview_with_cancel_latched(item_id, generation, &AtomicBool::new(false))
    }

    /// Preserves cancellation latched before this operation's wire identity
    /// exists, replaying it after the request is queued and clearing it on a
    /// terminal response. An already-set `cancelled` token returns without a
    /// request. The caller sets that token before forwarding cancellation
    /// through its installed CancelHandle.
    pub fn preview_with_cancel_latched(
        &mut self,
        item_id: &str,
        generation: u64,
        cancelled: &AtomicBool,
    ) -> Result<crikey_core::PreviewOutcome, HostError> {
        // Consume cancellation installed before this call without letting it
        // survive an early return into an unrelated callback.
        let cancel_latched = self.link.cancel_latched.swap(false, Ordering::AcqRel);
        if cancelled.load(Ordering::Acquire) {
            return Ok(crikey_core::PreviewOutcome::Cancelled);
        }
        self.ensure_alive()?;
        if !self.handshake.preview {
            return Ok(crikey_core::PreviewOutcome::Unsupported);
        }
        if item_id.is_empty() || item_id.len() > 256 {
            return Err(HostError::Protocol("preview item identity exceeds bounds".into()));
        }
        if self.next_request_id == u64::MAX {
            return Err(HostError::Protocol("preview request identity exhausted".into()));
        }
        let request_id = self.begin_request(generation);
        self.send_control(Envelope {
            connection_id: self.link.connection_id,
            request_id,
            generation,
            deadline_ms: self.options.call_timeout_ms,
            payload: Some(Payload::PreviewRequest(message::PreviewRequest {
                item_id: item_id.to_owned(),
                ..Default::default()
            })),
            unknown: Default::default(),
        })?;
        // A handle may have cancelled the previous wire identity before
        // begin_request, or the new identity before its request was queued.
        // Replay only after the request so either ordering reaches the callback.
        if cancel_latched
            || self.link.cancel_latched.load(Ordering::Acquire)
            || cancelled.load(Ordering::Acquire)
        {
            self.link.send_cancel();
        }
        let end = deadline(self.options.call_timeout_ms);
        loop {
            let event = self.next_event(end, request_id, generation)?;
            let reply = match event {
                ReaderEvent::Envelope(value) => value,
                ReaderEvent::Failure(failure) => return Err(self.transport_error(failure)),
            };
            if reply.generation != generation {
                return Err(self.protocol_failure("preview response generation did not match request"));
            }
            match reply.payload {
                Some(Payload::PreviewResponse(response)) => {
                    self.link.cancel_latched.store(false, Ordering::Release);
                    let outcome =
                        crikey_native_protocol::preview::from_response(response).map_err(|detail| {
                            HostError::PluginFailed {
                                plugin: self.spec.plugin.clone(),
                                detail,
                            }
                        })?;
                    self.mark_call_succeeded();
                    return Ok(outcome);
                }
                Some(Payload::Log(log)) => self.record_log(log.message),
                Some(Payload::Error(error)) => {
                    self.link.cancel_latched.store(false, Ordering::Release);
                    return Err(self.error_payload(error, request_id));
                }
                _ => return Err(self.protocol_failure("unexpected preview response")),
            }
        }
    }

    /// Requests explicit declarations on this catalog-originating worker.
    /// The caller must validate each returned target against its current
    /// owner-qualified catalog before binding owner/availability or publishing.
    pub fn commands(&mut self, generation: u64) -> Result<crikey_core::command::PluginCommands, HostError> {
        self.commands_with_cancel_latched(generation, &AtomicBool::new(false))
    }

    pub fn commands_with_cancel_latched(
        &mut self,
        generation: u64,
        cancelled: &AtomicBool,
    ) -> Result<crikey_core::command::PluginCommands, HostError> {
        use crikey_core::command::PluginCommands;
        let cancel_latched = self.link.cancel_latched.swap(false, Ordering::AcqRel);
        if cancelled.load(Ordering::Acquire) {
            return Ok(PluginCommands::Cancelled);
        }
        self.ensure_alive()?;
        if !self.handshake.commands {
            return Ok(PluginCommands::Unsupported);
        }
        if !self.catalog_initialized {
            return Err(HostError::Protocol(
                "commands require this worker's complete catalog".into(),
            ));
        }
        if self.next_request_id == u64::MAX {
            return Err(HostError::Protocol("commands request identity exhausted".into()));
        }
        let request_id = self.begin_request(generation);
        self.send_control(Envelope {
            connection_id: self.link.connection_id,
            request_id,
            generation,
            deadline_ms: self.options.call_timeout_ms,
            payload: Some(Payload::CommandsRequest(message::CommandsRequest::default())),
            unknown: Default::default(),
        })?;
        if cancel_latched
            || self.link.cancel_latched.load(Ordering::Acquire)
            || cancelled.load(Ordering::Acquire)
        {
            self.link.send_cancel();
        }
        let end = deadline(self.options.call_timeout_ms);
        loop {
            let reply = match self.next_event(end, request_id, generation)? {
                ReaderEvent::Envelope(value) => value,
                ReaderEvent::Failure(failure) => return Err(self.transport_error(failure)),
            };
            if reply.generation != generation {
                return Err(self.protocol_failure("commands response generation did not match request"));
            }
            match reply.payload {
                Some(Payload::CommandsResponse(response)) => {
                    self.link.cancel_latched.store(false, Ordering::Release);
                    let outcome =
                        crikey_native_protocol::commands::from_response(response).map_err(|detail| {
                            HostError::PluginFailed {
                                plugin: self.spec.plugin.clone(),
                                detail,
                            }
                        })?;
                    self.mark_call_succeeded();
                    return Ok(outcome);
                }
                Some(Payload::Log(log)) => self.record_log(log.message),
                Some(Payload::Error(error)) => {
                    self.link.cancel_latched.store(false, Ordering::Release);
                    return Err(self.error_payload(error, request_id));
                }
                _ => return Err(self.protocol_failure("unexpected commands response")),
            }
        }
    }

    /// Asks the plugin for the next frame of an open page (spec 32.3).
    ///
    /// Host-driven, exactly like [`Self::suggest`]: the plugin answers the
    /// request it was given or it answers nothing. That is why the reply is
    /// latched as the only legal `PageFrame` for the duration of this call —
    /// a plugin that could push a frame whenever it liked could repaint the
    /// user's screen from behind whatever surface they were actually looking
    /// at.
    ///
    /// The frame is validated before it is returned, so a caller never sees a
    /// partially drawable page. A frame that fails validation is the plugin
    /// sending something no host may draw, which is a protocol violation and
    /// is reaped like any other.
    pub fn page(&mut self, request: &NativePageRequest) -> Result<PageFrame, HostError> {
        self.ensure_alive()?;
        let request_id = self.begin_request(request.generation);
        let envelope = Envelope {
            connection_id: self.link.connection_id,
            request_id,
            generation: request.generation,
            deadline_ms: self.options.call_timeout_ms,
            payload: Some(Payload::PageRequest(message::PageRequest {
                page_id: request.page_id.clone(),
                generation: request.generation,
                width: request.width,
                height: request.height,
                events: request.events.iter().map(to_proto_page_input).collect(),
                focused: request.focused,
                colour_surface: request.colour_surface,
                colour_text: request.colour_text,
                colour_accent: request.colour_accent,
                colour_muted: request.colour_muted,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        };
        // Latched before the request is written: the answer can be back
        // before `send_control` has returned on a fast transport.
        self.link.page_request.store(request_id, Ordering::Release);
        let outcome = self.page_reply(envelope, request_id, request.generation);
        self.link.page_request.store(0, Ordering::Release);
        outcome
    }

    fn page_reply(
        &mut self,
        envelope: Envelope,
        request_id: u64,
        generation: u64,
    ) -> Result<PageFrame, HostError> {
        self.send_control(envelope)?;
        let event = self.next_event(deadline(self.options.call_timeout_ms), request_id, generation)?;
        let reply = match event {
            ReaderEvent::Envelope(value) => value,
            ReaderEvent::Failure(failure) => return Err(self.transport_error(failure)),
        };
        match reply.payload {
            Some(Payload::PageFrame(frame)) => match decode_page_frame(&frame) {
                Ok(frame) => {
                    self.mark_call_succeeded();
                    Ok(frame)
                }
                Err(detail) => Err(self.protocol_failure(detail)),
            },
            Some(Payload::Error(error)) => Err(self.error_payload(error, request_id)),
            _ => Err(self.protocol_failure("unexpected page response".to_owned())),
        }
    }

    /// Performs a health request against the live plugin.
    pub fn health(&mut self) -> Result<HealthSnapshot, HostError> {
        self.ensure_alive()?;
        let request_id = self.begin_request(0);
        self.send_control(Envelope {
            connection_id: self.link.connection_id,
            request_id,
            generation: 0,
            deadline_ms: self.options.call_timeout_ms,
            payload: Some(Payload::HealthCheck(message::HealthCheck {
                nonce: request_id,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        })?;
        let event = self.next_event(deadline(self.options.call_timeout_ms), request_id, 0)?;
        let envelope = match event {
            ReaderEvent::Envelope(value) => value,
            ReaderEvent::Failure(failure) => return Err(self.transport_error(failure)),
        };
        match envelope.payload {
            Some(Payload::HealthReport(report)) if report.nonce == request_id => {
                self.mark_call_succeeded();
                Ok(HealthSnapshot {
                    healthy: report.healthy,
                    memory_bytes: report.memory_bytes,
                    queue_depth: report.queue_depth,
                    in_flight: report.in_flight,
                    detail: report.detail,
                })
            }
            Some(Payload::HealthReport(_)) => {
                Err(self.protocol_failure("health response nonce did not match request".to_owned()))
            }
            Some(Payload::Error(error)) => Err(self.error_payload(error, request_id)),
            _ => Err(self.protocol_failure("unexpected health response".to_owned())),
        }
    }

    /// Asks the live plugin to serve one resource (spec 16.4).
    ///
    /// Neither a decline nor a silence kills the worker, and that asymmetry
    /// with the query path is deliberate. A resource is decoration — an item's
    /// icon, in practice — so taking the process down over one would cost the
    /// user working suggestions to punish a missing picture. Transport death
    /// is still an error: that is not the plugin declining, it is the channel
    /// being gone.
    ///
    /// The two non-error outcomes are reported apart because they mean
    /// opposite things to a caller that memoizes. A decline is the plugin's
    /// answer and will not change; a silence is the absence of an answer,
    /// which the very next attempt may supply. Collapsing them — as this
    /// returned `Option` once did — is what let one slow resource child cost a
    /// plugin its icons for the rest of a session.
    pub fn request_resource(
        &mut self,
        kind: ResourceKind,
        reference: &str,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<ResourceOutcome, HostError> {
        self.ensure_alive()?;
        let request_id = self.begin_request(0);
        self.send_control(Envelope {
            connection_id: self.link.connection_id,
            request_id,
            generation: 0,
            deadline_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            payload: Some(Payload::ResourceRequest(message::ResourceRequest {
                kind: kind.to_proto(),
                reference: reference.to_owned(),
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        })?;

        let end = deadline(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX));
        loop {
            let remaining = end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(ResourceOutcome::Silent);
            }
            let envelope = match self.link.recv(remaining) {
                Ok(ReaderEvent::Envelope(envelope)) => envelope,
                Ok(ReaderEvent::Failure(failure)) => return Err(self.transport_error(failure)),
                Err(RecvTimeoutError::Timeout) => return Ok(ResourceOutcome::Silent),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(self.transport_error(ReaderFailure {
                        error: ProtocolError::Closed,
                        protocol_violation: self.link.protocol_violation(),
                        resource_limit: self.link.resource_limit.load(Ordering::Acquire),
                    }))
                }
            };
            if let Some(Payload::ResourceRequest(request)) = envelope.payload.as_ref() {
                self.answer_resource_request(&envelope, request)?;
                continue;
            }
            if envelope.request_id != request_id {
                // Whatever else is still in the reader queue belongs to a call
                // that already returned. Streaming payloads still owe a credit
                // back or the plugin stalls on the next query.
                if matches!(
                    envelope.payload,
                    Some(Payload::Results(_)) | Some(Payload::CatalogBatch(_))
                ) {
                    self.replenish_credit()?;
                }
                continue;
            }
            match envelope.payload {
                Some(Payload::ResourceResponse(response)) => {
                    self.mark_call_succeeded();
                    if !response.found
                        || response.error.is_some()
                        || response.content.len() > max_bytes
                        || response.content.is_empty()
                    {
                        return Ok(ResourceOutcome::Declined);
                    }
                    return Ok(ResourceOutcome::Served(PluginResource {
                        reference: response.reference,
                        content: response.content,
                        media_type: response.media_type,
                    }));
                }
                Some(Payload::Log(log)) => self.record_log(log.message),
                // An `Error` here is the SDK's answer for a plugin that does
                // not implement resources at all, which is a decline, not a
                // violation.
                Some(Payload::Error(_)) | Some(_) | None => return Ok(ResourceOutcome::Declined),
            }
        }
    }

    /// Returns a clonable out-of-band cancellation handle.
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle {
            link: Arc::clone(&self.link),
        }
    }

    /// Observes callable connection liveness without reaping or waiting for cleanup.
    pub(crate) fn poll_alive(&mut self) -> bool {
        // Logical retirement is independent of physical ownership. A failed
        // containment/reap attempt must not revive a terminal connection.
        if self.retirement_kind.is_some()
            || self.exit.is_some()
            || self.process_status.is_some()
            || self.observed_exit.is_some()
            || self.link.closed.load(Ordering::Acquire)
            || self.link.protocol_violation()
        {
            return false;
        }
        match self.child.as_ref().map(child_exited) {
            Some(Ok(false)) => true,
            Some(Ok(true)) => {
                self.observed_exit = Some(Ok(()));
                false
            }
            Some(Err(_)) => {
                self.observed_exit = Some(Err(()));
                false
            }
            None => false,
        }
    }

    /// Reports callable connection liveness, settling an already-observed process exit.
    pub fn is_alive(&mut self) -> bool {
        if self.poll_alive() {
            return true;
        }
        if self.exit.is_none() && self.observed_exit == Some(Ok(())) {
            let _ = self.finish_teardown(false, None);
        }
        false
    }

    /// Returns stream diagnostics.
    pub fn diagnostics(&self) -> StreamDiagnostics {
        let mut diagnostics = self.diagnostics;
        diagnostics.peak_queue_depth = diagnostics
            .peak_queue_depth
            .max(self.link.peak_queue_depth.load(Ordering::Acquire) as u32);
        diagnostics
    }

    /// Hard-stops and reaps the child. Missing status is not a teardown proof.
    pub fn kill(&mut self) -> ExitRecord {
        self.finish_teardown(false, Some(ExitKind::Killed))
            .unwrap_or_else(|_| {
                self.exit_record(self.sticky_exit_kind(ExitKind::Killed), self.process_status)
            })
    }

    /// Requests orderly shutdown, hard-killing only after its bound.
    pub fn shutdown(mut self) -> ExitRecord {
        self.shutdown_inner()
    }

    /// Retains the child and unfinished pipe threads on failure.
    pub(crate) fn shutdown_checked(&mut self) -> Result<ExitRecord, HostError> {
        if let Some(exit) = &self.exit {
            return match self.closing_failure {
                Some(error) => Err(self.teardown_error(error)),
                None => Ok(exit.clone()),
            };
        }
        let exit = self.finish_teardown(true, None)?;
        if let Some(error) = self.closing_failure {
            return Err(self.teardown_error(error));
        }
        // Open-runtime guest failures still feed normal restart policy. During
        // global closing, newly settled control loss cannot authorize mutation.
        if self.link.admission.is_closed()
            && matches!(exit.kind, ExitKind::Crashed | ExitKind::ProtocolViolation)
        {
            return Err(self.teardown_error("native worker lost control during shutdown"));
        }
        Ok(exit)
    }
    pub(crate) fn finish_forced_checked(&mut self) -> Result<ExitRecord, HostError> {
        self.finish_teardown(false, Some(ExitKind::Killed))
    }

    fn teardown_error(&self, detail: &'static str) -> HostError {
        HostError::PluginFailed {
            plugin: self.spec.plugin.clone(),
            detail: detail.into(),
        }
    }

    fn finish_teardown(&mut self, graceful: bool, kind: Option<ExitKind>) -> Result<ExitRecord, HostError> {
        if let Some(exit) = &self.exit {
            return Ok(exit.clone());
        }
        if let Some(error) = self.thread_failure {
            return Err(self.teardown_error(error));
        }
        // Retrying physical cleanup must not overwrite the guest failure that
        // initiated it, even when the first attempt cannot yet reap or join.
        self.retirement_kind = self.retirement_kind.or(kind);
        let mut forced = false;
        if self.process_status.is_none() {
            let child = self
                .child
                .as_ref()
                .ok_or_else(|| self.teardown_error("native child ownership unavailable"))?;
            let mut exited =
                child_exited(child).map_err(|_| self.teardown_error("native exit observation failed"))?;
            if graceful && exited && !self.containment_reap_ready && self.link.admission.is_closed() {
                self.closing_failure = Some("native worker exited before shutdown");
            }
            if graceful && !exited && !self.containment_reap_ready {
                let request_id = self.begin_request(0);
                let end = deadline(self.options.shutdown_timeout_ms);
                // Cleanup control is permitted after callback admission closes.
                let _ = self.link.send_until(
                    Envelope {
                        connection_id: self.link.connection_id,
                        request_id,
                        generation: 0,
                        deadline_ms: self.options.shutdown_timeout_ms,
                        payload: Some(Payload::Shutdown(message::Shutdown {
                            immediate: false,
                            unknown: Default::default(),
                        })),
                        unknown: Default::default(),
                    },
                    end,
                );
                loop {
                    exited = child_exited(self.child.as_ref().expect("owned child"))
                        .map_err(|_| self.teardown_error("native exit observation failed"))?;
                    if exited || Instant::now() >= end {
                        break;
                    }
                    if self
                        .link
                        .recv(
                            end.saturating_duration_since(Instant::now())
                                .min(Duration::from_millis(5)),
                        )
                        .is_err()
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
                forced = !exited;
            }
            if !self.containment_reap_ready {
                // The leader remains unreaped here, even after EOF or a clean
                // graceful exit, so its process-group identity cannot be reused.
                #[cfg(unix)]
                let signalled = prepare_group_reap(self.child.as_ref().expect("owned child"));
                #[cfg(windows)]
                let signalled = self.job.as_ref().is_some_and(|job| terminate_job_checked(job.0));
                #[cfg(not(any(unix, windows)))]
                let signalled = false;
                if !signalled {
                    return Err(self.teardown_error("native containment termination failed"));
                }
                self.containment_reap_ready = true;
            }
            self.link.close();
            let end = Instant::now() + Duration::from_millis(CHILD_REAP_TIMEOUT_MS);
            let child = self.child.as_mut().expect("owned child");
            self.process_status = wait_child_after_termination(child, end);
            if self.process_status.is_none() {
                return Err(self.teardown_error("native process reap was not confirmed"));
            }
        }
        self.link.close();
        let status = self.process_status.expect("confirmed native process status");
        let retirement_kind = self.retirement_kind.unwrap_or_else(|| {
            if forced {
                ExitKind::Killed
            } else {
                ExitKind::from_status(&status)
            }
        });
        self.retirement_kind = Some(retirement_kind);
        #[cfg(unix)]
        if !wait_group_absent(self.child.as_ref().expect("owned child").id()) {
            return Err(self.teardown_error("native process group remains owned"));
        }
        let end = Instant::now() + Duration::from_millis(CHILD_REAP_TIMEOUT_MS);
        let reader = settle_link_thread(&self.link.reader, end);
        let writer = settle_link_thread(&self.link.writer_thread, end);
        let stderr = settle_owned_thread(&mut self.stderr_reader, end);
        let outcomes = [reader, writer, stderr];
        if outcomes
            .iter()
            .any(|outcome| *outcome == Err("native pipe worker panicked"))
        {
            self.thread_failure = Some("native pipe worker panicked");
        }
        if let Some(error) = self.thread_failure {
            return Err(self.teardown_error(error));
        }
        for outcome in outcomes {
            if let Err(error) = outcome {
                return Err(self.teardown_error(error));
            }
        }
        self.release_job();
        let exit = self.exit_record(self.sticky_exit_kind(retirement_kind), Some(status));
        self.exit = Some(exit.clone());
        Ok(exit)
    }

    fn shutdown_inner(&mut self) -> ExitRecord {
        self.shutdown_checked().unwrap_or_else(|_| {
            self.exit
                .clone()
                .unwrap_or_else(|| self.exit_record(ExitKind::Crashed, self.process_status))
        })
    }

    #[cfg(windows)]
    fn release_job(&mut self) {
        // Dropping the owner is the close; taking it here puts that close at
        // the point shutdown expects rather than at worker drop.
        drop(self.job.take());
    }

    #[cfg(not(windows))]
    fn release_job(&mut self) {}

    fn send_control(&mut self, envelope: Envelope) -> Result<(), HostError> {
        let result = self.link.send(envelope);
        if let Err(error) = &result {
            match error {
                HostError::ResourceLimit { .. } => {
                    self.failure_kind = Some(crikey_plugin_supervisor::FailureKind::ResourceLimit);
                }
                HostError::Protocol(_) => {
                    self.failure_kind = Some(crikey_plugin_supervisor::FailureKind::ProtocolViolation);
                    self.fail_and_reap(ExitKind::ProtocolViolation);
                }
                HostError::Crashed { .. } => {
                    self.failure_kind = Some(crikey_plugin_supervisor::FailureKind::Crash);
                    self.fail_and_reap(ExitKind::Crashed);
                }
                _ => {}
            }
        }
        result
    }

    fn protocol_failure(&mut self, detail: impl Into<String>) -> HostError {
        self.failure_kind = Some(crikey_plugin_supervisor::FailureKind::ProtocolViolation);
        self.fail_and_reap(ExitKind::ProtocolViolation);
        HostError::Protocol(detail.into())
    }

    fn validate_batch_sequence(
        &mut self,
        sequence: u64,
        expected: &mut u64,
        stream: &str,
    ) -> Result<(), HostError> {
        if sequence != *expected {
            return Err(self.protocol_failure(format!(
                "{stream} batch sequence {sequence} does not follow expected sequence {expected}"
            )));
        }
        *expected = expected.saturating_add(1);
        Ok(())
    }

    fn validate_nested_error(
        &mut self,
        error: &message::StructuredError,
        request_id: u64,
        context: &str,
    ) -> Result<(), HostError> {
        if error.request_id != 0 && error.request_id != request_id {
            return Err(self.protocol_failure(format!(
                "{context} error request_id {} does not match envelope request_id {request_id}",
                error.request_id
            )));
        }
        Ok(())
    }

    fn ensure_alive(&mut self) -> Result<(), HostError> {
        if self.is_alive() {
            Ok(())
        } else if self.link.protocol_violation() || self.retirement_kind == Some(ExitKind::ProtocolViolation)
        {
            // An asynchronous reader rejection can precede callback entry.
            // Preserve its cause without admitting another request or joining.
            Err(HostError::Protocol(
                "native protocol connection is retired".into(),
            ))
        } else {
            Err(HostError::Closed)
        }
    }

    fn mark_call_succeeded(&mut self) {
        self.call_succeeded = true;
    }

    fn begin_request(&mut self, generation: u64) -> u64 {
        self.call_succeeded = false;
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        self.link.set_current(request_id, generation);
        request_id
    }

    fn next_event(
        &mut self,
        end: Instant,
        request_id: u64,
        generation: u64,
    ) -> Result<ReaderEvent, HostError> {
        loop {
            let remaining = end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(self.timeout_and_kill());
            }
            match self.link.recv(remaining) {
                Ok(ReaderEvent::Envelope(envelope)) => {
                    // A resource response for another request id answers a
                    // fetch the host already abandoned at its own deadline.
                    // Recording it as an echo mismatch would blame the plugin
                    // for the host's impatience, so it is simply dropped.
                    if matches!(envelope.payload, Some(Payload::ResourceResponse(_)))
                        && envelope.request_id != request_id
                    {
                        continue;
                    }
                    if is_stale(&envelope, request_id, generation) {
                        let request_mismatch = envelope.request_id != request_id;
                        let generation_mismatch = generation != 0 && envelope.generation != generation;
                        record_echo_mismatch(
                            &self.link.mismatch,
                            request_mismatch,
                            generation_mismatch,
                            format!(
                                "response echo request={} generation={} expected request={} generation={}",
                                envelope.request_id, envelope.generation, request_id, generation
                            ),
                        );
                        self.diagnostics.rejected_stale = self.diagnostics.rejected_stale.saturating_add(1);
                        self.diagnostics.dropped_obsolete =
                            self.diagnostics.dropped_obsolete.saturating_add(1);
                        if matches!(
                            envelope.payload,
                            Some(Payload::Results(_)) | Some(Payload::CatalogBatch(_))
                        ) {
                            self.replenish_credit()?;
                        }
                        continue;
                    }
                    if let Some(Payload::ResourceRequest(request)) = envelope.payload.as_ref() {
                        self.answer_resource_request(&envelope, request)?;
                        continue;
                    }
                    return Ok(ReaderEvent::Envelope(envelope));
                }
                Ok(event @ ReaderEvent::Failure(_)) => return Ok(event),
                Err(RecvTimeoutError::Timeout) => return Err(self.timeout_and_kill()),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(self.transport_error(ReaderFailure {
                        error: ProtocolError::Closed,
                        protocol_violation: self.link.protocol_violation(),
                        resource_limit: self.link.resource_limit.load(Ordering::Acquire),
                    }))
                }
            }
        }
    }
    fn answer_resource_request(
        &mut self,
        envelope: &Envelope,
        request: &message::ResourceRequest,
    ) -> Result<(), HostError> {
        self.send_control(Envelope {
            connection_id: self.link.connection_id,
            request_id: envelope.request_id,
            generation: envelope.generation,
            deadline_ms: 0,
            payload: Some(Payload::ResourceResponse(message::ResourceResponse {
                reference: request.reference.clone(),
                found: false,
                content: Vec::new(),
                media_type: String::new(),
                error: None,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        })
    }

    fn replenish_credit(&mut self) -> Result<(), HostError> {
        match self.link.send_flow() {
            Ok(()) => {
                self.diagnostics.credits_granted = self.diagnostics.credits_granted.saturating_add(1);
                Ok(())
            }
            Err(error) => {
                let detail = format!("failed to replenish stream credit: {error:?}");
                if self.link.protocol_violation() {
                    self.failure_kind = Some(crikey_plugin_supervisor::FailureKind::ProtocolViolation);
                    self.fail_and_reap(ExitKind::ProtocolViolation);
                    Err(HostError::Protocol(detail))
                } else {
                    self.failure_kind = Some(crikey_plugin_supervisor::FailureKind::Crash);
                    self.fail_and_reap(ExitKind::Crashed);
                    Err(self.crashed_error(&detail))
                }
            }
        }
    }

    fn truncate(&mut self, request_id: u64, generation: u64) -> Result<(), HostError> {
        self.diagnostics.truncated_calls = self.diagnostics.truncated_calls.saturating_add(1);
        self.link.set_current(request_id, generation);
        // Cancel before replenishing credit: the final credit lets a cooperative
        // plugin emit its terminal batch without authorizing more partial results.
        self.link.send_cancel();
        self.replenish_credit()?;
        Ok(())
    }
    fn await_terminal(
        &mut self,
        request_id: u64,
        generation: u64,
        end: Instant,
        next_sequence: &mut u64,
    ) -> Result<usize, HostError> {
        let mut terminal_batches = 0usize;
        loop {
            let event = self.next_event(end, request_id, generation)?;
            let envelope = match event {
                ReaderEvent::Envelope(value) => value,
                ReaderEvent::Failure(failure) => return Err(self.transport_error(failure)),
            };
            match envelope.payload {
                Some(Payload::Results(batch)) => {
                    self.validate_batch_sequence(batch.sequence, next_sequence, "result")?;
                    if !matches!(batch.state.as_i32(), 1..=4) {
                        return Err(self.protocol_failure("unknown result batch state".to_owned()));
                    }
                    terminal_batches = terminal_batches.saturating_add(1);
                    let bytes = batch.encode().len();
                    self.record_batch(bytes, batch.items.len());
                    self.replenish_credit()?;
                    if matches!(batch.state.as_i32(), 2..=4) {
                        return Ok(terminal_batches);
                    }
                }
                Some(Payload::Log(log)) => self.record_log(log.message),
                Some(Payload::Error(error)) => return Err(self.error_payload(error, request_id)),
                Some(_) | None => {
                    return Err(self.protocol_failure(
                        "unexpected payload while waiting for terminal result".to_owned(),
                    ));
                }
            }
        }
    }

    fn record_batch(&mut self, bytes: usize, items: usize) {
        self.diagnostics.batches = self.diagnostics.batches.saturating_add(1);
        self.diagnostics.items = self.diagnostics.items.saturating_add(items as u64);
        self.diagnostics.bytes = self.diagnostics.bytes.saturating_add(bytes as u64);
    }

    fn record_log(&mut self, _message: String) {}

    fn transport_error(&mut self, failure: ReaderFailure) -> HostError {
        let revoked = self.link.revoked_request.load(Ordering::Acquire);
        if revoked != 0 && revoked == self.link.current_request.load(Ordering::Acquire) {
            return HostError::Closed;
        }
        let failure = if self.is_sensitive() {
            ReaderFailure {
                error: ProtocolError::Closed,
                ..failure
            }
        } else {
            failure
        };
        if failure.resource_limit || self.link.resource_limit.load(Ordering::Acquire) {
            self.failure_kind = Some(crikey_plugin_supervisor::FailureKind::ResourceLimit);
            self.fail_and_reap(ExitKind::Killed);
            HostError::ResourceLimit {
                plugin: self.spec.plugin.clone(),
                detail: format!("reader queue resource limit: {:?}", failure.error),
            }
        } else if failure.protocol_violation || self.link.protocol_violation() {
            self.failure_kind = Some(crikey_plugin_supervisor::FailureKind::ProtocolViolation);
            self.fail_and_reap(ExitKind::ProtocolViolation);
            HostError::Protocol(format!("transport violation: {:?}", failure.error))
        } else {
            self.failure_kind = Some(crikey_plugin_supervisor::FailureKind::Crash);
            self.fail_and_reap(ExitKind::Crashed);
            self.crashed_error(&format!("transport closed: {:?}", failure.error))
        }
    }

    fn timeout_and_kill(&mut self) -> HostError {
        if self.link.protocol_violation() {
            self.failure_kind = Some(crikey_plugin_supervisor::FailureKind::ProtocolViolation);
            self.fail_and_reap(ExitKind::ProtocolViolation);
            return HostError::Protocol("transport protocol violation detected".to_owned());
        }
        // Pipe EOF can lag process termination. A guest that has already
        // exited did not exhaust a live callback's execution budget.
        if self
            .child
            .as_ref()
            .is_some_and(|child| matches!(child_exited(child), Ok(true)))
        {
            self.failure_kind = Some(crikey_plugin_supervisor::FailureKind::Crash);
            self.fail_and_reap(ExitKind::Crashed);
            return self.crashed_error("transport closed: native worker exited before its terminal response");
        }
        self.failure_kind = Some(crikey_plugin_supervisor::FailureKind::Timeout);
        self.fail_and_reap(ExitKind::Killed);
        HostError::Timeout {
            plugin: self.spec.plugin.clone(),
            detail: "aggregate call deadline elapsed".to_owned(),
        }
    }

    fn crashed_error(&self, detail: &str) -> HostError {
        let tail = lock_unpoisoned(&self.stderr_tail).text();
        let detail = if tail.is_empty() {
            detail.to_owned()
        } else {
            format!("{detail}: {tail}")
        };
        HostError::Crashed {
            plugin: self.spec.plugin.clone(),
            detail,
        }
    }
    fn sticky_exit_kind(&self, fallback: ExitKind) -> ExitKind {
        if self.link.protocol_violation() {
            ExitKind::ProtocolViolation
        } else {
            fallback
        }
    }

    fn error_payload(&mut self, error: message::StructuredError, request_id: u64) -> HostError {
        let mut error = error;
        if self.is_sensitive() {
            error.message.clear();
            error.detail.clear();
        }
        let plugin_error = plugin_error(&error);
        if let Err(protocol) = self.validate_nested_error(&error, request_id, "structured") {
            return protocol;
        }
        match error.code.as_i32() {
            1 => self.protocol_failure(error_detail(&plugin_error)),
            2 | 4 | 6 => HostError::PluginFailed {
                plugin: self.spec.plugin.clone(),
                detail: error_detail(&plugin_error),
            },
            3 => {
                self.failure_kind = Some(crikey_plugin_supervisor::FailureKind::Timeout);
                HostError::Timeout {
                    plugin: self.spec.plugin.clone(),
                    detail: error_detail(&plugin_error),
                }
            }
            5 => {
                self.failure_kind = Some(crikey_plugin_supervisor::FailureKind::ResourceLimit);
                HostError::ResourceLimit {
                    plugin: self.spec.plugin.clone(),
                    detail: error_detail(&plugin_error),
                }
            }
            _ => self.protocol_failure(format!(
                "unknown structured error code {}: {}",
                error.code.as_i32(),
                error_detail(&plugin_error)
            )),
        }
    }

    fn fail_and_reap(&mut self, kind: ExitKind) {
        if self.link.admission.is_closed() && matches!(kind, ExitKind::Crashed | ExitKind::ProtocolViolation)
        {
            self.closing_failure = Some("native worker lost control during shutdown");
        }
        let _ = self.finish_teardown(false, Some(kind));
    }

    fn exit_record(&self, kind: ExitKind, status: Option<std::process::ExitStatus>) -> ExitRecord {
        let (code, signal) = status.map_or((None, None), |status| {
            let code = status.code();
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                (code, status.signal())
            }
            #[cfg(not(unix))]
            {
                (code, None)
            }
        });
        let stderr_tail = lock_unpoisoned(&self.stderr_tail).text();
        ExitRecord {
            kind,
            code,
            signal,
            stderr_tail,
            failure_kind: self.failure_kind,
        }
    }
}

impl Drop for NativeWorker {
    fn drop(&mut self) {
        if self.observed_exit.is_some() {
            self.is_alive();
        }
        if self.exit.is_none() {
            self.fail_and_reap(ExitKind::Killed);
        }
    }
}

#[derive(Debug)]
struct ChildStdioTransport {
    stdout: ChildStdout,
}

impl Transport for ChildStdioTransport {
    fn send(&mut self, _envelope: &Envelope) -> Result<(), ProtocolError> {
        Err(ProtocolError::Rejected(
            "stdio reader transport has no writer".to_owned(),
        ))
    }

    fn recv(&mut self) -> Result<Envelope, ProtocolError> {
        let mut buffer = Vec::new();
        read_frame(&mut self.stdout, &mut buffer)?;
        Envelope::decode(&buffer)
    }

    fn recv_with_diagnostics(&mut self, sensitive: &AtomicBool) -> Result<Envelope, ProtocolError> {
        let mut buffer = Vec::new();
        read_frame(&mut self.stdout, &mut buffer)?;
        Envelope::decode_with_diagnostics(&buffer, sensitive)
    }

    fn try_clone_handle(&self) -> Result<Box<dyn Transport>, ProtocolError> {
        Err(ProtocolError::Rejected(
            "child stdio transport cannot clone its read handle".to_owned(),
        ))
    }

    fn set_read_timeout(&mut self, _timeout: Option<Duration>) -> Result<(), ProtocolError> {
        Ok(())
    }

    fn close(&mut self) {}
}

fn spawn_stdio_writer(
    mut stdin: ChildStdin,
    receiver: Receiver<Envelope>,
    closed: Arc<AtomicBool>,
    pending: Arc<AtomicUsize>,
    admission: NativeAdmission,
    revoked: Arc<AtomicU64>,
    observations: Arc<Mutex<VecDeque<ProtocolObservation>>>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("crikey-native-writer".into())
        .spawn(move || loop {
            if closed.load(Ordering::Acquire) {
                drain_pending(&receiver, &pending);
                break;
            }
            match receiver.recv_timeout(Duration::from_millis(SOCKET_POLL_MS)) {
                Ok(envelope) => {
                    let Some(_dispatch) = admission.commit(is_callback(&envelope)) else {
                        revoked.store(envelope.request_id, Ordering::Release);
                        pending.fetch_sub(1, Ordering::AcqRel);
                        continue;
                    };
                    observe_outbound(&observations, &envelope);
                    let mut encoded = envelope.encode();
                    let result = write_frame(&mut stdin, &encoded);
                    if matches!(envelope.payload, Some(Payload::TypedExecuteRequest(_))) {
                        encoded.fill(0);
                    }
                    pending.fetch_sub(1, Ordering::AcqRel);
                    if result.is_err() {
                        drain_pending(&receiver, &pending);
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    drain_pending(&receiver, &pending);
                    break;
                }
            }
        })
}

fn drain_pending(receiver: &Receiver<Envelope>, pending: &AtomicUsize) {
    while receiver.try_recv().is_ok() {
        pending.fetch_sub(1, Ordering::AcqRel);
    }
}

fn spawn_blocking_reader(
    mut transport: Box<dyn Transport>,
    context: ReaderContext,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("crikey-native-reader".into())
        .spawn(move || loop {
            if context.closed.load(Ordering::Acquire) {
                break;
            }
            match transport.recv_with_diagnostics(&context.sensitive) {
                Ok(envelope) => {
                    if !validate_inbound(&envelope, &context) {
                        context.protocol_violation.store(true, Ordering::Release);
                        let failure = ReaderEvent::Failure(ReaderFailure {
                            error: ProtocolError::Malformed("illegal plugin-to-host envelope".to_owned()),
                            protocol_violation: true,
                            resource_limit: false,
                        });
                        let _ = enqueue_reader_event(&context, failure);
                        transport.close();
                        break;
                    }
                    if !accept_credit(&envelope, &context.credits) {
                        context.protocol_violation.store(true, Ordering::Release);
                        let failure = ReaderEvent::Failure(ReaderFailure {
                            error: ProtocolError::Malformed("plugin sent a batch without credit".to_owned()),
                            protocol_violation: true,
                            resource_limit: false,
                        });
                        let _ = enqueue_reader_event(&context, failure);
                        transport.close();
                        break;
                    }
                    if !enqueue_reader_event(&context, ReaderEvent::Envelope(Box::new(envelope))) {
                        transport.close();
                        break;
                    }
                }
                Err(error) => {
                    let violation = is_protocol_violation_error(&error);
                    if violation {
                        context.protocol_violation.store(true, Ordering::Release);
                    }
                    let _ = enqueue_reader_event(
                        &context,
                        ReaderEvent::Failure(ReaderFailure {
                            error,
                            protocol_violation: violation,
                            resource_limit: context.resource_limit.load(Ordering::Acquire),
                        }),
                    );
                    break;
                }
            }
        })
}

fn spawn_multiplex_reader(
    mut transport: Box<dyn Transport>,
    outgoing: Receiver<Envelope>,
    context: ReaderContext,
    writer_pending: Arc<AtomicUsize>,
    admission: NativeAdmission,
    revoked: Arc<AtomicU64>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("crikey-native-transport".into())
        .spawn(move || {
            let _ = transport.set_read_timeout(Some(Duration::from_millis(SOCKET_POLL_MS)));
            loop {
                if context.closed.load(Ordering::Acquire) {
                    drain_pending(&outgoing, &writer_pending);
                    transport.close();
                    break;
                }
                loop {
                    match outgoing.try_recv() {
                        Ok(envelope) => {
                            let Some(_dispatch) = admission.commit(is_callback(&envelope)) else {
                                revoked.store(envelope.request_id, Ordering::Release);
                                writer_pending.fetch_sub(1, Ordering::AcqRel);
                                continue;
                            };
                            observe_outbound(&context.observations, &envelope);
                            let result = transport.send(&envelope);
                            writer_pending.fetch_sub(1, Ordering::AcqRel);
                            if let Err(error) = result {
                                drain_pending(&outgoing, &writer_pending);
                                let _ = enqueue_reader_event(
                                    &context,
                                    ReaderEvent::Failure(ReaderFailure {
                                        error,
                                        protocol_violation: false,
                                        resource_limit: false,
                                    }),
                                );
                                transport.close();
                                return;
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            drain_pending(&outgoing, &writer_pending);
                            transport.close();
                            return;
                        }
                    }
                }
                match transport.recv_with_diagnostics(&context.sensitive) {
                    Ok(envelope) => {
                        if !validate_inbound(&envelope, &context) {
                            context.protocol_violation.store(true, Ordering::Release);
                            let _ = enqueue_reader_event(
                                &context,
                                ReaderEvent::Failure(ReaderFailure {
                                    error: ProtocolError::Malformed(
                                        "illegal plugin-to-host envelope".to_owned(),
                                    ),
                                    protocol_violation: true,
                                    resource_limit: false,
                                }),
                            );
                            transport.close();
                            return;
                        }
                        if !accept_credit(&envelope, &context.credits) {
                            context.protocol_violation.store(true, Ordering::Release);
                            let _ = enqueue_reader_event(
                                &context,
                                ReaderEvent::Failure(ReaderFailure {
                                    error: ProtocolError::Malformed(
                                        "plugin sent a batch without credit".to_owned(),
                                    ),
                                    protocol_violation: true,
                                    resource_limit: false,
                                }),
                            );
                            transport.close();
                            return;
                        }
                        if !enqueue_reader_event(&context, ReaderEvent::Envelope(Box::new(envelope))) {
                            transport.close();
                            return;
                        }
                    }
                    Err(ProtocolError::Timeout) => {}
                    Err(error) => {
                        let violation = is_protocol_violation_error(&error);
                        if violation {
                            context.protocol_violation.store(true, Ordering::Release);
                        }
                        let _ = enqueue_reader_event(
                            &context,
                            ReaderEvent::Failure(ReaderFailure {
                                error,
                                protocol_violation: violation,
                                resource_limit: context.resource_limit.load(Ordering::Acquire),
                            }),
                        );
                        return;
                    }
                }
            }
        })
}

/// Strip diagnostic text both on queue admission and again on publication:
/// events queued before the transition must not bypass the incarnation latch.
fn sanitize_event(event: &mut ReaderEvent, privacy: &Mutex<StderrTail>) {
    let state = lock_unpoisoned(privacy);
    if !state.sensitive {
        return;
    }
    let envelope = match event {
        ReaderEvent::Failure(failure) => {
            failure.error = ProtocolError::Closed;
            return;
        }
        ReaderEvent::Envelope(envelope) => envelope,
    };
    let clear_error = |error: &mut message::StructuredError| {
        error.message.clear();
        error.detail.clear();
    };
    match envelope.payload.as_mut() {
        Some(Payload::Log(log)) => log.message.clear(),
        Some(Payload::Error(error)) => clear_error(error),
        Some(Payload::CatalogBatch(batch)) => {
            if let Some(error) = &mut batch.error {
                clear_error(error);
            }
        }
        Some(Payload::Results(batch)) => {
            if let Some(error) = &mut batch.error {
                clear_error(error);
            }
        }
        Some(Payload::ExecuteResult(result)) => {
            if let Some(error) = &mut result.error {
                clear_error(error);
            }
        }
        Some(Payload::ResourceResponse(response)) => {
            if let Some(error) = &mut response.error {
                clear_error(error);
            }
        }
        Some(Payload::PreviewResponse(response)) => response.detail.clear(),
        Some(Payload::CommandsResponse(response)) => response.detail.clear(),
        Some(Payload::HealthReport(report)) => report.detail.clear(),
        _ => {}
    }
}

fn enqueue_reader_event(context: &ReaderContext, event: ReaderEvent) -> bool {
    let mut event = event;
    if matches!(&event, ReaderEvent::Envelope(envelope) if matches!(envelope.payload, Some(Payload::Log(_))))
        && lock_unpoisoned(&context.privacy).sensitive
    {
        return true;
    }
    sanitize_event(&mut event, &context.privacy);
    let bytes = reader_event_size(&event);
    if bytes > READER_QUEUE_MAX_BYTES {
        context.resource_limit.store(true, Ordering::Release);
        return false;
    }
    let mut event = event;
    loop {
        if context.closed.load(Ordering::Acquire) {
            return false;
        }
        let mut reserved_depth = false;
        let mut depth = context.queue.load(Ordering::Acquire);
        loop {
            if depth >= READER_QUEUE_CAPACITY {
                break;
            }
            match context
                .queue
                .compare_exchange_weak(depth, depth + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    reserved_depth = true;
                    break;
                }
                Err(next) => depth = next,
            }
        }
        if !reserved_depth {
            thread::yield_now();
            continue;
        }
        let mut reserved_bytes = false;
        let mut current = context.queued_bytes.load(Ordering::Acquire);
        loop {
            if current.saturating_add(bytes) > READER_QUEUE_MAX_BYTES {
                break;
            }
            match context.queued_bytes.compare_exchange_weak(
                current,
                current.saturating_add(bytes),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    reserved_bytes = true;
                    break;
                }
                Err(next) => current = next,
            }
        }
        if !reserved_bytes {
            context.queue.fetch_sub(1, Ordering::AcqRel);
            context.resource_limit.store(true, Ordering::Release);
            return false;
        }
        update_peak(&context.peak, context.queue.load(Ordering::Acquire));
        match context.sender.try_send(event) {
            Ok(()) => return true,
            Err(TrySendError::Full(value)) => {
                event = value;
                context.queue.fetch_sub(1, Ordering::AcqRel);
                context.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
                thread::yield_now();
            }
            Err(TrySendError::Disconnected(_)) => {
                context.queue.fetch_sub(1, Ordering::AcqRel);
                context.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
                return false;
            }
        }
    }
}

fn validate_inbound(envelope: &Envelope, context: &ReaderContext) -> bool {
    push_observation(
        &context.observations,
        ProtocolObservation {
            direction: "plugin->host",
            kind: envelope_kind(envelope),
            request_id: envelope.request_id,
            connection_id: envelope.connection_id,
            generation: envelope.generation,
        },
    );
    let first_handshake = matches!(envelope.payload, Some(Payload::Handshake(_)))
        && !context.handshake_seen.swap(true, Ordering::AcqRel);
    if first_handshake {
        return envelope.connection_id == 0 && legal_plugin_payload(envelope, 0);
    }
    if matches!(envelope.payload, Some(Payload::Handshake(_))) {
        return false;
    }
    if envelope.connection_id != context.connection_id {
        record_echo_mismatch(
            &context.mismatch,
            false,
            false,
            format!(
                "connection_id echo {} != {}",
                envelope.connection_id, context.connection_id
            ),
        );
        return false;
    }
    if matches!(envelope.payload, Some(Payload::StructuredViewResponse(_))) {
        return envelope.request_id != 0
            && context
                .view_request
                .compare_exchange(envelope.request_id, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
    }
    legal_plugin_payload(envelope, context.page_request.load(Ordering::Acquire))
}

/// Whether a plugin may send this payload at all.
///
/// `page_request` is the request id of the frame the host is waiting for, or
/// zero when it is waiting for none. A page frame is a reply and only a
/// reply: an unsolicited one is refused here exactly as an unsolicited
/// `Event` is, because a plugin that could draw whenever it wanted would own
/// the user's screen rather than borrow it (spec 32.3).
fn legal_plugin_payload(envelope: &Envelope, page_request: u64) -> bool {
    if let Some(Payload::PageFrame(_)) = envelope.payload {
        return page_request != 0 && envelope.request_id == page_request;
    }
    matches!(
        envelope.payload,
        Some(Payload::Handshake(_))
            | Some(Payload::Results(_))
            | Some(Payload::CatalogBatch(_))
            | Some(Payload::ExecuteResult(_))
            | Some(Payload::TypedExecuteResponse(_))
            | Some(Payload::PreviewResponse(_))
            | Some(Payload::CommandsResponse(_))
            | Some(Payload::Error(_))
            | Some(Payload::Log(_))
            | Some(Payload::HealthReport(_))
            | Some(Payload::ResourceRequest(_))
            | Some(Payload::ResourceResponse(_))
    )
}

fn accept_credit(envelope: &Envelope, credits: &AtomicI64) -> bool {
    if let Some(Payload::StructuredViewResponse(response)) = &envelope.payload {
        if response.status != 1 {
            return crikey_native_protocol::structured_view::bounded_refusal(envelope);
        }
    }
    if !matches!(
        envelope.payload,
        Some(Payload::Results(_)) | Some(Payload::CatalogBatch(_)) | Some(Payload::StructuredViewResponse(_))
    ) {
        return true;
    }
    let mut current = credits.load(Ordering::Acquire);
    loop {
        if current <= 0 {
            return false;
        }
        match credits.compare_exchange(current, current - 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(next) => current = next,
        }
    }
}

fn is_protocol_violation_error(error: &ProtocolError) -> bool {
    matches!(
        error,
        ProtocolError::FrameTooLarge(_)
            // The frame was legal on the wire and only its decoded size was
            // refused. Still the plugin's fault -- the decodable ceiling is
            // discoverable through `message::max_decodable_items`, and the
            // shipped SDKs split on it -- but the error names batch sizing
            // rather than framing, so the diagnostic points at the real cause.
            | ProtocolError::DecodeBudgetExceeded { .. }
            | ProtocolError::UnsupportedVersion(_)
            | ProtocolError::Malformed(_)
            | ProtocolError::Rejected(_)
    )
}

fn receive_handshake(
    link: &Arc<WorkerLink>,
    token: &str,
    options: &WorkerOptions,
) -> Result<PluginHandshake, HostError> {
    let end = deadline(options.startup_timeout_ms);
    let event = match link.recv(end.saturating_duration_since(Instant::now())) {
        Ok(value) => value,
        Err(RecvTimeoutError::Timeout) => {
            return Err(HostError::Handshake("startup handshake timed out".to_owned()))
        }
        Err(RecvTimeoutError::Disconnected) => {
            return Err(HostError::Handshake("startup channel closed".to_owned()))
        }
    };
    let envelope = match event {
        ReaderEvent::Envelope(value) => value,
        ReaderEvent::Failure(value) => {
            return Err(HostError::Handshake(format!(
                "startup transport: {:?}",
                value.error
            )))
        }
    };
    let handshake = match envelope.payload {
        Some(Payload::Handshake(value)) => value,
        _ => return Err(HostError::Handshake("first frame was not a handshake".to_owned())),
    };
    let rejection = if handshake.session_token != token {
        Some("session token mismatch".to_owned())
    } else if handshake.protocol_version != PROTOCOL_VERSION {
        Some(format!(
            "unsupported protocol version {}",
            handshake.protocol_version
        ))
    } else {
        None
    };
    let ack = message::HandshakeAck {
        protocol_version: PROTOCOL_VERSION,
        host_capabilities: vec![
            "streaming_catalog".to_owned(),
            "streaming_suggestions".to_owned(),
            "cancellation".to_owned(),
            crikey_core::PREVIEW_CAPABILITY.to_owned(),
            crikey_core::command::COMMANDS_CAPABILITY.to_owned(),
            crikey_core::structured_arguments::TYPED_ARGUMENTS_CAPABILITY.to_owned(),
            crikey_core::result_presentation::RESULT_PRESENTATION_CAPABILITY.to_owned(),
            crikey_core::structured_view::STRUCTURED_VIEW_CAPABILITY.to_owned(),
        ],
        host_version: env!("CARGO_PKG_VERSION").to_owned(),
        accepted: rejection.is_none(),
        reject_reason: rejection.clone().unwrap_or_default(),
        max_frame_bytes: crikey_native_protocol::MAX_FRAME_BYTES as u64,
        initial_credits: options.limits.initial_credits,
        unknown: Default::default(),
    };
    link.set_current(envelope.request_id, envelope.generation);
    link.send(Envelope {
        connection_id: link.connection_id,
        request_id: envelope.request_id,
        generation: envelope.generation,
        deadline_ms: 0,
        payload: Some(Payload::HandshakeAck(ack)),
        unknown: Default::default(),
    })?;
    if let Some(reason) = rejection {
        link.wait_writer_idle(Instant::now() + Duration::from_millis(CONTROL_WRITE_TIMEOUT_MS));
        return Err(HostError::Handshake(reason));
    }
    Ok(PluginHandshake {
        plugin_id: handshake.plugin_id,
        plugin_name: handshake.plugin_name,
        plugin_version: handshake.plugin_version,
        sdk_version: handshake.sdk_version,
        protocol_version: handshake.protocol_version,
        capabilities: capabilities_from_strings(&handshake.capabilities),
        structured_view: handshake
            .capabilities
            .iter()
            .any(|s| s == crikey_core::structured_view::STRUCTURED_VIEW_CAPABILITY),
        typed_arguments: handshake
            .capabilities
            .iter()
            .any(|s| s == crikey_core::structured_arguments::TYPED_ARGUMENTS_CAPABILITY),
        result_presentation: handshake
            .capabilities
            .iter()
            .any(|s| s == crikey_core::result_presentation::RESULT_PRESENTATION_CAPABILITY),
        commands: handshake
            .capabilities
            .iter()
            .any(|s| s == crikey_core::command::COMMANDS_CAPABILITY),
        preview: handshake
            .capabilities
            .iter()
            .any(|s| s == crikey_core::PREVIEW_CAPABILITY),
    })
}

fn capabilities_from_strings(values: &[String]) -> Capabilities {
    let mut capabilities = Capabilities::default();
    for value in values {
        match value.as_str() {
            "streaming_catalog" | "streaming-catalog" => capabilities.streaming_catalog = true,
            "streaming_suggestions" | "streaming-suggestions" => capabilities.streaming_suggestions = true,
            "cancellation" => capabilities.cancellation = true,
            "configuration_updates" | "configuration-updates" => capabilities.configuration_updates = true,
            "events" => capabilities.events = true,
            _ => {}
        }
    }
    capabilities
}

fn make_endpoint(kind: TransportKind, _token: &str) -> Result<(Endpoint, Option<Listener>), HostError> {
    match kind {
        TransportKind::Stdio => Ok((Endpoint::Stdio, None)),
        TransportKind::UnixSocket => {
            let directory = private_endpoint_directory().map_err(HostError::Spawn)?;
            let name = endpoint_name().map_err(HostError::Spawn)?;
            let endpoint = Endpoint::UnixSocket(directory.join(format!("{name}.sock")));
            let listener = Listener::bind(&endpoint)
                .map_err(|error| HostError::Spawn(format!("bind endpoint: {error:?}")))?;
            Ok((endpoint, Some(listener)))
        }
        TransportKind::NamedPipe => {
            let name = endpoint_name().map_err(HostError::Spawn)?;
            let endpoint = Endpoint::NamedPipe(format!("crikey-native-{name}"));
            let listener = Listener::bind(&endpoint)
                .map_err(|error| HostError::Spawn(format!("bind endpoint: {error:?}")))?;
            Ok((endpoint, Some(listener)))
        }
    }
}

fn add_restricted_environment(command: &mut Command, spec: &LaunchSpec, endpoint: &Endpoint, token: &str) {
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    #[cfg(windows)]
    for key in ["SystemRoot", "SystemDrive", "TEMP", "TMP", "USERPROFILE"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    for (key, value) in &spec.environment {
        command.env(key, value);
    }
    command.env("CRIKEY_PLUGIN_ENDPOINT", endpoint.to_spec());
    command.env("CRIKEY_SESSION_TOKEN", token);
    command.env("CRIKEY_PLUGIN_ID", &spec.plugin.0);
    command.env("CRIKEY_PROTOCOL_VERSION", PROTOCOL_VERSION.to_string());
}

/// Spawns one native child from the process-wide spawner thread.
///
/// `PR_SET_PDEATHSIG` is thread-scoped: the kernel signals the child when the
/// *thread* that cloned it exits, not when this process does. Native calls run
/// on short-lived per-query dispatch threads, so spawning inline would arm a
/// `SIGKILL` that fires the moment the query which happened to start the plugin
/// finishes - the worker would die between keystrokes and every later query
/// would spend restart budget. One long-lived owner thread, which can only exit
/// with this process, makes the parent-death signal mean what it says.
#[cfg(target_os = "linux")]
fn spawn_child(command: Command) -> std::io::Result<Child> {
    spawner::spawn(command)
}

#[cfg(not(target_os = "linux"))]
fn spawn_child(mut command: Command) -> std::io::Result<Child> {
    command.spawn()
}

#[cfg(target_os = "linux")]
mod spawner {
    use std::process::{Child, Command};
    use std::sync::mpsc::{self, SyncSender};
    use std::sync::LazyLock;
    use std::thread;

    struct Request {
        command: Command,
        reply: SyncSender<std::io::Result<Child>>,
    }

    /// The one thread allowed to clone a plugin process.
    ///
    /// This serializes `fork`/`exec` across plugins. Process creation costs
    /// milliseconds against per-plugin call budgets measured in hundreds, and
    /// nothing waits on the queue except the spawn it submitted, so the shared
    /// owner is not a throughput bound.
    static REQUESTS: LazyLock<SyncSender<Request>> = LazyLock::new(|| {
        let (sender, receiver) = mpsc::sync_channel::<Request>(0);
        thread::Builder::new()
            .name("crikey-native-spawner".to_owned())
            .spawn(move || {
                for request in receiver {
                    let mut command = request.command;
                    let _ = request.reply.send(command.spawn());
                }
            })
            .expect("the native spawner thread starts");
        sender
    });

    pub(super) fn spawn(command: Command) -> std::io::Result<Child> {
        let (reply, answer) = mpsc::sync_channel(1);
        REQUESTS
            .send(Request { command, reply })
            .map_err(|_| std::io::Error::other("the native spawner thread is gone"))?;
        answer
            .recv()
            .map_err(|_| std::io::Error::other("the native spawner thread dropped a spawn"))?
    }
}

/// Arms `SIGKILL` on host death so a plugin cannot outlive its launcher
/// (spec 24.3). The request is recorded on the command and takes effect in the
/// child, which [`spawn_child`] clones from a thread that outlives every query.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn configure_parent_death(command: &mut Command) {
    const PR_SET_PDEATHSIG: i32 = 1;
    const SIGKILL: usize = 9;
    let launcher_pid = std::process::id() as i32;
    // SAFETY: these declarations match the Linux libc ABI.
    unsafe extern "C" {
        fn getppid() -> i32;
        fn prctl(option: i32, arg2: usize, arg3: usize, arg4: usize, arg5: usize) -> i32;
    }
    // SAFETY: `pre_exec` runs in the child between fork and exec. The closure
    // captures one `Copy` pid and calls two async-signal-safe syscalls; it
    // performs no allocation, locking, or Rust I/O.
    unsafe {
        command.pre_exec(move || {
            if prctl(PR_SET_PDEATHSIG, SIGKILL, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The launcher can die between fork and here, in which case the
            // death signal was already delivered and missed.
            if getppid() != launcher_pid {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "launcher exited before plugin setup completed",
                ));
            }
            Ok(())
        });
    }
}

fn session_token() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    fill_csprng(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn endpoint_name() -> Result<String, String> {
    let mut bytes = [0_u8; 12];
    fill_csprng(&mut bytes)?;
    let counter = ENDPOINT_COUNTER.fetch_add(1, Ordering::Relaxed);
    let random: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{random}-{counter:x}"))
}

#[cfg(unix)]
fn private_endpoint_directory() -> Result<std::path::PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_owned());
    let directory = base.join(format!("crikey-{user}"));
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    Ok(directory)
}

#[cfg(not(unix))]
fn private_endpoint_directory() -> Result<std::path::PathBuf, String> {
    Err("private Unix endpoint directories are unavailable on this platform".to_owned())
}

/// Fills `bytes` from the operating system's CSPRNG.
///
/// One source for the whole tree (the workspace names `getrandom` for exactly
/// this), rather than a per-platform hand-rolled call. The previous Windows
/// arm passed a null algorithm handle to `BCryptGenRandom` without
/// `BCRYPT_USE_SYSTEM_PREFERRED_RNG`, which the API rejects with
/// `STATUS_INVALID_HANDLE`: every session token and endpoint name on Windows
/// would have failed to generate, and no test on a Unix host could see it.
fn fill_csprng(bytes: &mut [u8]) -> Result<(), String> {
    getrandom::fill(bytes).map_err(|error| format!("the operating system CSPRNG failed: {error}"))
}

fn next_connection_id() -> u64 {
    loop {
        let value = CONNECTION_COUNTER.fetch_add(1, Ordering::AcqRel);
        if value != 0 {
            return value;
        }
    }
}

fn deadline(timeout_ms: u64) -> Instant {
    Instant::now()
        .checked_add(Duration::from_millis(timeout_ms))
        .unwrap_or_else(Instant::now)
}

fn envelope_kind(envelope: &Envelope) -> &'static str {
    envelope.payload.as_ref().map_or("none", Payload::kind)
}

fn is_stale(envelope: &Envelope, request_id: u64, generation: u64) -> bool {
    envelope.request_id != request_id || envelope.generation != generation
}

fn plugin_error(error: &message::StructuredError) -> PluginError {
    PluginError {
        message: error.message.clone(),
        detail: error.detail.clone(),
    }
}

/// Folds one execute reply into a host outcome, or names the violation that
/// makes the reply unusable.
///
/// A plugin that reports it opened a page without naming one has told the
/// host to display a surface it cannot ask anything about: every later frame
/// request quotes the page id back, so an empty id is an action that can
/// never be drawn and never be closed by its owner. It is refused here rather
/// than allowed to become a permanently blank page.
fn execute_outcome(result: &message::ExecuteResult) -> Result<ExecuteOutcome, &'static str> {
    if result.navigation.is_some() && result.outcome.as_i32() != 5 {
        return Err("unexpected route navigation");
    }
    match result.outcome.as_i32() {
        1 => Ok(ExecuteOutcome::Ok),
        2 => Ok(ExecuteOutcome::Failed(
            result
                .error
                .as_ref()
                .map(plugin_error)
                .unwrap_or_else(|| PluginError {
                    message: "plugin execution failed".to_owned(),
                    detail: String::new(),
                }),
        )),
        3 => Ok(ExecuteOutcome::Unsupported),
        4 if result.page_id.is_empty() => Err("execute opened a page without naming it"),
        4 => Ok(ExecuteOutcome::ShowPage {
            page_id: result.page_id.clone(),
        }),
        5 if result.page_id.is_empty() && result.error.is_none() => result
            .navigation
            .clone()
            .ok_or("missing route navigation")
            .and_then(|n| {
                crikey_native_protocol::structured_view::from_navigation(n)
                    .map(ExecuteOutcome::Navigate)
                    .map_err(|_| "invalid route navigation")
            }),
        _ => Err("unknown execute outcome"),
    }
}

/// Decodes and validates one page frame before any of it can be drawn.
///
/// Validation is not advisory here. A frame with a duplicate node id makes
/// the host's own hit testing ambiguous, and one with non-finite geometry
/// makes the renderer's arithmetic meaningless, so the frame is refused whole
/// rather than drawn as far as it happens to parse (spec 32.7).
fn decode_page_frame(frame: &message::PageFrame) -> Result<PageFrame, String> {
    let frame = from_proto_page_frame(frame);
    match frame.validate() {
        Ok(()) => Ok(frame),
        Err(error) => Err(format!("plugin page frame is not drawable: {error}")),
    }
}

fn valid_char_boundary(value: &str, requested: usize) -> usize {
    let mut end = requested.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn update_peak(peak: &AtomicUsize, depth: usize) {
    let mut current = peak.load(Ordering::Acquire);
    while depth > current {
        match peak.compare_exchange(current, depth, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return,
            Err(next) => current = next,
        }
    }
}

fn push_observation(observations: &Mutex<VecDeque<ProtocolObservation>>, observation: ProtocolObservation) {
    let mut observations = lock_unpoisoned(observations);
    if observations.len() == OBSERVATION_CAPACITY {
        let _ = observations.pop_front();
    }
    observations.push_back(observation);
}

fn record_echo_mismatch(
    mismatch: &Mutex<Option<EchoMismatch>>,
    request_id: bool,
    generation: bool,
    reason: String,
) {
    let mut mismatch = lock_unpoisoned(mismatch);
    match mismatch.as_mut() {
        Some(value) => {
            value.request_id |= request_id;
            value.generation |= generation;
            if value.reason.is_empty() {
                value.reason = reason;
            }
        }
        None => {
            *mismatch = Some(EchoMismatch {
                request_id,
                generation,
                reason,
            });
        }
    }
}

fn reader_event_size(event: &ReaderEvent) -> usize {
    match event {
        ReaderEvent::Envelope(envelope) => envelope.encode().len(),
        ReaderEvent::Failure(failure) => failure.error.to_string().len().saturating_add(1),
    }
}

fn wait_child_after_termination(child: &mut Child, end: Instant) -> Option<std::process::ExitStatus> {
    loop {
        match child_exited(child) {
            Ok(true) => return child.wait().ok(),
            Ok(false) if Instant::now() < end => thread::sleep(Duration::from_millis(5)),
            _ => return None,
        }
    }
}

fn settle_link_thread(slot: &Mutex<Option<JoinHandle<()>>>, end: Instant) -> Result<(), &'static str> {
    loop {
        let finished = lock_unpoisoned(slot).as_ref().map(JoinHandle::is_finished);
        match finished {
            None => return Ok(()),
            Some(true) => break,
            Some(false) if Instant::now() < end => thread::sleep(Duration::from_millis(1)),
            _ => return Err("native pipe worker did not settle"),
        }
    }
    let handle = lock_unpoisoned(slot).take();
    handle.map_or(Ok(()), |handle| {
        handle.join().map_err(|_| "native pipe worker panicked")
    })
}

fn settle_owned_thread(slot: &mut Option<JoinHandle<()>>, end: Instant) -> Result<(), &'static str> {
    while slot.as_ref().is_some_and(|handle| !handle.is_finished()) {
        if Instant::now() >= end {
            return Err("native pipe worker did not settle");
        }
        thread::sleep(Duration::from_millis(1));
    }
    slot.take().map_or(Ok(()), |handle| {
        handle.join().map_err(|_| "native pipe worker panicked")
    })
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn child_exited(child: &Child) -> std::io::Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { info.si_pid() } != 0)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn child_exited(child: &Child) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn WaitForSingleObject(handle: *mut std::ffi::c_void, milliseconds: u32) -> u32;
    }
    match unsafe { WaitForSingleObject(child.as_raw_handle(), 0) } {
        0 => Ok(true),
        258 => Ok(false),
        _ => Err(std::io::Error::last_os_error()),
    }
}

#[cfg(not(any(unix, windows)))]
fn child_exited(_: &Child) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "native process observation unavailable",
    ))
}

/// Sole owner of one Windows job-object handle; `Drop` closes it.
///
/// Startup has half a dozen early returns between `AssignProcessToJobObject`
/// and a constructed [`NativeWorker`], and a raw `usize` made every one of them
/// a place to forget the `CloseHandle`. The leak is not merely a handle-table
/// entry: the job carries `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so while any
/// handle to it remains open the kill never fires and descendants that the
/// direct `child.kill()` cannot reach keep running. An owner makes the release
/// unforgettable, because the compiler emits it on every path.
#[cfg(windows)]
#[derive(Debug)]
struct OwnedJob(usize);

#[cfg(windows)]
impl Drop for OwnedJob {
    fn drop(&mut self) {
        close_job_handle(self.0);
    }
}

/// Resumes the threads of a child created with `CREATE_SUSPENDED`.
///
/// `std::process::Child` does not surface the primary thread handle, so the
/// thread is found the documented way: a `TH32CS_SNAPTHREAD` Toolhelp snapshot
/// lists every thread on the system together with the process that owns it. A
/// process created suspended has exactly one thread and that thread cannot
/// exit, and the caller still holds the process handle so the identifier cannot
/// have been reused - the entry matched here is that primary thread. Resuming
/// nothing is an error rather than a shrug: it would leave a plugin that never
/// runs and a handshake that can only time out.
#[cfg(windows)]
#[allow(unsafe_code)]
fn resume_primary_thread(process_id: u32) -> Result<(), String> {
    use std::ffi::c_void;
    #[repr(C)]
    struct ThreadEntry32 {
        size: u32,
        usage: u32,
        thread_id: u32,
        owner_process_id: u32,
        base_priority: i32,
        delta_priority: i32,
        flags: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> *mut c_void;
        fn Thread32First(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
        fn Thread32Next(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
        fn OpenThread(access: u32, inherit: i32, thread_id: u32) -> *mut c_void;
        fn ResumeThread(thread: *mut c_void) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
    const THREAD_SUSPEND_RESUME: u32 = 0x0002;
    // `ResumeThread` reports failure as (DWORD)-1, not as zero: zero is the
    // legitimate "was not suspended" previous count.
    const RESUME_FAILED: u32 = u32::MAX;

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot.is_null() || snapshot as isize == -1 {
        return Err("CreateToolhelp32Snapshot failed".to_owned());
    }
    let mut entry = ThreadEntry32 {
        size: std::mem::size_of::<ThreadEntry32>() as u32,
        usage: 0,
        thread_id: 0,
        owner_process_id: 0,
        base_priority: 0,
        delta_priority: 0,
        flags: 0,
    };
    let mut resumed = 0_u32;
    let mut failure: Option<String> = None;
    // SAFETY: `snapshot` is a live Toolhelp handle and `entry` is valid
    // writable storage whose `size` field was set before the first call. The
    // loop closes the snapshot on every exit.
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more && failure.is_none() {
        if entry.owner_process_id == process_id {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.thread_id) };
            if thread.is_null() {
                failure = Some("OpenThread failed for the suspended child's thread".to_owned());
            } else {
                if unsafe { ResumeThread(thread) } == RESUME_FAILED {
                    failure = Some("ResumeThread failed for the suspended child's thread".to_owned());
                } else {
                    resumed += 1;
                }
                let _ = unsafe { CloseHandle(thread) };
            }
        }
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    let _ = unsafe { CloseHandle(snapshot) };
    if let Some(error) = failure {
        return Err(error);
    }
    if resumed == 0 {
        return Err("the suspended child had no thread to resume".to_owned());
    }
    Ok(())
}

/// The three fields Windows needs for a tree-wide memory ceiling.
///
/// Kept as a small value rather than folded into the FFI call because
/// `JOB_OBJECT_LIMIT_PROCESS_MEMORY` looks plausible while limiting each
/// process separately. WebKit and any other forking host could then multiply
/// the declared budget by its child count. The contract is the aggregate
/// `JOB_OBJECT_LIMIT_JOB_MEMORY` flag and `JobMemoryLimit`, with
/// `ProcessMemoryLimit` left zero.
#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
struct WindowsJobMemoryLimit {
    flag: u32,
    process_bytes: usize,
    job_bytes: usize,
}

#[cfg(any(windows, test))]
fn windows_job_memory_limit(bytes: u64) -> Result<WindowsJobMemoryLimit, String> {
    const LIMIT_JOB_MEMORY: u32 = 0x0200;
    let job_bytes = usize::try_from(bytes).map_err(|_| "memory limit does not fit this target".to_owned())?;
    Ok(WindowsJobMemoryLimit {
        flag: LIMIT_JOB_MEMORY,
        process_bytes: 0,
        job_bytes,
    })
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn create_job_for_child(child: &Child, limits: &crate::launch::ResourceLimits) -> Result<OwnedJob, String> {
    use std::ffi::c_void;
    #[repr(C)]
    struct IoCounters {
        read: u64,
        write: u64,
        other: u64,
        read_bytes: u64,
        write_bytes: u64,
        other_bytes: u64,
    }
    #[repr(C)]
    struct BasicLimitInfo {
        per_process_user_time: i64,
        per_job_user_time: i64,
        limit_flags: u32,
        minimum_working_set: usize,
        maximum_working_set: usize,
        active_process_limit: u32,
        affinity: usize,
        priority: u32,
        scheduling_class: u32,
    }
    #[repr(C)]
    struct ExtendedLimitInfo {
        basic: BasicLimitInfo,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory: usize,
        peak_job_memory: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> *mut c_void;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn SetInformationJobObject(job: *mut c_void, class: u32, info: *mut c_void, length: u32) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    const EXTENDED_LIMITS_CLASS: u32 = 9;
    const LIMIT_JOB_TIME: u32 = 0x0004;
    const LIMIT_ACTIVE_PROCESS: u32 = 0x0008;
    const LIMIT_JOB_MEMORY: u32 = 0x0200;
    const LIMIT_KILL_ON_CLOSE: u32 = 0x2000;
    let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if job.is_null() {
        return Err("CreateJobObjectW failed".to_owned());
    }
    let mut info = ExtendedLimitInfo {
        basic: BasicLimitInfo {
            per_process_user_time: 0,
            per_job_user_time: 0,
            limit_flags: LIMIT_KILL_ON_CLOSE,
            minimum_working_set: 0,
            maximum_working_set: 0,
            active_process_limit: 0,
            affinity: 0,
            priority: 0,
            scheduling_class: 0,
        },
        io: IoCounters {
            read: 0,
            write: 0,
            other: 0,
            read_bytes: 0,
            write_bytes: 0,
            other_bytes: 0,
        },
        process_memory_limit: 0,
        job_memory_limit: 0,
        peak_process_memory: 0,
        peak_job_memory: 0,
    };
    if let Some(bytes) = limits.max_memory_bytes {
        let memory = match windows_job_memory_limit(bytes) {
            Ok(memory) => memory,
            Err(error) => {
                unsafe { CloseHandle(job) };
                return Err(error);
            }
        };
        debug_assert_eq!(memory.flag, LIMIT_JOB_MEMORY);
        info.basic.limit_flags |= memory.flag;
        info.process_memory_limit = memory.process_bytes;
        info.job_memory_limit = memory.job_bytes;
    }
    if let Some(processes) = limits.max_processes {
        info.basic.limit_flags |= LIMIT_ACTIVE_PROCESS;
        info.basic.active_process_limit = match u32::try_from(processes) {
            Ok(value) => value,
            Err(_) => {
                unsafe { CloseHandle(job) };
                return Err("process limit does not fit Windows".to_owned());
            }
        };
    }
    if let Some(seconds) = limits.max_cpu_time_seconds {
        info.basic.limit_flags |= LIMIT_JOB_TIME;
        info.basic.per_job_user_time = match i64::try_from(seconds.saturating_mul(10_000_000)) {
            Ok(value) => value,
            Err(_) => {
                unsafe { CloseHandle(job) };
                return Err("CPU limit does not fit Windows".to_owned());
            }
        };
    }
    if unsafe {
        SetInformationJobObject(
            job,
            EXTENDED_LIMITS_CLASS,
            (&mut info as *mut ExtendedLimitInfo).cast(),
            std::mem::size_of::<ExtendedLimitInfo>() as u32,
        )
    } == 0
    {
        unsafe { CloseHandle(job) };
        return Err("SetInformationJobObject failed".to_owned());
    }
    let process = child.as_raw_handle();
    if unsafe { AssignProcessToJobObject(job, process) } == 0 {
        unsafe { CloseHandle(job) };
        return Err("AssignProcessToJobObject failed".to_owned());
    }
    Ok(OwnedJob(job as usize))
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn close_job_handle(handle: usize) {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    let _ = unsafe { CloseHandle(handle as *mut c_void) };
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn prepare_group_reap(child: &Child) -> bool {
    let pid = child.id();
    if pid <= 1 || pid > i32::MAX as u32 {
        return false;
    }
    if unsafe { libc::killpg(pid as i32, libc::SIGKILL) } == 0 {
        return true;
    }
    let error = std::io::Error::last_os_error().raw_os_error();
    if error == Some(libc::ESRCH) {
        return true;
    }
    #[cfg(target_vendor = "apple")]
    {
        prepare_darwin_group_reap(child, error)
    }
    #[cfg(not(target_vendor = "apple"))]
    false
}

#[cfg(all(unix, any(target_vendor = "apple", test)))]
#[allow(unsafe_code)]
fn prepare_darwin_group_reap(child: &Child, group_error: Option<i32>) -> bool {
    if group_error != Some(libc::EPERM) {
        return false;
    }
    // XNU's group iterator can lose proc_find access during proc_refdrain,
    // before exit publishes SZOMB to waitid. EPERM therefore does not imply
    // that an instantaneous nonreaping exit observation is ready.
    // See XNU kern_proc.c:pgrp_iterate and kern_exit.c:proc_exit.
    // Signal only the still-owned, unreaped leader: its PID cannot be reused.
    // A real permission error remains a failure. ESRCH permits the existing
    // bounded reap, not settlement: strict post-reap group absence and pipe
    // joins are still required, and the group must never be signalled again.
    if unsafe { libc::kill(child.id() as i32, libc::SIGKILL) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn wait_group_absent(pid: u32) -> bool {
    let end = Instant::now() + Duration::from_millis(CHILD_REAP_TIMEOUT_MS);
    loop {
        if unsafe { libc::killpg(pid as i32, 0) } != 0 {
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ESRCH) => return true,
                // Darwin zombie-only groups report EPERM until their actual
                // reaper runs. This is presence, never an absence proof.
                Some(libc::EPERM) => {}
                _ => return false,
            }
        }
        if Instant::now() >= end {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn terminate_job_checked(handle: usize) -> bool {
    use std::ffi::c_void;
    #[repr(C)]
    struct Accounting {
        user: i64,
        kernel: i64,
        period_user: i64,
        period_kernel: i64,
        faults: u32,
        total: u32,
        active: u32,
        terminated: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn TerminateJobObject(job: *mut c_void, code: u32) -> i32;
        fn QueryInformationJobObject(
            job: *mut c_void,
            class: i32,
            info: *mut c_void,
            length: u32,
            returned: *mut u32,
        ) -> i32;
    }
    if unsafe { TerminateJobObject(handle as *mut c_void, 1) } == 0 {
        return false;
    }
    let end = Instant::now() + Duration::from_millis(CHILD_REAP_TIMEOUT_MS);
    loop {
        let mut accounting: Accounting = unsafe { std::mem::zeroed() };
        if unsafe {
            QueryInformationJobObject(
                handle as *mut c_void,
                1,
                (&mut accounting as *mut Accounting).cast(),
                std::mem::size_of::<Accounting>() as u32,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return false;
        }
        if accounting.active == 0 {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(value) => value,
        Err(value) => value.into_inner(),
    }
}

#[derive(Debug, Default)]
struct StderrTail {
    bytes: VecDeque<u8>,
    sensitive: bool,
}

impl StderrTail {
    fn push(&mut self, bytes: &[u8]) {
        if self.sensitive {
            return;
        }
        for byte in bytes {
            if self.bytes.len() == STDERR_TAIL_BYTES {
                let _ = self.bytes.pop_front();
            }
            self.bytes.push_back(*byte);
        }
    }

    fn text(&self) -> String {
        if self.sensitive {
            return String::new();
        }
        let (first, second) = self.bytes.as_slices();
        if second.is_empty() {
            return String::from_utf8_lossy(first).into_owned();
        }
        let mut bytes = Vec::with_capacity(first.len() + second.len());
        bytes.extend_from_slice(first);
        bytes.extend_from_slice(second);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

fn spawn_stderr_drain(
    mut stderr: impl Read + Send + 'static,
    tail: Arc<Mutex<StderrTail>>,
) -> Result<JoinHandle<()>, String> {
    thread::Builder::new()
        .name("crikey-native-stderr".to_owned())
        .spawn(move || {
            let mut buffer = [0u8; 1024];
            loop {
                match stderr.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => lock_unpoisoned(&tail).push(&buffer[..count]),
                }
            }
        })
        .map_err(|error| error.to_string())
}

impl ExitKind {
    fn from_status(status: &std::process::ExitStatus) -> Self {
        if status.success() {
            Self::Clean
        } else {
            Self::Crashed
        }
    }
}

/// The three decisions a page makes at the protocol boundary, each of which
/// is invisible from an integration test because the fixture plugin is
/// well-behaved by construction: which execute outcome opens a page, which
/// page frame the reader will accept at all, and which one is drawable.
#[cfg(test)]
mod windows_job_limits {
    use super::*;

    /// A job-wide ceiling is one budget for the whole process tree. Setting
    /// the per-process field instead would let a three-process host consume
    /// three times the declared amount while every process remained legal.
    #[test]
    fn memory_limit_is_charged_to_the_job_not_each_process() {
        let limit = windows_job_memory_limit(768 * 1024 * 1024).expect("the limit fits");
        assert_eq!(limit.flag, 0x0200, "JOB_OBJECT_LIMIT_JOB_MEMORY");
        assert_eq!(limit.process_bytes, 0);
        assert_eq!(limit.job_bytes, 768 * 1024 * 1024);
    }
}

#[cfg(test)]
mod page_protocol {
    use crikey_core::{PageFrame, PageNode};
    use crikey_native_protocol::convert::to_proto_page_frame;
    use crikey_native_protocol::message::{self, Envelope, ExecuteOutcomeCode, Payload};

    use super::{decode_page_frame, execute_outcome, legal_plugin_payload, ExecuteOutcome};

    fn execute_result(outcome: ExecuteOutcomeCode, page_id: &str) -> message::ExecuteResult {
        message::ExecuteResult {
            outcome,
            error: None,
            page_id: page_id.to_owned(),
            navigation: None,
            unknown: Default::default(),
        }
    }

    fn envelope(request_id: u64, payload: Payload) -> Envelope {
        Envelope {
            connection_id: 1,
            request_id,
            generation: 0,
            deadline_ms: 0,
            payload: Some(payload),
            unknown: Default::default(),
        }
    }

    #[test]
    fn a_named_show_page_outcome_opens_that_page() {
        assert_eq!(
            execute_outcome(&execute_result(ExecuteOutcomeCode::ShowPage, "settings")),
            Ok(ExecuteOutcome::ShowPage {
                page_id: "settings".to_owned()
            })
        );
    }

    #[test]
    fn a_show_page_outcome_without_a_page_id_is_refused() {
        assert!(execute_outcome(&execute_result(ExecuteOutcomeCode::ShowPage, "")).is_err());
    }

    #[test]
    fn the_settled_outcomes_are_unchanged_by_pages() {
        assert_eq!(
            execute_outcome(&execute_result(ExecuteOutcomeCode::Ok, "")),
            Ok(ExecuteOutcome::Ok)
        );
        assert_eq!(
            execute_outcome(&execute_result(ExecuteOutcomeCode::Unsupported, "")),
            Ok(ExecuteOutcome::Unsupported)
        );
        assert!(execute_outcome(&execute_result(ExecuteOutcomeCode::from_i32(9), "")).is_err());
    }

    #[test]
    fn an_unsolicited_page_frame_is_refused_and_the_awaited_one_is_not() {
        let frame = envelope(7, Payload::PageFrame(to_proto_page_frame(&PageFrame::default())));
        assert!(!legal_plugin_payload(&frame, 0));
        assert!(!legal_plugin_payload(&frame, 6));
        assert!(legal_plugin_payload(&frame, 7));
    }

    #[test]
    fn a_frame_that_fails_validation_never_becomes_a_page() {
        let ambiguous = to_proto_page_frame(&PageFrame {
            nodes: vec![
                PageNode {
                    node_id: 4,
                    ..PageNode::default()
                },
                PageNode {
                    node_id: 4,
                    ..PageNode::default()
                },
            ],
            ..PageFrame::default()
        });
        assert!(decode_page_frame(&ambiguous).is_err());
        assert!(decode_page_frame(&to_proto_page_frame(&PageFrame::default())).is_ok());
    }
}

/// Windows-only because the code under test is Win32 itself: off Windows
/// neither the job object nor `CREATE_SUSPENDED` exists, so no Linux suite can
/// say anything about them. This is where the containment order and the job
/// owner's release are pinned by behaviour rather than by inspection.
#[cfg(all(test, windows))]
mod windows_containment {
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::time::{Duration, Instant};

    use super::{CommandExt, ContainedChildJob, CREATE_SUSPENDED};
    use crate::launch::ResourceLimits;

    /// Creates a frozen child the same way [`super::NativeWorker::spawn_inner`]
    /// does, so these tests exercise the real starting state.
    fn suspended(arguments: &[&str]) -> Child {
        let mut command = Command::new("cmd.exe");
        command.args(arguments);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.creation_flags(CREATE_SUSPENDED);
        command.spawn().expect("cmd.exe must be spawnable on Windows")
    }

    fn wait_bounded(child: &mut Child, within: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + within;
        loop {
            match child.try_wait().expect("try_wait must not fail") {
                Some(status) => return Some(status),
                None if Instant::now() >= deadline => return None,
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    /// The child stays frozen until the job is assigned and only then runs, so
    /// a plugin cannot allocate or spawn ahead of the limits the host reports.
    /// Drop the resume and the child never reaches its own exit code; drop the
    /// suspension and the first assertion sees it already gone.
    #[test]
    fn a_contained_child_is_resumed_only_after_the_job_is_assigned() {
        let mut child = suspended(&["/c", "exit", "3"]);
        assert_eq!(
            wait_bounded(&mut child, Duration::from_millis(300)),
            None,
            "a child created suspended must not run before it is contained"
        );
        let job = ContainedChildJob::assign_suspended(&child, &ResourceLimits::default())
            .expect("containment must succeed");
        job.resume(&child).expect("the retained job resumes its child");
        let status =
            wait_bounded(&mut child, Duration::from_secs(10)).expect("a resumed child must reach its exit");
        assert_eq!(
            status.code(),
            Some(3),
            "the contained child must go on to run its own code"
        );
        drop(job);
    }

    /// Dropping the owner closes the last job handle, and
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` then kills whatever is still inside
    /// it. A startup path that returned early holding a bare `usize` left this
    /// child running instead.
    #[test]
    fn dropping_the_job_owner_kills_the_contained_child() {
        let mut child = suspended(&["/c", "ping", "-n", "60", "127.0.0.1"]);
        let job = ContainedChildJob::assign_suspended(&child, &ResourceLimits::default())
            .expect("containment must succeed");
        job.resume(&child).expect("the retained job resumes its child");
        assert_eq!(
            wait_bounded(&mut child, Duration::from_millis(300)),
            None,
            "the contained child must still be running before the job is released"
        );
        drop(job);
        assert!(
            wait_bounded(&mut child, Duration::from_secs(10)).is_some(),
            "closing the last job handle must kill every process left in the job"
        );
    }
}

#[cfg(test)]
mod typed_privacy {
    use super::*;

    #[test]
    fn incarnation_latch_discards_stderr_and_clears_retained_tail() {
        let mut tail = StderrTail::default();
        tail.push(b"before");
        assert_eq!(tail.text(), "before");
        tail.sensitive = true;
        tail.bytes.clear();
        tail.push(b"submitted value from background callback");
        assert!(tail.bytes.is_empty());
        assert!(tail.text().is_empty());
    }

    #[test]
    fn queued_diagnostics_are_sanitized_at_publication() {
        let privacy = Mutex::new(StderrTail::default());
        let mut event = ReaderEvent::Envelope(Box::new(Envelope {
            payload: Some(Payload::Error(message::StructuredError {
                code: message::ErrorCode::from_i32(5),
                message: "callback echoed a value".into(),
                detail: "traceback echoed a value".into(),
                ..Default::default()
            })),
            ..Default::default()
        }));
        sanitize_event(&mut event, &privacy);
        lock_unpoisoned(&privacy).sensitive = true;
        sanitize_event(&mut event, &privacy);
        let ReaderEvent::Envelope(envelope) = event else {
            panic!("lost terminal");
        };
        let Some(Payload::Error(error)) = envelope.payload else {
            panic!("lost error code");
        };
        assert_eq!(error.code.as_i32(), 5);
        assert!(error.message.is_empty());
        assert!(error.detail.is_empty());
    }

    #[test]
    fn sensitive_reader_failures_keep_classification_not_raw_text() {
        let privacy = Mutex::new(StderrTail {
            sensitive: true,
            ..Default::default()
        });
        let mut event = ReaderEvent::Failure(ReaderFailure {
            error: ProtocolError::Malformed("raw frame secret".into()),
            protocol_violation: true,
            resource_limit: false,
        });
        sanitize_event(&mut event, &privacy);
        let ReaderEvent::Failure(failure) = event else {
            panic!("lost lifecycle");
        };
        assert!(matches!(failure.error, ProtocolError::Closed));
        assert!(failure.protocol_violation);
        assert!(!failure.resource_limit);
    }
}

fn presentation_capacity<T>(current: &Vec<T>, incoming: usize) -> usize {
    let needed = current.len().saturating_add(incoming);
    if needed <= current.capacity() {
        current.capacity()
    } else {
        needed.max(current.capacity().saturating_mul(2)).max(4)
    }
}

fn presentation_growth<T>(current: &Vec<T>, incoming: usize) -> usize {
    presentation_capacity(current, incoming)
        .saturating_sub(current.capacity())
        .saturating_mul(std::mem::size_of::<T>())
}

fn reserve_presentation<T>(current: &mut Vec<T>, incoming: usize) {
    let additional = presentation_capacity(current, incoming).saturating_sub(current.len());
    current.reserve_exact(additional);
}

include!("worker_view.rs");

#[cfg(test)]
mod view_credit_control {
    use super::{accept_credit, AtomicI64, Ordering};
    use crikey_native_protocol::{message as m, Message};

    fn reply(status: u32) -> m::Envelope {
        m::Envelope {
            connection_id: 1,
            request_id: 1,
            payload: Some(m::Payload::StructuredViewResponse(m::StructuredViewResponse {
                route_id: "grid".into(),
                logical_view: 1,
                surface: 1,
                status,
                ..Default::default()
            })),
            ..Default::default()
        }
    }
    #[test]
    fn only_bounded_descriptor_free_refusals_are_credit_exempt() {
        let credits = AtomicI64::new(0);
        for status in 2..=5 {
            let envelope = m::Envelope::decode(&reply(status).encode()).unwrap();
            assert!(accept_credit(&envelope, &credits));
            assert_eq!(credits.load(Ordering::Acquire), 0);
        }
        assert!(!accept_credit(&reply(1), &credits));
        assert!(!accept_credit(&reply(0), &credits));
        let mut content = reply(4);
        let Some(m::Payload::StructuredViewResponse(response)) = &mut content.payload else {
            unreachable!()
        };
        response.descriptor = Some(Box::default());
        assert!(!accept_credit(&content, &credits));
        let mut oversized = reply(4).encode();
        crikey_native_protocol::wire::encode_key(
            200,
            crikey_native_protocol::wire::WireType::Length,
            &mut oversized,
        );
        crikey_native_protocol::wire::encode_varint(1024, &mut oversized);
        oversized.resize(oversized.len() + 1024, 0);
        assert!(!accept_credit(
            &m::Envelope::decode(&oversized).unwrap(),
            &credits
        ));
        credits.store(1, Ordering::Release);
        assert!(accept_credit(&reply(1), &credits));
        assert_eq!(credits.load(Ordering::Acquire), 0);
    }
}

/// Owns the same kill-on-close containment used by native workers.
#[cfg(windows)]
#[derive(Debug)]
pub struct ContainedChildJob {
    job: OwnedJob,
    pid: u32,
}

#[cfg(windows)]
impl ContainedChildJob {
    pub fn prepare(command: &mut Command) {
        command.creation_flags(CREATE_SUSPENDED);
    }

    /// Assigns without running guest instructions. Retain this owner before resume.
    pub fn assign_suspended(child: &Child, limits: &crate::launch::ResourceLimits) -> Result<Self, String> {
        create_job_for_child(child, limits).map(|job| Self { job, pid: child.id() })
    }

    pub fn resume(&self, child: &Child) -> Result<(), String> {
        if child.id() != self.pid {
            return Err("contained child ownership mismatch".into());
        }
        if child_exited(child).map_err(|_| "contained child observation failed".to_owned())? {
            return Err("contained child exited before resume".into());
        }
        resume_primary_thread(self.pid)
    }

    pub fn terminate_and_wait(&self) -> Result<(), String> {
        if terminate_job_checked(self.job.0) {
            Ok(())
        } else {
            Err("contained process termination was not confirmed".into())
        }
    }
}

fn is_callback(envelope: &Envelope) -> bool {
    !matches!(
        envelope.payload,
        Some(
            Payload::HandshakeAck(_)
                | Payload::Cancel(_)
                | Payload::Shutdown(_)
                | Payload::Flow(_)
                | Payload::ResourceResponse(_)
        )
    )
}

fn observe_outbound(observations: &Mutex<VecDeque<ProtocolObservation>>, envelope: &Envelope) {
    push_observation(
        observations,
        ProtocolObservation {
            direction: "host->plugin",
            kind: envelope_kind(envelope),
            request_id: envelope.request_id,
            connection_id: envelope.connection_id,
            generation: envelope.generation,
        },
    );
}

#[cfg(all(test, unix))]
mod checked_native_ownership_tests {
    use super::*;

    #[test]
    fn queued_callback_is_revoked_at_the_real_stdio_writer_but_shutdown_is_sent() {
        let path = std::env::temp_dir().join(format!(
            "crikey-native-writer-{}-{}",
            std::process::id(),
            ENDPOINT_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "cat > \"$1\"", "native-writer-fixture"])
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let (sender, receiver) = mpsc::sync_channel(4);
        let pending = Arc::new(AtomicUsize::new(2));
        let revoked = Arc::new(AtomicU64::new(0));
        let admission = NativeAdmission::default();
        let envelope = |request_id, payload| Envelope {
            connection_id: 1,
            request_id,
            generation: 0,
            deadline_ms: 100,
            payload: Some(payload),
            unknown: Default::default(),
        };
        sender
            .send(envelope(
                1,
                Payload::Suggest(message::SuggestRequest {
                    text: "must-not-dispatch".into(),
                    ..Default::default()
                }),
            ))
            .unwrap();
        sender
            .send(envelope(2, Payload::Shutdown(message::Shutdown::default())))
            .unwrap();
        admission.close();
        let writer = spawn_stdio_writer(
            stdin,
            receiver,
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&pending),
            admission,
            Arc::clone(&revoked),
            Arc::new(Mutex::new(VecDeque::new())),
        )
        .unwrap();
        drop(sender);
        writer.join().unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while !child_exited(&child).unwrap() {
            assert!(Instant::now() < end, "the actual writer reader did not finish");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(prepare_group_reap(&child));
        assert!(wait_child_after_termination(&mut child, end).is_some());
        assert!(wait_group_absent(child.id()));
        assert_eq!(revoked.load(Ordering::Acquire), 1);
        assert_eq!(pending.load(Ordering::Acquire), 0);
        let bytes = std::fs::read(&path).unwrap();
        let mut input = std::io::Cursor::new(bytes);
        let mut frame = Vec::new();
        read_frame(&mut input, &mut frame).unwrap();
        assert!(matches!(
            Envelope::decode(&frame).unwrap().payload,
            Some(Payload::Shutdown(_))
        ));
        assert!(read_frame(&mut input, &mut frame).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn unconfirmed_live_exit_returns_without_consuming_child_ownership() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let started = Instant::now();
        let result = wait_child_after_termination(&mut child, started + Duration::from_millis(20));
        assert!(result.is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!child_exited(&child).unwrap());
        assert!(prepare_group_reap(&child));
        assert!(wait_child_after_termination(&mut child, Instant::now() + Duration::from_secs(5)).is_some());
        assert!(wait_group_absent(child.id()));
    }

    #[test]
    fn darwin_group_error_retains_startup_ownership_until_the_real_group_is_absent() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "read value"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let stderr = spawn_stderr_drain(
            child.stderr.take().unwrap(),
            Arc::new(Mutex::new(StderrTail::default())),
        )
        .unwrap();
        let mut owner = NativeStartupOwner {
            plugin: PluginId("startup-group-error".into()),
            child,
            stderr: Some(stderr),
            link: None,
            signalled: false,
            status: None,
            exit: None,
            thread_failure: None,
        };
        let mut member_command = Command::new("/bin/sh");
        member_command
            .args(["-c", "read value"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(owner.child.id() as i32);
        let mut member = member_command.spawn().unwrap();
        assert!(!child_exited(&owner.child).unwrap());
        // These are platform-policy inputs, not mocked signal operations:
        // unrecognized errors must leave the actual blocked child alive.
        assert!(!prepare_darwin_group_reap(&owner.child, Some(libc::EACCES)));
        assert!(!prepare_darwin_group_reap(&owner.child, None));
        assert!(!child_exited(&owner.child).unwrap());
        owner.signalled = prepare_darwin_group_reap(&owner.child, Some(libc::EPERM));
        assert!(owner.signalled, "the fallback sends a real leader SIGKILL");
        let failure = owner
            .settle()
            .expect_err("killing only the leader cannot settle a live group member");
        assert!(matches!(
            failure,
            HostError::PluginFailed { detail, .. }
                if detail == "native startup process group remains owned"
        ));
        assert!(owner.status.is_some(), "the actual leader was reaped");
        assert!(owner.exit.is_none(), "physical settlement is not published");
        assert!(!child_exited(&member).unwrap());
        member.kill().unwrap();
        member.wait().unwrap();
        let exit = owner.settle().expect("the actual startup owner must settle");
        assert_eq!(exit.signal, Some(libc::SIGKILL));
        assert_eq!(
            exit.failure_kind,
            Some(crikey_plugin_supervisor::FailureKind::Startup)
        );
        assert!(owner.status.is_some());
        assert!(owner.stderr.is_none());
        assert!(wait_group_absent(owner.child.id()));
        assert_eq!(owner.settle().unwrap().signal, exit.signal);
    }
}

#[cfg(windows)]
type StartupJob = Option<OwnedJob>;
#[cfg(not(windows))]
type StartupJob = ();

/// A child that physically exists even though startup did not yield a worker.
/// The same supervisor registration retains it until checked settlement succeeds.
#[derive(Debug)]
pub(crate) struct NativeStartupOwner {
    plugin: PluginId,
    child: Child,
    stderr: Option<JoinHandle<()>>,
    link: Option<Arc<WorkerLink>>,
    #[cfg(windows)]
    job: StartupJob,
    signalled: bool,
    status: Option<std::process::ExitStatus>,
    exit: Option<ExitRecord>,
    thread_failure: Option<&'static str>,
}

impl NativeStartupOwner {
    pub(crate) fn settle(&mut self) -> Result<ExitRecord, HostError> {
        if let Some(exit) = &self.exit {
            return Ok(exit.clone());
        }
        let plugin = &self.plugin;
        let error = |detail: &'static str| HostError::PluginFailed {
            plugin: plugin.clone(),
            detail: detail.into(),
        };
        if let Some(detail) = self.thread_failure {
            return Err(error(detail));
        }
        if let Some(link) = &self.link {
            link.close();
        }
        if self.status.is_none() {
            let _exited =
                child_exited(&self.child).map_err(|_| error("native startup exit observation failed"))?;
            if !self.signalled {
                #[cfg(unix)]
                let signalled = prepare_group_reap(&self.child);
                #[cfg(windows)]
                let signalled = match &self.job {
                    Some(job) => terminate_job_checked(job.0),
                    // A failed containment handoff never resumed the child.
                    None => _exited || self.child.kill().is_ok(),
                };
                #[cfg(not(any(unix, windows)))]
                let signalled = false;
                if !signalled {
                    return Err(error("native startup termination failed"));
                }
                self.signalled = true;
            }
            self.status = wait_child_after_termination(
                &mut self.child,
                Instant::now() + Duration::from_millis(CHILD_REAP_TIMEOUT_MS),
            );
            if self.status.is_none() {
                return Err(error("native startup process reap was not confirmed"));
            }
        }
        #[cfg(unix)]
        if !wait_group_absent(self.child.id()) {
            return Err(error("native startup process group remains owned"));
        }
        let end = Instant::now() + Duration::from_millis(CHILD_REAP_TIMEOUT_MS);
        let pipes = if let Some(link) = &self.link {
            [
                settle_link_thread(&link.reader, end),
                settle_link_thread(&link.writer_thread, end),
            ]
        } else {
            [Ok(()), Ok(())]
        };
        let stderr = settle_owned_thread(&mut self.stderr, end);
        let outcomes = [pipes[0], pipes[1], stderr];
        if outcomes
            .iter()
            .any(|outcome| *outcome == Err("native pipe worker panicked"))
        {
            self.thread_failure = Some("native pipe worker panicked");
        }
        if let Some(detail) = self.thread_failure {
            return Err(error(detail));
        }
        for outcome in outcomes {
            outcome.map_err(error)?;
        }
        let status = self.status.expect("confirmed startup status");
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        let exit = ExitRecord {
            kind: ExitKind::Crashed,
            code: status.code(),
            signal,
            stderr_tail: String::new(),
            failure_kind: Some(crikey_plugin_supervisor::FailureKind::Startup),
        };
        #[cfg(windows)]
        self.job.take();
        self.exit = Some(exit.clone());
        Ok(exit)
    }
}

impl Drop for NativeStartupOwner {
    fn drop(&mut self) {
        let _ = self.settle();
    }
}

fn retain_failed_startup(
    plugin: &PluginId,
    child: Child,
    stderr: Option<JoinHandle<()>>,
    link: Option<Arc<WorkerLink>>,
    _job: StartupJob,
    retained: &mut Option<NativeStartupOwner>,
) {
    let mut owner = NativeStartupOwner {
        plugin: plugin.clone(),
        child,
        stderr,
        link,
        #[cfg(windows)]
        job: _job,
        signalled: false,
        status: None,
        exit: None,
        thread_failure: None,
    };
    let _ = owner.settle();
    *retained = Some(owner);
}
