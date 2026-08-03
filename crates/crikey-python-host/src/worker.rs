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
use std::collections::{hash_map::Entry, HashMap};
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crikey_core::{Item, PluginId};
use crikey_package_manager::ImportPath;
use crikey_plugin_supervisor::{BudgetKind, OwnedBudgetGuard, PluginBudgetHandle};

use crate::protocol::{
    self, encode_background_admit, encode_background_cancel, encode_background_refuse, encode_build_catalog,
    encode_execute, encode_handshake, encode_set_cancel, encode_shutdown, encode_suggest,
    floor_char_boundary, KIND_BACKGROUND_COMPLETE, KIND_BACKGROUND_REGISTER, KIND_CATALOG_BATCH,
    KIND_EXECUTE_RESULT, KIND_HANDSHAKE_ACK, KIND_RESULT_BATCH, MAX_FRAME_BYTES, MAX_STDERR_TAIL_BYTES,
    PROTOCOL_EXCERPT_BYTES, PROTOCOL_VERSION,
};
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

/// Bound on the last step of reaping a child that has already closed stdout.
const REAP_GRACE: Duration = Duration::from_millis(250);

/// Polling interval while waiting for an exiting child's status.
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Bound on letting the stderr drain thread finish after the child is dead, so a
/// crash tail is complete without ever blocking on a grandchild that inherited
/// the pipe.
const STDERR_DRAIN_GRACE: Duration = Duration::from_millis(250);

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
    /// The child interpreter could not be started at all.
    #[error("the modern worker could not be started: {0}")]
    Spawn(String),

    /// The child said something that is not a frame of this protocol. Carries a
    /// bounded excerpt, never the whole (possibly enormous) offending line.
    #[error("the modern worker sent a line that is not a protocol frame: {0}")]
    Protocol(String),

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
    /// The one shared per-plugin budget owner. Background registration refuses
    /// work when this is absent rather than silently bypassing §13.5.
    pub shared_budget: Option<PluginBudgetHandle>,
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
            shared_budget: None,
            startup_timeout_ms: DEFAULT_STARTUP_TIMEOUT_MS,
            call_timeout_ms: DEFAULT_CALL_TIMEOUT_MS,
            shutdown_timeout_ms: DEFAULT_SHUTDOWN_TIMEOUT_MS,
        }
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
}

// ---------------------------------------------------------------------------
// The shared link
// ---------------------------------------------------------------------------

/// The parts of a worker that outlive the thread that owns it.
///
/// `stdin` is behind a mutex because two threads write frames to it: whichever
/// thread is inside a call, and whichever thread raises the cancel flag through
/// a [`CancelHandle`]. Both writes are a single small frame, so the lock is
/// never held across a blocking read.
#[derive(Debug)]
struct WorkerLink {
    next_id: AtomicU64,
    stdin: Mutex<Option<ChildStdin>>,
}

impl WorkerLink {
    fn next_id(&self) -> u64 {
        // Ids are only ever compared for equality with the reply that echoes
        // them, so wrapping after 2^64 frames is not a correctness concern.
        self.next_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }

    fn write_frame(&self, frame: &Value) -> io::Result<()> {
        // A poisoned mutex means another thread panicked mid-write. The frame
        // stream may be truncated, which the peer reports as a protocol error;
        // panicking again here would turn one plugin's fault into a host fault.
        let mut guard = self.stdin.lock().unwrap_or_else(|error| error.into_inner());
        let mut encoded =
            serde_json::to_string(frame).expect("a frame built from owned JSON values serialises");
        encoded.push('\n');
        match guard.as_mut() {
            Some(stdin) => {
                stdin.write_all(encoded.as_bytes())?;
                stdin.flush()
            }
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the modern worker's standard input is already closed",
            )),
        }
    }

    /// Closes the child's stdin: end of file on stdin means "no more requests".
    fn close_stdin(&self) {
        let mut guard = self.stdin.lock().unwrap_or_else(|error| error.into_inner());
        drop(guard.take());
    }
}
/// Bounded operator-visible counters for child-registered background work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackgroundDiagnostics {
    pub registered: u64,
    pub admitted: u64,
    pub refused: u64,
    pub completed: u64,
    pub cancelled: u64,
    pub failed: u64,
    pub unknown_completions: u64,
}

