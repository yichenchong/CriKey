//! The out-of-process CPython legacy worker (spec 4.2, 9, 13.4, 24.1, 24.3).
//!
//! Legacy plugin code never runs inside CriKey. It runs in a child process
//! started as `<interpreter> -S <shim_dir>/_crikey_legacy_worker.py`, and this
//! module is the parent side of that boundary: it spawns the child, hands it
//! the package it must load through the environment, serialises one callback at
//! a time over a newline-delimited JSON channel, and — the part that earns the
//! process boundary in the first place — survives every way the far side can
//! misbehave.
//!
//! # The channel
//!
//! One JSON object per line, in both directions. The child's stdout is a strict
//! protocol channel: anything a plugin prints is captured by the shim and
//! travels back inside the reply frame as `log`, never as a bare line, because
//! a `print` that reached stdout would be indistinguishable from a frame and
//! would desynchronise the stream for good. The child's stderr is a mirror of
//! the same output for live observability; the host drains it continuously on
//! its own thread so a chatty plugin can never deadlock on a full pipe, and
//! uses it only for the tail quoted in [`WorkerError::Crashed`].
//!
//! # Why `call` takes `&mut self`
//!
//! "Legacy callbacks are serialized per plugin instance" (acceptance 31.16) is
//! an invariant of the child: the shim runs one callback at a time on its main
//! thread. Taking `&mut self` makes a concurrent second call unrepresentable
//! rather than merely wrong, so the invariant cannot be violated by a caller.
//!
//! # Why wall-clock time is read here
//!
//! CriKey library logic takes virtual time as an explicit parameter and never
//! reads a clock. This module is the documented exception: the peer is a real
//! operating-system process, and no amount of virtual time makes a spinning
//! child stop spinning. Every bound is nevertheless supplied by the caller
//! through [`WorkerOptions`] — the clock is only used to measure elapsed time
//! against a budget that was passed in.
//!
//! # Bounds
//!
//! Every retained buffer has an explicit cap and a documented overflow rule:
//! [`MAX_FRAME_BYTES`] on one protocol line, [`MAX_LOG_LINES`] and
//! [`MAX_LOG_LINE_BYTES`] on a reply's log, [`MAX_STDERR_TAIL_BYTES`] on the
//! retained stderr tail. A plugin cannot make the host allocate without limit,
//! and an over-long line is a named protocol failure rather than growth.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crikey_core::{Action, ArgumentPolicy, Category, Generation, HitPolicy, Item, ItemId, PluginId};
use crikey_input_scheduler::Millis;
use serde_json::{json, Map, Value};

use crate::events::LegacyEventFlags;
use crate::interpreter::{Interpreter, PythonVersion};
use crate::package::LegacyPackage;
use crate::LegacyCallback;

/// The shim module the child interpreter executes.
///
/// Shipped in the crate's `python/` directory alongside `keypirinha.py`, and
/// named in [`WorkerOptions::new`] rather than discovered, so a host that
/// forgot to install the shim fails at spawn with a message naming the file.
pub const WORKER_ENTRY_FILE: &str = "_crikey_legacy_worker.py";

/// The CPython isolation flag the worker is started with.
///
/// `-S` and nothing stronger. `-E` and `-I` both discard `PYTHONPATH`, which is
/// how the shim directory reaches the child's import path — either of them
/// would unhook the shim and leave `import keypirinha` failing. `-S` skips
/// `site` (so a user's `site-packages` cannot shadow the shim) while leaving
/// the explicitly configured import path intact.
pub const WORKER_ISOLATION_FLAG: &str = "-S";

/// Carries the package content root to the child (spec 14.3).
///
/// The value is exactly [`crate::package::PackageRoot::content_root`], never
/// canonicalised: a plugin that compares it against a path the operator
/// configured must see the same spelling the operator wrote.
pub const ENV_PACKAGE_ROOT: &str = "CRIKEY_LEGACY_PACKAGE_ROOT";

/// Carries the plugin id the child answers as.
pub const ENV_PLUGIN_ID: &str = "CRIKEY_LEGACY_PLUGIN_ID";

/// Carries the package id, which is also the settings file's stem (spec 14.7).
pub const ENV_PACKAGE_ID: &str = "CRIKEY_LEGACY_PACKAGE_ID";

/// Carries the import name of the plugin entry module.
///
/// Legacy package ids are directory names and may contain characters that are
/// not valid in a Python identifier, so this is a label, not something the
/// child may hand to `importlib.import_module`.
pub const ENV_MAIN_MODULE: &str = "CRIKEY_LEGACY_MAIN_MODULE";

/// Carries the package-relative path of the plugin entry module.
///
/// This is what the child actually loads, through
/// `importlib.util.spec_from_file_location`, precisely because the import name
/// above may be unusable as one (`ignores-should-terminate.py`).
pub const ENV_MAIN_MODULE_PATH: &str = "CRIKEY_LEGACY_MAIN_MODULE_PATH";

/// Carries the package cache directory, when the caller supplies one.
///
/// Absent when the caller does not: the shim then picks its own temporary
/// location, so `package_cache_path()` never fails for want of host
/// configuration.
pub const ENV_CACHE_DIR: &str = "CRIKEY_LEGACY_CACHE_DIR";

/// Carries [`PROTOCOL_VERSION`] so a mismatched shim can refuse to speak.
pub const ENV_PROTOCOL_VERSION: &str = "CRIKEY_LEGACY_PROTOCOL_VERSION";

/// Overrides where [`shim_root`] looks for the shipped shim directory.
pub const ENV_SHIM_DIR_OVERRIDE: &str = "CRIKEY_LEGACY_SHIM_DIR";

/// The frame schema this host speaks. Echoed by the child's handshake.
pub const PROTOCOL_VERSION: u64 = 1;

/// Ceiling on one protocol line, in bytes.
///
/// Generous, because a legitimate catalog frame from a large package is large.
/// Overflow behaviour: the line is abandoned and reported as
/// [`WorkerError::Protocol`] carrying a bounded excerpt. The channel is not
/// resynchronised afterwards — a peer that emitted an eight-megabyte line has
/// already lost the framing — so the child is stopped.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Ceiling on the number of log lines retained from one reply.
///
/// Overflow behaviour: the first [`MAX_LOG_LINES`] lines are kept and one
/// synthetic line records how many were dropped, so a truncated log says that
/// it is truncated instead of quietly lying.
pub const MAX_LOG_LINES: usize = 512;

/// Ceiling on one retained log line, in bytes. Longer lines are truncated at a
/// character boundary with an explicit marker.
pub const MAX_LOG_LINE_BYTES: usize = 4096;

/// Ceiling on the retained stderr tail, in bytes.
///
/// Overflow behaviour: the *oldest* lines are dropped. This buffer exists to
/// explain a crash, and the interesting output of a crashing process is the
/// output nearest the crash.
pub const MAX_STDERR_TAIL_BYTES: usize = 8 * 1024;

/// How much of an over-long protocol line is quoted back in the error.
const PROTOCOL_EXCERPT_BYTES: usize = 4096;

/// Bound on the last step of reaping a child that has already closed its
/// stdout.
///
/// Such a child is exiting; this is only the gap between "the pipe reached end
/// of file" and "the kernel has the exit status". Bounded anyway, because a
/// plugin that closed stdout and then hung must not block host shutdown.
const REAP_GRACE: Duration = Duration::from_millis(250);

/// Polling interval while waiting for an exiting child's status.
///
/// `std` has no wait-with-timeout, and the alternative — a blocking `wait` —
/// would hand a hung child the power to block shutdown forever.
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(1);
/// Bound on frames waiting for the dedicated stdin writer. The queue is
/// deliberately finite: a stuck child must apply backpressure, not let the
/// host retain an unbounded stream of requests or control updates.
const WRITE_QUEUE_CAPACITY: usize = 32;

/// Bounded stdout frames waiting for the callback thread. A hostile peer that
/// emits replies without requests must apply backpressure rather than make the
/// host retain an unbounded stream.
const STDOUT_QUEUE_CAPACITY: usize = 8;

/// Default per-callback bound when the caller sets none (spec 9.6).
const DEFAULT_CALL_TIMEOUT_MS: Millis = 10_000;

/// Default bound on the startup handshake.
const DEFAULT_STARTUP_TIMEOUT_MS: Millis = 10_000;

/// Default bound on an orderly shutdown before the child is hard-stopped.
const DEFAULT_SHUTDOWN_TIMEOUT_MS: Millis = 5_000;

/// Control frame that raises or lowers the child's termination flag.
const CONTROL_SET_TERMINATE: &str = "set_terminate";

/// Control frame that asks the child to exit.
const CONTROL_SHUTDOWN: &str = "shutdown";

