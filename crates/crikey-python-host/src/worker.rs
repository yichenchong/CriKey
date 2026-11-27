//! The modern CPython worker boundary (spec 4.2, 15.6, 15.7; acceptance 31.10).
//!
//! A modern plugin never runs inside CriKey. It runs in a child process started
//! as `<interpreter> -S <sdk_root>/_crikey_modern_worker.py`, and this module is
//! the parent side of that boundary: it spawns the child, hands it the plugin it
//! must load and the assembled import path through the environment, serialises
//! one request at a time over a newline-delimited JSON channel, and — the part
//! that earns the process boundary — survives every way the far side can
//! misbehave.
//!
//! # The channel
//!
//! One JSON object per line, both directions (contract §2). The child's stdout
//! is a strict protocol channel: anything a plugin prints is captured by the
//! shim and travels back inside a reply's `log`, never as a bare line that would
//! desynchronise the stream. The child's stderr is drained continuously on its
//! own thread so a chatty plugin can never deadlock on a full pipe, and is kept
//! only as the bounded tail quoted in [`HostError::Crashed`].
//!
//! # Why the calls take `&mut self`
//!
//! A modern worker serves one request at a time. Taking `&mut self` makes a
//! concurrent second call unrepresentable rather than merely wrong, so per-plugin
//! request serialisation cannot be violated by a caller. [`ModernWorker`] is
//! therefore `Send` (a supervisor owns it off the UI thread) but not `Sync`.
//!
//! # Bounds
//!
//! Every retained buffer has an explicit cap: [`MAX_FRAME_BYTES`] on one line,
//! `MAX_LOG_LINES`/`MAX_LOG_LINE_BYTES` on a reply's log, [`MAX_STDERR_TAIL_BYTES`]
//! on the stderr tail. Every wait has a caller-supplied bound
//! ([`WorkerOptions`]). A plugin cannot make the host allocate without limit or
//! wait forever, and an over-long line is a named protocol failure.

use serde_json::{Map, Value};
use std::collections::{hash_map::Entry, BTreeMap, HashMap};
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crikey_core::command::{
    PluginCommandDeclaration, PluginCommands, COMMANDS_CAPABILITY, MAX_COMMANDS, MAX_COMMAND_BYTES,
    MAX_COMMAND_ID_BYTES, MAX_COMMAND_LABEL_BYTES,
};
use crikey_core::result_presentation::ResultPresentation;
use crikey_core::structured_arguments::{
    ArgumentSchema, ArgumentValues, ExecutableKey, OwnedItem, SubmissionFailureCode, SubmissionOutcome,
};
use crikey_core::structured_view::{
    RouteAdmission, RouteIdentity, StructuredRouteOutcome, TypedExecutionOutcome, ViewExecutionContext,
    ViewReply, ViewRequest, ViewRequestMode, STRUCTURED_VIEW_CAPABILITY,
};
use crikey_core::{Item, PluginId};
use crikey_package_manager::ImportPath;
use crikey_plugin_supervisor::{BudgetKind, OwnedBudgetGuard, PluginBudgetHandle};
use crikey_sandbox::SandboxPolicy;
use std::num::NonZeroU64;

use crate::interpreter::sanitize_python_environment;
use crate::protocol::{
    self, encode_background_admit, encode_background_cancel, encode_background_refuse, encode_build_catalog,
    encode_execute, encode_handshake, encode_set_cancel, encode_shutdown, encode_suggest,
    floor_char_boundary, KIND_BACKGROUND_COMPLETE, KIND_BACKGROUND_REGISTER, KIND_CATALOG_BATCH,
    KIND_EXECUTE_RESULT, KIND_HANDSHAKE_ACK, KIND_RESULT_BATCH, MAX_FRAME_BYTES, MAX_STDERR_TAIL_BYTES,
    PROTOCOL_EXCERPT_BYTES, PROTOCOL_VERSION,
};

static NEXT_WORKER_INCARNATION: AtomicU64 = AtomicU64::new(0);
use crate::Interpreter;

/// The worker module the child interpreter executes.
///
/// Shipped in the SDK directory located by [`crate::sdk_root`], and named here
/// rather than discovered, so a host missing the shim fails at spawn with a
/// message naming the file.
pub const WORKER_ENTRY_FILE: &str = "_crikey_modern_worker.py";

/// The CPython isolation flag the worker is started with.
///
/// `-S` and nothing stronger. `-E`/`-I` both discard `PYTHONPATH`, which is how
/// the host-assembled import path (plugin source, packaged modules, managed
/// deps, SDK) reaches the child — either would unhook it. `-S` skips `site` so a
/// user's `site-packages` cannot shadow imports while leaving the explicit path
/// intact (contract §1).
pub const WORKER_ISOLATION_FLAG: &str = "-S";

/// Carries the plugin id the child answers as.
pub const ENV_PLUGIN_ID: &str = "CRIKEY_MODERN_PLUGIN_ID";

/// Carries the plugin entrypoint, `"package.module:ClassName"`.
pub const ENV_ENTRYPOINT: &str = "CRIKEY_MODERN_ENTRYPOINT";

/// Carries [`PROTOCOL_VERSION`] so a mismatched shim can refuse to speak.
pub const ENV_PROTOCOL_VERSION: &str = "CRIKEY_MODERN_PROTOCOL_VERSION";

/// Carries the SDK directory the child was launched from.
pub const ENV_SDK_DIR: &str = "CRIKEY_MODERN_SDK_DIR";

/// Default bound on the startup handshake when the caller sets none.
const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 10_000;

/// Default per-call bound when the caller sets none (spec 9.6).
const DEFAULT_CALL_TIMEOUT_MS: u64 = 10_000;

/// Default bound on an orderly shutdown before the child is hard-stopped.
const DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = 5_000;

/// Aggregate ceiling on the total items one `suggest`/`build_catalog` may
/// stream across ALL of its frames.
///
/// The per-frame item decode is already bounded by [`MAX_FRAME_BYTES`], but a
/// plugin that streams partial batches without end would grow the host's
/// accumulator without limit. This caps the whole call: exceeding it ends the
/// call as a bounded protocol failure and stops the worker. A hundred thousand
/// items is far past any legitimate query or catalog, so a real plugin never
/// meets it.
const MAX_CALL_ITEMS: usize = 100_000;

/// Aggregate ceiling on the total log bytes one call may accumulate across ALL
/// of its frames.
///
/// One reply's log is bounded by `MAX_LOG_LINES`/`MAX_LOG_LINE_BYTES`, but a
/// plugin streaming many partial batches could log without end. This caps the
/// whole call; the bound is generous enough that a single maximal reply
/// (512 × 4096 bytes) fits many times over.
const MAX_CALL_LOG_BYTES: usize = 8 * 1024 * 1024;

/// Aggregate ceiling on retained log entries in one suggestion call. The byte
/// ceiling alone would still allow millions of tiny `String` allocations.
const MAX_CALL_LOG_LINES: usize = 16 * protocol::MAX_LOG_LINES;

/// Bound on the last step of reaping a child that has already closed stdout.
const REAP_GRACE: Duration = Duration::from_millis(250);

/// Polling interval while waiting for an exiting child's status.
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Bound on letting the stderr drain thread finish after the child is dead, so a
/// crash tail is complete without ever blocking on a grandchild that inherited
/// the pipe.
const STDERR_DRAIN_GRACE: Duration = Duration::from_millis(250);

/// Prevents an untrusted public timeout field from overflowing `Instant`.
const MAX_TIMEOUT_MILLIS: u64 = 365 * 24 * 60 * 60 * 1_000;

fn deadline_after_millis(milliseconds: u64) -> Instant {
    let bounded = milliseconds.min(MAX_TIMEOUT_MILLIS);
    Instant::now()
        .checked_add(Duration::from_millis(bounded))
        .unwrap_or_else(Instant::now)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything that can go wrong between CriKey and a modern worker.
///
/// A plugin *raising* is deliberately NOT in here: that is an `Ok` reply whose
/// outcome carries a [`PluginError`], because the worker is healthy and stays
/// usable (spec 15.7). Conflating the two would make plugin bugs look like
/// transport bugs. `Clone` so a supervisor can retain one while returning
/// another.
#[derive(Debug, Clone, thiserror::Error)]
pub enum HostError {
    /// The runtime's permanent fence refused bounded process/writer insertion.
    #[error("modern runtime admission closed")]
    AdmissionClosed,
    /// A child existed, but its process or owned pipe threads did not settle.
    #[error("modern worker cleanup failed")]
    TeardownFailed,
    /// Startup could not settle its child; ownership travels with the failure.
    #[error("modern worker startup cleanup failed")]
    StartupCleanup { worker: Arc<Mutex<ModernWorker>> },
    /// The child interpreter could not be started at all.
    #[error("the modern worker could not be started: {0}")]
    Spawn(String),

    /// The child said something that is not a frame of this protocol. Carries a
    /// bounded excerpt, never the whole (possibly enormous) offending line.
    #[error("the modern worker sent a line that is not a protocol frame: {0}")]
    Protocol(String),

    /// Declaration callback failed validation; transport and worker remain usable.
    #[error("modern commands callback failed: {0}")]
    CommandsFailed(&'static str),

    /// A call overran the bound the caller supplied (spec 9.6). The child is
    /// hard-stopped before this is returned, so one plugin cannot hang the host.
    #[error("the modern worker did not answer within its bound and was stopped")]
    Timeout,

    /// The interpreter died with a call in flight (spec 24.1, acceptance 31.10).
    /// `detail` is the child's stderr tail, so a crash can be diagnosed.
    #[error("plugin {}'s modern worker crashed: {detail}", plugin.0)]
    Crashed {
        /// The plugin whose worker died.
        plugin: PluginId,
        /// A bounded tail of what the child wrote to stderr before dying.
        detail: String,
    },

    /// No interpreter satisfies the plugin's `requires-python` (spec 14.11).
    #[error("no interpreter satisfies requires-python {required} (found {found})")]
    UnsatisfiedRequiresPython {
        /// The constraint the plugin declared.
        required: String,
        /// The version the best candidate reported.
        found: String,
    },

    /// Interpreter discovery failed for a reason other than version.
    #[error("modern interpreter discovery failed: {0}")]
    Interpreter(String),

    /// The plugin's `build_catalog` raised (pinned decision 2). Unlike a
    /// per-query suggest/execute fault (carried on the `Ok` path as a
    /// [`PluginError`]), a catalog fault is load-time: the provider records the
    /// plugin unavailable rather than surfacing a per-query error. The worker
    /// itself is healthy and STAYS ALIVE. `detail` is the plugin's message and,
    /// when present, its traceback.
    #[error("plugin {}'s build_catalog failed: {detail}", plugin.0)]
    PluginFailed {
        /// The plugin whose catalog build raised.
        plugin: PluginId,
        /// The plugin's message and, when present, its traceback.
        detail: String,
    },
}

/// A Python exception a plugin callback raised.
///
/// Carried on the `Ok` path ([`Suggestions::error`] when the batch is
/// [`BatchState::Failed`], or [`ExecuteOutcome::Failed`]), not by [`HostError`]:
/// the transport worked, the plugin did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginError {
    /// `str(exception)` — the message the plugin raised with.
    pub message: String,
    /// The formatted traceback, naming the frames that raised.
    pub traceback: String,
}

// ---------------------------------------------------------------------------
// Requests and replies
// ---------------------------------------------------------------------------

/// One `suggest` request addressed to a worker (contract §2).
#[derive(Debug, Clone)]
pub struct SuggestRequest {
    /// The query generation this work belongs to, so a stale answer can be
    /// recognised by the caller (the generation rejection lives in the app).
    pub generation: u64,
    /// The user's raw input.
    pub text: String,
    /// The normalised form the pipeline matched on.
    pub normalized: String,
    /// The item the user selected, when arguments are being typed against one.
    pub selected_item_id: Option<String>,
}

/// The terminal state of a suggestion batch (contract §2).
///
/// Only terminal states are representable: a `partial` batch is an internal step
/// the host folds into the accumulated result, never a value a caller sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchState {
    /// The plugin returned normally.
    Final,
    /// The plugin cooperatively returned after seeing cancellation (spec 15.7).
    Cancelled,
    /// The plugin raised; [`Suggestions::error`] carries the exception.
    Failed,
}

/// The result of a `suggest`, folded across all partial and terminal batches.
///
/// `error` is `Some` if and only if `state` is [`BatchState::Failed`]: a
/// `Final` or `Cancelled` batch never carries one.
#[derive(Debug, Clone)]
pub struct Suggestions {
    /// Every item the plugin emitted, in emission order across batches.
    pub items: Vec<Item>,
    /// Sidecar admitted against this exact complete result set.
    pub presentation: ResultPresentation,
    /// How the batch terminated.
    pub state: BatchState,
    /// What the plugin logged/printed while suggesting, bounded per contract §1.
    pub log: Vec<String>,
    /// The plugin's exception, present exactly when `state == Failed`.
    pub error: Option<PluginError>,
}