#[derive(Debug, Default)]
struct BackgroundCounters {
    registered: u64,
    admitted: u64,
    refused: u64,
    completed: u64,
    cancelled: u64,
    failed: u64,
    unknown_completions: u64,
}

#[derive(Debug)]
struct BackgroundState {
    budget: Option<PluginBudgetHandle>,
    guards: Mutex<HashMap<u64, OwnedBudgetGuard>>,
    counters: Mutex<BackgroundCounters>,
    closed: AtomicBool,
}

impl BackgroundState {
    fn new(budget: Option<PluginBudgetHandle>) -> Self {
        Self {
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
            unknown_completions: counters.unknown_completions,
        }
    }

    fn register(&self, task_id: u64, link: &WorkerLink) -> io::Result<()> {
        {
            let mut counters = self.counters.lock().unwrap_or_else(|error| error.into_inner());
            counters.registered = counters.registered.saturating_add(1);
        }
        let refused_reason = if self.closed.load(Ordering::Acquire) {
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

    fn complete(&self, task_id: u64, status: &str) -> bool {
        let removed = self
            .guards
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&task_id)
            .is_some();
        let mut counters = self.counters.lock().unwrap_or_else(|error| error.into_inner());
        if !removed {
            counters.unknown_completions = counters.unknown_completions.saturating_add(1);
        }
        match status {
            "cancelled" => counters.cancelled = counters.cancelled.saturating_add(1),
            "failed" => counters.failed = counters.failed.saturating_add(1),
            _ => counters.completed = counters.completed.saturating_add(1),
        }
        removed
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
        let _ = self.link.write_frame(&encode_set_cancel(true));
    }

    /// Lowers the flag so the next call starts uninterrupted.
    pub fn reset(&self) {
        let _ = self.link.write_frame(&encode_set_cancel(false));
    }
}

// ---------------------------------------------------------------------------
// Pipe drainage
// ---------------------------------------------------------------------------

/// What the stdout reader thread hands to the thread inside a call.
#[derive(Debug)]
enum StdoutEvent {
    /// One complete protocol line, newline stripped.
    Frame(String),
    /// A line reached [`MAX_FRAME_BYTES`]. The reader stops afterwards: a peer
    /// that emitted it has lost framing, and guessing where the next frame
    /// starts would invent data.
    Oversized { excerpt: String, bytes: usize },
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
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line: Vec<u8> = Vec::new();
        loop {
            line.clear();
            let event = match read_frame_line(&mut reader, &mut line) {
                Ok(LineRead::Frame) => {
                    let text = String::from_utf8_lossy(strip_carriage_return(&line)).into_owned();
                    match handle_background_frame(&text, &link, &background) {
                        Ok(true) => continue,
                        Ok(false) => StdoutEvent::Frame(text),
                        Err(error) => StdoutEvent::Failed(error),
                    }
                }
                Ok(LineRead::Eof) => return,
                Ok(LineRead::Oversized(bytes)) => StdoutEvent::Oversized {
                    excerpt: excerpt(&line),
                    bytes,
                },
                Err(error) => StdoutEvent::Failed(error.to_string()),
            };

            let fatal = !matches!(event, StdoutEvent::Frame(_));
            if sender.send(event).is_err() || fatal {
                return;
            }
        }
    });
}

/// Handles lifecycle frames independently of foreground request replies. This
/// is what lets a background completion release its Arc-owned guard while the
/// worker is idle and lets registration receive an admission immediately while
/// a synchronous callback is still running.
fn handle_background_frame(
    line: &str,
    link: &WorkerLink,
    background: &BackgroundState,
) -> Result<bool, String> {
    let Some(frame) = parse_object(line) else {
        return Ok(false);
    };
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
                return Err(format!("background_complete has unknown status {status:?}"));
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
}