/// Where the shipped shim directory lives at run time.
///
/// First hit wins: [`ENV_SHIM_DIR_OVERRIDE`], then `legacy-shim` beside the
/// running executable (the installed layout), then this crate's `python`
/// directory (the development layout). Deliberately does not prove that
/// [`WORKER_ENTRY_FILE`] is present — a caller that wants to fail early with a
/// good message checks `shim_root().join(WORKER_ENTRY_FILE).is_file()`, and a
/// caller that does not gets the same failure from [`LegacyWorker::spawn`].
pub fn shim_root() -> PathBuf {
    if let Some(configured) = std::env::var_os(ENV_SHIM_DIR_OVERRIDE) {
        return PathBuf::from(configured);
    }

    if let Some(directory) = std::env::current_exe().ok().and_then(|exe| {
        let installed = exe.parent()?.join("legacy-shim");
        installed.join(WORKER_ENTRY_FILE).is_file().then_some(installed)
    }) {
        return directory;
    }

    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/python"))
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// One live incarnation of a legacy plugin.
///
/// Distinct from the plugin id because a package reload replaces the instance
/// while keeping the id: a reply that echoed only the plugin would let a
/// superseded instance's late answer be mistaken for a live one (spec 9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstanceId(pub u64);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything that can go wrong between CriKey and a legacy worker.
///
/// A plugin *raising* is not in here: that is an `Ok` reply whose outcome is
/// [`LegacyOutcome::Failed`], because the worker is healthy and stays usable.
/// Conflating the two would make plugin bugs look like transport bugs.
///
/// Every variant names the plugin or the interpreter it concerns, because an
/// error that cannot be attributed cannot become an actionable diagnostic
/// (spec 26.2). `Clone` so a supervisor can retain one while returning another.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkerError {
    /// No interpreter could be used at all (spec 14.11).
    #[error("no usable CPython for the legacy compatibility layer{}: {reason}",
        path.as_ref().map(|path| format!(" at {}", path.display())).unwrap_or_default())]
    PythonUnavailable {
        /// The candidate that failed, when a specific one was named.
        path: Option<PathBuf>,
        /// Why it could not be used, in terms an operator can act on.
        reason: String,
    },

    /// The interpreter runs, but is older than CriKey supports (spec 14.11).
    #[error("the interpreter at {} reports Python {found}, but the legacy compatibility layer requires {minimum} or newer", path.display())]
    UnsupportedVersion {
        /// The interpreter that was probed.
        path: PathBuf,
        /// The version it reported.
        found: PythonVersion,
        /// The oldest version CriKey runs legacy plugin code on.
        minimum: PythonVersion,
    },

    /// The child said something that is not a frame of this protocol.
    #[error("plugin {} sent a line that is not a legacy protocol frame during {callback}: {line}", plugin.0)]
    Protocol {
        /// The plugin whose worker committed the violation.
        plugin: PluginId,
        /// The callback that was in flight.
        callback: LegacyCallback,
        /// The offending line, so a diagnostic can quote it.
        line: String,
    },

    /// The child died with a callback in flight (spec 24.1).
    #[error("plugin {}'s worker exited during {callback}{}", plugin.0,
        status.map(|status| format!(" with status {status}")).unwrap_or_else(|| String::from(" without an exit status")))]
    Crashed {
        /// The plugin whose worker died.
        plugin: PluginId,
        /// The callback that was in flight.
        callback: LegacyCallback,
        /// The exit status, when the platform reported one. `None` for a child
        /// stopped by a signal.
        status: Option<i32>,
        /// A bounded tail of what the child wrote to stderr before dying.
        stderr_tail: String,
    },

    /// A callback overran the bound the caller supplied (spec 9.6).
    ///
    /// The child is hard-stopped before this is returned: cooperation cannot be
    /// assumed, so one plugin must not be able to hang the host forever
    /// (acceptance 31.17).
    #[error("plugin {}'s {callback} did not answer within {waited_ms}ms and its worker was stopped", plugin.0)]
    Timeout {
        /// The plugin that would not answer.
        plugin: PluginId,
        /// The callback that overran.
        callback: LegacyCallback,
        /// How long the host actually waited. Never less than the bound given.
        waited_ms: Millis,
    },

    /// The host could not perform an operating-system operation.
    #[error("{operation} failed{}: {message}",
        plugin.as_ref().map(|plugin| format!(" for plugin {}", plugin.0)).unwrap_or_default())]
    Io {
        /// The plugin concerned, when the operation was on its behalf.
        plugin: Option<PluginId>,
        /// What was being attempted, phrased so the message reads as a sentence.
        operation: String,
        /// The underlying failure, already rendered: an `io::Error` is neither
        /// `Clone` nor `PartialEq`, and both are part of this type's contract.
        message: String,
    },
}

/// A Python exception a plugin callback raised.
///
/// Carried by [`LegacyOutcome::Failed`], not by [`WorkerError`]: the transport
/// worked, the plugin did not. Attribution is part of the value because a
/// traceback that cannot be tied to a plugin and a callback cannot become a
/// diagnostic (spec 26.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginException {
    /// The plugin that raised.
    pub plugin: PluginId,
    /// The callback it raised in.
    pub callback: LegacyCallback,
    /// The exception class name, e.g. `ValueError`.
    pub exception_type: String,
    /// `str(exception)`.
    pub message: String,
    /// The formatted traceback, naming the frames that raised.
    pub traceback: String,
}

// ---------------------------------------------------------------------------
// Requests and replies
// ---------------------------------------------------------------------------

/// What the host is asking a legacy plugin to do.
///
/// Deliberately not `PartialEq`: two requests are never interchangeable, since
/// each one belongs to exactly one generation of exactly one instance, and an
/// equality that ignored that would invite comparing them.
#[derive(Debug, Clone)]
pub enum LegacyRequestKind {
    /// `on_start`: the plugin may read settings and prepare (spec 13.2).
    Start,
    /// `on_catalog`: the plugin publishes its static catalog (spec 14.8).
    Catalog,
    /// `on_suggest` for a fresh query, with no item selected.
    InitialSuggest {
        /// The user's input.
        query: String,
    },
    /// `on_suggest` for arguments typed against an already selected item.
    ArgumentSuggest {
        /// The user's input after the selected item.
        query: String,
        /// The item the user selected.
        selected: ItemId,
    },
    /// `on_execute`: the user launched an item, optionally through an action.
    Execute {
        /// The item the user picked.
        item: Box<Item>,
        /// The secondary action, when the user chose one.
        action: Option<Action>,
    },
    /// `on_activated`: the launcher became visible.
    Activated,
    /// `on_deactivated`: the launcher was dismissed.
    Deactivated,
    /// `on_events`: something the plugin subscribed to changed (spec 14.4).
    Events {
        /// The coalesced flag set. Never empty (spec 14.6).
        flags: LegacyEventFlags,
    },
}

/// One callback dispatch, addressed to one instance of one plugin.
#[derive(Debug, Clone)]
pub struct LegacyRequest {
    /// Who is being asked.
    pub plugin: PluginId,
    /// Which incarnation of it.
    pub instance: InstanceId,
    /// The query generation this work belongs to, so a late answer can be
    /// recognised as stale (acceptance 31.7).
    pub generation: Generation,
    /// What is being asked.
    pub kind: LegacyRequestKind,
}

impl LegacyRequest {
    /// The documented callback this request invokes.
    pub fn callback(&self) -> LegacyCallback {
        match self.kind {
            LegacyRequestKind::Start => LegacyCallback::OnStart,
            LegacyRequestKind::Catalog => LegacyCallback::OnCatalog,
            LegacyRequestKind::InitialSuggest { .. } | LegacyRequestKind::ArgumentSuggest { .. } => {
                LegacyCallback::OnSuggest
            }
            LegacyRequestKind::Execute { .. } => LegacyCallback::OnExecute,
            LegacyRequestKind::Activated => LegacyCallback::OnActivated,
            LegacyRequestKind::Deactivated => LegacyCallback::OnDeactivated,
            LegacyRequestKind::Events { .. } => LegacyCallback::OnEvents,
        }
    }
}

/// What a callback did.
#[derive(Debug, Clone)]
pub enum LegacyOutcome {
    /// The callback ran and had nothing to publish: `on_start`, `on_activated`,
    /// `on_deactivated`, `on_events`.
    Acknowledged,
    /// `set_suggestions` was called with this batch.
    Suggestions(Vec<Item>),
    /// `set_catalog` was called: the plugin's catalog is exactly this.
    SetCatalog(Vec<Item>),
    /// `merge_catalog` was called: these items are added to what is already
    /// catalogued (spec 14.8).
    ///
    /// A separate variant rather than a flag because the host, not the shim,
    /// owns merging: the shim reports the intent and the host decides what it
    /// means for the catalog it holds.
    MergeCatalog(Vec<Item>),
    /// A catalog or suggestion callback returned without publishing anything.
    ///
    /// Distinct from an empty batch on purpose: this is the obsolete-work
    /// abandon path a cooperative plugin takes when `should_terminate()` goes
    /// true, and publishing an empty batch instead would clobber the live
    /// suggestion list the abandoned work was supposed to leave alone
    /// (spec 9.2, 14.5).
    Abandoned,
    /// `on_execute` completed.
    Executed,
    /// The callback raised. The worker is healthy and remains usable.
    Failed(PluginException),
}