/// What one `execute` did.
#[derive(Debug, Clone)]
pub enum ExecuteOutcome {
    /// The action completed.
    Ok,
    /// A negotiated action requests a fresh owner-local standard route.
    Navigate(StructuredRouteOutcome),
    /// The plugin raised; the worker is healthy and remains usable.
    Failed(PluginError),
}

/// How a worker's child process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerExit {
    /// The exit status, or `None` when the child was stopped by a signal.
    pub code: Option<i32>,
    /// Whether the host had to kill the child rather than let it exit.
    pub hard_stopped: bool,
}

// ---------------------------------------------------------------------------
// Spawn options
// ---------------------------------------------------------------------------

/// Everything the host decides about a worker before it exists.
///
/// Timeouts are public fields *and* have chainable builders, so a caller can set
/// them either way; every bound is caller-supplied rather than read from a clock
/// or a global.
#[derive(Debug, Clone)]
pub struct WorkerOptions {
    /// The plugin these options are for.
    pub plugin: PluginId,
    /// The entrypoint `"package.module:ClassName"` the child loads.
    pub entrypoint: String,
    /// The assembled import path handed to the child (spec 15.4).
    pub import_path: ImportPath,
    /// Whether the manifest granted `background-execution`.
    ///
    /// Enforced here, in the host, at background admission: a denied worker
    /// refuses every `background_register` its child sends before any budget
    /// slot is taken. The decision is deliberately not exported to the child,
    /// because the shim that would read it is plugin-adjacent code a hostile
    /// plugin can replace, and a gate that only the guarded side applies is
    /// no gate at all.
    ///
    /// Direct `WorkerOptions` callers retain the historical capability for
    /// compatibility; manifest-driven providers explicitly set this value.
    pub allow_background_execution: bool,
    /// Whether the manifest granted `environment`.
    ///
    /// `false`, the default, starts the child from an empty environment plus
    /// the restricted base the host puts back; `true` hands it the ambient
    /// environment as well. The grant has to decide something real, or a
    /// manifest that asks for environment access and one that does not would
    /// produce identical interpreters.
    pub inherit_environment: bool,
    /// The one shared per-plugin budget owner. Background registration refuses
    /// work when this is absent rather than silently bypassing §13.5.
    pub shared_budget: Option<PluginBudgetHandle>,
    /// What the child may write to, and whether it may open TCP sockets.
    ///
    /// A modern plugin is handed no writable directory by the host: its
    /// package is read-only material and it keeps nothing of its own, so the
    /// default is scratch space and the usual device files and nothing else.
    /// Providers narrow or widen this from the manifest.
    pub sandbox: SandboxPolicy,
    /// Bound on the startup handshake, in milliseconds.
    pub startup_timeout_ms: u64,
    /// Bound on every call, in milliseconds (spec 9.6).
    pub call_timeout_ms: u64,
    /// Bound on an orderly shutdown before a hard stop, in milliseconds.
    pub shutdown_timeout_ms: u64,
}

impl WorkerOptions {
    /// Options for `plugin`, loading `entrypoint` with `import_path`.
    pub fn new(plugin: PluginId, entrypoint: impl Into<String>, import_path: ImportPath) -> Self {
        Self {
            plugin,
            entrypoint: entrypoint.into(),
            import_path,
            allow_background_execution: true,
            inherit_environment: false,
            shared_budget: None,
            sandbox: crikey_sandbox::plugin_policy(Vec::<std::path::PathBuf>::new(), false),
            startup_timeout_ms: DEFAULT_STARTUP_TIMEOUT_MS,
            call_timeout_ms: DEFAULT_CALL_TIMEOUT_MS,
            shutdown_timeout_ms: DEFAULT_SHUTDOWN_TIMEOUT_MS,
        }
    }

    /// Sets what the child may write to and whether it may open TCP sockets.
    pub fn with_sandbox(mut self, sandbox: SandboxPolicy) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Bounds the startup handshake.
    pub fn with_startup_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.startup_timeout_ms = timeout_ms;
        self
    }

    /// Bounds every call (spec 9.6). Exceeding it stops the child.
    pub fn with_call_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.call_timeout_ms = timeout_ms;
        self
    }

    /// Bounds an orderly shutdown before a hard stop.
    pub fn with_shutdown_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.shutdown_timeout_ms = timeout_ms;
        self
    }

    /// Attaches the shared per-plugin budget owner used by every dispatch
    /// category, including child-registered background tasks.
    pub fn with_shared_budget(mut self, budget: PluginBudgetHandle) -> Self {
        self.shared_budget = Some(budget);
        self
    }

    /// Enables or disables host-admitted background execution.
    pub fn with_background_execution(mut self, allowed: bool) -> Self {
        self.allow_background_execution = allowed;
        self
    }

    /// Enables or disables inheritance of this process's environment.
    pub fn with_environment_inheritance(mut self, inherited: bool) -> Self {
        self.inherit_environment = inherited;
        self
    }
}

// ---------------------------------------------------------------------------
// The shared link
// ---------------------------------------------------------------------------

/// A serialized request for the dedicated stdin writer.
struct StdinRequest {
    bytes: Vec<u8>,
    completion: SyncSender<Result<(), String>>,
}
impl Drop for StdinRequest {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

struct PipeBytes(Vec<u8>);
impl std::ops::Deref for PipeBytes {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        &self.0
    }
}
impl std::ops::DerefMut for PipeBytes {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        &mut self.0
    }
}
impl Drop for PipeBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}
type CallbackAdmissionFn = dyn Fn(&mut dyn FnMut() -> bool) -> bool + Send + Sync;
struct CallbackAdmission(Arc<CallbackAdmissionFn>);
impl std::fmt::Debug for CallbackAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CallbackAdmission")
    }
}
/// The parts of a worker that outlive the thread that owns it.
/// Frames are handed to a dedicated writer thread. The mutex protects only
/// taking or cloning the channel sender; no child I/O happens while it is held.
#[derive(Debug)]
struct WorkerLink {
    next_id: AtomicU64,
    stdin: Mutex<Option<SyncSender<StdinRequest>>>,
    stdin_thread: Mutex<Option<JoinHandle<()>>>,
    sensitive: AtomicBool,
    callback_admission: Mutex<Option<CallbackAdmission>>,
    cleanup_failed: AtomicBool,
}

fn spawn_stdin_writer(mut stdin: ChildStdin, receiver: Receiver<StdinRequest>) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("crikey-python-stdin".into())
        .spawn(move || {
            for request in receiver {
                let result = stdin
                    .write_all(&request.bytes)
                    .and_then(|()| stdin.flush())
                    .map_err(|error| error.to_string());
                let failed = result.is_err();
                let _ = request.completion.send(result);
                if failed {
                    return;
                }
            }
        })
}

impl WorkerLink {
    fn next_id(&self) -> u64 {
        // Ids are only ever compared for equality with the reply that echoes
        // them, so wrapping after 2^64 frames is not a correctness concern.
        self.next_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }

    fn enqueue_frame(&self, frame: &Value) -> io::Result<Receiver<Result<(), String>>> {
        let mut size = protocol::FrameSize::default();
        serde_json::to_writer(&mut size, frame).map_err(io::Error::other)?;
        size.write_all(b"\n")?;
        let mut encoded = Vec::with_capacity(size.0);
        serde_json::to_writer(&mut encoded, frame).map_err(io::Error::other)?;
        encoded.push(b'\n');
        if matches!(
            frame.get("kind").and_then(Value::as_str),
            Some("set_cancel" | "shutdown" | "background_refuse" | "background_cancel")
        ) {
            self.enqueue_bytes_unfenced(encoded)
        } else {
            self.enqueue_bytes(encoded)
        }
    }

    fn enqueue_bytes(&self, mut encoded: Vec<u8>) -> io::Result<Receiver<Result<(), String>>> {
        let admission = self.callback_admission.lock().unwrap_or_else(|e| e.into_inner());
        let mut result = None;
        let mut send = || {
            let queued = self.enqueue_bytes_unfenced(std::mem::take(&mut encoded));
            let ok = queued.is_ok();
            result = Some(queued);
            ok
        };
        match admission.as_ref() {
            Some(admission) => {
                (admission.0)(&mut send);
            }
            None => {
                send();
            }
        }
        encoded.fill(0);
        result.unwrap_or_else(|| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "runtime admission closed",
            ))
        })
    }

    fn enqueue_bytes_unfenced(&self, mut encoded: Vec<u8>) -> io::Result<Receiver<Result<(), String>>> {
        let (completion_sender, completion_receiver) = mpsc::sync_channel(1);
        let sender = {
            let guard = self.stdin.lock().unwrap_or_else(|error| error.into_inner());
            guard.clone()
        };
        let Some(sender) = sender else {
            encoded.fill(0);
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the modern worker's standard input is already closed",
            ));
        };
        sender
            .try_send(StdinRequest {
                bytes: encoded,
                completion: completion_sender,
            })
            .map_err(|_| {
                io::Error::new(io::ErrorKind::WouldBlock, "the stdin writer queue is unavailable")
            })?;
        Ok(completion_receiver)
    }

    fn write_frame(&self, frame: &Value) -> io::Result<()> {
        match self.enqueue_frame(frame)?.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(io::Error::new(io::ErrorKind::BrokenPipe, error)),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the stdin writer stopped before completing the frame",
            )),
        }
    }

    /// Queues a control frame without waiting for the child to consume it.
    /// Shutdown uses this path so its timeout also covers a child that stopped
    /// reading stdin.
    fn queue_frame(&self, frame: &Value) -> io::Result<()> {
        let completion = self.enqueue_frame(frame)?;
        drop(completion);
        Ok(())
    }

    /// Closes the channel: the writer then drops the child's stdin.
    fn close_stdin(&self) {
        let mut guard = self.stdin.lock().unwrap_or_else(|error| error.into_inner());
        drop(guard.take());
    }

    /// Joins the writer after the child has been stopped, without allowing a
    /// grandchild that inherited stdin to hold teardown forever.
    fn finish_stdin_writer(&self) {
        let mut owner = self
            .stdin_thread
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(handle) = owner.as_ref() else {
            return;
        };
        let deadline = Instant::now() + REAP_GRACE;
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(REAP_POLL_INTERVAL);
        }
        if handle.is_finished() {
            if owner.take().expect("finished writer retained").join().is_err() {
                self.cleanup_failed.store(true, Ordering::Release);
            }
        } else {
            self.cleanup_failed.store(true, Ordering::Release);
        }
    }
}
/// Bounded operator-visible counters for child-registered background work.
///
/// Every counter here is one the host itself raises, so each is an
/// observation and not a claim: `registered` is what the child announced,
/// `admitted`/`refused` are this side's admission decisions, and the three
/// outcome counters are the terminals the child reported for tasks the host
/// admitted. A refusal has no outcome counter: it is already counted as a
/// refusal, and the child's `refused` terminal only acknowledges it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackgroundDiagnostics {
    pub registered: u64,
    pub admitted: u64,
    pub refused: u64,
    pub completed: u64,
    pub cancelled: u64,
    pub failed: u64,
}

#[derive(Debug, Default)]
struct BackgroundCounters {
    registered: u64,
    admitted: u64,
    refused: u64,
    completed: u64,
    cancelled: u64,
    failed: u64,
}

#[derive(Debug)]
struct BackgroundState {
    /// The manifest's `permissions.background-execution` decision, checked
    /// ahead of the budget so a plugin that never asked for the permission
    /// cannot occupy a slot it is not entitled to.
    allowed: bool,
    budget: Option<PluginBudgetHandle>,
    guards: Mutex<HashMap<u64, OwnedBudgetGuard>>,
    counters: Mutex<BackgroundCounters>,
    closed: AtomicBool,
}

impl BackgroundState {
    fn new(budget: Option<PluginBudgetHandle>, allowed: bool) -> Self {
        Self {
            allowed,
            budget,
            guards: Mutex::new(HashMap::new()),
            counters: Mutex::new(BackgroundCounters::default()),
            closed: AtomicBool::new(false),
        }
    }

    fn diagnostics(&self) -> BackgroundDiagnostics {
        let counters = self.counters.lock().unwrap_or_else(|error| error.into_inner());
        BackgroundDiagnostics {
            registered: counters.registered,
            admitted: counters.admitted,
            refused: counters.refused,
            completed: counters.completed,
            cancelled: counters.cancelled,
            failed: counters.failed,
        }
    }