impl StderrTail {
    fn push(&mut self, line: String) {
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

fn spawn_stderr_drain(stderr: ChildStderr, tail: Arc<Mutex<StderrTail>>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line: Vec<u8> = Vec::new();
        loop {
            line.clear();
            match read_frame_line(&mut reader, &mut line) {
                Ok(LineRead::Frame) => {
                    let text = String::from_utf8_lossy(strip_carriage_return(&line)).into_owned();
                    tail.lock().unwrap_or_else(|error| error.into_inner()).push(text);
                }
                // Nothing more will arrive, or the log channel itself is broken.
                // Either way the tail stops growing and the thread ends rather
                // than spinning on a dead pipe.
                Ok(LineRead::Eof | LineRead::Oversized(_)) | Err(_) => return,
            }
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
    child: Option<Child>,
    process_id: u32,
    reaped: Option<ExitStatus>,
    hard_stopped: bool,
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
        let sdk = crate::sdk_root();
        let entry = sdk.join(WORKER_ENTRY_FILE);
        if !entry.is_file() {
            return Err(HostError::Spawn(format!(
                "the modern worker entry {WORKER_ENTRY_FILE} is missing from {}",
                sdk.display()
            )));
        }

        let mut command = Command::new(interpreter.path());
        command
            .arg(WORKER_ISOLATION_FLAG)
            .arg(&entry)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The child reaches its plugin, managed deps and the SDK through
            // `PYTHONPATH` alone, which is why the isolation flag can only be
            // `-S`. Global site-packages is never on it (spec 15.4).
            .env("PYTHONPATH", options.import_path.to_pythonpath())
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

        let mut child = command
            .spawn()
            .map_err(|error| HostError::Spawn(format!("the process could not be started: {error}")))?;
        let process_id = child.id();

        // Piped by construction immediately above, so these are `Some`.
        let stdin = child.stdin.take().expect("worker stdin is piped");
        let stdout = child.stdout.take().expect("worker stdout is piped");
        let stderr = child.stderr.take().expect("worker stderr is piped");

        let (sender, frames) = mpsc::sync_channel(128);
        let link = Arc::new(WorkerLink {
            next_id: AtomicU64::new(0),
            stdin: Mutex::new(Some(stdin)),
        });
        let background = Arc::new(BackgroundState::new(options.shared_budget.clone()));
        spawn_stdout_reader(stdout, sender, Arc::clone(&link), Arc::clone(&background));
        let tail = Arc::new(Mutex::new(StderrTail::default()));
        let stderr_thread = spawn_stderr_drain(stderr, Arc::clone(&tail));

        let mut worker = Self {
            plugin: options.plugin.clone(),
            options,
            link,
            frames,
            background,
            stderr: tail,
            stderr_thread: Some(stderr_thread),
            child: Some(child),
            process_id,
            reaped: None,
            hard_stopped: false,
            alive: true,
        };

        if let Err(error) = worker.handshake() {
            let _ = worker.reap();
            return Err(error);
        }

        Ok(worker)
    }

    /// Whether the worker is still serving.
    ///
    /// A crash, protocol failure or timeout observed by any call marks the
    /// worker not alive, so this answers without probing the process (and thus
    /// without `&mut self`).
    pub fn is_alive(&self) -> bool {
        self.alive
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

    /// Runs the plugin's `build_catalog`, folding its catalog batches.
    pub fn build_catalog(&mut self) -> Result<Vec<Item>, HostError> {
        if !self.alive {
            return Err(self.crashed());
        }

        let id = self.link.next_id();
        if self.link.write_frame(&encode_build_catalog(id)).is_err() {
            self.fail_and_reap();
            return Err(self.crashed());
        }

        // One deadline for the WHOLE call: every frame is read against the same
        // budget, decremented as time passes, so a plugin that streams partial
        // catalog batches forever is bounded rather than resetting the clock
        // with each frame.
        let deadline = Instant::now() + Duration::from_millis(self.options.call_timeout_ms);
        let mut items = Vec::new();
        let mut log_bytes = 0usize;
        loop {
            let line = self.recv_frame_until(deadline)?;
            let frame = self.expect_frame(&line, id, KIND_CATALOG_BATCH)?;

            // A terminal frame carrying an `error` object is a load-time catalog
            // fault (pinned decision 2): the plugin raised in `build_catalog`.
            // The worker itself is healthy and stays alive; the provider records
            // the plugin unavailable.
            if frame_has_error(&frame) {
                let error = decode_plugin_error(&frame);
                return Err(HostError::PluginFailed {
                    plugin: self.plugin.clone(),
                    detail: join_detail(error),
                });
            }

            match protocol::decode_items(&self.plugin, &frame) {
                Some(mut batch) => items.append(&mut batch),
                None => return Err(self.protocol_error(&line)),
            }
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

            if frame.get("done").and_then(Value::as_bool) == Some(true) {
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
        self.suggest_inner(request, true)
    }

    /// Runs a request against a cancellation flag deliberately latched by the
    /// caller. The next ordinary [`Self::suggest`] clears that flag.
    pub fn suggest_with_cancel_latched(
        &mut self,
        request: &SuggestRequest,
    ) -> Result<Suggestions, HostError> {
        self.suggest_inner(request, false)
    }

    fn suggest_inner(
        &mut self,
        request: &SuggestRequest,
        reset_cancel: bool,
    ) -> Result<Suggestions, HostError> {
        if !self.alive {
            return Err(self.crashed());
        }

        // Lower any cancel flag a PRIOR call raised before this one begins, so a
        // worker reused after a cooperative cancellation is not permanently
        // "cancelled" (spec 15.7). A caller that deliberately latched
        // cancellation for this request opts out; its next ordinary request
        // resets the flag.
        if reset_cancel {
            let _ = self.link.write_frame(&encode_set_cancel(false));
        }

        let id = self.link.next_id();
        if self.link.write_frame(&encode_suggest(id, request)).is_err() {
            self.fail_and_reap();
            return Err(self.crashed());
        }

        // One deadline for the WHOLE call (see `build_catalog`): a plugin that
        // streams partial batches forever is bounded, not hung, because each
        // frame is read against the same shrinking budget rather than a fresh
        // per-frame one.
        let deadline = Instant::now() + Duration::from_millis(self.options.call_timeout_ms);
        let mut items = Vec::new();
        let mut log = Vec::new();
        let mut log_bytes = 0usize;
        loop {
            let line = self.recv_frame_until(deadline)?;
            let frame = self.expect_frame(&line, id, KIND_RESULT_BATCH)?;

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
            log.append(&mut protocol::decode_log(&frame));

            match frame.get("state").and_then(Value::as_str) {
                Some("partial") => continue,
                Some("final") => {
                    return Ok(Suggestions {
                        items,
                        state: BatchState::Final,
                        log,
                        error: None,
                    })
                }
                Some("cancelled") => {
                    return Ok(Suggestions {
                        items,
                        state: BatchState::Cancelled,
                        log,
                        error: None,
                    })
                }
                Some("failed") => {
                    return Ok(Suggestions {
                        items,
                        state: BatchState::Failed,
                        log,
                        error: Some(decode_plugin_error(&frame)),
                    })
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
    ) -> Result<ExecuteOutcome, HostError> {
        if !self.alive {
            return Err(self.crashed());
        }

        let id = self.link.next_id();
        if self
            .link
            .write_frame(&encode_execute(id, item, action_id, argument))
            .is_err()
        {
            self.fail_and_reap();
            return Err(self.crashed());
        }

        let line = self.recv_frame(self.options.call_timeout_ms)?;
        let frame = self.expect_frame(&line, id, KIND_EXECUTE_RESULT)?;

        match frame.get("status").and_then(Value::as_str) {
            Some("ok") => Ok(ExecuteOutcome::Ok),
            Some("failed") => Ok(ExecuteOutcome::Failed(decode_plugin_error(&frame))),
            _ => Err(self.protocol_error(&line)),
        }
    }

    /// Asks the child to exit, then makes sure it did (spec 24.3).
    pub fn shutdown(mut self) -> WorkerExit {
        self.stop_child()
    }

    // -- internals ----------------------------------------------------------

    /// Sends the handshake and waits for the child's acknowledgement.
    fn handshake(&mut self) -> Result<(), HostError> {
        let id = self.link.next_id();
        if self.link.write_frame(&encode_handshake(id)).is_err() {
            self.fail_and_reap();
            return Err(self.crashed());
        }

        let line = self.recv_frame(self.options.startup_timeout_ms)?;
        let Some(frame) = parse_object(&line) else {
            return Err(self.protocol_error(&line));
        };

        let acknowledged = frame.get("id").and_then(Value::as_u64) == Some(id)
            && frame.get("kind").and_then(Value::as_str) == Some(KIND_HANDSHAKE_ACK)
            && frame.get("protocol_version").and_then(Value::as_u64) == Some(u64::from(PROTOCOL_VERSION));

        if acknowledged {
            Ok(())
        } else {
            Err(self.protocol_error(&line))
        }
    }

    /// Parses a reply line and checks it echoes `id` and is `kind`, or turns it
    /// into the protocol failure it is (stopping the worker).
    fn expect_frame(&mut self, line: &str, id: u64, kind: &str) -> Result<Map<String, Value>, HostError> {
        let Some(frame) = parse_object(line) else {
            return Err(self.protocol_error(line));
        };
        if frame.get("id").and_then(Value::as_u64) != Some(id)
            || frame.get("kind").and_then(Value::as_str) != Some(kind)
        {
            return Err(self.protocol_error(line));
        }
        Ok(frame)
    }

    /// Waits up to `budget_ms` for one protocol line.
    fn recv_frame(&mut self, budget_ms: u64) -> Result<String, HostError> {
        self.recv_frame_until(Instant::now() + Duration::from_millis(budget_ms))
    }

    /// Waits for one protocol line until `deadline`. Reaching it, or observing
    /// the channel break, stops the child so a stuck plugin cannot hold the
    /// host. A single `deadline` shared across every frame of one call is what
    /// makes a whole `suggest`/`build_catalog` bounded, not just each frame.
    fn recv_frame_until(&mut self, deadline: Instant) -> Result<String, HostError> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.fail_and_reap();
                return Err(HostError::Timeout);
            }

            match self.frames.recv_timeout(remaining) {
                Ok(StdoutEvent::Frame(line)) => return Ok(line),
                Ok(StdoutEvent::Oversized { excerpt, bytes }) => {
                    // Framing is lost, so the channel cannot be resynchronised.
                    self.fail_and_reap();
                    return Err(HostError::Protocol(format!(
                        "{excerpt} [crikey: line of {bytes} bytes reached the \
                         {MAX_FRAME_BYTES}-byte frame limit and was abandoned]"
                    )));
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
    fn protocol_error(&mut self, line: &str) -> HostError {
        self.fail_and_reap();
        HostError::Protocol(bounded_excerpt(line))
    }

    /// Marks the worker dead and reaps its child, then lets the stderr tail
    /// finish so a crash diagnostic is complete.
    fn fail_and_reap(&mut self) {
        self.alive = false;
        self.background.reap();
        let _ = self.reap();
        self.drain_stderr();
    }

    /// Lets the stderr drain thread finish reading a dead child's output, giving
    /// up after a bound so a grandchild that inherited the pipe cannot block.
    fn drain_stderr(&mut self) {
        let Some(handle) = self.stderr_thread.take() else {
            return;
        };
        let deadline = Instant::now() + STDERR_DRAIN_GRACE;
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(REAP_POLL_INTERVAL);
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
        // Otherwise the handle is dropped (detached): the tail is whatever the
        // drain thread had read, and it keeps updating the shared buffer.
    }

    fn crashed(&self) -> HostError {
        self.crashed_with(None)
    }

    fn crashed_with(&self, detail: Option<String>) -> HostError {
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

        HostError::Crashed {
            plugin: self.plugin.clone(),
            detail: tail,
        }
    }

    /// Ends the child, killing it only if it is still alive.
    fn reap(&mut self) -> Option<ExitStatus> {
        self.background.reap();
        self.link.close_stdin();
        let Some(mut child) = self.child.take() else {
            return self.reaped;
        };

        if matches!(child.try_wait(), Ok(None)) {
            hard_kill(self.process_id, &mut child);
            self.hard_stopped = true;
        }
        self.reaped = child.wait().ok();
        self.reaped
    }

    /// Asks the child to exit cooperatively, then makes sure it did.
    fn stop_child(&mut self) -> WorkerExit {
        self.alive = false;
        self.background.reap();
        if let Some(status) = self.reaped {
            return WorkerExit {
                code: status.code(),
                hard_stopped: self.hard_stopped,
            };
        }

        let Some(mut child) = self.child.take() else {
            return WorkerExit {
                code: None,
                hard_stopped: self.hard_stopped,
            };
        };

        let id = self.link.next_id();
        let _ = self.link.write_frame(&encode_shutdown(id));
        // Redundant on purpose: end of file on stdin means the same thing, and a
        // shim that missed the frame still sees the pipe close.
        self.link.close_stdin();

        // The child closing stdout is the event that says it is leaving; waiting
        // for that rather than polling for a status keeps an orderly shutdown as
        // fast as the child is.
        let budget = Duration::from_millis(self.options.shutdown_timeout_ms);
        let started = Instant::now();
        let mut left_voluntarily = false;
        loop {
            let remaining = budget.checked_sub(started.elapsed()).unwrap_or_default();
            if remaining.is_zero() {
                break;
            }
            match self.frames.recv_timeout(remaining) {
                // A farewell frame. Nothing asked for one, and nothing reads it.
                Ok(_) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    left_voluntarily = true;
                    break;
                }
                Err(RecvTimeoutError::Timeout) => break,
            }
        }

        let status = if left_voluntarily {
            wait_bounded(&mut child, REAP_GRACE)
        } else {
            None
        };

        let status = match status {
            Some(status) => status,
            None => {
                hard_kill(self.process_id, &mut child);
                self.hard_stopped = true;
                match child.wait() {
                    Ok(status) => status,
                    Err(_) => {
                        return WorkerExit {
                            code: None,
                            hard_stopped: true,
                        }
                    }
                }
            }
        };

        self.reaped = Some(status);
        WorkerExit {
            code: status.code(),
            hard_stopped: self.hard_stopped,
        }
    }
}

/// Reaping is not the caller's responsibility to remember: a dropped worker
/// whose child outlived it would be an orphan running plugin code with nobody
/// listening (spec 24.3).
impl Drop for ModernWorker {
    fn drop(&mut self) {
        self.background.reap();
        if self.child.is_some() {
            let _ = self.reap();
        }
    }
}

// ---------------------------------------------------------------------------
// Decoding helpers
// ---------------------------------------------------------------------------

fn parse_object(line: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(line) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

fn bounded_excerpt(line: &str) -> String {
    let end = floor_char_boundary(line, PROTOCOL_EXCERPT_BYTES);
    line[..end].to_owned()
}

/// Reads a reply frame's `error` object into a [`PluginError`].
///
/// A missing or malformed error still yields a `PluginError` rather than
/// failing: a `failed` reply promises the plugin raised, and the least we owe a
/// caller is an (empty) structured error rather than a transport failure.
fn decode_plugin_error(frame: &Map<String, Value>) -> PluginError {
    let error = frame.get("error").and_then(Value::as_object);
    let field = |name: &str| {
        error
            .and_then(|object| object.get(name))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    PluginError {
        message: field("message"),
        traceback: field("traceback"),
    }
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

/// Hard-stops the child, reaching its whole process group on Unix.
///
/// On Unix the child is its own group leader (`process_group(0)` at spawn), so
/// signalling the group kills any grandchildren a plugin forked as well as the
/// leader (spec 24.3). Off Unix `std::process` offers no portable group kill, so
/// only the direct child is reached — an honest, documented limit. The caller
/// still `wait()`s the leader to reap it after this returns.
fn hard_kill(process_id: u32, child: &mut Child) {
    #[cfg(unix)]
    kill_process_group(process_id);
    #[cfg(not(unix))]
    {
        let _ = process_id;
    }
    let _ = child.kill();
}

/// Sends `SIGKILL` to an entire process group (spec 24.3).
///
/// Killing a process *group* needs the `killpg(3)`/`kill(2)` syscall; the
/// standard library only kills a single child ([`Child::kill`]) and exposes no
/// safe wrapper for the group case. This is therefore this crate's only
/// `unsafe`, isolated to this one function and declaring `killpg` directly so no
/// new dependency is pulled in. Its arguments are a validated pid-as-pgid (the
/// group was created by `CommandExt::process_group(0)`) and a constant signal;
/// the call reads and writes no memory.
#[cfg(unix)]
fn kill_process_group(pgid: u32) {
    extern "C" {
        fn killpg(pgrp: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    #[allow(
        unsafe_code,
        reason = "no safe std API kills a process group; args are a validated pgid and a constant signal (spec 24.3)"
    )]
    unsafe {
        let _ = killpg(pgid as i32, SIGKILL);
    }
}

/// Waits for an exiting child's status, giving up after `budget`.
fn wait_bounded(child: &mut Child, budget: Duration) -> Option<ExitStatus> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if started.elapsed() >= budget {
            return None;
        }
        thread::sleep(REAP_POLL_INTERVAL);
    }
}