/// One reply, echoing the envelope of the request that caused it.
#[derive(Debug, Clone)]
pub struct LegacyResponse {
    /// The plugin that answered.
    pub plugin: PluginId,
    /// The instance that answered.
    pub instance: InstanceId,
    /// The generation the answered request belonged to.
    pub generation: Generation,
    /// The callback that was answered.
    pub callback: LegacyCallback,
    /// What it did.
    pub outcome: LegacyOutcome,
    /// What the plugin printed while doing it, bounded by [`MAX_LOG_LINES`] and
    /// [`MAX_LOG_LINE_BYTES`].
    pub log: Vec<String>,
    /// How many times the callback consulted `should_terminate()`.
    ///
    /// Zero for a plugin that never polls. This is the only way to tell a
    /// cooperative plugin from an uncooperative one that does not depend on
    /// timing, which is what makes a conformance check on it deterministic
    /// (spec 14.5, acceptance 31.17).
    pub terminate_polls: u32,
}

impl LegacyResponse {
    /// A bare acknowledgement of `request`.
    pub fn started(request: &LegacyRequest) -> Self {
        Self::echo(request, LegacyOutcome::Acknowledged)
    }

    /// A suggestion batch answering `request`.
    pub fn suggestions(request: &LegacyRequest, items: Vec<Item>) -> Self {
        Self::echo(request, LegacyOutcome::Suggestions(items))
    }

    /// A wholesale catalog replacement answering `request`.
    pub fn set_catalog(request: &LegacyRequest, items: Vec<Item>) -> Self {
        Self::echo(request, LegacyOutcome::SetCatalog(items))
    }

    /// A catalog addition answering `request` (spec 14.8).
    pub fn merge_catalog(request: &LegacyRequest, items: Vec<Item>) -> Self {
        Self::echo(request, LegacyOutcome::MergeCatalog(items))
    }

    fn echo(request: &LegacyRequest, outcome: LegacyOutcome) -> Self {
        Self {
            plugin: request.plugin.clone(),
            instance: request.instance,
            generation: request.generation,
            callback: request.callback(),
            outcome,
            log: Vec::new(),
            terminate_polls: 0,
        }
    }
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
/// Timeouts live here rather than being read from a clock or a global, which is
/// what keeps every bound in this module explicit and caller-supplied.
#[derive(Debug, Clone)]
pub struct WorkerOptions {
    plugin: PluginId,
    shim_dir: PathBuf,
    cache_dir: Option<PathBuf>,
    startup_timeout_ms: Millis,
    call_timeout_ms: Millis,
    shutdown_timeout_ms: Millis,
    env: Vec<(OsString, OsString)>,
}

impl WorkerOptions {
    /// Options for `plugin`, whose child loads the shim from `shim_dir`.
    pub fn new(plugin: PluginId, shim_dir: impl Into<PathBuf>) -> Self {
        Self {
            plugin,
            shim_dir: shim_dir.into(),
            cache_dir: None,
            startup_timeout_ms: DEFAULT_STARTUP_TIMEOUT_MS,
            call_timeout_ms: DEFAULT_CALL_TIMEOUT_MS,
            shutdown_timeout_ms: DEFAULT_SHUTDOWN_TIMEOUT_MS,
            env: Vec::new(),
        }
    }

    /// Bounds the startup handshake.
    pub fn with_startup_timeout_ms(mut self, timeout_ms: Millis) -> Self {
        self.startup_timeout_ms = timeout_ms;
        self
    }

    /// Bounds every callback (spec 9.6). Exceeding it stops the child.
    pub fn with_call_timeout_ms(mut self, timeout_ms: Millis) -> Self {
        self.call_timeout_ms = timeout_ms;
        self
    }

    /// Bounds an orderly shutdown before the child is hard-stopped.
    pub fn with_shutdown_timeout_ms(mut self, timeout_ms: Millis) -> Self {
        self.shutdown_timeout_ms = timeout_ms;
        self
    }

    /// Sets the directory the child serves `package_cache_path()` from.
    pub fn with_cache_dir(mut self, cache_dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = Some(cache_dir.into());
        self
    }

    /// Adds one environment variable for the child.
    ///
    /// Configured variables are applied before CriKey's protocol variables, so
    /// a caller cannot replace the package identity, import path, or isolation
    /// settings the worker needs in order to remain attributable and usable.
    pub fn with_env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env
            .push((key.as_ref().to_os_string(), value.as_ref().to_os_string()));
        self
    }

    /// The plugin these options are for.
    pub fn plugin(&self) -> &PluginId {
        &self.plugin
    }

    /// The directory holding [`WORKER_ENTRY_FILE`].
    pub fn shim_dir(&self) -> &Path {
        &self.shim_dir
    }
}

// ---------------------------------------------------------------------------
// The shared link
// ---------------------------------------------------------------------------

/// One frame waiting for the dedicated stdin writer. The writer owns the
/// operating-system pipe, so no caller holds a lock across `write_all` or
/// `flush`.
#[derive(Debug)]
struct WriteCommand {
    encoded: String,
    ack: Option<mpsc::SyncSender<io::Result<()>>>,
}

#[derive(Debug)]
struct WriteQueueState {
    closed: bool,
    pending: VecDeque<WriteCommand>,
}

#[derive(Debug)]
struct WriteQueue {
    state: Mutex<WriteQueueState>,
    wake: Condvar,
}

fn spawn_stdin_writer(mut stdin: ChildStdin, queue: Arc<WriteQueue>) {
    thread::spawn(move || loop {
        let command = {
            let mut state = queue.state.lock().unwrap_or_else(|error| error.into_inner());
            loop {
                if let Some(command) = state.pending.pop_front() {
                    break Some(command);
                }
                if state.closed {
                    break None;
                }
                state = queue.wake.wait(state).unwrap_or_else(|error| error.into_inner());
            }
        };
        let Some(command) = command else {
            return;
        };

        let result = stdin
            .write_all(command.encoded.as_bytes())
            .and_then(|_| stdin.flush());
        let error = result
            .as_ref()
            .err()
            .map(|error| (error.kind(), error.to_string()));
        if let Some(ack) = command.ack {
            let reply = error.as_ref().map_or(Ok(()), |(kind, message)| {
                Err(io::Error::new(*kind, message.clone()))
            });
            let _ = ack.send(reply);
        }
        let Some((kind, message)) = error else {
            continue;
        };

        let pending = {
            let mut state = queue.state.lock().unwrap_or_else(|error| error.into_inner());
            state.closed = true;
            std::mem::take(&mut state.pending)
        };
        for command in pending {
            if let Some(ack) = command.ack {
                let _ = ack.send(Err(io::Error::new(kind, message.clone())));
            }
        }
        return;
    });
}

/// The parts of a worker that outlive the thread that owns it.
#[derive(Debug)]
struct WorkerLink {
    terminate: AtomicBool,
    next_id: AtomicU64,
    queue: Arc<WriteQueue>,
}

impl WorkerLink {
    fn next_id(&self) -> u64 {
        // Ids are only ever compared for equality with the reply that echoes
        // them, so wrapping after 2^64 frames is not a correctness concern.
        self.next_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }

    fn write_frame(&self, frame: &Value, budget: Duration) -> io::Result<()> {
        let mut encoded =
            serde_json::to_string(frame).expect("a frame built from owned JSON values serialises");
        encoded.push('\n');
        let deadline = Instant::now().checked_add(budget).unwrap_or_else(Instant::now);
        let (ack_sender, ack_receiver) = mpsc::sync_channel(1);
        self.enqueue_until(
            WriteCommand {
                encoded,
                ack: Some(ack_sender),
            },
            deadline,
        )?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        match ack_receiver.recv_timeout(remaining) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the legacy worker's stdin writer exceeded its bound",
            )),
            Err(RecvTimeoutError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the legacy worker's stdin writer stopped",
            )),
        }
    }

    /// Enqueues a frame without waiting for the writer. Shutdown uses this
    /// path because closing stdin immediately after the enqueue is the fallback
    /// if the child never reads the orderly control frame.
    fn enqueue_without_wait(&self, frame: &Value) -> io::Result<()> {
        let mut encoded =
            serde_json::to_string(frame).expect("a frame built from owned JSON values serialises");
        encoded.push('\n');
        self.enqueue_until(WriteCommand { encoded, ack: None }, Instant::now())
    }

    fn enqueue_until(&self, command: WriteCommand, deadline: Instant) -> io::Result<()> {
        let mut command = Some(command);
        loop {
            let mut state = self.queue.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.closed {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the legacy worker's standard input is already closed",
                ));
            }
            if state.pending.len() < WRITE_QUEUE_CAPACITY {
                state
                    .pending
                    .push_back(command.take().expect("command is present"));
                self.queue.wake.notify_one();
                return Ok(());
            }
            drop(state);
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the legacy worker's stdin queue is full",
                ));
            }
            thread::sleep(REAP_POLL_INTERVAL);
        }
    }

    /// Closes the child's stdin, which is the second way to ask it to exit:
    /// end of file on stdin means "no more callbacks are coming".
    fn close_stdin(&self) {
        let mut state = self.queue.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        self.queue.wake.notify_all();
    }
}