    fn register(&self, task_id: u64, link: &WorkerLink) -> io::Result<()> {
        {
            let mut counters = self.counters.lock().unwrap_or_else(|error| error.into_inner());
            counters.registered = counters.registered.saturating_add(1);
        }
        // The permission is tested first and before any slot is taken: a
        // denial is a property of the manifest, so it holds regardless of how
        // much budget happens to be free, and a refusal that had already
        // acquired a guard would let an unentitled plugin crowd out an
        // entitled sibling for as long as the refusal took to travel back.
        let refused_reason = if !self.allowed {
            Some("the manifest does not grant permissions.background-execution")
        } else if self.closed.load(Ordering::Acquire) {
            Some("background dispatch is shutting down")
        } else if self.budget.is_none() {
            Some("no shared per-plugin background budget was supplied")
        } else {
            None
        };

        if let Some(reason) = refused_reason {
            self.refuse();
            return link.write_frame(&encode_background_refuse(task_id, reason));
        }

        let budget = self.budget.as_ref().expect("checked above");
        let Some(guard) = budget.try_acquire_owned(BudgetKind::Background) else {
            self.refuse();
            return link.write_frame(&encode_background_refuse(
                task_id,
                "max-background-tasks budget is full",
            ));
        };

        let duplicate = {
            let mut guards = self.guards.lock().unwrap_or_else(|error| error.into_inner());
            match guards.entry(task_id) {
                Entry::Vacant(slot) => {
                    slot.insert(guard);
                    false
                }
                Entry::Occupied(_) => {
                    drop(guard);
                    true
                }
            }
        };
        if duplicate {
            self.refuse();
            return link.write_frame(&encode_background_refuse(task_id, "duplicate background task id"));
        }

        {
            let mut counters = self.counters.lock().unwrap_or_else(|error| error.into_inner());
            counters.admitted = counters.admitted.saturating_add(1);
        }
        if let Err(error) = link.write_frame(&encode_background_admit(task_id)) {
            self.guards
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&task_id);
            return Err(error);
        }
        Ok(())
    }

    fn refuse(&self) {
        let mut counters = self.counters.lock().unwrap_or_else(|error| error.into_inner());
        counters.refused = counters.refused.saturating_add(1);
    }

    /// Retires one task the child reports as finished, releasing the budget
    /// slot it held.
    ///
    /// A `refused` terminal adds to no outcome counter: it is the child
    /// acknowledging a refusal this side already counted, and a refused task
    /// never held a slot to release.
    fn complete(&self, task_id: u64, status: &str) {
        if status == "refused" {
            return;
        }
        self.guards
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&task_id);
        let mut counters = self.counters.lock().unwrap_or_else(|error| error.into_inner());
        match status {
            "cancelled" => counters.cancelled = counters.cancelled.saturating_add(1),
            "failed" => counters.failed = counters.failed.saturating_add(1),
            _ => counters.completed = counters.completed.saturating_add(1),
        }
    }

    fn release_all(&self) {
        self.guards
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    fn reap(&self) {
        self.closed.store(true, Ordering::Release);
        self.release_all();
    }
}

/// Raises (or lowers) the cooperative cancellation flag of a live worker.
///
/// `Send + Sync + Clone` because the thread that cancels is never the thread
/// blocked inside a call — that thread is, by definition, waiting for the
/// callback the flag is meant to interrupt (spec 15.7). Each call writes a
/// `set_cancel` control frame; the child's control-reader thread applies it to a
/// `threading.Event` its callback polls through `SuggestContext.cancelled`.
#[derive(Debug, Clone)]
pub struct CancelHandle {
    link: Arc<WorkerLink>,
}

impl CancelHandle {
    /// Asks the plugin to abandon its current work (spec 9.4, 15.7).
    ///
    /// Always writes the control frame (idempotent on the child's `Event`), so
    /// the flag reliably lands even when cancellation is requested the instant a
    /// call begins. A failed write is deliberately ignored: a dead worker is
    /// reported by the call that discovers it, not by the canceller.
    pub fn cancel(&self) {
        let _ = self.link.queue_frame(&encode_set_cancel(true));
    }

    /// Lowers the flag so the next call starts uninterrupted.
    pub fn reset(&self) {
        let _ = self.link.queue_frame(&encode_set_cancel(false));
    }
}

// ---------------------------------------------------------------------------
// Pipe drainage
// ---------------------------------------------------------------------------

/// What the stdout reader thread hands to the thread inside a call.
#[derive(Debug)]
enum StdoutEvent {
    /// One complete protocol line, newline stripped.
    Frame(Arc<Map<String, Value>>, bool),
    /// A line reached [`MAX_FRAME_BYTES`]. The reader stops afterwards: a peer
    /// that emitted it has lost framing, and guessing where the next frame
    /// starts would invent data.
    Oversized { excerpt: String, bytes: usize },
    /// The child emitted bytes that cannot be decoded as UTF-8, or a malformed
    /// lifecycle frame. These are protocol failures, not interpreter crashes.
    Protocol(String),
    /// Reading the pipe itself failed.
    Failed(String),
}

/// Outcome of one bounded line read.
#[derive(Debug)]
enum LineRead {
    Frame,
    Eof,
    Oversized(usize),
}

/// Reads one newline-terminated line into `line`, refusing to grow past
/// [`MAX_FRAME_BYTES`].
///
/// Hand-rolled rather than `BufRead::read_until` because that one has no cap: a
/// peer that never sends a newline would make the host allocate until it dies,
/// which is the defect this function exists to prevent.
fn read_frame_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<LineRead> {
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };

        if available.is_empty() {
            // A final frame without its newline is still a frame; anything else
            // at end of file is nothing at all.
            return Ok(if line.is_empty() {
                LineRead::Eof
            } else {
                LineRead::Frame
            });
        }

        let room = MAX_FRAME_BYTES.saturating_sub(line.len());
        match available.iter().position(|byte| *byte == b'\n') {
            Some(end) => {
                let overflowed = end > room;
                line.extend_from_slice(&available[..end.min(room)]);
                reader.consume(end + 1);
                return Ok(if overflowed {
                    LineRead::Oversized(line.len())
                } else {
                    LineRead::Frame
                });
            }
            None => {
                let taken = available.len();
                let overflowed = taken > room;
                line.extend_from_slice(&available[..taken.min(room)]);
                reader.consume(taken);
                if overflowed {
                    return Ok(LineRead::Oversized(line.len()));
                }
            }
        }
    }
}

fn spawn_stdout_reader(
    stdout: ChildStdout,
    sender: SyncSender<StdoutEvent>,
    link: Arc<WorkerLink>,
    background: Arc<BackgroundState>,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("crikey-python-stdout".into())
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = PipeBytes(Vec::new());
            loop {
                if link.sensitive.load(Ordering::Acquire) {
                    line.fill(0);
                }
                line.clear();
                let event = match read_frame_line(&mut reader, &mut line) {
                    Ok(LineRead::Frame) => {
                        let bytes = strip_carriage_return(&line);
                        match std::str::from_utf8(bytes) {
                            Ok(text) => {
                                let sensitive = link.sensitive.load(Ordering::Acquire);
                                match crate::bounded_json::decode(text, sensitive) {
                                    Some(frame) => {
                                        match handle_background_frame(&frame, &link, &background) {
                                            Ok(true) => continue,
                                            Ok(false) => StdoutEvent::Frame(Arc::new(frame), sensitive),
                                            Err(error) => StdoutEvent::Protocol(error),
                                        }
                                    }
                                    None => {
                                        StdoutEvent::Protocol(if link.sensitive.load(Ordering::Acquire) {
                                            "invalid bounded JSON frame".into()
                                        } else {
                                            bounded_excerpt(text)
                                        })
                                    }
                                }
                            }
                            Err(_) => {
                                StdoutEvent::Protocol("the modern worker emitted invalid UTF-8".to_owned())
                            }
                        }
                    }
                    Ok(LineRead::Eof) => return,
                    Ok(LineRead::Oversized(bytes)) => StdoutEvent::Oversized {
                        excerpt: if link.sensitive.load(Ordering::Acquire) {
                            String::new()
                        } else {
                            excerpt(&line)
                        },
                        bytes,
                    },
                    Err(error) => StdoutEvent::Failed(error.to_string()),
                };

                let fatal = !matches!(event, StdoutEvent::Frame(..));
                if sender.send(event).is_err() || fatal {
                    return;
                }
            }
        })
}

