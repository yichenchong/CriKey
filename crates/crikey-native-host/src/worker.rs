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

use crikey_core::{ActionId, Item, ItemId, PluginId};
use crikey_native_protocol::convert::from_proto_item;
use crikey_native_protocol::frame::{read_frame, write_frame};
use crikey_native_protocol::message::{self, Envelope, Payload};
use crikey_native_protocol::transport::{Listener, Transport};
use crikey_native_protocol::{Capabilities, Endpoint, Message, ProtocolError, PROTOCOL_VERSION};

use crate::launch::{configure_command, LaunchSpec, TransportKind, WorkerOptions};
use crate::stream::{
    error_detail, BatchState, EchoMismatch, ExecuteOutcome, HealthSnapshot, NativeSuggestRequest,
    PluginError, ProtocolObservation, StreamDiagnostics, Suggestions, MAX_LOG_RECORDS, OBSERVATION_CAPACITY,
    READER_QUEUE_CAPACITY, READER_QUEUE_MAX_BYTES,
};

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

/// Identity and capabilities reported by the plugin during startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginHandshake {
    pub plugin_id: String,
    pub plugin_name: String,
    pub plugin_version: String,
    pub sdk_version: String,
    pub protocol_version: u32,
    pub capabilities: Capabilities,
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
    reader: Mutex<Option<JoinHandle<()>>>,
    writer_thread: Mutex<Option<JoinHandle<()>>>,
}

impl WorkerLink {
    fn observe(&self, direction: &'static str, envelope: &Envelope) {
        push_observation(
            &self.observations,
            ProtocolObservation {
                direction,
                kind: envelope_kind(envelope),
                request_id: envelope.request_id,
                connection_id: envelope.connection_id,
                generation: envelope.generation,
            },
        );
    }

    fn observations(&self) -> Vec<ProtocolObservation> {
        lock_unpoisoned(&self.observations).iter().cloned().collect()
    }

    fn echo_mismatch(&self) -> Option<EchoMismatch> {
        lock_unpoisoned(&self.mismatch).clone()
    }

    fn send(&self, envelope: &Envelope) -> Result<(), HostError> {
        let end = Instant::now()
            .checked_add(Duration::from_millis(CONTROL_WRITE_TIMEOUT_MS))
            .unwrap_or_else(Instant::now);
        self.send_until(envelope, end)
    }