/// Raises and lowers the cooperative termination flag of a live worker.
///
/// `Send + Sync + Clone` because the thread that raises the flag is never the
/// thread blocked inside [`LegacyWorker::call`] — that thread is, by
/// definition, waiting for the callback the flag is meant to interrupt.
///
/// The host raises and the host lowers: nothing clears the flag implicitly, so
/// `signal()` before a call is still raised when that call's first
/// `should_terminate()` poll happens, which is what makes a conformance check
/// on cooperation deterministic without a clock.
#[derive(Debug, Clone)]
pub struct TerminateHandle {
    link: Arc<WorkerLink>,
}

impl TerminateHandle {
    /// Asks the plugin to abandon its current work (spec 9.2, 14.5).
    pub fn signal(&self) {
        self.set(true);
    }

    /// Lowers the flag so the next callback starts uninterrupted.
    pub fn clear(&self) {
        self.set(false);
    }

    /// Whether the flag is currently raised.
    pub fn is_signalled(&self) -> bool {
        self.link.terminate.load(Ordering::SeqCst)
    }

    /// The host-side flag is authoritative even when the child is already gone,
    /// so a failed enqueue is deliberately ignored: the caller asked to change
    /// host state, and a dead worker is reported by the call that discovers it.
    ///
    /// The queue mutex is held only while swapping the flag and appending to a
    /// bounded in-memory queue. The actual pipe write happens on the dedicated
    /// writer thread, never while this lock is held.
    fn set(&self, raised: bool) {
        let mut state = self
            .link
            .queue
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.link.terminate.swap(raised, Ordering::SeqCst) == raised {
            return;
        }
        if state.closed || state.pending.len() >= WRITE_QUEUE_CAPACITY {
            return;
        }

        // Out of band, never queued behind the in-flight call: the child's
        // stdin reader thread applies this to a `threading.Event` while its
        // main thread is still inside the callback (acceptance 31.17).
        let id = self.link.next_id();
        let mut encoded = serde_json::to_string(&json!({
            "id": id,
            "callback": CONTROL_SET_TERMINATE,
            "payload": { "terminate": raised },
        }))
        .expect("a control frame built from owned JSON values serialises");
        encoded.push('\n');
        state.pending.push_back(WriteCommand { encoded, ack: None });
        self.link.queue.wake.notify_one();
    }
}

// ---------------------------------------------------------------------------
// Pipe drainage
// ---------------------------------------------------------------------------

/// What the stdout reader thread hands to the thread inside `call`.
#[derive(Debug)]
enum StdoutEvent {
    /// One complete protocol line, newline stripped and known to be UTF-8.
    Frame(String),
    /// A complete protocol line that was not valid UTF-8.
    InvalidUtf8 { excerpt: String, bytes: usize },
    /// A line reached [`MAX_FRAME_BYTES`]. The reader stops afterwards: a peer
    /// that emitted it has lost framing, and guessing where the next frame
    /// starts would invent data.
    Oversized { excerpt: String, bytes: usize },
    /// The stream ended with bytes that were not terminated by a newline.
    Partial { excerpt: String, bytes: usize },
    /// Reading the pipe itself failed.
    Failed(String),
}