/// Handles lifecycle frames independently of foreground request replies. This
/// is what lets a background completion release its Arc-owned guard while the
/// worker is idle and lets registration receive an admission immediately while
/// a synchronous callback is still running.
fn handle_background_frame(
    frame: &Map<String, Value>,
    link: &WorkerLink,
    background: &BackgroundState,
) -> Result<bool, String> {
    match frame.get("kind").and_then(Value::as_str) {
        Some(KIND_BACKGROUND_REGISTER) => {
            let Some(task_id) = frame.get("task_id").and_then(Value::as_u64) else {
                return Err("background_register missing task_id".to_owned());
            };
            background
                .register(task_id, link)
                .map_err(|error| format!("background admission reply failed: {error}"))?;
            Ok(true)
        }
        Some(KIND_BACKGROUND_COMPLETE) => {
            let Some(task_id) = frame.get("task_id").and_then(Value::as_u64) else {
                return Err("background_complete missing task_id".to_owned());
            };
            let Some(status) = frame.get("status").and_then(Value::as_str) else {
                return Err("background_complete missing status".to_owned());
            };
            if !matches!(status, "ok" | "cancelled" | "failed" | "refused") {
                return Err("background_complete has invalid status".into());
            }
            background.complete(task_id, status);
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn strip_carriage_return(line: &[u8]) -> &[u8] {
    match line.last() {
        Some(b'\r') => &line[..line.len() - 1],
        _ => line,
    }
}

fn excerpt(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line);
    let end = floor_char_boundary(&text, PROTOCOL_EXCERPT_BYTES);
    text[..end].to_owned()
}

/// The last [`MAX_STDERR_TAIL_BYTES`] of a child's stderr.
///
/// Drained continuously on its own thread, so a plugin that writes more than a
/// pipe buffer's worth of output can never block waiting for the host to read
/// it — which, with the host blocked waiting for a reply, would be a deadlock
/// with the plugin on both ends.
#[derive(Debug, Default)]
struct StderrTail {
    lines: std::collections::VecDeque<String>,
    bytes: usize,
    sensitive: bool,
}

impl StderrTail {
    fn push(&mut self, mut line: String) {
        if self.sensitive {
            return;
        }
        const MARKER: &str = "[crikey: stderr line truncated] ";
        let line_limit = MAX_STDERR_TAIL_BYTES.saturating_sub(1);
        if line.len() > line_limit {
            let suffix_limit = line_limit.saturating_sub(MARKER.len());
            let mut start = line.len().saturating_sub(suffix_limit);
            while start < line.len() && !line.is_char_boundary(start) {
                start += 1;
            }
            line = format!("{MARKER}{}", &line[start..]);
        }

        self.bytes = self.bytes.saturating_add(line.len() + 1);
        self.lines.push_back(line);
        while self.bytes > MAX_STDERR_TAIL_BYTES {
            match self.lines.pop_front() {
                Some(dropped) => self.bytes = self.bytes.saturating_sub(dropped.len() + 1),
                None => {
                    self.bytes = 0;
                    break;
                }
            }
        }
    }

    fn render(&self) -> String {
        if self.sensitive {
            return String::new();
        }
        let mut rendered = String::with_capacity(self.bytes);
        for line in &self.lines {
            if !rendered.is_empty() {
                rendered.push('\n');
            }
            rendered.push_str(line);
        }
        rendered
    }
}

/// The variables a stripped modern-Python child still needs.
///
/// Deliberately the same shape as the native host's restricted base, and for
/// the same reason: an interpreter started from a genuinely empty environment
/// cannot find its own shared library on Windows, cannot resolve a home
/// directory for `expanduser`, and writes temporary files wherever the
/// platform default happens to point. Everything else — tokens, proxies,
/// session state, the user's own configuration — stays with the host unless
/// `permissions.environment` says otherwise.
fn add_restricted_environment(command: &mut Command) {
    #[cfg(unix)]
    const KEYS: [&str; 5] = ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"];
    #[cfg(windows)]
    const KEYS: [&str; 6] = ["PATH", "SystemRoot", "SystemDrive", "TEMP", "TMP", "USERPROFILE"];
    #[cfg(not(any(unix, windows)))]
    const KEYS: [&str; 1] = ["PATH"];

    for key in KEYS {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

fn spawn_stderr_drain(stderr: ChildStderr, tail: Arc<Mutex<StderrTail>>) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("crikey-python-stderr".into())
        .spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = PipeBytes(Vec::new());
            loop {
                let available = match reader.fill_buf() {
                    Ok([]) => {
                        let mut tail = tail.lock().unwrap_or_else(|error| error.into_inner());
                        if !tail.sensitive && !line.is_empty() {
                            tail.push(String::from_utf8_lossy(strip_carriage_return(&line)).into_owned());
                        }
                        return;
                    }
                    Err(_) => return,
                    Ok(bytes) => bytes,
                };
                let count = available
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(available.len(), |n| n + 1);
                {
                    let mut tail = tail.lock().unwrap_or_else(|error| error.into_inner());
                    if tail.sensitive {
                        line.fill(0);
                        line.clear();
                    } else {
                        let room = MAX_FRAME_BYTES.saturating_sub(line.len());
                        line.extend_from_slice(&available[..count.min(room)]);
                        if available[count - 1] == b'\n' {
                            if line.last() == Some(&b'\n') {
                                line.pop();
                            }
                            tail.push(String::from_utf8_lossy(strip_carriage_return(&line)).into_owned());
                            line.clear();
                        }
                    }
                }
                reader.consume(count);
            }
        })
}

// ---------------------------------------------------------------------------
// The worker
// ---------------------------------------------------------------------------

/// A live modern plugin, running in its own operating-system process.
///
/// `Send` so a supervisor can own one away from the user-interface thread
/// (spec 4.2). Not `Sync`, and the calls take `&mut self`: requests are
/// serialised per worker by construction. `Drop` reaps the child (its whole
/// process group on Unix). An interpreter crash is [`HostError::Crashed`] and
/// the host STAYS ALIVE (acceptance 31.10).
#[derive(Debug)]
pub struct ModernWorker {
    plugin: PluginId,
    options: WorkerOptions,
    link: Arc<WorkerLink>,
    frames: Receiver<StdoutEvent>,
    background: Arc<BackgroundState>,
    stderr: Arc<Mutex<StderrTail>>,
    stderr_thread: Option<JoinHandle<()>>,
    stdout_thread: Option<JoinHandle<()>>,
    cleanup_failed: bool,
    child: Option<Child>,
    #[cfg(unix)]
    process_id: u32,
    tree_settled: bool,
    #[cfg(windows)]
    job: Option<crikey_native_host::ContainedChildJob>,
    incarnation: u64,
    reaped: Option<ExitStatus>,
    hard_stopped: bool,
    preview_supported: bool,
    commands_supported: bool,
    typed_supported: bool,
    presentation_supported: bool,
    structured_supported: bool,
    active_view: Option<ViewReply>,
    presentation: ResultPresentation,
    catalog_schemas: BTreeMap<ExecutableKey, Arc<ArgumentSchema>>,
    query_schemas: BTreeMap<ExecutableKey, Arc<ArgumentSchema>>,
    alive: bool,
}

impl ModernWorker {
    /// Starts a child interpreter for `options.plugin` and completes its
    /// handshake.
    ///
    /// Returns only once the child has answered the handshake, so a caller
    /// holding a `ModernWorker` holds a process that is running the shim. A child
    /// that fails to announce itself is reaped before the error is returned; no
    /// path out of this function leaks a process.
    pub fn spawn(interpreter: &Interpreter, options: WorkerOptions) -> Result<Self, HostError> {
        Self::spawn_with_admission(interpreter, options, |send| send())
    }

    /// Serializes process creation and all callback writer insertions with closure.
    pub fn spawn_with_admission(
        interpreter: &Interpreter,
        options: WorkerOptions,
        admission: impl Fn(&mut dyn FnMut() -> bool) -> bool + Send + Sync + 'static,
    ) -> Result<Self, HostError> {
        let admission = CallbackAdmission(Arc::new(admission));
        let incarnation = NEXT_WORKER_INCARNATION
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| value.checked_add(1))
            .map_err(|_| HostError::Spawn("worker incarnation counter exhausted".into()))?
            .checked_add(1)
            .ok_or_else(|| HostError::Spawn("worker incarnation counter exhausted".into()))?;
        let sdk = crate::sdk_root();
        let entry = sdk.join(WORKER_ENTRY_FILE);
        if !entry.is_file() {
            return Err(HostError::Spawn(format!(
                "the modern worker entry {WORKER_ENTRY_FILE} is missing from {}",
                sdk.display()
            )));
        }
        let pythonpath = options
            .import_path
            .to_pythonpath()
            .map_err(|error| HostError::Spawn(format!("the plugin import path is invalid: {error}")))?;

        let mut command = Command::new(interpreter.path());
        #[cfg(windows)]
        crikey_native_host::ContainedChildJob::prepare(&mut command);
        if options.inherit_environment {
            // The manifest bought the ambient environment. `PYTHON*` is still
            // the host's to own: those variables select an interpreter's
            // import path and byte-compilation behaviour, which is worker
            // configuration and not a plugin capability.
            sanitize_python_environment(&mut command);
        } else {
            command.env_clear();
            add_restricted_environment(&mut command);
        }
        command
            .arg(WORKER_ISOLATION_FLAG)
            .arg(&entry)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The child reaches its plugin, managed deps and the SDK through
            // `PYTHONPATH` alone, which is why the isolation flag can only be
            // `-S`. Global site-packages is never on it (spec 15.4).
            .env("PYTHONPATH", pythonpath)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            // Unbuffered, because a buffered reply frame is one the host waits
            // for until a deadline it never should have reached.
            .env("PYTHONUNBUFFERED", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .env(ENV_PROTOCOL_VERSION, PROTOCOL_VERSION.to_string())
            .env(ENV_PLUGIN_ID, &options.plugin.0)
            .env(ENV_ENTRYPOINT, &options.entrypoint)
            .env(ENV_SDK_DIR, &sdk);

        // Own process group so a hard stop can signal the whole subtree, not
        // just the leader: a plugin that forks must never leave grandchildren
        // running with nobody listening (spec 24.3). Unix only — `std::process`
        // exposes no portable equivalent.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        // Confinement is installed last so it covers the finished command, and
        // it is prepared here rather than in the caller so that the descriptor
        // the child restricts itself with is built before the fork. An
        // unavailable sandbox is not a spawn failure: the report says what the
        // child is actually subject to (spec 20.2).
        let sandbox = options.sandbox.prepare();
        sandbox.install(&mut command);

        let mut spawned = None;
        (admission.0)(&mut || {
            let result = command.spawn();
            let ok = result.is_ok();
            spawned = Some(result);
            ok
        });
        let mut child = spawned
            .ok_or(HostError::AdmissionClosed)?
            .map_err(|error| HostError::Spawn(format!("the process could not be started: {error}")))?;
        #[cfg(unix)]
        let process_id = child.id();

        // Piped by construction immediately above, so these are `Some`.
        let stdin = child.stdin.take().expect("worker stdin is piped");
        let stdout = child.stdout.take().expect("worker stdout is piped");
        let stderr = child.stderr.take().expect("worker stderr is piped");

        // Projected frames are larger than their JSON source. Keep only one
        // prefetched foreground frame; pipe backpressure bounds decoded memory.
        let (sender, frames) = mpsc::sync_channel(1);
        let (stdin_sender, stdin_requests) = mpsc::sync_channel(128);
        let link = Arc::new(WorkerLink {
            next_id: AtomicU64::new(0),
            stdin: Mutex::new(Some(stdin_sender)),
            stdin_thread: Mutex::new(None),
            sensitive: AtomicBool::new(false),
            callback_admission: Mutex::new(Some(admission)),
            cleanup_failed: AtomicBool::new(false),
        });
        let background = Arc::new(BackgroundState::new(
            options.shared_budget.clone(),
            options.allow_background_execution,
        ));
        let tail = Arc::new(Mutex::new(StderrTail::default()));

        let mut worker = Self {
            plugin: options.plugin.clone(),
            options,
            link,
            frames,
            background,
            stderr: tail,
            stderr_thread: None,
            child: Some(child),
            stdout_thread: None,
            cleanup_failed: false,
            #[cfg(unix)]
            process_id,
            tree_settled: false,
            #[cfg(windows)]
            job: None,
            incarnation,
            reaped: None,
            hard_stopped: false,
            preview_supported: false,
            commands_supported: false,
            typed_supported: false,
            presentation_supported: false,
            structured_supported: false,
            active_view: None,
            presentation: ResultPresentation::default(),
            catalog_schemas: BTreeMap::new(),
            query_schemas: BTreeMap::new(),
            alive: true,
        };
        let writer = match spawn_stdin_writer(stdin, stdin_requests) {
            Ok(thread) => thread,
            Err(_) => {
                return Err(worker.failed_startup(HostError::Spawn("stdin thread could not start".into())))
            }
        };
        *worker.link.stdin_thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(writer);
        worker.stdout_thread = match spawn_stdout_reader(
            stdout,
            sender,
            Arc::clone(&worker.link),
            Arc::clone(&worker.background),
        ) {
            Ok(thread) => Some(thread),
            Err(_) => {
                return Err(worker.failed_startup(HostError::Spawn("stdout thread could not start".into())))
            }
        };
        worker.stderr_thread = match spawn_stderr_drain(stderr, Arc::clone(&worker.stderr)) {
            Ok(thread) => Some(thread),
            Err(_) => {
                return Err(worker.failed_startup(HostError::Spawn("stderr thread could not start".into())))
            }
        };

        #[cfg(windows)]
        match crikey_native_host::ContainedChildJob::assign_suspended(
            worker.child.as_ref().expect("new suspended child"),
            &crikey_native_host::ResourceLimits::default(),
        ) {
            Ok(job) => worker.job = Some(job),
            Err(_) => {
                return Err(worker.failed_startup(HostError::Spawn("worker containment failed".into())));
            }
        }
        #[cfg(windows)]
        {
            let resumed = worker
                .job
                .as_ref()
                .expect("assigned job retained")
                .resume(worker.child.as_ref().expect("suspended child retained"));
            if resumed.is_err() {
                return Err(worker.failed_startup(HostError::Spawn("worker could not resume".into())));
            }
        }
        if let Err(error) = worker.handshake() {
            return Err(worker.failed_startup(error));
        }

        Ok(worker)
    }

    fn failed_startup(mut self, error: HostError) -> HostError {
        if self.shutdown_checked().is_err() {
            HostError::StartupCleanup {
                worker: Arc::new(Mutex::new(self)),
            }
        } else {
            error
        }
    }

    /// Whether the worker is still serving.
    ///
    /// A crash, protocol failure or timeout observed by a call marks the worker
    /// not alive. This cached accessor does not probe the process; a background
    /// supervisor uses [`Self::poll_alive`] to observe idle exits.
    pub fn is_alive(&self) -> bool {
        self.alive
    }

    /// Nonblocking child-status probe for a background supervisor.
    ///
    /// Unlike `is_alive`, this observes an idle process exit. It only marks the
    /// worker dead and saves its status: reader/writer cleanup remains with
    /// shutdown or drop, so callers can publish retirement before cleanup.
    pub fn poll_alive(&mut self) -> bool {
        if self.alive {
            match self.child.as_ref().map(child_exited) {
                Some(Ok(false)) => {}
                Some(Ok(true)) | Some(Err(_)) | None => self.alive = false,
            }
        }
        self.alive
    }

    /// Process-lifetime identity, never reused even if the operating system reuses a PID.
    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    /// Returns bounded lifecycle counters for host-managed background work.
    pub fn background_diagnostics(&self) -> BackgroundDiagnostics {
        self.background.diagnostics()
    }

    /// Cancels all admitted background tasks and releases their host guards.
    ///
    /// The child receives the cancellation request as a control frame; the
    /// host drops every guard immediately because cancellation is a terminal
    /// dispatch path even if the child is about to crash or is unresponsive.
    pub fn cancel_background_tasks(&self) {
        let _ = self.link.write_frame(&encode_background_cancel(None));
        self.background.release_all();
    }

    /// A handle that cancels this worker's in-flight call from another thread.
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle {
            link: Arc::clone(&self.link),
        }
    }

    pub fn supports_structured_views(&self) -> bool {
        self.structured_supported
    }

    pub fn retire_view(&mut self) {
        self.active_view = None;
    }

    /// One bounded Open/Refresh operation on the existing supervised worker.
    pub fn structured_view(&mut self, request: &ViewRequest) -> Result<ViewReply, HostError> {
        self.structured_view_with_admission(request, |send| send())
    }

    /// Revalidate the sole endpoint authority at the existing writer enqueue.
    pub fn structured_view_with_admission(
        &mut self,
        request: &ViewRequest,
        admission: impl FnOnce(&mut dyn FnMut() -> bool) -> bool,
    ) -> Result<ViewReply, HostError> {
        if !self.structured_supported {
            return Err(HostError::Protocol("structured view unsupported".into()));
        }
        if !self.alive {
            return Err(self.crashed());
        }
        if request.mode == ViewRequestMode::Refresh
            && !self.active_view.as_ref().is_some_and(|v| {
                v.context.route == request.route
                    && v.context.session == request.session
                    && Some(v.context.revision) == request.previous_revision
            })
        {
            return Err(HostError::Protocol("structured view unavailable".into()));
        }
        let id = self
            .link
            .next_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .ok()
            .and_then(|n| n.checked_add(1))
            .and_then(NonZeroU64::new)
            .ok_or_else(|| HostError::Protocol("view request counter exhausted".into()))?;
        let outgoing = crate::views::request(id.get(), request, &self.plugin)
            .ok_or_else(|| HostError::Protocol("invalid structured view request".into()))?;
        self.active_view = None;
        let deadline = deadline_after_millis(self.options.call_timeout_ms);
        let mut queued = None;
        let admitted = admission(&mut || {
            let result = self.link.enqueue_frame(&outgoing);
            let ok = result.is_ok();
            queued = Some(result);
            ok
        });
        if !admitted && queued.is_none() {
            return Err(HostError::Protocol("structured view unavailable".into()));
        }
        let completion = match queued {
            Some(result) => self.checked_callback_io(result)?,
            None => return Err(HostError::Protocol("callback admission unavailable".into())),
        };
        match completion.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(())) => {}
            Err(RecvTimeoutError::Timeout) => {
                self.fail_and_reap();
                return Err(HostError::Timeout);
            }
            _ => {
                self.fail_and_reap();
                return Err(self.crashed());
            }
        }
        let frame = self.recv_frame_until(deadline)?;
        let frame = self.expect_frame(&frame, id.get(), "structured_view_result")?;
        if frame.get("route").and_then(Value::as_str) != Some(request.route.0.as_str())
            || frame.get("session") != outgoing.get("session")
        {
            return Err(self.protocol_error("invalid view correlation"));
        }
        if frame.get("status").and_then(Value::as_str) != Some("content") {
            return Err(HostError::Protocol(
                match frame.get("code").and_then(Value::as_str) {
                    Some("unsupported") => "structured view unsupported",
                    Some("cancelled") => "structured view cancelled",
                    Some("unavailable") => "structured view unavailable",
                    _ => "structured view failed",
                }
                .into(),
            ));
        }
        let descriptor = match frame
            .get("descriptor")
            .and_then(|v| crate::views::descriptor(&self.plugin, v))
        {
            Some(crate::views::DescriptorDecode::Content(descriptor)) => descriptor,
            Some(crate::views::DescriptorDecode::Rejected) => {
                return Err(HostError::Protocol("structured view content rejected".into()))
            }
            None => return Err(self.protocol_error("invalid view descriptor")),
        };
        let context = ViewExecutionContext {
            route: request.route.clone(),
            session: request.session,
            revision: id,
        };
        let admission = RouteAdmission::worker(
            RouteIdentity {
                owner: self.plugin.clone(),
                route: request.route.clone(),
                incarnation: NonZeroU64::new(self.incarnation).expect("physical incarnation"),
            },
            context.surface(),
        )
        .map_err(|_| self.protocol_error("invalid view admission"))?;
        descriptor
            .validate(&admission)
            .map_err(|_| HostError::Protocol("structured view content rejected".into()))?;
        let reply = ViewReply {
            context,
            descriptor: Arc::new(descriptor),
        };
        self.active_view = Some(reply.clone());
        Ok(reply)
    }

    fn view_executable(
        &self,
        context: &ViewExecutionContext,
        item: &Item,
        action: Option<&str>,
    ) -> Option<Option<&Arc<ArgumentSchema>>> {
        if !self.structured_supported || !self.alive {
            return None;
        }
        let view = self.active_view.as_ref()?;
        if &view.context != context || item.plugin_id != self.plugin {
            return None;
        }
        let definition = view
            .descriptor
            .definitions
            .iter()
            .find(|d| d.item.stable_id == item.stable_id && d.item.plugin_id == item.plugin_id)?;
        // Compare complete frozen definitions without allocating a JSON copy.
        if !crate::views::same_item(&definition.item, item) {
            return None;
        }
        let presentation = match action {
            Some(id) => {
                let action = definition.item.actions.iter().find(|a| a.action_id.0 == id)?;
                definition.presentation(Some(&action.action_id))?
            }
            None if definition.item.actions.is_empty() && definition.actionless_policy.is_some() => {
                definition.presentation(None)?
            }
            None => return None,
        };
        Some(presentation.schema.as_ref())
    }
    /// Optional capability negotiated independently of ordinary suggestions.
    pub fn supports_preview(&self) -> bool {
        self.preview_supported
    }

    /// Blocking worker operation. Call only from a supervised background thread.
    /// Identity is opaque to the plugin and must be echoed without modification.
    /// Cancellation is latched: the supervisor resets `cancel_handle()` before
    /// installing its cancellation watcher, avoiding a lost cancel at admission.
    pub fn preview(&mut self, item: &Item, identity: &Value) -> Result<Value, HostError> {
        if !self.preview_supported {
            return Ok(serde_json::json!({"status":"unsupported","identity":identity}));
        }
        if !self.alive {
            return Err(self.crashed());
        }
        let id = self
            .link
            .next_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| value.checked_add(1))
            .map_err(|_| HostError::Protocol("preview request counter exhausted".into()))?
            .checked_add(1)
            .ok_or_else(|| HostError::Protocol("preview request counter exhausted".into()))?;
        let request = serde_json::json!({
            "id":id, "kind":"preview", "identity":identity,
            "item":protocol::encode_item(item),
        });
        let sent = self.link.write_frame(&request);
        self.checked_callback_io(sent)?;
        let line = self.recv_frame(self.options.call_timeout_ms)?;
        let frame = self.expect_frame(&line, id, "preview_result")?;
        if frame.get("identity") != Some(identity)
            || protocol::decode_log(&frame).is_none()
            || !matches!(
                frame.get("status").and_then(Value::as_str),
                Some("content" | "empty" | "unsupported" | "cancelled" | "failed")
            )
        {
            return Err(self.protocol_error(&line));
        }
        drop(line);
        Ok(Value::Object(
            Arc::try_unwrap(frame).expect("single foreground frame owner"),
        ))
    }

    /// Explicit SDK override negotiated by this worker, not inferred from items.
    pub fn supports_commands(&self) -> bool {
        self.commands_supported
    }

    /// Fetch declarations on the catalog/suggestion worker, never a new child.
    ///
    /// Call on the existing supervised background path with its action budget.
    /// Reset the cancel handle before installing its watcher (as for preview).
    /// Capture `incarnation()` at admission and recheck the provider's source
    /// at completion before publishing. Healthy idle release after publication
    /// does not revoke installed catalog declarations.
    pub fn commands(&mut self) -> Result<PluginCommands, HostError> {
        if !self.commands_supported {
            return Ok(PluginCommands::Unsupported);
        }
        if !self.alive {
            return Err(self.crashed());
        }
        let incarnation = self.incarnation;
        let id = self
            .link
            .next_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| value.checked_add(1))
            .map_err(|_| HostError::Protocol("commands request counter exhausted".into()))?
            .checked_add(1)
            .ok_or_else(|| HostError::Protocol("commands request counter exhausted".into()))?;
        let sent = self
            .link
            .write_frame(&serde_json::json!({"id":id, "kind":"commands"}));
        self.checked_callback_io(sent)?;
        let line = self.recv_frame(self.options.call_timeout_ms)?;
        let frame = self.expect_frame(&line, id, "commands_result")?;
        if protocol::decode_log(&frame).is_none() {
            return Err(self.protocol_error(&line));
        }
        if self.incarnation != incarnation || !self.poll_alive() {
            return Err(self.crashed());
        }
        match frame.get("status").and_then(Value::as_str) {
            Some("unsupported") => Ok(PluginCommands::Unsupported),
            Some("cancelled") => Ok(PluginCommands::Cancelled),
            Some("failed") => Err(HostError::CommandsFailed("invalid or failed commands callback")),
            Some("content") => {
                let declarations = decode_commands(frame.get("declarations")).ok_or(
                    HostError::CommandsFailed("invalid or oversized command declarations"),
                )?;
                Ok(PluginCommands::Declarations(declarations))
            }
            _ => Err(self.protocol_error(&line)),
        }
    }

    /// Delivers the latest complete configuration state to the plugin (spec 21.4).
    ///
    /// Queued rather than written synchronously: the child may be inside a
    /// callback and not reading stdin, and a host that blocked here would let a
    /// slow plugin stall the thread publishing configuration to every OTHER
    /// plugin. The frame carries no `id` and expects no reply, so there is
    /// nothing to correlate; the child applies it on its main loop, in order,
    /// after whatever call is in flight.
    ///
    /// `complete` says whether `values` is the whole state rather than a delta.
    /// The host only ever publishes whole states — that IS the coalescing rule of
    /// spec 21.4 — so a caller passing `false` is describing a delta path that
    /// does not exist yet.
    ///
    /// A worker that has already died is an error rather than a silent no-op: the
    /// caller is publishing configuration and needs to know the plugin did not
    /// get it.
    pub fn send_configuration(
        &mut self,
        values: &BTreeMap<String, String>,
        complete: bool,
    ) -> Result<(), HostError> {
        if !self.alive {
            return Err(self.crashed());
        }
        let sent = self
            .link
            .queue_frame(&protocol::encode_configuration(values, complete));
        self.checked_callback_io(sent)?;
        Ok(())
    }

    /// Runs the plugin's `build_catalog`, folding its catalog batches.
    pub fn build_catalog(&mut self) -> Result<Vec<Item>, HostError> {
        self.build_catalog_with_timeout_ms(self.options.call_timeout_ms)
    }

    /// Runs the plugin's `build_catalog` with a budget for this whole call.
    ///
    /// The worker's configured timeout is unchanged, so catalog work on a
    /// query worker can retain its own budget without extending later queries.
    pub fn build_catalog_with_timeout_ms(&mut self, timeout_ms: u64) -> Result<Vec<Item>, HostError> {
        if !self.alive {
            return Err(self.crashed());
        }
        self.catalog_schemas.clear();

        let id = self.link.next_id();
        let sent = self.link.write_frame(&encode_build_catalog(id));
        self.checked_callback_io(sent)?;

        // One deadline for the WHOLE call: every frame is read against the same
        // budget, decremented as time passes, so a plugin that streams partial
        // catalog batches forever is bounded rather than resetting the clock
        // with each frame.
        let deadline = deadline_after_millis(timeout_ms);
        let mut items = Vec::new();
        let mut log_bytes = 0usize;
        let mut presentation = ResultPresentation::default();
        let mut presentation_bytes = 0;
        self.presentation = ResultPresentation::default();
        loop {
            let line = self.recv_frame_until(deadline)?;
            let frame = self.expect_frame(&line, id, KIND_CATALOG_BATCH)?;
            if crate::typed::append(
                &self.plugin,
                &frame,
                &mut presentation,
                self.typed_supported,
                self.presentation_supported,
                &mut presentation_bytes,
            )
            .is_none()
            {
                return Err(self.protocol_error("invalid sidecar"));
            }

            let Some(done) = frame.get("done").and_then(Value::as_bool) else {
                return Err(self.protocol_error(&line));
            };
            if protocol::decode_log(&frame).is_none() {
                return Err(self.protocol_error(&line));
            }

            // A terminal frame carrying an `error` object is a load-time catalog
            // fault (pinned decision 2): the plugin raised in `build_catalog`.
            // The worker itself is healthy and stays alive; the provider records
            // the plugin unavailable.
            if frame_has_error(&frame) {
                let Some(error) = decode_plugin_error(&frame) else {
                    return Err(self.protocol_error(&line));
                };
                if !done {
                    return Err(self.protocol_error(&line));
                }
                return Err(HostError::PluginFailed {
                    plugin: self.plugin.clone(),
                    detail: join_detail(error),
                });
            }

            let Some(mut batch) = protocol::decode_items(&self.plugin, &frame) else {
                return Err(self.protocol_error(&line));
            };
            items.append(&mut batch);

            if items.len() > MAX_CALL_ITEMS {
                return Err(self.overflow(format!(
                    "build_catalog streamed more than {MAX_CALL_ITEMS} items and was stopped"
                )));
            }

            log_bytes = log_bytes.saturating_add(log_byte_len(&frame));
            if log_bytes > MAX_CALL_LOG_BYTES {
                return Err(self.overflow(format!(
                    "build_catalog streamed more than {MAX_CALL_LOG_BYTES} log bytes and was stopped"
                )));
            }

            if done {
                if crate::typed::validate(&self.plugin, &presentation, &items).is_none() {
                    return Err(self.protocol_error("invalid sidecar"));
                }
                self.record_schemas(&presentation, true)?;
                self.presentation = presentation;
                return Ok(items);
            }
        }
    }

    /// Runs the plugin's `suggest`, folding its zero-or-more partial batches and
    /// the single terminal one into an accumulated [`Suggestions`].
    ///
    /// A plugin that raises or cooperatively cancels is the `Ok` path: the
    /// transport is healthy. Only a transport fault is an `Err`.
    pub fn suggest(&mut self, request: &SuggestRequest) -> Result<Suggestions, HostError> {
        self.suggest_inner(request, true, None)
    }

    /// Runs a request against a cancellation flag deliberately latched by the
    /// caller. The next ordinary [`Self::suggest`] clears that flag.
    pub fn suggest_with_cancel_latched(
        &mut self,
        request: &SuggestRequest,
    ) -> Result<Suggestions, HostError> {
        self.suggest_inner(request, false, None)
    }

    pub fn supports_typed_arguments(&self) -> bool {
        self.typed_supported
    }
    pub fn supports_result_presentation(&self) -> bool {
        self.presentation_supported
    }
    pub fn presentation(&self) -> &ResultPresentation {
        &self.presentation
    }
    /// Preserves the supervisor's already installed cancellation latch.
    pub fn suggest_scoped(
        &mut self,
        request: &SuggestRequest,
        scope: Option<&str>,
    ) -> Result<Suggestions, HostError> {
        self.suggest_inner(request, false, scope)
    }
    fn record_schemas(&mut self, presentation: &ResultPresentation, catalog: bool) -> Result<(), HostError> {
        let other = if catalog {
            self.query_schemas.len()
        } else {
            self.catalog_schemas.len()
        };
        if other.saturating_add(presentation.schemas.len()) > 4096 {
            return Err(self.protocol_error("schema registry bound"));
        }
        let registry = if catalog {
            &mut self.catalog_schemas
        } else {
            &mut self.query_schemas
        };
        registry.clear();
        for binding in &presentation.schemas {
            registry.insert(binding.executable.clone(), Arc::clone(&binding.schema));
        }
        Ok(())
    }
    fn admit_suggestions(
        &mut self,
        presentation: &ResultPresentation,
        items: &[Item],
    ) -> Result<ResultPresentation, HostError> {
        if crate::typed::validate(&self.plugin, presentation, items).is_none() {
            return Err(self.protocol_error("invalid sidecar"));
        }
        self.record_schemas(presentation, false)?;
        Ok(presentation.clone())
    }
    pub fn confirm_typed_schema(
        &mut self,
        item: &Item,
        action_id: Option<&str>,
        schema: &ArgumentSchema,
    ) -> Result<bool, HostError> {
        let key = ExecutableKey {
            target: OwnedItem {
                owner: self.plugin.clone(),
                item: item.stable_id.clone(),
            },
            action: action_id.map(|a| crikey_core::ActionId(a.to_owned())),
        };
        if !self.typed_supported || !self.alive {
            return Ok(false);
        }
        let Some(expected) = crate::typed::fingerprint(schema) else {
            return Ok(false);
        };
        let matches = |worker: &Self| {
            [&worker.query_schemas, &worker.catalog_schemas]
                .into_iter()
                .filter_map(|registry| registry.get(&key))
                .any(|binding| crate::typed::fingerprint(binding).as_deref() == Some(expected.as_str()))
        };
        if matches(self) {
            return Ok(true);
        }
        if !self.catalog_schemas.contains_key(&key) {
            self.build_catalog()?;
        }
        Ok(matches(self))
    }

    /// Latch diagnostics before any plaintext encoding or writer queue admission.
    /// Cancellation remains latched; use the existing cancel_handle watcher.
    pub fn typed_execute(
        &mut self,
        item: &Item,
        action_id: Option<&str>,
        schema: &ArgumentSchema,
        values: ArgumentValues,
        view: Option<&ViewExecutionContext>,
    ) -> Result<TypedExecutionOutcome, HostError> {
        self.typed_execute_with_admission(item, action_id, schema, values, view, |send| send())
    }

    /// Run `admission(send)` after confirmation/encoding. The caller holds its
    /// exact source gate only across `send`, which enqueues without waiting on I/O.
    pub fn typed_execute_with_admission(
        &mut self,
        item: &Item,
        action_id: Option<&str>,
        schema: &ArgumentSchema,
        values: ArgumentValues,
        view: Option<&ViewExecutionContext>,
        admission: impl FnOnce(&mut dyn FnMut() -> bool) -> bool,
    ) -> Result<TypedExecutionOutcome, HostError> {
        if !self.typed_supported {
            return Ok(TypedExecutionOutcome::Submission(SubmissionOutcome::Failed(
                SubmissionFailureCode::Unsupported,
            )));
        }
        if item.plugin_id != self.plugin {
            return Ok(TypedExecutionOutcome::Submission(SubmissionOutcome::Failed(
                SubmissionFailureCode::Unavailable,
            )));
        }
        if !self.alive {
            return Err(self.crashed());
        }
        let confirmed = if let Some(context) = view {
            self.view_executable(context, item, action_id)
                .flatten()
                .is_some_and(|current| current.fingerprint().ok() == schema.fingerprint().ok())
        } else {
            self.confirm_typed_schema(item, action_id, schema)?
        };
        if !confirmed {
            return Ok(TypedExecutionOutcome::Submission(SubmissionOutcome::Failed(
                SubmissionFailureCode::Unavailable,
            )));
        }
        if schema.validate_values(&values).is_err() {
            return Ok(TypedExecutionOutcome::Submission(SubmissionOutcome::Failed(
                SubmissionFailureCode::Failed,
            )));
        }
        {
            let mut tail = self.stderr.lock().unwrap_or_else(|e| e.into_inner());
            self.link.sensitive.store(true, Ordering::Release);
            tail.sensitive = true;
            for line in &mut tail.lines {
                let mut bytes = std::mem::take(line).into_bytes();
                bytes.fill(0);
            }
            tail.lines.clear();
            tail.bytes = 0;
        }
        let id = self
            .link
            .next_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| value.checked_add(1))
            .map_err(|_| HostError::Protocol("typed request counter exhausted".into()))?
            .checked_add(1)
            .ok_or_else(|| HostError::Protocol("typed request counter exhausted".into()))?;
        let frozen = protocol::encode_item(item);
        let fingerprint = crate::typed::fingerprint(schema).expect("validated schema");
        let context = view.map(crate::views::context);
        let mut size = protocol::FrameSize::default();
        protocol::write_typed(
            &mut size,
            id,
            &frozen,
            action_id,
            &fingerprint,
            &values,
            context.as_ref(),
        )
        .map_err(|_| HostError::Protocol("typed request exceeds frame bound".into()))?;
        let mut bytes = Vec::with_capacity(size.0);
        protocol::write_typed(
            &mut bytes,
            id,
            &frozen,
            action_id,
            &fingerprint,
            &values,
            context.as_ref(),
        )
        .expect("preflighted typed frame");
        drop(values);
        let mut bytes = Some(PipeBytes(bytes));
        let mut queued = None;
        let deadline = deadline_after_millis(self.options.call_timeout_ms);
        let admitted = admission(&mut || {
            let Some(mut buffer) = bytes.take() else {
                return false;
            };
            let result = self.link.enqueue_bytes(std::mem::take(&mut buffer.0));
            let ok = result.is_ok();
            queued = Some(result);
            ok
        });
        if !admitted && queued.is_none() {
            return Ok(TypedExecutionOutcome::Submission(SubmissionOutcome::Failed(
                SubmissionFailureCode::Unavailable,
            )));
        }
        let completion = match queued {
            Some(result) => self.checked_callback_io(result)?,
            None => return Err(HostError::Protocol("callback admission unavailable".into())),
        };
        match completion.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(())) => {}
            Err(RecvTimeoutError::Timeout) => {
                self.fail_and_reap();
                return Err(HostError::Timeout);
            }
            _ => {
                self.fail_and_reap();
                return Err(self.crashed());
            }
        }
        let line = self.recv_frame_until(deadline)?;
        let frame = self.expect_frame(&line, id, "typed_execute_result")?;
        if frame.get("status").and_then(Value::as_str) == Some("navigate") {
            if !self.structured_supported {
                return Err(self.protocol_error("unnegotiated navigation"));
            }
            if frame.contains_key("failures") || frame.contains_key("code") || frame_has_error(&frame) {
                return Err(self.protocol_error("conflicting typed outcome"));
            }
            return crate::views::navigation(&frame)
                .map(TypedExecutionOutcome::Navigate)
                .ok_or_else(|| self.protocol_error("invalid navigation outcome"));
        }
        crate::typed::outcome(&frame, schema)
            .map(TypedExecutionOutcome::Submission)
            .ok_or_else(|| self.protocol_error("invalid typed outcome"))
    }

    fn suggest_inner(
        &mut self,
        request: &SuggestRequest,
        reset_cancel: bool,
        scope: Option<&str>,
    ) -> Result<Suggestions, HostError> {
        if !self.alive {
            return Err(self.crashed());
        }
        // A replacement query retires its predecessor before any callback or
        // transport failure can leave stale executable bindings behind.
        self.query_schemas.clear();

        // Lower any cancel flag a PRIOR call raised before this one begins, so a
        // worker reused after a cooperative cancellation is not permanently
        // "cancelled" (spec 15.7). A caller that deliberately latched
        // cancellation for this request opts out; its next ordinary request
        // resets the flag.
        if reset_cancel {
            let _ = self.link.queue_frame(&encode_set_cancel(false));
        }

        let id = self.link.next_id();
        let mut outgoing = encode_suggest(id, request);
        if self.presentation_supported {
            if let Some(scope) = scope {
                outgoing["scope_id"] = Value::String(scope.to_owned());
            }
        } else if scope.is_some() {
            return Err(HostError::Protocol("scope unsupported".into()));
        }
        let sent = self.link.write_frame(&outgoing);
        self.checked_callback_io(sent)?;

        // One deadline for the WHOLE call (see `build_catalog`): a plugin that
        // streams partial batches forever is bounded, not hung, because each
        // frame is read against the same shrinking budget rather than a fresh
        // per-frame one.
        let deadline = deadline_after_millis(self.options.call_timeout_ms);
        let mut items = Vec::new();
        let mut log = Vec::new();
        let mut log_bytes = 0usize;
        let mut presentation = ResultPresentation::default();
        let mut presentation_bytes = 0;
        loop {
            let line = self.recv_frame_until(deadline)?;
            let frame = self.expect_frame(&line, id, KIND_RESULT_BATCH)?;
            if crate::typed::append(
                &self.plugin,
                &frame,
                &mut presentation,
                self.typed_supported,
                self.presentation_supported,
                &mut presentation_bytes,
            )
            .is_none()
            {
                return Err(self.protocol_error("invalid sidecar"));
            }
            let Some(generation) = frame.get("generation").and_then(Value::as_u64) else {
                return Err(self.protocol_error(&line));
            };
            if generation != request.generation {
                return Err(self.protocol_error(&line));
            }
            match protocol::decode_items(&self.plugin, &frame) {
                Some(mut batch) => items.append(&mut batch),
                None => return Err(self.protocol_error(&line)),
            }
            if items.len() > MAX_CALL_ITEMS {
                return Err(self.overflow(format!(
                    "suggest streamed more than {MAX_CALL_ITEMS} items and was stopped"
                )));
            }

            log_bytes = log_bytes.saturating_add(log_byte_len(&frame));
            if log_bytes > MAX_CALL_LOG_BYTES {
                return Err(self.overflow(format!(
                    "suggest streamed more than {MAX_CALL_LOG_BYTES} log bytes and was stopped"
                )));
            }
            let Some(mut batch_log) = protocol::decode_log(&frame) else {
                return Err(self.protocol_error(&line));
            };
            if log.len().saturating_add(batch_log.len()) > MAX_CALL_LOG_LINES {
                return Err(self.overflow(format!(
                    "suggest streamed more than {MAX_CALL_LOG_LINES} log lines and was stopped"
                )));
            }
            log.append(&mut batch_log);

            match frame.get("state").and_then(Value::as_str) {
                Some("partial") => {
                    if frame_has_error(&frame) {
                        return Err(self.protocol_error(&line));
                    }
                    continue;
                }
                Some("final") => {
                    if frame_has_error(&frame) {
                        return Err(self.protocol_error(&line));
                    }
                    return Ok(Suggestions {
                        presentation: self.admit_suggestions(&presentation, &items)?,
                        items,
                        state: BatchState::Final,
                        log,
                        error: None,
                    });
                }
                Some("cancelled") => {
                    if frame_has_error(&frame) {
                        return Err(self.protocol_error(&line));
                    }
                    return Ok(Suggestions {
                        presentation: ResultPresentation::default(),
                        items,
                        state: BatchState::Cancelled,
                        log,
                        error: None,
                    });
                }
                Some("failed") => {
                    let Some(error) = decode_plugin_error(&frame) else {
                        return Err(self.protocol_error(&line));
                    };
                    return Ok(Suggestions {
                        presentation: ResultPresentation::default(),
                        items,
                        state: BatchState::Failed,
                        log,
                        error: Some(error),
                    });
                }
                _ => return Err(self.protocol_error(&line)),
            }
        }
    }

    /// Runs the plugin's `execute` for `item`, optionally through `action_id`.
    ///
    /// A plugin that raises is [`ExecuteOutcome::Failed`] on the `Ok` path and
    /// the worker stays usable; only a transport fault is an `Err`.
    pub fn execute(
        &mut self,
        item: &Item,
        action_id: Option<&str>,
        argument: Option<&str>,
        view: Option<&ViewExecutionContext>,
    ) -> Result<ExecuteOutcome, HostError> {
        self.execute_with_admission(item, action_id, argument, view, |send| send())
    }

    pub fn execute_with_admission(
        &mut self,
        item: &Item,
        action_id: Option<&str>,
        argument: Option<&str>,
        view: Option<&ViewExecutionContext>,
        admission: impl FnOnce(&mut dyn FnMut() -> bool) -> bool,
    ) -> Result<ExecuteOutcome, HostError> {
        if !self.alive {
            return Err(self.crashed());
        }
        if let Some(context) = view {
            if !matches!(self.view_executable(context, item, action_id), Some(None)) {
                return Ok(ExecuteOutcome::Failed(PluginError {
                    message: "route executable unavailable".into(),
                    traceback: String::new(),
                }));
            }
        } else if self
            .catalog_schemas
            .keys()
            .chain(self.query_schemas.keys())
            .any(|key| {
                key.target.item == item.stable_id
                    && (action_id.is_none() || key.action.as_ref().map(|a| a.0.as_str()) == action_id)
            })
        {
            return Ok(ExecuteOutcome::Failed(PluginError {
                message: "typed target requires typed execution".into(),
                traceback: String::new(),
            }));
        }

        let id = self.link.next_id();
        let mut frame = encode_execute(id, item, action_id, argument);
        if let Some(context) = view {
            frame["view_context"] = crate::views::context(context);
        }
        let deadline = deadline_after_millis(self.options.call_timeout_ms);
        let mut queued = None;
        if !admission(&mut || {
            let result = self.link.enqueue_frame(&frame);
            let ok = result.is_ok();
            queued = Some(result);
            ok
        }) && queued.is_none()
        {
            return Ok(ExecuteOutcome::Failed(PluginError {
                message: "route executable unavailable".into(),
                traceback: String::new(),
            }));
        }
        let completion = match queued {
            Some(result) => self.checked_callback_io(result)?,
            None => return Err(HostError::Protocol("callback admission unavailable".into())),
        };
        match completion.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(())) => {}
            Err(RecvTimeoutError::Timeout) => {
                self.fail_and_reap();
                return Err(HostError::Timeout);
            }
            _ => {
                self.fail_and_reap();
                return Err(self.crashed());
            }
        }

        let line = self.recv_frame_until(deadline)?;
        let frame = self.expect_frame(&line, id, KIND_EXECUTE_RESULT)?;
        if protocol::decode_log(&frame).is_none() {
            return Err(self.protocol_error(&line));
        }

        match frame.get("status").and_then(Value::as_str) {
            Some("navigate") => {
                if !self.structured_supported {
                    return Err(self.protocol_error("unnegotiated navigation"));
                }
                if frame_has_error(&frame) {
                    return Err(self.protocol_error("conflicting execute outcome"));
                }
                crate::views::navigation(&frame)
                    .map(ExecuteOutcome::Navigate)
                    .ok_or_else(|| self.protocol_error("invalid navigation outcome"))
            }
            Some("ok") => {
                if frame_has_error(&frame) {
                    return Err(self.protocol_error(&line));
                }
                Ok(ExecuteOutcome::Ok)
            }
            Some("failed") => {
                let Some(error) = decode_plugin_error(&frame) else {
                    return Err(self.protocol_error(&line));
                };
                Ok(ExecuteOutcome::Failed(error))
            }
            _ => Err(self.protocol_error(&line)),
        }
    }

    /// Asks the child to exit, then makes sure it did (spec 24.3).
    pub fn shutdown(mut self) -> WorkerExit {
        self.stop_child()
    }

    /// Retains every unfinished child and pipe thread on failure.
    pub fn shutdown_checked(&mut self) -> Result<WorkerExit, HostError> {
        let exit = self.stop_child();
        // Release a reader blocked delivering its final frame, after child exit.
        let (_, empty) = mpsc::sync_channel(1);
        drop(std::mem::replace(&mut self.frames, empty));
        if let Some(reader) = self.stdout_thread.as_ref() {
            let deadline = Instant::now() + REAP_GRACE;
            while !reader.is_finished() && Instant::now() < deadline {
                thread::sleep(REAP_POLL_INTERVAL);
            }
            if reader.is_finished() {
                self.cleanup_failed |= self
                    .stdout_thread
                    .take()
                    .expect("finished reader retained")
                    .join()
                    .is_err();
            } else {
                self.cleanup_failed = true;
            }
        }
        if self.reaped.is_none()
            || !self.tree_settled
            || self.cleanup_failed
            || self.link.cleanup_failed.load(Ordering::Acquire)
        {
            return Err(HostError::TeardownFailed);
        }
        Ok(exit)
    }

    // -- internals ----------------------------------------------------------

    fn checked_callback_io<T>(&mut self, result: io::Result<T>) -> Result<T, HostError> {
        match result {
            Ok(value) => Ok(value),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Err(HostError::AdmissionClosed),
            Err(_) => {
                self.fail_and_reap();
                Err(self.crashed())
            }
        }
    }

    /// Sends the handshake and waits for the child's acknowledgement.
    fn handshake(&mut self) -> Result<(), HostError> {
        let id = self.link.next_id();
        let sent = self.link.write_frame(&encode_handshake(id));
        self.checked_callback_io(sent)?;

        let line = self.recv_frame(self.options.startup_timeout_ms)?;
        let frame = &line;

        let acknowledged = frame.get("id").and_then(Value::as_u64) == Some(id)
            && frame.get("kind").and_then(Value::as_str) == Some(KIND_HANDSHAKE_ACK)
            && frame.get("protocol_version").and_then(Value::as_u64) == Some(u64::from(PROTOCOL_VERSION));

        if acknowledged {
            let capabilities = frame.get("capabilities").and_then(Value::as_array);
            self.structured_supported = capabilities
                .is_some_and(|c| c.iter().any(|v| v.as_str() == Some(STRUCTURED_VIEW_CAPABILITY)));
            self.typed_supported =
                capabilities.is_some_and(|c| c.iter().any(|v| v.as_str() == Some("typed_arguments_v1")));
            self.presentation_supported =
                capabilities.is_some_and(|c| c.iter().any(|v| v.as_str() == Some("result_presentation_v1")));
            self.preview_supported = frame
                .get("capabilities")
                .and_then(Value::as_array)
                .is_some_and(|values| values.iter().any(|value| value.as_str() == Some("preview_v1")));
            self.commands_supported =
                frame
                    .get("capabilities")
                    .and_then(Value::as_array)
                    .is_some_and(|values| {
                        values
                            .iter()
                            .any(|value| value.as_str() == Some(COMMANDS_CAPABILITY))
                    });
            Ok(())
        } else {
            Err(self.protocol_error(&line))
        }
    }

    /// Parses a reply line and checks it echoes `id` and is `kind`, or turns it
    /// into the protocol failure it is (stopping the worker).
    fn expect_frame(
        &mut self,
        line: &Arc<Map<String, Value>>,
        id: u64,
        kind: &str,
    ) -> Result<Arc<Map<String, Value>>, HostError> {
        let frame = line;
        if frame.get("id").and_then(Value::as_u64) != Some(id)
            || frame.get("kind").and_then(Value::as_str) != Some(kind)
        {
            return Err(self.protocol_error(line));
        }
        Ok(Arc::clone(frame))
    }

    /// Waits up to `budget_ms` for one protocol line.
    fn recv_frame(&mut self, budget_ms: u64) -> Result<Arc<Map<String, Value>>, HostError> {
        self.recv_frame_until(deadline_after_millis(budget_ms))
    }

    /// Waits for one protocol line until `deadline`. Reaching it, or observing
    /// the channel break, stops the child so a stuck plugin cannot hold the
    /// host. A single `deadline` shared across every frame of one call is what
    /// makes a whole `suggest`/`build_catalog` bounded, not just each frame.
    fn recv_frame_until(&mut self, deadline: Instant) -> Result<Arc<Map<String, Value>>, HostError> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.fail_and_reap();
                return Err(HostError::Timeout);
            }

            match self.frames.recv_timeout(remaining) {
                Ok(StdoutEvent::Frame(mut line, sanitized)) => {
                    if !sanitized && self.link.sensitive.load(Ordering::Acquire) {
                        let frame = Arc::get_mut(&mut line).expect("reader transfers exclusive frame");
                        for key in ["log", "error", "detail"] {
                            if let Some(value) = frame.remove(key) {
                                let replacement = match (key, value.is_null()) {
                                    ("log", _) => serde_json::json!([]),
                                    ("error", true) => Value::Null,
                                    ("error", false) => {
                                        serde_json::json!({"message":"plugin callback failed","traceback":""})
                                    }
                                    _ => Value::String("plugin callback failed".into()),
                                };
                                frame.insert(key.into(), replacement);
                            }
                        }
                    }
                    return Ok(line);
                }
                Ok(StdoutEvent::Oversized { excerpt, bytes }) => {
                    let excerpt = if self.link.sensitive.load(Ordering::Acquire) {
                        String::new()
                    } else {
                        excerpt
                    };
                    // Framing is lost, so the channel cannot be resynchronised.
                    self.fail_and_reap();
                    return Err(HostError::Protocol(format!(
                        "{excerpt} [crikey: line of {bytes} bytes reached the \
                         {MAX_FRAME_BYTES}-byte frame limit and was abandoned]"
                    )));
                }
                Ok(StdoutEvent::Protocol(message)) => {
                    let message = if self.link.sensitive.load(Ordering::Acquire) {
                        let mut bytes = message.into_bytes();
                        bytes.fill(0);
                        "invalid modern protocol frame".to_owned()
                    } else {
                        message
                    };
                    self.fail_and_reap();
                    return Err(HostError::Protocol(message));
                }
                Ok(StdoutEvent::Failed(message)) => {
                    self.fail_and_reap();
                    return Err(self.crashed_with(Some(message)));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // End of file on the protocol channel: the child is gone.
                    self.fail_and_reap();
                    return Err(self.crashed());
                }
                // A spurious wakeup, not an answer: the deadline decides.
                Err(RecvTimeoutError::Timeout) => continue,
            }
        }
    }

    /// Ends the call as a bounded protocol failure after an aggregate item or
    /// log-byte cap was exceeded, stopping the worker: a plugin that streams
    /// without end cannot be trusted to ever terminate the call on its own.
    fn overflow(&mut self, message: String) -> HostError {
        self.fail_and_reap();
        HostError::Protocol(message)
    }

    /// Turns one malformed reply line into a bounded protocol failure and stops
    /// the worker: a desynchronised channel cannot be trusted.
    fn protocol_error(&mut self, _line: impl std::fmt::Debug) -> HostError {
        self.fail_and_reap();
        HostError::Protocol("invalid modern protocol frame".into())
    }

    /// Marks the worker dead and reaps its child, then lets the stderr tail
    /// finish so a crash diagnostic is complete.
    fn fail_and_reap(&mut self) {
        self.alive = false;
        let _ = self.reap();
        self.drain_stderr();
    }

    /// Lets the stderr drain thread finish reading a dead child's output, giving
    /// up after a bound so a grandchild that inherited the pipe cannot block.
    fn drain_stderr(&mut self) {
        let Some(handle) = self.stderr_thread.as_ref() else {
            return;
        };
        let deadline = Instant::now() + STDERR_DRAIN_GRACE;
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(REAP_POLL_INTERVAL);
        }
        if handle.is_finished() {
            self.cleanup_failed |= self
                .stderr_thread
                .take()
                .expect("finished stderr retained")
                .join()
                .is_err();
        }
        // A failed bounded join retains its canonical handle for later settlement.
        else {
            self.cleanup_failed = true;
        }
    }

    fn crashed(&self) -> HostError {
        self.crashed_with(None)
    }

    fn crashed_with(&self, detail: Option<String>) -> HostError {
        if self.link.sensitive.load(Ordering::Acquire) {
            return HostError::Crashed {
                plugin: self.plugin.clone(),
                detail: "sensitive worker unavailable".into(),
            };
        }
        let mut tail = self
            .stderr
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .render();
        if let Some(detail) = detail {
            if !tail.is_empty() {
                tail.push('\n');
            }
            tail.push_str(&format!(
                "[crikey: reading the modern protocol channel failed: {detail}]"
            ));
        }

        if tail.is_empty() {
            tail.push_str("no stderr output");
        }
        HostError::Crashed {
            plugin: self.plugin.clone(),
            detail: tail,
        }
    }

    /// Settles the complete tree before relinquishing the leader's identity.
    fn reap(&mut self) -> Option<ExitStatus> {
        self.alive = false;
        self.link.close_stdin();
        if let Some(child) = self.child.as_mut() {
            let exited = child_exited(child).unwrap_or(false);
            #[cfg(unix)]
            let may_reap = match kill_process_group(self.process_id) {
                Ok(()) => true,
                // XNU killpg1 filters SZOMB members before counting permitted
                // targets, so a held, exited leader alone yields EPERM. Reap
                // only that observed leader, then require actual group absence
                // below; EPERM itself never proves that descendants are gone.
                Err(error) => {
                    cfg!(target_vendor = "apple") && exited && error.raw_os_error() == Some(libc::EPERM)
                }
            };
            #[cfg(windows)]
            let may_reap = match &self.job {
                Some(job) => job.terminate_and_wait().is_ok(),
                None => exited || child.kill().is_ok(), // failed containment, never resumed
            };
            #[cfg(not(any(unix, windows)))]
            let may_reap = false;
            if !may_reap {
                self.cleanup_failed = true;
                return self.reaped;
            }
            self.hard_stopped |= !exited;
            if let Some(status) = wait_bounded(child, REAP_GRACE) {
                self.reaped = Some(status);
                self.child = None;
            }
        }
        if self.reaped.is_some() && !self.tree_settled {
            #[cfg(unix)]
            {
                let deadline = Instant::now() + REAP_GRACE;
                loop {
                    match process_group_absent(self.process_id) {
                        Ok(true) => {
                            self.tree_settled = true;
                            break;
                        }
                        Ok(false) if Instant::now() < deadline => thread::sleep(REAP_POLL_INTERVAL),
                        _ => break,
                    }
                }
            }
            #[cfg(windows)]
            {
                self.tree_settled = self
                    .job
                    .as_ref()
                    .is_none_or(|job| job.terminate_and_wait().is_ok());
            }
            if self.tree_settled {
                self.background.reap();
            }
        }
        self.link.finish_stdin_writer();
        self.reaped
    }

    /// Asks the child to exit cooperatively, then makes sure it did.
    fn stop_child(&mut self) -> WorkerExit {
        self.alive = false;
        if self.child.is_some() && self.reaped.is_none() {
            let id = self.link.next_id();
            let _ = self.link.queue_frame(&encode_shutdown(id));
            self.link.close_stdin();
            let deadline = Instant::now() + Duration::from_millis(self.options.shutdown_timeout_ms);
            while self
                .child
                .as_ref()
                .is_some_and(|child| matches!(child_exited(child), Ok(false)))
                && Instant::now() < deadline
            {
                while self.frames.try_recv().is_ok() {}
                thread::sleep(REAP_POLL_INTERVAL);
            }
        }
        self.reap();
        self.drain_stderr();
        WorkerExit {
            code: self.reaped.and_then(|status| status.code()),
            hard_stopped: self.hard_stopped,
        }
    }
}