    fn send_until(&self, envelope: &Envelope, end: Instant) -> Result<(), HostError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(HostError::Closed);
        }
        let encoded = envelope.encode();
        if encoded.len() > crikey_native_protocol::MAX_FRAME_BYTES {
            self.resource_limit.store(true, Ordering::Release);
            return Err(HostError::ResourceLimit {
                plugin: self.plugin.clone(),
                detail: format!(
                    "host {} envelope is {} bytes (limit {})",
                    envelope_kind(envelope),
                    encoded.len(),
                    crikey_native_protocol::MAX_FRAME_BYTES
                ),
            });
        }
        self.observe("host->plugin", envelope);
        let _guard = lock_unpoisoned(&self.write_mutex);
        match &self.writer {
            WriterKind::Queued { sender, pending } => {
                let mut value = envelope.clone();
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
                            return Err(self.write_error(envelope, ProtocolError::Closed));
                        }
                        Err(TrySendError::Full(next)) => {
                            value = next;
                            if Instant::now() >= end {
                                pending.fetch_sub(1, Ordering::AcqRel);
                                return Err(self.write_error(envelope, ProtocolError::Timeout));
                            }
                            thread::yield_now();
                        }
                    }
                }
            }
        }
    }

    fn write_error(&self, envelope: &Envelope, error: ProtocolError) -> HostError {
        let detail = format!("failed to write {} frame: {error:?}", envelope_kind(envelope));
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
        let receiver = lock_unpoisoned(&self.events);
        match receiver.recv_timeout(timeout) {
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
                Ok(event)
            }
            Err(error) => Err(error),
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
        let _ = self.send(&envelope);
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
        match self.send(&envelope) {
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

    fn join_reader(&self) {
        if let Some(handle) = lock_unpoisoned(&self.reader).take() {
            join_bounded(handle, Duration::from_millis(CHILD_REAP_TIMEOUT_MS));
        }
        if let Some(handle) = lock_unpoisoned(&self.writer_thread).take() {
            join_bounded(handle, Duration::from_millis(CHILD_REAP_TIMEOUT_MS));
        }
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
    options: WorkerOptions,
    handshake: PluginHandshake,
    link: Arc<WorkerLink>,
    child: Option<Child>,
    stderr_tail: Arc<Mutex<StderrTail>>,
    stderr_reader: Option<JoinHandle<()>>,
    exit: Option<ExitRecord>,
    diagnostics: StreamDiagnostics,
    sandbox_report: crikey_sandbox::SandboxReport,
    next_request_id: u64,
    call_succeeded: bool,
    failure_kind: Option<crikey_plugin_supervisor::FailureKind>,
    #[cfg(windows)]
    job: Option<OwnedJob>,
}

impl NativeWorker {
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
    pub fn spawn(spec: LaunchSpec, options: WorkerOptions) -> Result<Self, HostError> {
        let plugin = spec.plugin.clone();
        Self::spawn_inner(spec, options).map_err(|error| match error {
            HostError::Spawn(detail) => HostError::Spawn(format!("plugin `{}`: {detail}", plugin.0)),
            HostError::Handshake(detail) => HostError::Handshake(format!("plugin `{}`: {detail}", plugin.0)),
            other => other,
        })
    }

    fn spawn_inner(spec: LaunchSpec, options: WorkerOptions) -> Result<Self, HostError> {
        if options.limits.initial_credits == 0 {
            return Err(HostError::ResourceLimit {
                plugin: spec.plugin.clone(),
                detail: "initial_credits must be greater than zero".to_owned(),
            });
        }
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
        let mut child = spawn_child(command).map_err(|error| HostError::Spawn(error.to_string()))?;
        // Held as an owner, not a raw handle: every early return below this
        // point drops it, which closes the job and - through
        // `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` - kills descendants that the
        // direct `child.kill()` cannot see.
        #[cfg(windows)]
        let job = match contain_child(&child, &options.limits) {
            Ok(value) => value,
            Err(error) => {
                terminate_and_reap(&mut child);
                return Err(HostError::Spawn(error));
            }
        };
        let stderr = match child.stderr.take() {
            Some(value) => value,
            None => {
                terminate_and_reap(&mut child);
                return Err(HostError::Spawn("child stderr was not piped".to_owned()));
            }
        };
        let stderr_tail = Arc::new(Mutex::new(StderrTail::default()));
        let stderr_reader = match spawn_stderr_drain(stderr, Arc::clone(&stderr_tail)) {
            Ok(handle) => handle,
            Err(error) => {
                terminate_and_reap(&mut child);
                return Err(HostError::Spawn(format!("stderr drain thread: {error}")));
            }
        };
        let credits = Arc::new(AtomicI64::new(0));
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
        let writer_pending = Arc::new(AtomicUsize::new(0));
        let (transport, writer, outgoing, writer_thread) = match options.transport {
            TransportKind::Stdio => {
                let stdin = match child.stdin.take() {
                    Some(value) => value,
                    None => {
                        terminate_and_reap(&mut child);
                        join_bounded(stderr_reader, Duration::from_millis(CHILD_REAP_TIMEOUT_MS));
                        return Err(HostError::Spawn("child stdin was not piped".to_owned()));
                    }
                };
                let stdout = match child.stdout.take() {
                    Some(value) => value,
                    None => {
                        terminate_and_reap(&mut child);
                        join_bounded(stderr_reader, Duration::from_millis(CHILD_REAP_TIMEOUT_MS));
                        return Err(HostError::Spawn("child stdout was not piped".to_owned()));
                    }
                };
                let (sender, receiver) = mpsc::sync_channel(READER_QUEUE_CAPACITY);
                let writer = WriterKind::Queued {
                    sender,
                    pending: Arc::clone(&writer_pending),
                };
                let writer_thread =
                    spawn_stdio_writer(stdin, receiver, Arc::clone(&closed), Arc::clone(&writer_pending));
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
                        terminate_and_reap(&mut child);
                        join_bounded(stderr_reader, Duration::from_millis(CHILD_REAP_TIMEOUT_MS));
                        return Err(HostError::Spawn("native listener was not created".to_owned()));
                    }
                };
                let transport = match listener.accept(Some(Duration::from_millis(options.startup_timeout_ms)))
                {
                    Ok(value) => value,
                    Err(error) => {
                        terminate_and_reap(&mut child);
                        join_bounded(stderr_reader, Duration::from_millis(CHILD_REAP_TIMEOUT_MS));
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
        };
        let reader = if let Some(outgoing_receiver) = outgoing {
            spawn_multiplex_reader(
                transport,
                outgoing_receiver,
                reader_context,
                Arc::clone(&writer_pending),
            )
        } else {
            spawn_blocking_reader(transport, reader_context)
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
            reader: Mutex::new(Some(reader)),
            writer_thread: Mutex::new(writer_thread),
        });
        let handshake = match receive_handshake(&link, &token, &options) {
            Ok(value) => value,
            Err(error) => {
                link.close();
                terminate_and_reap(&mut child);
                join_bounded(stderr_reader, Duration::from_millis(CHILD_REAP_TIMEOUT_MS));
                link.join_reader();
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
            options,
            handshake,
            link,
            child: Some(child),
            stderr_tail,
            stderr_reader: Some(stderr_reader),
            exit: None,
            diagnostics: StreamDiagnostics::default(),
            sandbox_report,
            next_request_id: 1,
            call_succeeded: false,
            failure_kind: None,
            #[cfg(windows)]
            job: Some(job),
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
        self.send_control(&envelope)
    }

    /// Streams and folds the plugin catalog under aggregate bounds.
    pub fn build_catalog(&mut self) -> Result<Vec<Item>, HostError> {
        self.ensure_alive()?;
        let request_id = self.begin_request(0);
        let envelope = Envelope {
            connection_id: self.link.connection_id,
            request_id,
            generation: 0,
            deadline_ms: self.options.call_timeout_ms,
            payload: Some(Payload::CatalogRequest(message::CatalogRequest {
                max_items: self.options.limits.max_catalog_items as u64,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        };
        self.send_control(&envelope)?;
        let end = deadline(self.options.call_timeout_ms);
        let mut items = Vec::new();
        let mut batches = 0usize;
        let mut bytes = 0usize;
        let mut next_sequence = 0_u64;
        loop {
            let event = self.next_event(end, request_id, 0)?;
            let envelope = match event {
                ReaderEvent::Envelope(value) => value,
                ReaderEvent::Failure(failure) => return Err(self.transport_error(failure)),
            };
            match envelope.payload {
                Some(Payload::CatalogBatch(batch)) => {
                    self.validate_batch_sequence(batch.sequence, &mut next_sequence, "catalog")?;
                    if let Some(error) = batch.error.as_ref() {
                        self.validate_nested_error(error, request_id, "catalog")?;
                    }
                    batches = batches.saturating_add(1);
                    let batch_bytes = batch.encode().len();
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
                max_batches: self.options.limits.max_batches_per_query as u64,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        };
        self.send_control(&envelope)?;
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
        loop {
            let event = self.next_event(end, request_id, request.generation)?;
            let envelope = match event {
                ReaderEvent::Envelope(value) => value,
                ReaderEvent::Failure(failure) => return Err(self.transport_error(failure)),
            };
            match envelope.payload {
                Some(Payload::Results(batch)) => {
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
                    let limit_exceeded = batches > self.options.limits.max_batches_per_query
                        || bytes > self.options.limits.max_bytes_per_query;
                    if limit_exceeded && state == 1 {
                        self.truncate(request_id, request.generation)?;
                        let terminal_batches =
                            self.await_terminal(request_id, request.generation, end, &mut next_sequence)?;
                        self.mark_call_succeeded();
                        return Ok(Suggestions {
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
                            self.mark_call_succeeded();
                            return Ok(Suggestions {
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
                    if log_records < MAX_LOG_RECORDS {
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

    /// Executes an action request; plugin failures stay on the `Ok` path.
    pub fn execute(
        &mut self,
        item: &ItemId,
        action: Option<&ActionId>,
        argument: Option<&str>,
    ) -> Result<ExecuteOutcome, HostError> {
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
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        };
        self.send_control(&envelope)?;
        let event = self.next_event(deadline(self.options.call_timeout_ms), request_id, 0)?;
        let envelope = match event {
            ReaderEvent::Envelope(value) => value,
            ReaderEvent::Failure(failure) => return Err(self.transport_error(failure)),
        };
        match envelope.payload {
            Some(Payload::ExecuteResult(result)) => match result.outcome.as_i32() {
                1 => {
                    self.mark_call_succeeded();
                    Ok(ExecuteOutcome::Ok)
                }
                2 => {
                    if let Some(error) = result.error.as_ref() {
                        self.validate_nested_error(error, request_id, "execute")?;
                    }
                    self.mark_call_succeeded();
                    Ok(ExecuteOutcome::Failed(
                        result
                            .error
                            .as_ref()
                            .map(plugin_error)
                            .unwrap_or_else(|| PluginError {
                                message: "plugin execution failed".to_owned(),
                                detail: String::new(),
                            }),
                    ))
                }
                3 => {
                    self.mark_call_succeeded();
                    Ok(ExecuteOutcome::Unsupported)
                }
                _ => Err(self.protocol_failure("unknown execute outcome".to_owned())),
            },
            Some(Payload::Error(error)) => Err(self.error_payload(error, request_id)),
            _ => Err(self.protocol_failure("unexpected execute response".to_owned())),
        }
    }

    /// Performs a health request against the live plugin.
    pub fn health(&mut self) -> Result<HealthSnapshot, HostError> {
        self.ensure_alive()?;
        let request_id = self.begin_request(0);
        self.send_control(&Envelope {
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
    /// `Ok(None)` covers every way a plugin can decline: it does not have the
    /// reference, it answered with an error, it served more than `max_bytes`,
    /// or it said nothing before `timeout`. None of those kills the worker,
    /// and that asymmetry with the query path is deliberate. A resource is
    /// decoration -- an item's icon, in practice -- so taking the process down
    /// over one would cost the user working suggestions to punish a missing
    /// picture. Transport death is still an error: that is not the plugin
    /// declining, it is the channel being gone.
    pub fn request_resource(
        &mut self,
        kind: ResourceKind,
        reference: &str,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<Option<PluginResource>, HostError> {
        self.ensure_alive()?;
        let request_id = self.begin_request(0);
        self.send_control(&Envelope {
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
                return Ok(None);
            }
            let envelope = match self.link.recv(remaining) {
                Ok(ReaderEvent::Envelope(envelope)) => envelope,
                Ok(ReaderEvent::Failure(failure)) => return Err(self.transport_error(failure)),
                Err(RecvTimeoutError::Timeout) => return Ok(None),
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
                        return Ok(None);
                    }
                    return Ok(Some(PluginResource {
                        reference: response.reference,
                        content: response.content,
                        media_type: response.media_type,
                    }));
                }
                Some(Payload::Log(log)) => self.record_log(log.message),
                // An `Error` here is the SDK's answer for a plugin that does
                // not implement resources at all, which is a decline, not a
                // violation.
                Some(Payload::Error(_)) | Some(_) | None => return Ok(None),
            }
        }
    }

    /// Returns a clonable out-of-band cancellation handle.
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle {
            link: Arc::clone(&self.link),
        }
    }

    /// Returns whether the child process is still running.
    pub fn is_alive(&mut self) -> bool {
        if self.exit.is_some() {
            return false;
        }
        let status = match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(status) => status,
                Err(_) => {
                    self.failure_kind = Some(crikey_plugin_supervisor::FailureKind::Crash);
                    self.fail_and_reap(ExitKind::Crashed);
                    return false;
                }
            },
            None => return false,
        };
        match status {
            Some(status) => {
                self.release_job();
                self.join_stderr();
                let kind = self.sticky_exit_kind(ExitKind::from_status(&status));
                let exit = self.exit_record(kind, Some(status));
                self.exit = Some(exit);
                self.link.close();
                false
            }
            None => true,
        }
    }

    /// Returns stream diagnostics.
    pub fn diagnostics(&self) -> StreamDiagnostics {
        let mut diagnostics = self.diagnostics;
        diagnostics.peak_queue_depth = diagnostics
            .peak_queue_depth
            .max(self.link.peak_queue_depth.load(Ordering::Acquire) as u32);
        diagnostics
    }

    /// Hard-stops and reaps the child.
    pub fn kill(&mut self) -> ExitRecord {
        if let Some(exit) = &self.exit {
            return exit.clone();
        }
        self.link.close();
        self.terminate_owned_tree();
        let status = if let Some(child) = self.child.as_mut() {
            wait_child_after_termination(
                child,
                Instant::now() + Duration::from_millis(CHILD_REAP_TIMEOUT_MS),
            )
        } else {
            None
        };
        self.release_job();
        self.join_stderr();
        let exit = self.exit_record(self.sticky_exit_kind(ExitKind::Killed), status);
        self.exit = Some(exit.clone());
        self.link.join_reader();
        exit
    }

    /// Requests orderly shutdown, hard-killing after its bound.
    pub fn shutdown(mut self) -> ExitRecord {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> ExitRecord {
        if let Some(exit) = &self.exit {
            self.link.close();
            self.link.join_reader();
            return exit.clone();
        }
        if !self.is_alive() {
            let exit = self
                .exit
                .clone()
                .unwrap_or_else(|| self.exit_record(ExitKind::Crashed, None));
            self.link.close();
            self.link.join_reader();
            return exit;
        }
        let request_id = self.begin_request(0);
        let _ = self.send_control(&Envelope {
            connection_id: self.link.connection_id,
            request_id,
            generation: 0,
            deadline_ms: self.options.shutdown_timeout_ms,
            payload: Some(Payload::Shutdown(message::Shutdown {
                immediate: false,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        });
        let end = deadline(self.options.shutdown_timeout_ms);
        loop {
            if Instant::now() >= end {
                return self.kill();
            }
            let remaining = end.saturating_duration_since(Instant::now());
            match self.link.recv(remaining) {
                Ok(ReaderEvent::Failure(_))
                | Err(RecvTimeoutError::Disconnected)
                | Err(RecvTimeoutError::Timeout) => break,
                Ok(ReaderEvent::Envelope(_)) => {}
            }
            if !self.is_alive() {
                break;
            }
        }
        let status = if let Some(child) = self.child.as_mut() {
            match wait_child_bounded(child, end) {
                Some(status) => Some(status),
                None => return self.kill(),
            }
        } else {
            None
        };
        let kind = status
            .as_ref()
            .map(ExitKind::from_status)
            .map(|kind| self.sticky_exit_kind(kind))
            .unwrap_or_else(|| self.sticky_exit_kind(ExitKind::Clean));
        self.release_job();
        self.join_stderr();
        let exit = self.exit_record(kind, status);
        self.exit = Some(exit.clone());
        self.link.close();
        self.link.join_reader();
        exit
    }

    fn terminate_owned_tree(&mut self) {
        #[cfg(windows)]
        if let Some(job) = self.job.as_ref() {
            job.terminate();
        }
        if let Some(child) = self.child.as_mut() {
            terminate_child_tree(child);
        }
    }

    #[cfg(windows)]
    fn release_job(&mut self) {
        // Dropping the owner is the close; taking it here puts that close at
        // the point shutdown expects rather than at worker drop.
        drop(self.job.take());
    }

    #[cfg(not(windows))]
    fn release_job(&mut self) {}

    fn send_control(&mut self, envelope: &Envelope) -> Result<(), HostError> {
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
        self.send_control(&Envelope {
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
        if self.exit.is_some() {
            return;
        }
        let kind = self.sticky_exit_kind(kind);
        self.link.close();
        // A closed channel is not evidence that the child exited.  Kill
        // the owned tree before any wait, then bound the reap (spec 24.1).
        self.terminate_owned_tree();
        let status = if let Some(child) = self.child.as_mut() {
            wait_child_after_termination(
                child,
                Instant::now() + Duration::from_millis(CHILD_REAP_TIMEOUT_MS),
            )
        } else {
            None
        };
        self.release_job();
        self.join_stderr();
        self.exit = Some(self.exit_record(kind, status));
        self.link.join_reader();
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
    fn join_stderr(&mut self) {
        if let Some(handle) = self.stderr_reader.take() {
            join_bounded(handle, Duration::from_millis(CHILD_REAP_TIMEOUT_MS));
        }
    }
}

impl Drop for NativeWorker {
    fn drop(&mut self) {
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
) -> JoinHandle<()> {
    thread::spawn(move || loop {
        if closed.load(Ordering::Acquire) {
            drain_pending(&receiver, &pending);
            break;
        }
        match receiver.recv_timeout(Duration::from_millis(SOCKET_POLL_MS)) {
            Ok(envelope) => {
                let result = write_frame(&mut stdin, &envelope.encode());
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

fn spawn_blocking_reader(mut transport: Box<dyn Transport>, context: ReaderContext) -> JoinHandle<()> {
    thread::spawn(move || loop {
        if context.closed.load(Ordering::Acquire) {
            break;
        }
        match transport.recv() {
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
) -> JoinHandle<()> {
    thread::spawn(move || {
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
            match transport.recv() {
                Ok(envelope) => {
                    if !validate_inbound(&envelope, &context) {
                        context.protocol_violation.store(true, Ordering::Release);
                        let _ = enqueue_reader_event(
                            &context,
                            ReaderEvent::Failure(ReaderFailure {
                                error: ProtocolError::Malformed("illegal plugin-to-host envelope".to_owned()),
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

fn enqueue_reader_event(context: &ReaderContext, event: ReaderEvent) -> bool {
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
        return envelope.connection_id == 0 && legal_plugin_payload(envelope);
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
    legal_plugin_payload(envelope)
}

fn legal_plugin_payload(envelope: &Envelope) -> bool {
    matches!(
        envelope.payload,
        Some(Payload::Handshake(_))
            | Some(Payload::Results(_))
            | Some(Payload::CatalogBatch(_))
            | Some(Payload::ExecuteResult(_))
            | Some(Payload::Error(_))
            | Some(Payload::Log(_))
            | Some(Payload::HealthReport(_))
            | Some(Payload::ResourceRequest(_))
            | Some(Payload::ResourceResponse(_))
    )
}

fn accept_credit(envelope: &Envelope, credits: &AtomicI64) -> bool {
    if !matches!(
        envelope.payload,
        Some(Payload::Results(_)) | Some(Payload::CatalogBatch(_))
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
        ],
        host_version: env!("CARGO_PKG_VERSION").to_owned(),
        accepted: rejection.is_none(),
        reject_reason: rejection.clone().unwrap_or_default(),
        max_frame_bytes: crikey_native_protocol::MAX_FRAME_BYTES as u64,
        initial_credits: options.limits.initial_credits,
        unknown: Default::default(),
    };
    link.set_current(envelope.request_id, envelope.generation);
    link.send(&Envelope {
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

fn join_bounded(handle: JoinHandle<()>, timeout: Duration) {
    let end = Instant::now().checked_add(timeout).unwrap_or_else(Instant::now);
    while !handle.is_finished() && Instant::now() < end {
        thread::yield_now();
    }
    if handle.is_finished() {
        let _ = handle.join();
    }
}

fn wait_child_bounded(child: &mut Child, end: Instant) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < end => thread::sleep(Duration::from_millis(5)),
            Ok(None) => return None,
            Err(_) if Instant::now() < end => thread::sleep(Duration::from_millis(5)),
            Err(_) => return None,
        }
    }
}
fn wait_child_after_termination(child: &mut Child, end: Instant) -> Option<std::process::ExitStatus> {
    match wait_child_bounded(child, end) {
        Some(status) => Some(status),
        None => child.wait().ok(),
    }
}

fn terminate_and_reap(child: &mut Child) {
    terminate_child_tree(child);
    let _ = wait_child_after_termination(
        child,
        Instant::now() + Duration::from_millis(CHILD_REAP_TIMEOUT_MS),
    );
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
impl OwnedJob {
    /// Kills every process still in the job, without giving up ownership.
    fn terminate(&self) {
        terminate_job_handle(self.0);
    }
}

#[cfg(windows)]
impl Drop for OwnedJob {
    fn drop(&mut self) {
        close_job_handle(self.0);
    }
}

/// Binds a freshly created, still-suspended child to its job and then lets it
/// run.
///
/// The order is the whole point, so it lives in one function rather than in a
/// sequence of statements a later edit could reorder: the job is created and
/// the process assigned to it while the child is frozen, and only then is the
/// primary thread resumed. A child that cannot be resumed is not left frozen -
/// returning here drops the job owner, whose close fires
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` on the whole tree, and the caller
/// additionally reaps the direct child.
#[cfg(windows)]
fn contain_child(child: &Child, limits: &crate::launch::ResourceLimits) -> Result<OwnedJob, String> {
    let job = create_job_for_child(child, limits)?;
    resume_primary_thread(child.id())?;
    Ok(job)
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
    const LIMIT_PROCESS_MEMORY: u32 = 0x0100;
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
        info.basic.limit_flags |= LIMIT_PROCESS_MEMORY;
        info.process_memory_limit = match usize::try_from(bytes) {
            Ok(value) => value,
            Err(_) => {
                unsafe { CloseHandle(job) };
                return Err("memory limit does not fit this target".to_owned());
            }
        };
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
fn terminate_job_handle(handle: usize) {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn TerminateJobObject(job: *mut c_void, code: u32) -> i32;
    }
    let _ = unsafe { TerminateJobObject(handle as *mut c_void, 1) };
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
fn terminate_child_tree(child: &mut Child) {
    kill_process_group(child.id());
    let _ = child.kill();
}

#[cfg(not(unix))]
fn terminate_child_tree(child: &mut Child) {
    let _ = child.kill();
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn kill_process_group(pid: u32) {
    // `killpg(0)` signals the CALLER's process group - this launcher, its shell
    // and its session. Every caller passes a live `Child::id()`, and each kill
    // precedes its reap so the pid cannot have been recycled, but a misdirected
    // group kill is unrecoverable: refuse the values that could only ever mean
    // "myself" or "init".
    if pid <= 1 {
        return;
    }
    unsafe extern "C" {
        fn killpg(pgrp: i32, sig: i32) -> i32;
    }
    #[allow(unsafe_code)]
    unsafe {
        let _ = killpg(pid as i32, 9);
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
}

impl StderrTail {
    fn push(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if self.bytes.len() == STDERR_TAIL_BYTES {
                let _ = self.bytes.pop_front();
            }
            self.bytes.push_back(*byte);
        }
    }

    fn text(&self) -> String {
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

/// Windows-only because the code under test is Win32 itself: off Windows
/// neither the job object nor `CREATE_SUSPENDED` exists, so no Linux suite can
/// say anything about them. This is where the containment order and the job
/// owner's release are pinned by behaviour rather than by inspection.
#[cfg(all(test, windows))]
mod windows_containment {
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::time::{Duration, Instant};

    use super::{contain_child, CommandExt, CREATE_SUSPENDED};
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
        let job = contain_child(&child, &ResourceLimits::default()).expect("containment must succeed");
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
        let job = contain_child(&child, &ResourceLimits::default()).expect("containment must succeed");
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