#[derive(Debug)]
enum LineRead {
    Frame,
    /// The stream ended after a non-empty, unterminated line.
    Partial(usize),
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
            // JSON-lines framing requires a newline terminator. A final
            // fragment is not a frame: silently accepting it would turn a
            // truncated response into a valid answer.
            return Ok(if line.is_empty() {
                LineRead::Eof
            } else {
                LineRead::Partial(line.len())
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
fn spawn_stdout_reader(stdout: ChildStdout, sender: mpsc::SyncSender<StdoutEvent>) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line: Vec<u8> = Vec::new();
        loop {
            line.clear();
            let event = match read_frame_line(&mut reader, &mut line) {
                Ok(LineRead::Frame) => {
                    let bytes = strip_carriage_return(&line);
                    match String::from_utf8(bytes.to_vec()) {
                        Ok(frame) => StdoutEvent::Frame(frame),
                        Err(_) => StdoutEvent::InvalidUtf8 {
                            excerpt: excerpt(bytes),
                            bytes: bytes.len(),
                        },
                    }
                }
                // Dropping the sender is how end of file reaches the caller:
                // `recv` then answers `Disconnected`, which is the crash path.
                Ok(LineRead::Eof) => return,
                Ok(LineRead::Oversized(bytes)) => StdoutEvent::Oversized {
                    excerpt: excerpt(&line),
                    bytes,
                },
                Ok(LineRead::Partial(bytes)) => StdoutEvent::Partial {
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
/// it — which, with the host blocked waiting for the reply, would be a deadlock
/// with the plugin on both ends.
#[derive(Debug, Default)]
struct StderrTail {
    lines: std::collections::VecDeque<String>,
    bytes: usize,
}

impl StderrTail {
    fn push(&mut self, line: String) {
        let line = if line.len() + 1 > MAX_STDERR_TAIL_BYTES {
            let marker = "[crikey: stderr line truncated; retaining its tail] ";
            let suffix_limit = MAX_STDERR_TAIL_BYTES
                .saturating_sub(marker.len())
                .saturating_sub(1);
            let mut start = line.len().saturating_sub(suffix_limit);
            while start < line.len() && !line.is_char_boundary(start) {
                start += 1;
            }
            let mut retained = format!("{marker}{}", &line[start..]);
            if retained.len() + 1 > MAX_STDERR_TAIL_BYTES {
                let end = floor_char_boundary(&retained, MAX_STDERR_TAIL_BYTES.saturating_sub(1));
                retained.truncate(end);
            }
            retained
        } else {
            line
        };

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

fn spawn_stderr_drain(stderr: ChildStderr, tail: Arc<Mutex<StderrTail>>) {
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
                Ok(LineRead::Partial(_)) => {
                    let text = String::from_utf8_lossy(strip_carriage_return(&line)).into_owned();
                    tail.lock().unwrap_or_else(|error| error.into_inner()).push(text);
                    return;
                }
                Ok(LineRead::Oversized(bytes)) => {
                    // Stderr is observational, so an over-long line does not
                    // destroy framing. Record the truncation and continue
                    // consuming the remainder; stopping here would let a
                    // chatty plugin fill the stderr pipe and deadlock.
                    tail.lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(format!(
                            "[crikey: stderr line reached the {bytes}-byte \
                             bound and was truncated]"
                        ));
                }
                // Nothing more will arrive, or the log channel itself is
                // broken. Either way the tail stops growing and the thread ends
                // rather than spinning on a dead pipe.
                Ok(LineRead::Eof) | Err(_) => return,
            }
        }
    });
}

// ---------------------------------------------------------------------------
// The worker
// ---------------------------------------------------------------------------

/// A live legacy plugin, running in its own operating-system process.
///
/// `Send` so a supervisor can own one away from the user-interface thread
/// (spec 4.2). Not `Sync`, and `call` takes `&mut self`: callbacks are
/// serialized per instance by construction (acceptance 31.16).
#[derive(Debug)]
pub struct LegacyWorker {
    plugin: PluginId,
    options: WorkerOptions,
    link: Arc<WorkerLink>,
    frames: Receiver<StdoutEvent>,
    stderr: Arc<Mutex<StderrTail>>,
    child: Option<Child>,
    process_id: u32,
    reaped: Option<ExitStatus>,
    hard_stopped: bool,
}

impl LegacyWorker {
    /// Starts a child interpreter for `package` and completes its handshake.
    ///
    /// Returns only once the child has announced itself, so a caller holding a
    /// `LegacyWorker` holds a process that is running the shim — not merely one
    /// that was spawned. A child that fails to announce itself is reaped before
    /// the error is returned; no path out of this function leaks a process.
    pub fn spawn(
        interpreter: &Interpreter,
        package: &LegacyPackage,
        options: WorkerOptions,
    ) -> Result<Self, WorkerError> {
        let entry = options.shim_dir.join(WORKER_ENTRY_FILE);
        if !entry.is_file() {
            return Err(WorkerError::Io {
                plugin: Some(options.plugin.clone()),
                operation: format!("locating the legacy worker entry point {}", entry.display()),
                message: format!(
                    "{} does not contain {WORKER_ENTRY_FILE}",
                    options.shim_dir.display()
                ),
            });
        }

        let main_module_path = package
            .modules
            .iter()
            .find(|module| module.import_name == package.main_module)
            .map(|module| module.relative_path.clone())
            .ok_or_else(|| WorkerError::Io {
                plugin: Some(options.plugin.clone()),
                operation: format!("resolving the entry module of package {}", package.id.as_str()),
                message: format!(
                    "the package declares {} as its main module but lists no such file",
                    package.main_module
                ),
            })?;
        let content_root = package.root.content_root();

        let mut command = Command::new(interpreter.path());
        // Custom variables are applied first. The variables below are part of
        // the host/worker protocol and must win over plugin configuration:
        // allowing them to be overridden can load a different module or break
        // the worker's identity and isolation.
        for (key, value) in &options.env {
            command.env(key, value);
        }
        command
            .arg(WORKER_ISOLATION_FLAG)
            .arg(&entry)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The shim reaches the child through `PYTHONPATH` alone, which is
            // why the isolation flag can only ever be `-S`. The host's own
            // PYTHONPATH is replaced rather than extended: a legacy plugin's
            // imports must resolve against its package and the shim, never
            // against whatever the operator configured for unrelated tools.
            .env("PYTHONPATH", &options.shim_dir)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            // Unbuffered, because a buffered reply frame is a reply the host
            // waits for until a deadline it never should have reached.
            .env("PYTHONUNBUFFERED", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .env(ENV_PROTOCOL_VERSION, PROTOCOL_VERSION.to_string())
            .env(ENV_PACKAGE_ROOT, content_root)
            .env(ENV_PLUGIN_ID, &options.plugin.0)
            .env(ENV_PACKAGE_ID, package.id.as_str())
            .env(ENV_MAIN_MODULE, &package.main_module)
            .env(ENV_MAIN_MODULE_PATH, &main_module_path);
        if let Some(cache_dir) = &options.cache_dir {
            command.env(ENV_CACHE_DIR, cache_dir);
        }

        // Own process group so a hard stop can signal the whole subtree, not
        // just the leader: a plugin that forks (subprocess.Popen, os.system,
        // multiprocessing) must never leave grandchildren running plugin code
        // with nobody listening (spec 24.3). `process_group(0)` makes the
        // child's pgid equal to its own pid, so `self.process_id` names the
        // group. Unix only — `std::process` exposes no portable equivalent.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command.spawn().map_err(|error| WorkerError::PythonUnavailable {
            path: Some(interpreter.path().to_path_buf()),
            reason: format!("the legacy worker process could not be started: {error}"),
        })?;
        let process_id = child.id();

        // Piped by construction immediately above, so these are `Some`.
        let stdin = child.stdin.take().expect("worker stdin is piped");
        let stdout = child.stdout.take().expect("worker stdout is piped");
        let stderr = child.stderr.take().expect("worker stderr is piped");

        let (sender, frames) = mpsc::sync_channel(STDOUT_QUEUE_CAPACITY);
        spawn_stdout_reader(stdout, sender);
        let tail = Arc::new(Mutex::new(StderrTail::default()));
        spawn_stderr_drain(stderr, Arc::clone(&tail));
        let queue = Arc::new(WriteQueue {
            state: Mutex::new(WriteQueueState {
                closed: false,
                pending: VecDeque::new(),
            }),
            wake: Condvar::new(),
        });
        spawn_stdin_writer(stdin, Arc::clone(&queue));

        let mut worker = Self {
            plugin: options.plugin.clone(),
            options,
            link: Arc::new(WorkerLink {
                terminate: AtomicBool::new(false),
                next_id: AtomicU64::new(0),
                queue,
            }),
            frames,
            stderr: tail,
            child: Some(child),
            process_id,
            reaped: None,
            hard_stopped: false,
        };

        if let Err(error) = worker.handshake() {
            let _ = worker.reap();
            return Err(error);
        }

        Ok(worker)
    }

    /// The operating-system process identifier of the child running the plugin.
    ///
    /// Retained rather than read from the handle so it stays answerable after
    /// the child has been reaped, which is exactly when a diagnostic about a
    /// crash needs it.
    pub fn process_id(&self) -> u32 {
        self.process_id
    }

    /// Whether the child is still alive.
    ///
    /// Takes `&mut self` because answering reaps a child that has exited: an
    /// exited child nobody waited for stays in the process table as a zombie,
    /// and a launcher that leaks one per plugin reload leaks them forever
    /// (spec 24.3).
    pub fn is_running(&mut self) -> bool {
        if self.reaped.is_some() {
            return false;
        }

        match self.child.as_mut().map(Child::try_wait) {
            Some(Ok(None)) => true,
            Some(Ok(Some(status))) => {
                self.reaped = Some(status);
                false
            }
            // A handle that cannot be asked is not a handle that can be
            // trusted to be alive; reap it rather than leaving a child
            // undisposed.
            Some(Err(_)) => {
                let _ = self.reap();
                false
            }
            None => false,
        }
    }

    /// A handle that raises this worker's cooperative termination flag.
    pub fn terminate_handle(&self) -> TerminateHandle {
        TerminateHandle {
            link: Arc::clone(&self.link),
        }
    }

    /// Runs one callback and waits for its reply, bounded by
    /// [`WorkerOptions::with_call_timeout_ms`].
    ///
    /// A plugin that raises answers `Ok` with [`LegacyOutcome::Failed`] and
    /// stays usable. A plugin that overruns its bound, dies, or breaks the
    /// framing answers `Err`, and in each of those cases the child is reaped
    /// before this returns.
    pub fn call(&mut self, request: LegacyRequest) -> Result<LegacyResponse, WorkerError> {
        let callback = request.callback();
        if request.plugin != self.plugin {
            return Err(WorkerError::Io {
                plugin: Some(self.plugin.clone()),
                operation: format!("dispatching {callback}"),
                message: format!(
                    "request targets plugin {}, but this worker serves {}",
                    request.plugin.0, self.plugin.0
                ),
            });
        }
        if self.reaped.is_some() {
            return Err(self.crashed(callback));
        }

        let id = self.link.next_id();
        let frame = encode_request(id, &request, self.link.terminate.load(Ordering::SeqCst));
        let call_budget = Duration::from_millis(self.options.call_timeout_ms);
        let call_started = Instant::now();
        if let Err(error) = self.link.write_frame(&frame, call_budget) {
            let waited_ms = millis(call_started.elapsed()).max(self.options.call_timeout_ms);
            let _ = self.reap();
            if error.kind() == io::ErrorKind::TimedOut {
                return Err(WorkerError::Timeout {
                    plugin: self.plugin.clone(),
                    callback,
                    waited_ms,
                });
            }
            return Err(self.crashed(callback));
        }

        let remaining = call_budget
            .checked_sub(call_started.elapsed())
            .unwrap_or_default();
        if remaining.is_zero() {
            let waited_ms = millis(call_started.elapsed()).max(self.options.call_timeout_ms);
            let _ = self.reap();
            return Err(WorkerError::Timeout {
                plugin: self.plugin.clone(),
                callback,
                waited_ms,
            });
        }

        let line = self.await_frame_duration(remaining, self.options.call_timeout_ms, callback)?;
        match decode_response(&request, id, line) {
            Ok(response) => Ok(response),
            Err(error) => {
                let _ = self.reap();
                Err(error)
            }
        }
    }

    /// Stops the child and reaps it, returning how it ended.
    ///
    /// Never reports the plugin's misbehaviour as an error: a worker that had to
    /// be killed is described by [`WorkerExit::hard_stopped`], and shutting down
    /// a worker that already died is not a failure either. The `Result` is there
    /// so callers can treat teardown like every other worker operation.
    pub fn shutdown(mut self) -> Result<WorkerExit, WorkerError> {
        Ok(self.stop_child())
    }

    /// Waits for the startup handshake (spec 24.1).
    ///
    /// The child announces itself before importing any plugin code, so this
    /// proves the interpreter and the shim work while saying nothing about the
    /// plugin — a broken plugin import is a `Failed` reply to `on_start`, not a
    /// dead handshake, which is what lets the host report it as a plugin fault.
    fn handshake(&mut self) -> Result<(), WorkerError> {
        let line = self.await_frame(self.options.startup_timeout_ms, LegacyCallback::OnStart)?;

        let ready = serde_json::from_str::<Value>(&line).ok().and_then(|value| {
            let frame = value.as_object()?;
            let ready = frame.get("ready")?.as_bool()?;
            let protocol = frame.get("protocol")?.as_u64()?;
            (ready && protocol == PROTOCOL_VERSION).then_some(())
        });

        ready.ok_or_else(|| WorkerError::Protocol {
            plugin: self.plugin.clone(),
            callback: LegacyCallback::OnStart,
            line,
        })
    }
    /// Waits up to `budget_ms` for one protocol line.
    ///
    /// This is where the hard bound lives. Exceeding it stops the child, so a
    /// plugin that ignores cooperative termination still cannot hold the host
    /// (spec 9.6, acceptance 31.17).
    fn await_frame(&mut self, budget_ms: Millis, callback: LegacyCallback) -> Result<String, WorkerError> {
        self.await_frame_duration(Duration::from_millis(budget_ms), budget_ms, callback)
    }

    fn await_frame_duration(
        &mut self,
        budget: Duration,
        minimum_wait_ms: Millis,
        callback: LegacyCallback,
    ) -> Result<String, WorkerError> {
        let started = Instant::now();

        loop {
            let remaining = budget.checked_sub(started.elapsed()).unwrap_or_default();
            if remaining.is_zero() {
                // `.max` guards only clock granularity: the loop already waited
                // the whole budget, and a reported wait shorter than the bound
                // would misdescribe what happened.
                let waited_ms = millis(started.elapsed()).max(minimum_wait_ms);
                let _ = self.reap();
                return Err(WorkerError::Timeout {
                    plugin: self.plugin.clone(),
                    callback,
                    waited_ms,
                });
            }

            match self.frames.recv_timeout(remaining) {
                Ok(StdoutEvent::Frame(line)) => return Ok(line),
                Ok(StdoutEvent::InvalidUtf8 { excerpt, bytes }) => {
                    let _ = self.reap();
                    return Err(WorkerError::Protocol {
                        plugin: self.plugin.clone(),
                        callback,
                        line: format!("{excerpt} [crikey: {bytes}-byte protocol line was not valid UTF-8]"),
                    });
                }
                Ok(StdoutEvent::Oversized { excerpt, bytes }) => {
                    // Framing is lost, so the channel cannot be resynchronised.
                    let _ = self.reap();
                    return Err(WorkerError::Protocol {
                        plugin: self.plugin.clone(),
                        callback,
                        line: format!(
                            "{excerpt} [crikey: line of {bytes} bytes reached the \
                             {MAX_FRAME_BYTES}-byte frame limit and was abandoned]"
                        ),
                    });
                }
                Ok(StdoutEvent::Partial { excerpt, bytes }) => {
                    let _ = self.reap();
                    return Err(WorkerError::Protocol {
                        plugin: self.plugin.clone(),
                        callback,
                        line: format!(
                            "{excerpt} [crikey: stream ended with an unterminated \
                             {bytes}-byte protocol line]"
                        ),
                    });
                }
                Ok(StdoutEvent::Failed(message)) => {
                    // The protocol channel itself broke. The child may still be
                    // alive, but it has no way left to answer, so it is stopped
                    // and the reason is carried into the diagnostic.
                    let _ = self.reap();
                    return Err(self.crashed_with(callback, Some(message)));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // End of file on the protocol channel: the child is gone.
                    let _ = self.reap();
                    return Err(self.crashed(callback));
                }
                // A spurious wakeup, not an answer: the deadline decides.
                Err(RecvTimeoutError::Timeout) => continue,
            }
        }
    }

    fn crashed(&self, callback: LegacyCallback) -> WorkerError {
        self.crashed_with(callback, None)
    }

    fn crashed_with(&self, callback: LegacyCallback, detail: Option<String>) -> WorkerError {
        // Best effort by construction: the tail is whatever the drain thread had
        // read when the crash was observed. Blocking here to collect the rest
        // would trade a diagnostic detail for a possible hang.
        let mut stderr_tail = self
            .stderr
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .render();
        if let Some(detail) = detail {
            if !stderr_tail.is_empty() {
                stderr_tail.push('\n');
            }
            stderr_tail.push_str(&format!(
                "[crikey: reading the legacy protocol channel failed: {detail}]"
            ));
        }

        WorkerError::Crashed {
            plugin: self.plugin.clone(),
            callback,
            status: self.reaped.and_then(|status| status.code()),
            stderr_tail,
        }
    }

    /// Ends the child, killing it only if it is still alive.
    ///
    /// Checking first is what keeps [`WorkerExit::hard_stopped`] honest: a child
    /// that already exited was not hard-stopped, and reporting otherwise would
    /// make every crash look like a host intervention.
    fn reap(&mut self) -> Option<ExitStatus> {
        self.link.close_stdin();

        let Some(mut child) = self.child.take() else {
            return self.reaped;
        };

        let status = match child.try_wait() {
            Ok(Some(status)) => Some(status),
            Ok(None) | Err(_) => {
                hard_kill(self.process_id, &mut child);
                self.hard_stopped = true;
                wait_bounded(&mut child, REAP_GRACE).or_else(|| {
                    reap_in_background(child);
                    None
                })
            }
        };
        self.reaped = status;
        self.reaped
    }

    /// Asks the child to exit, then makes sure it did (spec 24.3).
    fn stop_child(&mut self) -> WorkerExit {
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
        let _ = self.link.enqueue_without_wait(&json!({
            "id": id,
            "callback": CONTROL_SHUTDOWN,
            "payload": {},
        }));
        // Redundant on purpose: end of file on stdin means the same thing, and
        // a shim that missed the frame still sees the pipe close.
        self.link.close_stdin();

        // The child closing stdout is the event that says it is leaving.
        // Waiting for that rather than polling for an exit status keeps an
        // orderly shutdown as fast as the child is.
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
                match wait_bounded(&mut child, REAP_GRACE) {
                    Some(status) => status,
                    None => {
                        reap_in_background(child);
                        return WorkerExit {
                            code: None,
                            hard_stopped: true,
                        };
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
impl Drop for LegacyWorker {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.reap();
        }
    }
}

/// Hard-stops the child, reaching its whole process group on Unix.
///
/// On Unix the child is its own group leader (`process_group(0)` at spawn), so
/// signalling the group kills any grandchildren a plugin forked as well as the
/// leader (spec 24.3). Off Unix `std::process` offers no portable group kill,
/// so only the direct child is reached — an honest, documented limit rather
/// than a false claim of tree-kill. The caller polls for a bounded grace period
/// and hands any still-exiting child to a background reaper.
fn hard_kill(process_id: u32, child: &mut Child) {
    #[cfg(unix)]
    kill_process_group(process_id);
    #[cfg(not(unix))]
    {
        // Grandchildren are unreachable here; only the leader is killed below.
        let _ = process_id;
    }
    let _ = child.kill();
}

/// Sends `SIGKILL` to an entire process group (spec 24.3).
///
/// Killing a process *group* needs the `killpg(3)`/`kill(2)` syscall; the
/// standard library only kills a single child ([`Child::kill`]) and exposes no
/// safe wrapper for the group case. This is therefore the crate's only
/// `unsafe`, isolated to this one function and declaring `killpg` directly so
/// no new dependency is pulled in. Its arguments are a validated pid-as-pgid
/// (the group was created by `CommandExt::process_group(0)`) and a constant
/// signal; the call reads and writes no memory.
#[cfg(unix)]
fn kill_process_group(pgid: u32) {
    // `int killpg(int pgrp, int sig)` — POSIX. SIGKILL is 9 on Linux and macOS.
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

/// Continues reaping after the caller's bounded grace period expires. The
/// background poll owns the child handle, so the worker never returns a live
/// or zombie child while also blocking indefinitely in `wait()`.
fn reap_in_background(mut child: Child) {
    thread::spawn(move || loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(REAP_POLL_INTERVAL),
        }
    });
}
/// Waits for an exiting child's status, giving up after `budget`.
///
/// Polled because `std` offers no wait-with-timeout, and the alternative — a
/// blocking `wait` — would let a child that closed its pipes and then hung
/// block host shutdown forever.
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

fn millis(duration: Duration) -> Millis {
    Millis::try_from(duration.as_millis()).unwrap_or(Millis::MAX)
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

fn encode_request(id: u64, request: &LegacyRequest, terminate: bool) -> Value {
    let payload = match &request.kind {
        LegacyRequestKind::Start
        | LegacyRequestKind::Catalog
        | LegacyRequestKind::Activated
        | LegacyRequestKind::Deactivated => json!({}),
        LegacyRequestKind::InitialSuggest { query } => json!({
            "query": query,
            "initial": true,
            "selected_id": Value::Null,
        }),
        LegacyRequestKind::ArgumentSuggest { query, selected } => json!({
            "query": query,
            "initial": false,
            "selected_id": selected.0,
        }),
        LegacyRequestKind::Execute { item, action } => json!({
            "item": encode_item(item),
            "action": action.as_ref().map(encode_action),
        }),
        LegacyRequestKind::Events { flags } => json!({ "flags": flags.bits() }),
    };

    json!({
        "id": id,
        "callback": request.callback().as_str(),
        "plugin": request.plugin.0,
        "instance": request.instance.0,
        "generation": request.generation.get(),
        // The host's current flag, applied by the child when it dequeues this
        // request. That is what makes a flag raised *before* a call visible to
        // the callback's very first poll.
        "terminate": terminate,
        "payload": payload,
    })
}

/// Renders an item the way the documented legacy API spells it.
///
/// `icon_handle` is deliberately absent: it is an opaque in-process object and
/// cannot cross a process boundary. The hint names are the documented
/// `ItemArgsHint`/`ItemHitHint` member names lowercased, so the shim maps them
/// back to the enums a plugin actually sees.
fn encode_item(item: &Item) -> Value {
    let args_hint = match item.argument_policy {
        ArgumentPolicy::Forbidden => "forbidden",
        ArgumentPolicy::Optional => "accepted",
        ArgumentPolicy::Required => "required",
    };
    let hit_hint = match item.hit_policy {
        HitPolicy::Recorded => "keepall",
        HitPolicy::Ignored => "ignore",
    };

    json!({
        "category": item.category.as_str(),
        "label": item.label,
        "short_desc": item.description,
        "target": item.target,
        "args_hint": args_hint,
        "hit_hint": hit_hint,
        "loop_on_suggest": false,
        "data_bag": Value::Null,
    })
}

fn encode_action(action: &Action) -> Value {
    json!({
        "name": action.action_id.0,
        "label": action.label,
        "short_desc": action.description,
    })
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Turns one reply line into a response, or into the protocol failure it is.
///
/// Strict about shape at every step. A decoder that only asked "does this
/// parse" would accept a bare array and then index into something that has no
/// fields, and a decoder that ignored the echoed id would let a stale reply
/// answer a live request.
fn decode_response(
    request: &LegacyRequest,
    expected_id: u64,
    line: String,
) -> Result<LegacyResponse, WorkerError> {
    let callback = request.callback();
    let violation = |line: String| WorkerError::Protocol {
        plugin: request.plugin.clone(),
        callback,
        line,
    };

    let Ok(Value::Object(frame)) = serde_json::from_str::<Value>(&line) else {
        return Err(violation(line));
    };

    if frame.get("id").and_then(Value::as_u64) != Some(expected_id) {
        return Err(violation(line));
    }
    if frame.get("callback").and_then(Value::as_str) != Some(callback.as_str()) {
        return Err(violation(line));
    }
    let Some(succeeded) = frame.get("ok").and_then(Value::as_bool) else {
        return Err(violation(line));
    };

    let outcome = if succeeded {
        match decode_outcome(&request.plugin, callback, &frame) {
            Some(outcome) => outcome,
            None => return Err(violation(line)),
        }
    } else {
        match decode_failure(request, callback, &frame) {
            Some(failure) => LegacyOutcome::Failed(failure),
            None => return Err(violation(line)),
        }
    };
    let terminate_polls = match frame.get("terminate_polls") {
        None => 0,
        Some(value) => match value.as_u64().and_then(|polls| u32::try_from(polls).ok()) {
            Some(polls) => polls,
            None => return Err(violation(line)),
        },
    };
    let log = match decode_log(&frame) {
        Some(log) => log,
        None => return Err(violation(line)),
    };

    Ok(LegacyResponse {
        plugin: request.plugin.clone(),
        instance: request.instance,
        generation: request.generation,
        callback,
        outcome,
        log,
        terminate_polls,
    })
}

fn decode_outcome(
    plugin: &PluginId,
    callback: LegacyCallback,
    frame: &Map<String, Value>,
) -> Option<LegacyOutcome> {
    match frame.get("outcome").and_then(Value::as_str)? {
        "set_catalog" if callback == LegacyCallback::OnCatalog => {
            let items = decode_items(plugin, frame)?;
            // `merge_catalog` and `set_catalog` differ only in whether the
            // batch replaces the plugin's catalog (spec 14.8).
            Some(if frame.get("merge").and_then(Value::as_bool) == Some(true) {
                LegacyOutcome::MergeCatalog(items)
            } else {
                LegacyOutcome::SetCatalog(items)
            })
        }
        "suggestions" if callback == LegacyCallback::OnSuggest => {
            Some(LegacyOutcome::Suggestions(decode_items(plugin, frame)?))
        }
        "abandoned" if matches!(callback, LegacyCallback::OnCatalog | LegacyCallback::OnSuggest) => {
            Some(LegacyOutcome::Abandoned)
        }
        "executed" if callback == LegacyCallback::OnExecute => Some(LegacyOutcome::Executed),
        "acknowledged"
            if matches!(
                callback,
                LegacyCallback::OnStart
                    | LegacyCallback::OnActivated
                    | LegacyCallback::OnDeactivated
                    | LegacyCallback::OnEvents
            ) =>
        {
            Some(LegacyOutcome::Acknowledged)
        }
        _ => None,
    }
}

fn decode_failure(
    request: &LegacyRequest,
    callback: LegacyCallback,
    frame: &Map<String, Value>,
) -> Option<PluginException> {
    if frame.get("outcome").and_then(Value::as_str) != Some("failed") {
        return None;
    }
    let error = frame.get("error")?.as_object()?;
    if error.get("kind").and_then(Value::as_str) != Some("plugin-exception") {
        return None;
    }

    Some(PluginException {
        plugin: request.plugin.clone(),
        callback,
        exception_type: error.get("type")?.as_str()?.to_owned(),
        message: error.get("message")?.as_str()?.to_owned(),
        traceback: error.get("traceback")?.as_str()?.to_owned(),
    })
}

fn decode_items(plugin: &PluginId, frame: &Map<String, Value>) -> Option<Vec<Item>> {
    let entries = frame.get("items")?.as_array()?;
    let mut items = Vec::with_capacity(entries.len());
    for entry in entries {
        items.push(decode_item(plugin, entry)?);
    }
    Some(items)
}

/// Builds a core item from what a legacy plugin can express.
///
/// Ownership and identity are the host's to assign and never the plugin's to
/// claim: a plugin that could name another plugin's id could inject items into
/// its catalog (spec 10.2), and a legacy item carries no identifier of its own,
/// so the host derives a stable one from what it does carry.
fn decode_item(plugin: &PluginId, value: &Value) -> Option<Item> {
    let object = value.as_object()?;
    let category = decode_category(object.get("category")?.as_str()?);
    let target = object.get("target")?.as_str()?.to_owned();
    let stable_id = ItemId::derived(plugin, &category, &target);
    let argument_policy = decode_argument_policy(object.get("args_hint")?.as_str()?)?;
    let hit_policy = decode_hit_policy(object.get("hit_hint")?.as_str()?)?;

    Some(Item {
        stable_id,
        plugin_id: plugin.clone(),
        category,
        label: object.get("label")?.as_str()?.to_owned(),
        description: object.get("short_desc")?.as_str()?.to_owned(),
        target,
        // A legacy item has no search terms, no icon reference the host can
        // resolve, no score hint and no metadata of its own. Inventing any of
        // them here would fabricate ranking input.
        search_terms: Vec::new(),
        icon_reference: None,
        argument_policy,
        hit_policy,
        score_hint: 0,
        metadata: Default::default(),
        actions: Vec::new(),
    })
}

/// An unknown category is a plugin-defined one, never an error: the category
/// set is extensible by design (spec 10.3).
fn decode_category(name: &str) -> Category {
    match name {
        "application" => Category::Application,
        "file" => Category::File,
        "directory" => Category::Directory,
        "url" => Category::Url,
        "command" => Category::Command,
        "expression" => Category::Expression,
        "keyword" => Category::Keyword,
        "contact" => Category::Contact,
        "clipboard-item" => Category::ClipboardItem,
        other => Category::PluginDefined(other.to_owned()),
    }
}

/// Both the documented legacy spelling and CriKey's own are accepted, because
/// the shim maps from `ItemArgsHint` and a rename on either side must not
/// silently change an item's behaviour. An unknown hint *is* an error: guessing
/// would let an item accept arguments it was declared to forbid.
fn decode_argument_policy(hint: &str) -> Option<ArgumentPolicy> {
    match hint.to_ascii_lowercase().as_str() {
        "forbidden" => Some(ArgumentPolicy::Forbidden),
        "accepted" | "optional" => Some(ArgumentPolicy::Optional),
        "required" => Some(ArgumentPolicy::Required),
        _ => None,
    }
}

/// `NOARGS` records the hit like `KEEPALL` does: it constrains arguments, not
/// history, and the argument policy is where that already lives.
fn decode_hit_policy(hint: &str) -> Option<HitPolicy> {
    match hint.to_ascii_lowercase().as_str() {
        "keepall" | "keep_all" | "noargs" | "no_args" | "recorded" => Some(HitPolicy::Recorded),
        "ignore" | "ignored" => Some(HitPolicy::Ignored),
        _ => None,
    }
}

/// Retains a bounded, self-describing record of what the plugin printed.
fn decode_log(frame: &Map<String, Value>) -> Option<Vec<String>> {
    let Some(entries) = frame.get("log").and_then(Value::as_array) else {
        return Some(Vec::new());
    };

    let mut log: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    for entry in entries {
        let text = entry.as_str()?;
        if log.len() >= MAX_LOG_LINES {
            dropped += 1;
            continue;
        }
        log.push(clamp_log_line(text));
    }

    if dropped > 0 {
        // A truncated log says so. A log that silently ended would be read as
        // a plugin that silently stopped.
        log.push(format!(
            "[crikey: {dropped} further log line(s) dropped; a reply retains at most \
             {MAX_LOG_LINES}]"
        ));
    }
    Some(log)
}

fn clamp_log_line(text: &str) -> String {
    if text.len() <= MAX_LOG_LINE_BYTES {
        return text.to_owned();
    }

    let marker = format!("[crikey: log line truncated at {MAX_LOG_LINE_BYTES} bytes]");
    let prefix_limit = MAX_LOG_LINE_BYTES.saturating_sub(marker.len());
    let end = floor_char_boundary(text, prefix_limit);
    format!("{}{}", &text[..end], marker)
}
/// The largest index at or below `limit` that splits `text` between characters.
///
/// Hand-written because `str::floor_char_boundary` is still unstable, and
/// slicing a multi-byte character in half would panic on a plugin's output.
fn floor_char_boundary(text: &str, limit: usize) -> usize {
    let mut end = limit.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kind: LegacyRequestKind) -> LegacyRequest {
        LegacyRequest {
            plugin: PluginId("legacy.unit".to_owned()),
            instance: InstanceId(7),
            generation: Generation::from_raw(3),
            kind,
        }
    }

    #[test]
    fn a_reply_that_echoes_another_request_is_a_protocol_violation() {
        let asked = request(LegacyRequestKind::Catalog);
        let error = decode_response(
            &asked,
            2,
            r#"{"id":1,"ok":true,"outcome":"acknowledged"}"#.to_owned(),
        )
        .expect_err("a reply carrying the wrong id answers nothing");

        assert!(matches!(error, WorkerError::Protocol { .. }));
    }

    #[test]
    fn a_reply_for_the_wrong_callback_is_a_protocol_violation() {
        let asked = request(LegacyRequestKind::Catalog);
        let error = decode_response(
            &asked,
            1,
            r#"{"id":1,"ok":true,"callback":"on_suggest","outcome":"abandoned"}"#.to_owned(),
        )
        .expect_err("a callback cannot answer a different callback");

        assert!(matches!(error, WorkerError::Protocol { .. }));
    }

    #[test]
    fn a_shim_internal_failure_is_a_transport_defect_not_a_plugin_bug() {
        let asked = request(LegacyRequestKind::Catalog);
        let error = decode_response(
            &asked,
            1,
            r#"{"id":1,"ok":false,"error":{"kind":"shim-internal","message":"broken"}}"#.to_owned(),
        )
        .expect_err("only a plugin exception is a plugin failure");

        assert!(matches!(error, WorkerError::Protocol { .. }));
    }

    #[test]
    fn the_merge_flag_decides_between_replacing_and_extending_a_catalog() {
        let asked = request(LegacyRequestKind::Catalog);
        let frame = r#"{"id":1,"ok":true,"callback":"on_catalog","outcome":"set_catalog","merge":true,"items":[
            {"category":"keyword","label":"L","short_desc":"D","target":"T",
             "args_hint":"forbidden","hit_hint":"noargs"}]}"#;

        let response = decode_response(&asked, 1, frame.to_owned()).expect("a well-formed frame");
        match response.outcome {
            LegacyOutcome::MergeCatalog(items) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].hit_policy, HitPolicy::Recorded);
                assert_eq!(
                    items[0].stable_id,
                    ItemId::derived(&asked.plugin, &Category::Keyword, "T"),
                    "the host derives identity; the plugin never claims it"
                );
            }
            other => panic!("merge_catalog must not be reported as {other:?}"),
        }
    }