/// Reaping is not the caller's responsibility to remember: a dropped worker
/// whose child outlived it would be an orphan running plugin code with nobody
/// listening (spec 24.3).
impl Drop for ModernWorker {
    fn drop(&mut self) {
        self.reap();
        self.drain_stderr();
    }
}

// ---------------------------------------------------------------------------
// Decoding helpers
// ---------------------------------------------------------------------------

fn bounded_excerpt(line: &str) -> String {
    if line.is_empty() {
        return "<empty line>".to_owned();
    }
    let end = floor_char_boundary(line, PROTOCOL_EXCERPT_BYTES);
    line[..end].to_owned()
}

/// Reads a reply frame's `error` object into a [`PluginError`].
///
/// A missing or malformed error is a protocol failure. A failed reply promises
/// a structured exception, and accepting an empty substitute would hide a
/// broken or hostile worker response.
fn decode_plugin_error(frame: &Map<String, Value>) -> Option<PluginError> {
    let error = frame.get("error")?.as_object()?;
    let message = error.get("message")?.as_str()?.to_owned();
    let traceback = error.get("traceback")?.as_str()?.to_owned();
    Some(PluginError { message, traceback })
}

/// Whether a reply frame carries a present (non-null) `error` object.
///
/// The SDK omits `error` (or sets it null) on the normal path and includes a
/// `{message,traceback}` object only when a callback raised, so this cleanly
/// separates a real catalog fault from an empty catalog (pinned decision 2).
fn frame_has_error(frame: &Map<String, Value>) -> bool {
    frame.get("error").is_some_and(|value| !value.is_null())
}

/// Renders a [`PluginError`] as a single `HostError::PluginFailed` detail: the
/// message, with the traceback appended when the plugin supplied one.
fn join_detail(error: PluginError) -> String {
    if error.traceback.is_empty() {
        error.message
    } else if error.message.is_empty() {
        error.traceback
    } else {
        format!("{}\n{}", error.message, error.traceback)
    }
}

/// The total byte length of a reply frame's raw `log` strings, for the
/// aggregate per-call log-byte cap. Counts the plugin's bytes before the
/// bounded-log clamp, so a flood is measured by what the plugin actually sent.
fn log_byte_len(frame: &Map<String, Value>) -> usize {
    frame
        .get("log")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::len)
                .sum::<usize>()
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Process teardown
// ---------------------------------------------------------------------------

/// Signals the whole owned group while its leader PID is still unreaped.
#[cfg(unix)]
fn kill_process_group(pgid: u32) -> io::Result<()> {
    if pgid <= 1 || pgid > i32::MAX as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid owned process group",
        ));
    }
    #[allow(unsafe_code)]
    let result = unsafe { libc::killpg(pgid as i32, libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn child_exited(child: &Child) -> io::Result<bool> {
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
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { info.si_pid() } != 0)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn child_exited(child: &Child) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn WaitForSingleObject(handle: *mut std::ffi::c_void, milliseconds: u32) -> u32;
    }
    match unsafe { WaitForSingleObject(child.as_raw_handle(), 0) } {
        0 => Ok(true),
        258 => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(not(any(unix, windows)))]
fn child_exited(_: &Child) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process observation unavailable",
    ))
}