    #[test]
    fn an_unknown_argument_hint_is_refused_rather_than_guessed() {
        assert_eq!(decode_argument_policy("accepted"), Some(ArgumentPolicy::Optional));
        assert_eq!(decode_argument_policy("REQUIRED"), Some(ArgumentPolicy::Required));
        assert_eq!(decode_argument_policy("whatever"), None);
    }

    #[test]
    fn an_unknown_category_is_plugin_defined_rather_than_refused() {
        assert_eq!(
            decode_category("dev.example.thing"),
            Category::PluginDefined("dev.example.thing".to_owned())
        );
    }

    #[test]
    fn a_log_is_bounded_and_says_when_it_was_truncated() {
        let entries: Vec<Value> = (0..MAX_LOG_LINES + 5)
            .map(|index| Value::String(format!("line {index}")))
            .collect();
        let mut frame = Map::new();
        frame.insert("log".to_owned(), Value::Array(entries));

        let log = decode_log(&frame).expect("a log array has string entries");
        assert_eq!(log.len(), MAX_LOG_LINES + 1);
        assert!(log[MAX_LOG_LINES].contains("5 further log line(s) dropped"));
    }

    #[test]
    fn an_over_long_log_line_is_cut_on_a_character_boundary() {
        let long = "é".repeat(MAX_LOG_LINE_BYTES);
        let mut frame = Map::new();
        frame.insert("log".to_owned(), Value::Array(vec![Value::String(long.clone())]));
        let log = decode_log(&frame).expect("a log array has string entries");
        assert!(log[0].len() <= MAX_LOG_LINE_BYTES);
        assert!(log[0].len() < long.len());
        assert!(log[0].contains("truncated"));
    }