#[cfg(unix)]
fn process_group_absent(pgid: u32) -> io::Result<bool> {
    if pgid <= 1 || pgid > i32::MAX as u32 {
        return Err(io::Error::other("invalid process group"));
    }
    #[allow(unsafe_code)]
    let result = unsafe { libc::killpg(pgid as i32, 0) };
    if result == 0 {
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(true)
    } else {
        Err(error)
    }
}

/// Waits for an exiting child's status, giving up after `budget`.
fn wait_bounded(child: &mut Child, budget: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + budget;
    loop {
        match child_exited(child) {
            Ok(true) => return child.wait().ok(),
            Ok(false) if Instant::now() < deadline => thread::sleep(REAP_POLL_INTERVAL),
            _ => return None,
        }
    }
}

/// Decode only item targets; owner, host targets and availability cannot cross
/// this boundary. Validate borrowed fields before allocating declaration strings.
fn decode_commands(value: Option<&Value>) -> Option<Vec<PluginCommandDeclaration>> {
    let values = value?.as_array()?;
    if values.len() > MAX_COMMANDS {
        return None;
    }
    let mut bytes = 0usize;
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
        let object = value.as_object()?;
        if object.len() != 5 {
            return None;
        }
        let id = object.get("command_id")?.as_str()?;
        let item = object.get("item_id")?.as_str()?;
        let action = match object.get("action_id")? {
            Value::Null => None,
            value => Some(value.as_str()?),
        };
        for id in [Some(id), Some(item), action].into_iter().flatten() {
            if id.is_empty() || id.len() > MAX_COMMAND_ID_BYTES || id.chars().any(char::is_control) {
                return None;
            }
            bytes = bytes.checked_add(id.len())?;
        }
        let label = object.get("label")?.as_str()?;
        if label.trim().is_empty() || label.len() > MAX_COMMAND_LABEL_BYTES || !seen.insert(id) {
            return None;
        }
        let policy = object.get("argument_policy")?.as_str()?;
        if !matches!(policy, "forbidden" | "optional" | "required") {
            return None;
        }
        bytes = bytes.checked_add(label.len())?;
        if bytes > MAX_COMMAND_BYTES {
            return None;
        }
    }
    let commands: Vec<_> = values
        .iter()
        .map(|value| {
            Some(PluginCommandDeclaration {
                command_id: value.get("command_id")?.as_str()?.to_owned(),
                label: value.get("label")?.as_str()?.to_owned(),
                item_id: crikey_core::ItemId(value.get("item_id")?.as_str()?.to_owned()),
                action_id: match value.get("action_id")? {
                    Value::Null => None,
                    value => Some(crikey_core::ActionId(value.as_str()?.to_owned())),
                },
                argument_policy: protocol::decode_argument_policy(value.get("argument_policy")?.as_str()?),
            })
        })
        .collect::<Option<_>>()?;
    Some(commands)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_writer_join_retains_the_actual_unfinished_thread() {
        let (release, blocked) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("retained-writer-test".into())
            .spawn(move || {
                blocked.recv().unwrap();
            })
            .unwrap();
        let link = WorkerLink {
            next_id: AtomicU64::new(0),
            stdin: Mutex::new(None),
            stdin_thread: Mutex::new(Some(thread)),
            sensitive: AtomicBool::new(false),
            callback_admission: Mutex::new(None),
            cleanup_failed: AtomicBool::new(false),
        };
        link.finish_stdin_writer();
        {
            let owner = link.stdin_thread.lock().unwrap();
            assert!(owner.as_ref().is_some_and(|thread| !thread.is_finished()));
        }
        assert!(link.cleanup_failed.load(Ordering::Acquire));
        release.send(()).unwrap();
        link.finish_stdin_writer();
        assert!(link.stdin_thread.lock().unwrap().is_none());
    }

    #[test]
    fn command_decode_refuses_authority_fields_and_invalid_targets() {
        let declaration = serde_json::json!({
            "command_id": "stable", "label": "Explicit", "item_id": "item",
            "action_id": null, "argument_policy": "forbidden"
        });
        assert!(decode_commands(Some(&serde_json::json!([declaration.clone()]))).is_some());
        for (key, value) in [
            ("provider_id", serde_json::json!("other.owner")),
            ("availability", serde_json::json!("available")),
            ("host_target", serde_json::json!("open-settings")),
            ("item_id", serde_json::json!("control\u{7f}")),
            ("action_id", serde_json::json!("")),
            ("command_id", serde_json::json!("x".repeat(257))),
            ("argument_policy", serde_json::json!("unknown")),
            ("label", serde_json::json!("x".repeat(8193))),
        ] {
            let mut invalid = declaration.clone();
            invalid[key] = value;
            assert!(
                decode_commands(Some(&serde_json::json!([invalid]))).is_none(),
                "{key}"
            );
        }
        assert!(decode_commands(Some(&serde_json::json!([declaration.clone(), declaration]))).is_none());
    }
    #[test]
    fn empty_protocol_lines_have_a_clear_error_excerpt() {
        assert_eq!(bounded_excerpt(""), "<empty line>");
    }

    #[test]
    fn stderr_tail_keeps_a_bounded_suffix_of_one_long_line() {
        let mut tail = StderrTail::default();
        tail.push("x".repeat(MAX_STDERR_TAIL_BYTES * 2));
        let rendered = tail.render();
        assert!(rendered.len() <= MAX_STDERR_TAIL_BYTES);
        assert!(rendered.starts_with("[crikey: stderr line truncated] "));
    }

    #[test]
    fn frame_reader_accepts_a_complete_final_line_without_newline() {
        let mut reader = std::io::Cursor::new(br#"{"kind":"final"}"#.to_vec());
        let mut line = Vec::new();
        assert!(matches!(
            read_frame_line(&mut reader, &mut line),
            Ok(LineRead::Frame)
        ));
        assert_eq!(line, br#"{"kind":"final"}"#);
    }

    #[test]
    fn queued_protocol_excerpt_is_rechecked_by_the_actual_receive_path() {
        use crikey_package_manager::{EnvironmentId, MaterializedEnvironment};
        const MARKER: &str = "QUEUED_BEFORE_TYPED_LATCH_PRIVATE_DIAGNOSTIC";
        for sensitive in [false, true] {
            let owner = PluginId("queued-protocol".into());
            let env = MaterializedEnvironment {
                id: EnvironmentId("queued-protocol-env".into()),
                site_dir: ".".into(),
            };
            let path = std::path::Path::new(".");
            let options = WorkerOptions::new(
                owner.clone(),
                "unused:Unused",
                ImportPath::assemble(path, &[], &env, path),
            );
            let (sender, frames) = mpsc::sync_channel(1);
            // Exactly the event the stdout reader queued before sensitivity.
            sender.send(StdoutEvent::Protocol(MARKER.to_owned())).unwrap();
            let link = Arc::new(WorkerLink {
                next_id: AtomicU64::new(0),
                stdin: Mutex::new(None),
                stdin_thread: Mutex::new(None),
                sensitive: AtomicBool::new(false),
                callback_admission: Mutex::new(None),
                cleanup_failed: AtomicBool::new(false),
            });
            let mut worker = ModernWorker {
                plugin: owner,
                options,
                link: Arc::clone(&link),
                frames,
                background: Arc::new(BackgroundState::new(None, false)),
                stderr: Arc::new(Mutex::new(StderrTail::default())),
                stderr_thread: None,
                stdout_thread: None,
                cleanup_failed: false,
                child: None,
                #[cfg(unix)]
                process_id: 0,
                tree_settled: false,
                #[cfg(windows)]
                job: None,
                incarnation: 1,
                reaped: None,
                hard_stopped: false,
                preview_supported: false,
                commands_supported: false,
                typed_supported: true,
                presentation_supported: false,
                structured_supported: false,
                active_view: None,
                presentation: ResultPresentation::default(),
                catalog_schemas: BTreeMap::new(),
                query_schemas: BTreeMap::new(),
                alive: true,
            };
            link.sensitive.store(sensitive, Ordering::Release);
            let error = worker.recv_frame(1000).unwrap_err();
            assert!(matches!(error, HostError::Protocol(_)));
            assert_eq!(format!("{error:?}").contains(MARKER), !sensitive);
        }
    }
}