    #[test]
    fn an_over_long_line_is_a_bounded_protocol_failure_not_unbounded_growth() {
        let oversized = vec![b'x'; MAX_FRAME_BYTES + 4096];
        let mut reader = BufReader::new(oversized.as_slice());
        let mut line = Vec::new();

        match read_frame_line(&mut reader, &mut line).expect("reading a capped line succeeds") {
            LineRead::Oversized(bytes) => assert_eq!(bytes, MAX_FRAME_BYTES),
            other => panic!("an over-long line must be refused, got {other:?}"),
        }
    }

    #[test]
    fn the_stderr_tail_keeps_the_output_nearest_the_crash() {
        let mut tail = StderrTail::default();
        for index in 0..4096 {
            tail.push(format!("line {index}"));
        }

        let rendered = tail.render();
        assert!(rendered.len() <= MAX_STDERR_TAIL_BYTES);
        assert!(
            rendered.contains("line 4095"),
            "the newest line survives; it is the one that explains the crash"
        );
        assert!(!rendered.contains("line 0\n"), "the oldest lines are dropped");
    }

    #[test]
    fn a_single_huge_stderr_line_keeps_a_bounded_tail() {
        let mut tail = StderrTail::default();
        tail.push("x".repeat(MAX_STDERR_TAIL_BYTES + 128));
        let rendered = tail.render();
        assert!(rendered.len() <= MAX_STDERR_TAIL_BYTES);
        assert!(rendered.contains("retaining its tail"));
        assert!(rendered.ends_with(&"x".repeat(64)));
    }

    #[test]
    fn a_request_frame_carries_the_envelope_the_reply_must_echo() {
        let asked = request(LegacyRequestKind::InitialSuggest {
            query: "hello".to_owned(),
        });
        let frame = encode_request(11, &asked, true);

        assert_eq!(frame["id"], json!(11));
        assert_eq!(frame["callback"], json!("on_suggest"));
        assert_eq!(frame["instance"], json!(7));
        assert_eq!(frame["generation"], json!(3));
        assert_eq!(frame["terminate"], json!(true));
        assert_eq!(frame["payload"]["initial"], json!(true));
        assert_eq!(frame["payload"]["selected_id"], Value::Null);
    }

    #[test]
    fn a_frame_never_contains_a_raw_newline() {
        let asked = request(LegacyRequestKind::ArgumentSuggest {
            query: "line one\nline two".to_owned(),
            selected: ItemId("chosen".to_owned()),
        });

        let encoded = serde_json::to_string(&encode_request(1, &asked, false)).expect("a frame serialises");
        assert!(
            !encoded.contains('\n'),
            "one object per line: an unescaped newline would split one frame into two"
        );
    }
}
