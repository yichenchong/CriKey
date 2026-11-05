//! Lazy, one-surface supervisor for the out-of-process WPE host.
//!
//! The launcher thread only writes bounded mailboxes and polls completed
//! output. Executable discovery, process creation, endpoint acceptance, framed
//! I/O, and teardown all stay on the manager thread below.

use std::collections::VecDeque;
#[cfg(target_os = "linux")]
use std::ffi::{CString, OsString};
#[cfg(target_os = "linux")]
use std::fmt::Write as _;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io::Write as _;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(target_os = "linux")]
use std::os::unix::fs::DirBuilderExt;
#[cfg(all(target_os = "linux", test))]
use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;
#[cfg(target_os = "linux")]
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use crate::create_private_dir;

use crikey_core::{PluginId, WebStorage};
use crikey_native_protocol::message::{
    CaretArea, CloseSurface, Envelope, Gone, ImeEvent, LoadState, Navigate, OpenSurface, Payload, RawInput,
    Resize, WebFrame, WebGoneCode, WebImeCode, WebInputCode, WebLoadCode, WebStorageMode,
};
#[cfg(target_os = "linux")]
use crikey_native_protocol::transport::Listener;
use crikey_native_protocol::transport::Transport;
#[cfg(target_os = "linux")]
use crikey_native_protocol::Endpoint;
use crikey_native_protocol::ProtocolError;
use crikey_ui::{WebImeEvent, WebSurfaceFrame, WebSurfaceInput};

#[cfg(target_os = "linux")]
pub(crate) const ENV_WEB_HOST_OVERRIDE: &str = "CRIKEY_WEB_HOST";
#[cfg(target_os = "linux")]
const WEB_HOST_BINARY: &str = "crikey-web-host";
const COMMAND_QUEUE_CAPACITY: usize = 64;
const NOTICE_QUEUE_CAPACITY: usize = 8;
#[cfg(target_os = "linux")]
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const READ_POLL: Duration = Duration::from_millis(12);
#[cfg(target_os = "linux")]
const WRITE_TIMEOUT: Duration = Duration::from_millis(250);
const MANAGER_IDLE_POLL: Duration = Duration::from_millis(25);
// Bound diagnostic-only reads after a failed write independently of how much
// stale output a peer buffered. Each receive retains its configured timeout.
const MAX_FAILURE_DIAGNOSTIC_MESSAGES: usize = 64;
#[cfg(target_os = "linux")]
const MIN_WEB_MEMORY_BYTES: u64 = 256 * 1024 * 1024;
#[cfg(target_os = "linux")]
const MAX_WEB_MEMORY_BYTES: u64 = 768 * 1024 * 1024;
#[cfg(target_os = "linux")]
const PROCESS_TERMINATION_PASSES: usize = 20;
#[cfg(target_os = "linux")]
const PROCESS_TERMINATION_POLL: Duration = Duration::from_millis(10);

pub(crate) type WebWake = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug)]
pub(crate) enum WebHostNotice {
    Frame(Arc<WebSurfaceFrame>),
    Caret {
        surface_id: u64,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    },
    Fatal {
        surface_id: u64,
        message: String,
    },
}

#[derive(Debug)]
struct OpenRequest {
    surface_id: u64,
    #[cfg(target_os = "linux")]
    plugin: PluginId,
    pixel_width: u32,
    pixel_height: u32,
    url: String,
    storage: WebStorage,
    #[cfg(target_os = "linux")]
    storage_root: PathBuf,
}

#[derive(Debug)]
enum Control {
    Open(OpenRequest),
    Navigate {
        surface_id: u64,
        url: String,
    },
    Resize {
        surface_id: u64,
        pixel_width: u32,
        pixel_height: u32,
    },
    Input(Input),
    Close {
        surface_id: u64,
        reason: String,
    },
}

#[derive(Debug)]
struct Input {
    surface_id: u64,
    payload: Payload,
}

#[derive(Debug, Default)]
struct Mailbox {
    latest_frame: Option<Arc<WebSurfaceFrame>>,
    latest_caret: Option<(u64, f32, f32, f32, f32)>,
    notices: VecDeque<WebHostNotice>,
}

impl Mailbox {
    fn clear_surface(&mut self, surface_id: u64) {
        if self
            .latest_frame
            .as_ref()
            .is_some_and(|frame| frame.surface_id == surface_id)
        {
            self.latest_frame = None;
        }
        if self.latest_caret.is_some_and(|(id, _, _, _, _)| id == surface_id) {
            self.latest_caret = None;
        }
        self.notices.retain(|notice| match notice {
            WebHostNotice::Fatal { surface_id: id, .. } => *id != surface_id,
            WebHostNotice::Frame(frame) => frame.surface_id != surface_id,
            WebHostNotice::Caret { surface_id: id, .. } => *id != surface_id,
        });
    }

    fn push_notice(&mut self, notice: WebHostNotice) {
        if self.notices.len() == NOTICE_QUEUE_CAPACITY {
            self.notices.pop_front();
        }
        self.notices.push_back(notice);
    }
}

/// One manager for the process-wide single visible plugin page.
///
/// Construction starts one sleeping Rust thread, not an engine. The WPE host
/// is discovered and spawned only after [`Self::open`] receives an admitted
/// `PageFrame.web` declaration.
pub(crate) struct WebHostSupervisor {
    command: SyncSender<Control>,
    mailbox: Arc<Mutex<Mailbox>>,
    desired_surface: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    next_surface_id: u64,
    #[cfg(target_os = "linux")]
    storage_root: PathBuf,
    manager: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for WebHostSupervisor {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = out.debug_struct("WebHostSupervisor");
        debug
            .field("desired_surface", &self.desired_surface.load(Ordering::Acquire))
            .field("next_surface_id", &self.next_surface_id);
        #[cfg(target_os = "linux")]
        debug.field("storage_root", &self.storage_root);
        debug.finish()
    }
}

impl WebHostSupervisor {
    pub(crate) fn new(storage_root: PathBuf, wake: WebWake) -> Self {
        Self::with_spawner(storage_root, wake, Arc::new(ProductionSpawner))
    }

    fn with_spawner(storage_root: PathBuf, wake: WebWake, spawner: Arc<dyn HostSpawner>) -> Self {
        #[cfg(not(target_os = "linux"))]
        let _ = storage_root;
        let (command_tx, command_rx) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let desired_surface = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let manager_mailbox = Arc::clone(&mailbox);
        let manager_desired = Arc::clone(&desired_surface);
        let manager_shutdown = Arc::clone(&shutdown);
        let manager = std::thread::Builder::new()
            .name("crikey-web-supervisor".to_owned())
            .spawn(move || {
                manager_loop(
                    command_rx,
                    manager_mailbox,
                    manager_desired,
                    manager_shutdown,
                    wake,
                    spawner,
                );
            })
            .expect("the web supervisor thread must start");
        Self {
            command: command_tx,
            mailbox,
            desired_surface,
            shutdown,
            next_surface_id: 1,
            #[cfg(target_os = "linux")]
            storage_root,
            manager: Some(manager),
        }
    }

    /// Allocates a nonzero launcher-owned id and queues one admitted surface.
    ///
    /// A queued replacement is ordered after teardown of the current process by
    /// the single manager thread; there is never a second live engine.
    pub(crate) fn open(
        &mut self,
        plugin: PluginId,
        pixel_width: u32,
        pixel_height: u32,
        url: String,
        storage: WebStorage,
    ) -> Result<u64, String> {
        if pixel_width == 0 || pixel_height == 0 {
            return Err("the web surface viewport must be at least one pixel on each side".to_owned());
        }
        let surface_id = self.next_surface_id;
        if surface_id == 0 {
            return Err("the web surface id space is exhausted".to_owned());
        }
        self.next_surface_id = surface_id.checked_add(1).unwrap_or(0);
        #[cfg(not(target_os = "linux"))]
        let _ = plugin;
        let request = OpenRequest {
            surface_id,
            #[cfg(target_os = "linux")]
            plugin,
            pixel_width,
            pixel_height,
            url,
            storage,
            #[cfg(target_os = "linux")]
            storage_root: self.storage_root.clone(),
        };
        queue_surface_open(&self.mailbox, &self.desired_surface, surface_id, || {
            self.command
                .try_send(Control::Open(request))
                .map_err(|error| queue_error("open a web surface", error))
        })?;
        Ok(surface_id)
    }

    pub(crate) fn navigate(&self, surface_id: u64, url: String) -> Result<(), String> {
        if self.desired_surface.load(Ordering::Acquire) != surface_id || url.is_empty() {
            return Ok(());
        }
        self.command
            .try_send(Control::Navigate { surface_id, url })
            .map_err(|error| queue_error("navigate the web surface", error))
    }

    pub(crate) fn resize(&self, surface_id: u64, pixel_width: u32, pixel_height: u32) -> Result<(), String> {
        if self.desired_surface.load(Ordering::Acquire) != surface_id {
            return Ok(());
        }
        if pixel_width == 0 || pixel_height == 0 {
            return Err("the web surface viewport must be at least one pixel on each side".to_owned());
        }
        self.command
            .try_send(Control::Resize {
                surface_id,
                pixel_width,
                pixel_height,
            })
            .map_err(|error| queue_error("resize the web surface", error))
    }

    pub(crate) fn send_input(&self, input: WebSurfaceInput) -> Result<(), String> {
        let raw = raw_input(input);
        let surface_id = raw.surface_id;
        if self.desired_surface.load(Ordering::Acquire) != surface_id {
            return Ok(());
        }
        self.command
            .try_send(Control::Input(Input {
                surface_id,
                payload: Payload::RawInput(raw),
            }))
            .map_err(input_queue_error)
    }

    pub(crate) fn send_ime(&self, event: WebImeEvent) -> Result<(), String> {
        let event = ime_event(event);
        let surface_id = event.surface_id;
        if self.desired_surface.load(Ordering::Acquire) != surface_id {
            return Ok(());
        }
        self.command
            .try_send(Control::Input(Input {
                surface_id,
                payload: Payload::ImeEvent(event),
            }))
            .map_err(input_queue_error)
    }

    pub(crate) fn close(&self, reason: impl Into<String>) {
        let surface_id = self.desired_surface.swap(0, Ordering::AcqRel);
        if surface_id == 0 {
            return;
        }
        lock_recover(&self.mailbox).clear_surface(surface_id);
        // A full command queue cannot keep a surface alive: the manager also
        // compares its active id to `desired_surface` on every turn. This send
        // preserves the human-readable reason when capacity is available.
        let _ = self.command.try_send(Control::Close {
            surface_id,
            reason: reason.into(),
        });
    }

    /// Takes bounded completed output without waiting for the host.
    pub(crate) fn poll(&self) -> Vec<WebHostNotice> {
        let mut mailbox = lock_recover(&self.mailbox);
        let desired = self.desired_surface.load(Ordering::Acquire);
        let mut output = Vec::with_capacity(4);
        if let Some(frame) = mailbox.latest_frame.take() {
            if desired != 0 && frame.surface_id == desired {
                output.push(WebHostNotice::Frame(frame));
            }
        }
        if let Some((surface_id, x, y, width, height)) = mailbox.latest_caret.take() {
            if desired != 0 && surface_id == desired {
                output.push(WebHostNotice::Caret {
                    surface_id,
                    x,
                    y,
                    width,
                    height,
                });
            }
        }
        while let Some(notice) = mailbox.notices.pop_front() {
            let current = match &notice {
                WebHostNotice::Fatal { surface_id, .. } => *surface_id == desired,
                WebHostNotice::Frame(frame) => frame.surface_id == desired,
                WebHostNotice::Caret { surface_id, .. } => *surface_id == desired,
            };
            if current {
                output.push(notice);
            }
        }
        output
    }
}

impl Drop for WebHostSupervisor {
    fn drop(&mut self) {
        self.desired_surface.store(0, Ordering::Release);
        self.shutdown.store(true, Ordering::Release);
        if let Some(manager) = self.manager.take() {
            let _ = manager.join();
        }
    }
}

/// Clears the old mailbox before making a new surface observable to the
/// manager. Publications take the same lock before checking ownership, so an
/// old publisher cannot overwrite a replacement while waiting for this lock.
fn queue_surface_open(
    mailbox: &Mutex<Mailbox>,
    desired_surface: &AtomicU64,
    surface_id: u64,
    enqueue: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    {
        let mut mailbox = lock_recover(mailbox);
        mailbox.latest_frame = None;
        mailbox.latest_caret = None;
        desired_surface.store(surface_id, Ordering::Release);
    }
    if let Err(error) = enqueue() {
        desired_surface.store(0, Ordering::Release);
        return Err(error);
    }
    Ok(())
}

fn queue_error<T>(operation: &str, error: TrySendError<T>) -> String {
    match error {
        TrySendError::Full(_) => format!("cannot {operation}: the bounded web-host command queue is full"),
        TrySendError::Disconnected(_) => {
            format!("cannot {operation}: the web-host supervisor is not running")
        }
    }
}

fn input_queue_error(error: TrySendError<Control>) -> String {
    match error {
        TrySendError::Full(_) => {
            "the bounded web-host command queue is full; the surface is not answering input".to_owned()
        }
        TrySendError::Disconnected(_) => "the web-host supervisor is not running".to_owned(),
    }
}

fn raw_input(input: WebSurfaceInput) -> RawInput {
    match input {
        WebSurfaceInput::Key {
            surface_id,
            timestamp_ms,
            pressed,
            keysym,
            hardware_keycode,
            repeat,
            modifiers,
        } => RawInput {
            surface_id,
            kind: if pressed {
                WebInputCode::KeyDown
            } else {
                WebInputCode::KeyUp
            },
            timestamp_ms,
            keysym,
            hardware_keycode,
            repeat,
            modifiers,
            ..RawInput::default()
        },
        WebSurfaceInput::PointerMove {
            surface_id,
            timestamp_ms,
            x,
            y,
            modifiers,
        } => RawInput {
            surface_id,
            kind: WebInputCode::PointerMove,
            timestamp_ms,
            x,
            y,
            modifiers,
            ..RawInput::default()
        },
        WebSurfaceInput::PointerButton {
            surface_id,
            timestamp_ms,
            x,
            y,
            button,
            pressed,
            press_count,
            modifiers,
        } => RawInput {
            surface_id,
            kind: if pressed {
                WebInputCode::PointerDown
            } else {
                WebInputCode::PointerUp
            },
            timestamp_ms,
            x,
            y,
            modifiers,
            button,
            press_count,
            ..RawInput::default()
        },
        WebSurfaceInput::Scroll {
            surface_id,
            timestamp_ms,
            x,
            y,
            delta_x,
            delta_y,
            precise,
            modifiers,
            stop,
        } => RawInput {
            surface_id,
            kind: WebInputCode::Scroll,
            timestamp_ms,
            x,
            y,
            delta_x,
            delta_y,
            precise,
            modifiers,
            stop,
            ..RawInput::default()
        },
        WebSurfaceInput::Focus { surface_id, focused } => RawInput {
            surface_id,
            kind: if focused {
                WebInputCode::FocusIn
            } else {
                WebInputCode::FocusOut
            },
            ..RawInput::default()
        },
    }
}

fn ime_event(event: WebImeEvent) -> ImeEvent {
    match event {
        WebImeEvent::Preedit {
            surface_id,
            text,
            cursor,
        } => {
            let (cursor_begin_chars, cursor_end_chars, has_cursor) = cursor
                .map(|(begin, end)| (begin, end, true))
                .unwrap_or((0, 0, false));
            ImeEvent {
                surface_id,
                kind: WebImeCode::Preedit,
                text,
                cursor_begin_chars,
                cursor_end_chars,
                has_cursor,
                ..ImeEvent::default()
            }
        }
        WebImeEvent::Commit { surface_id, text } => ImeEvent {
            surface_id,
            kind: WebImeCode::Commit,
            text,
            ..ImeEvent::default()
        },
    }
}

trait HostSpawner: Send + Sync {
    fn spawn(&self, request: &OpenRequest) -> Result<RunningHost, String>;
}

trait ManagedProcess: Send {
    fn try_wait(&mut self) -> Result<Option<ExitStatus>, String>;
    fn terminate(&mut self);
}

struct RunningHost {
    transport: Box<dyn Transport>,
    process: Box<dyn ManagedProcess>,
}

impl std::fmt::Debug for RunningHost {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("RunningHost")
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct ActiveHost {
    surface_id: u64,
    pixel_width: u32,
    pixel_height: u32,
    url: String,
    latest_generation: u64,
    runtime: RunningHost,
}

#[derive(Debug)]
struct ProductionSpawner;

impl HostSpawner for ProductionSpawner {
    fn spawn(&self, request: &OpenRequest) -> Result<RunningHost, String> {
        spawn_production(request)
    }
}

fn manager_loop(
    command: Receiver<Control>,
    mailbox: Arc<Mutex<Mailbox>>,
    desired_surface: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    wake: WebWake,
    spawner: Arc<dyn HostSpawner>,
) {
    let mut active: Option<ActiveHost> = None;
    while !shutdown.load(Ordering::Acquire) {
        let desired = desired_surface.load(Ordering::Acquire);
        if active.as_ref().is_some_and(|host| host.surface_id != desired) {
            teardown(&mut active, "surface was dismissed or replaced");
        }

        let mut did_work = false;
        for _ in 0..COMMAND_QUEUE_CAPACITY {
            match command.try_recv() {
                Ok(next) => {
                    did_work = true;
                    handle_control(
                        next,
                        &mut active,
                        &mailbox,
                        &desired_surface,
                        &wake,
                        spawner.as_ref(),
                    );
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    shutdown.store(true, Ordering::Release);
                    break;
                }
            }
        }

        if let Some(host) = active.as_mut() {
            let exit_status = match host.runtime.process.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    let surface_id = host.surface_id;
                    publish_fatal(
                        &mailbox,
                        &desired_surface,
                        &wake,
                        surface_id,
                        format!("cannot inspect the web host process: {error}"),
                    );
                    teardown(&mut active, "process inspection failed");
                    continue;
                }
            };

            // The host flushes Gone/load failures before exiting. Its process
            // can already be reaped while complete messages remain buffered on
            // the socket. Drain one envelope per manager turn before replacing
            // that diagnostic with a generic exit status; ownership and shutdown
            // are still checked between envelopes.
            match host.runtime.transport.recv() {
                Ok(envelope) => {
                    if let Some(reason) = handle_incoming(envelope, host, &mailbox, &desired_surface, &wake) {
                        teardown(&mut active, &reason);
                    }
                }
                Err(ProtocolError::Timeout) if exit_status.is_none() => {}
                Err(error) => {
                    let surface_id = host.surface_id;
                    let message = exit_status.map_or_else(
                        || format!("web host connection failed: {error}"),
                        |status| format!("web host exited unexpectedly ({status})"),
                    );
                    publish_fatal(&mailbox, &desired_surface, &wake, surface_id, message);
                    teardown(
                        &mut active,
                        if exit_status.is_some() {
                            "host exited"
                        } else {
                            "connection failed"
                        },
                    );
                }
            }
        } else if !did_work {
            match command.recv_timeout(MANAGER_IDLE_POLL) {
                Ok(next) => handle_control(
                    next,
                    &mut active,
                    &mailbox,
                    &desired_surface,
                    &wake,
                    spawner.as_ref(),
                ),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }
    teardown(&mut active, "launcher is shutting down");
}

fn handle_control(
    command: Control,
    active: &mut Option<ActiveHost>,
    mailbox: &Arc<Mutex<Mailbox>>,
    desired_surface: &AtomicU64,
    wake: &WebWake,
    spawner: &dyn HostSpawner,
) {
    match command {
        Control::Open(request) => {
            if desired_surface.load(Ordering::Acquire) != request.surface_id {
                return;
            }
            teardown(active, "surface was replaced");
            let surface_id = request.surface_id;
            let pixel_width = request.pixel_width;
            let pixel_height = request.pixel_height;
            let url = request.url.clone();
            match spawner.spawn(&request) {
                Ok(mut runtime) => {
                    // Startup can outlive dismissal/replacement. Never open a
                    // document for an obsolete owner.
                    if desired_surface.load(Ordering::Acquire) != surface_id {
                        runtime.transport.close();
                        runtime.process.terminate();
                        return;
                    }
                    let storage_mode = match request.storage {
                        WebStorage::Ephemeral => WebStorageMode::Ephemeral,
                        WebStorage::Persistent => WebStorageMode::Persistent,
                    };
                    let opened = send_payload(
                        &mut runtime.transport,
                        Payload::OpenSurface(OpenSurface {
                            surface_id,
                            pixel_width,
                            pixel_height,
                            url: url.clone(),
                            storage_mode,
                            ..OpenSurface::default()
                        }),
                    );
                    if let Err(error) = opened {
                        runtime.transport.close();
                        runtime.process.terminate();
                        publish_fatal(
                            mailbox,
                            desired_surface,
                            wake,
                            surface_id,
                            format!("cannot open the web surface: {error}"),
                        );
                        return;
                    }
                    *active = Some(ActiveHost {
                        surface_id,
                        pixel_width,
                        pixel_height,
                        url,
                        latest_generation: 0,
                        runtime,
                    });
                }
                Err(error) => publish_fatal(mailbox, desired_surface, wake, surface_id, error),
            }
        }
        Control::Navigate { surface_id, .. }
        | Control::Resize { surface_id, .. }
        | Control::Input(Input { surface_id, .. })
            if desired_surface.load(Ordering::Acquire) != surface_id => {}
        Control::Navigate { surface_id, url } => {
            let Some(host) = active.as_mut().filter(|host| host.surface_id == surface_id) else {
                return;
            };
            if url.is_empty() || host.url == url {
                return;
            }
            match send_payload(
                &mut host.runtime.transport,
                Payload::Navigate(Navigate {
                    surface_id,
                    url: url.clone(),
                    ..Navigate::default()
                }),
            ) {
                Ok(()) => host.url = url,
                Err(error) => {
                    publish_control_failure(
                        host,
                        mailbox,
                        desired_surface,
                        wake,
                        format!("cannot navigate the web surface: {error}"),
                    );
                    teardown(active, "navigation transport failed");
                }
            }
        }
        Control::Resize {
            surface_id,
            pixel_width,
            pixel_height,
        } => {
            let Some(host) = active.as_mut().filter(|host| host.surface_id == surface_id) else {
                return;
            };
            if host.pixel_width == pixel_width && host.pixel_height == pixel_height {
                return;
            }
            match send_payload(
                &mut host.runtime.transport,
                Payload::Resize(Resize {
                    surface_id,
                    pixel_width,
                    pixel_height,
                    ..Resize::default()
                }),
            ) {
                Ok(()) => {
                    host.pixel_width = pixel_width;
                    host.pixel_height = pixel_height;
                }
                Err(error) => {
                    publish_control_failure(
                        host,
                        mailbox,
                        desired_surface,
                        wake,
                        format!("cannot resize the web surface: {error}"),
                    );
                    teardown(active, "resize transport failed");
                }
            }
        }
        Control::Input(event) => {
            let Some(host) = active.as_mut().filter(|host| host.surface_id == event.surface_id) else {
                return;
            };
            if let Err(error) = send_payload(&mut host.runtime.transport, event.payload) {
                publish_control_failure(
                    host,
                    mailbox,
                    desired_surface,
                    wake,
                    format!("web host input transport failed: {error}"),
                );
                teardown(active, "input transport failed");
            }
        }
        Control::Close { surface_id, reason } => {
            if active.as_ref().is_some_and(|host| host.surface_id == surface_id) {
                teardown(active, &reason);
            }
        }
    }
}

fn handle_incoming(
    envelope: Envelope,
    host: &mut ActiveHost,
    mailbox: &Arc<Mutex<Mailbox>>,
    desired_surface: &AtomicU64,
    wake: &WebWake,
) -> Option<String> {
    match envelope.payload {
        Some(Payload::WebFrame(frame)) => {
            if frame.surface_id != host.surface_id || frame.generation <= host.latest_generation {
                return None;
            }
            if let Err(error) = frame.validate() {
                let message = format!("web host protocol violation: {error}");
                publish_fatal(mailbox, desired_surface, wake, host.surface_id, message.clone());
                return Some(message);
            }
            host.latest_generation = frame.generation;
            publish_frame(mailbox, desired_surface, wake, frame);
            None
        }
        Some(Payload::CaretArea(caret)) => {
            if caret.surface_id != host.surface_id {
                return None;
            }
            if !caret.x.is_finite()
                || !caret.y.is_finite()
                || !caret.width.is_finite()
                || !caret.height.is_finite()
                || caret.width < 0.0
                || caret.height < 0.0
            {
                let message =
                    "web host protocol violation: caret area is not finite and non-negative".to_owned();
                publish_fatal(mailbox, desired_surface, wake, host.surface_id, message.clone());
                return Some(message);
            }
            publish_caret(mailbox, desired_surface, wake, caret);
            None
        }
        Some(Payload::LoadState(state)) => {
            handle_load_state(state, host.surface_id, mailbox, desired_surface, wake)
        }
        Some(Payload::Gone(gone)) => {
            if gone.surface_id != host.surface_id {
                return None;
            }
            let message = gone_message(&gone);
            publish_fatal(mailbox, desired_surface, wake, host.surface_id, message.clone());
            Some(message)
        }
        Some(other) => {
            let message = format!(
                "web host protocol violation: received unexpected `{}` message",
                other.kind()
            );
            publish_fatal(mailbox, desired_surface, wake, host.surface_id, message.clone());
            Some(message)
        }
        None => {
            let message = "web host protocol violation: received an empty envelope".to_owned();
            publish_fatal(mailbox, desired_surface, wake, host.surface_id, message.clone());
            Some(message)
        }
    }
}

fn handle_load_state(
    state: LoadState,
    surface_id: u64,
    mailbox: &Arc<Mutex<Mailbox>>,
    desired_surface: &AtomicU64,
    wake: &WebWake,
) -> Option<String> {
    if state.surface_id != surface_id {
        return None;
    }
    match state.state {
        WebLoadCode::Started | WebLoadCode::Redirected | WebLoadCode::Committed | WebLoadCode::Finished => {
            None
        }
        WebLoadCode::Failed => {
            let location = if state.url.is_empty() {
                "the requested page".to_owned()
            } else {
                format!("`{}`", state.url)
            };
            let detail = if state.failure.is_empty() {
                "the engine did not provide a reason".to_owned()
            } else {
                state.failure
            };
            let message = format!("web page load failed for {location}: {detail}");
            publish_fatal(mailbox, desired_surface, wake, surface_id, message.clone());
            Some(message)
        }
        WebLoadCode::StateUnspecified => {
            let message = "web host protocol violation: load state is unspecified".to_owned();
            publish_fatal(mailbox, desired_surface, wake, surface_id, message.clone());
            Some(message)
        }
    }
}

fn gone_message(gone: &Gone) -> String {
    let reason = match gone.reason {
        WebGoneCode::Closed => "closed",
        WebGoneCode::MemoryLimit => "exceeded its memory limit",
        WebGoneCode::Crashed => "crashed",
        WebGoneCode::StartupFailed => "failed to start",
        WebGoneCode::ProtocolViolation => "reported a protocol violation",
        WebGoneCode::ReasonUnspecified => "stopped for an unspecified reason",
    };
    if gone.detail.is_empty() {
        format!("web host {reason}")
    } else {
        format!("web host {reason}: {}", gone.detail)
    }
}

fn publish_frame(
    mailbox: &Arc<Mutex<Mailbox>>,
    desired_surface: &AtomicU64,
    wake: &WebWake,
    frame: WebFrame,
) {
    if desired_surface.load(Ordering::Acquire) != frame.surface_id {
        return;
    }
    let frame = Arc::new(WebSurfaceFrame {
        surface_id: frame.surface_id,
        generation: frame.generation,
        pixel_width: frame.pixel_width,
        pixel_height: frame.pixel_height,
        rgba8: Arc::from(frame.rgba8),
    });
    let mut mailbox = lock_recover(mailbox);
    if desired_surface.load(Ordering::Acquire) != frame.surface_id {
        return;
    }
    mailbox.latest_frame = Some(frame);
    drop(mailbox);
    wake();
}

fn publish_caret(
    mailbox: &Arc<Mutex<Mailbox>>,
    desired_surface: &AtomicU64,
    wake: &WebWake,
    caret: CaretArea,
) {
    let mut mailbox = lock_recover(mailbox);
    if desired_surface.load(Ordering::Acquire) != caret.surface_id {
        return;
    }
    mailbox.latest_caret = Some((caret.surface_id, caret.x, caret.y, caret.width, caret.height));
    drop(mailbox);
    wake();
}

fn publish_fatal(
    mailbox: &Arc<Mutex<Mailbox>>,
    desired_surface: &AtomicU64,
    wake: &WebWake,
    surface_id: u64,
    message: String,
) {
    let mut mailbox = lock_recover(mailbox);
    if desired_surface.load(Ordering::Acquire) != surface_id {
        return;
    }
    mailbox.latest_frame = None;
    mailbox.latest_caret = None;
    mailbox.push_notice(WebHostNotice::Fatal { surface_id, message });
    drop(mailbox);
    wake();
}

fn send_payload(transport: &mut Box<dyn Transport>, payload: Payload) -> Result<(), ProtocolError> {
    transport.send(&Envelope {
        payload: Some(payload),
        ..Envelope::default()
    })
}

/// A host may flush its terminal diagnostic and exit before a queued control
/// reaches it. Preserve that inbound reason, without retrying any failed write
/// or allowing teardown to write into a potentially partial outbound frame.
fn publish_control_failure(
    host: &mut ActiveHost,
    mailbox: &Arc<Mutex<Mailbox>>,
    desired_surface: &AtomicU64,
    wake: &WebWake,
    fallback: String,
) {
    let deadline = Instant::now() + MANAGER_IDLE_POLL;
    let mut terminal = false;
    let mut completed = 0;
    while completed < MAX_FAILURE_DIAGNOSTIC_MESSAGES {
        if Instant::now() >= deadline || desired_surface.load(Ordering::Acquire) != host.surface_id {
            break;
        }
        let envelope = match host.runtime.transport.recv() {
            Ok(envelope) => envelope,
            // FrameReader retains partial prefixes/bodies across timeouts.
            // Resume that same reader within the grace deadline; only complete
            // envelopes count toward the diagnostic-work cap.
            Err(ProtocolError::Timeout) => continue,
            Err(_) => break,
        };
        completed += 1;
        // Unknown, stale and nonterminal output cannot replace the original
        // control error. In particular, do not publish frames while retiring.
        let current_terminal = match envelope.payload.as_ref() {
            Some(Payload::LoadState(state)) => {
                state.surface_id == host.surface_id && state.state == WebLoadCode::Failed
            }
            Some(Payload::Gone(gone)) => gone.surface_id == host.surface_id,
            _ => false,
        };
        if current_terminal {
            terminal = handle_incoming(envelope, host, mailbox, desired_surface, wake).is_some();
            break;
        }
    }
    host.runtime.transport.close();
    if !terminal {
        publish_fatal(mailbox, desired_surface, wake, host.surface_id, fallback);
    }
}

fn teardown(active: &mut Option<ActiveHost>, reason: &str) {
    let Some(mut host) = active.take() else {
        return;
    };
    let _ = send_payload(
        &mut host.runtime.transport,
        Payload::CloseSurface(CloseSurface {
            surface_id: host.surface_id,
            reason: reason.to_owned(),
            ..CloseSurface::default()
        }),
    );
    host.runtime.transport.close();
    host.runtime.process.terminate();
}

fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(target_os = "linux")]
fn spawn_production(request: &OpenRequest) -> Result<RunningHost, String> {
    start_cgroup_supervised(CgroupLease::prepare, |cgroup| spawn_contained(request, cgroup))
}

/// Keep admission ahead of every host-execution path, with injectable setup
/// for deterministic refusal tests that do not require cgroup privileges.
#[cfg(target_os = "linux")]
fn start_cgroup_supervised<T>(
    prepare: impl FnOnce() -> Result<CgroupLease, String>,
    execute: impl FnOnce(CgroupLease) -> Result<T, String>,
) -> Result<T, String> {
    let cgroup = prepare().map_err(|error| {
        format!("web surface refused: required cgroup-v2 supervision is unavailable: {error}")
    })?;
    execute(cgroup)
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn spawn_contained(request: &OpenRequest, mut cgroup: CgroupLease) -> Result<RunningHost, String> {
    let executable = web_host_executable()?;
    let storage_dir = match request.storage {
        WebStorage::Ephemeral => None,
        WebStorage::Persistent => Some(persistent_storage_dir(&request.storage_root, &request.plugin)?),
    };
    let endpoint = PrivateEndpoint::create()?;
    let listener = Listener::bind(endpoint.endpoint())
        .map_err(|error| format!("cannot bind the private web-host endpoint: {error}"))?;

    let mut command = Command::new(&executable);
    command
        .arg("--endpoint")
        .arg(endpoint.argument())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(storage_dir) = storage_dir.as_ref() {
        command.arg("--storage-dir").arg(storage_dir);
    }
    let attachment = cgroup.attachment.as_raw_fd();
    let launcher_pid = std::process::id() as libc::pid_t;
    // SAFETY: this closure executes after fork and before exec. It calls only
    // async-signal-safe libc functions, creates a new process group/session,
    // and arranges graceful SIGTERM when the launcher dies. The host installs
    // SIGTERM cleanup; manager teardown escalates over the whole tree.
    unsafe {
        command.pre_exec(move || prepare_host_process(launcher_pid, attachment));
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot spawn `{}`: {error}", executable.display()))?;
    let mut transport = match listener.accept(Some(ACCEPT_TIMEOUT)) {
        Ok(transport) => transport,
        Err(error) => {
            terminate_child_tree(&mut child, &mut cgroup);
            return Err(format!(
                "web host did not connect to its private endpoint within {} ms: {error}",
                ACCEPT_TIMEOUT.as_millis()
            ));
        }
    };
    if let Err(error) = transport.set_read_timeout(Some(READ_POLL)) {
        transport.close();
        terminate_child_tree(&mut child, &mut cgroup);
        return Err(format!("cannot set the web-host receive deadline: {error}"));
    }
    if let Err(error) = transport.set_write_timeout(Some(WRITE_TIMEOUT)) {
        transport.close();
        terminate_child_tree(&mut child, &mut cgroup);
        return Err(format!("cannot set the web-host send deadline: {error}"));
    }
    drop(listener);
    drop(endpoint);
    Ok(RunningHost {
        transport,
        process: Box::new(ProductionProcess {
            child,
            cgroup,
            terminated: false,
        }),
    })
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn prepare_host_process(launcher_pid: libc::pid_t, attachment: RawFd) -> std::io::Result<()> {
    // SAFETY: this function is used only in pre_exec; libc calls here are
    // async-signal-safe and attachment formats the pid on the stack.
    unsafe {
        if libc::setsid() < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // A dead launcher may have been replaced by a subreaper, not pid 1.
        if libc::getppid() != launcher_pid {
            libc::_exit(127);
        }
    }
    attach_current_process(attachment)
}

#[cfg(not(target_os = "linux"))]
fn spawn_production(_request: &OpenRequest) -> Result<RunningHost, String> {
    Err("web surfaces require Linux and an installed WPE web host; this launcher remains available without them"
        .to_owned())
}

#[cfg(target_os = "linux")]
fn host_file_name() -> String {
    format!("{WEB_HOST_BINARY}{}", std::env::consts::EXE_SUFFIX)
}

#[cfg(target_os = "linux")]
fn web_host_executable() -> Result<PathBuf, String> {
    resolve_web_host_executable(
        std::env::var_os(ENV_WEB_HOST_OVERRIDE),
        std::env::current_exe().ok(),
        std::env::var_os("PATH"),
    )
}

#[cfg(target_os = "linux")]
fn resolve_web_host_executable(
    configured: Option<OsString>,
    launcher: Option<PathBuf>,
    search_path: Option<OsString>,
) -> Result<PathBuf, String> {
    if let Some(configured) = configured {
        if configured.is_empty() {
            return Err(format!("{ENV_WEB_HOST_OVERRIDE} is set but empty"));
        }
        let path = PathBuf::from(configured);
        return executable_file(&path).ok_or_else(|| {
            format!(
                "{ENV_WEB_HOST_OVERRIDE} names `{}`, which is not an executable file",
                path.display()
            )
        });
    }

    let name = host_file_name();
    if let Some(executable) = launcher {
        if let Some(directory) = executable.parent() {
            if let Some(beside) = executable_file(&directory.join(&name)) {
                return Ok(beside);
            }
            // Cargo's test harnesses live in `target/<profile>/deps`; the host
            // built for the same profile lives one directory above.
            if directory.file_name().is_some_and(|part| part == "deps") {
                if let Some(profile) = directory.parent() {
                    if let Some(beside_profile) = executable_file(&profile.join(&name)) {
                        return Ok(beside_profile);
                    }
                }
            }
        }
    }
    if let Some(path) = search_path {
        for directory in std::env::split_paths(&path) {
            if let Some(candidate) = executable_file(&directory.join(&name)) {
                return Ok(candidate);
            }
        }
    }

    Err(format!(
        "{WEB_HOST_BINARY} is unavailable; install it beside the launcher, put it on PATH, or set \
         {ENV_WEB_HOST_OVERRIDE} to an executable file"
    ))
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn executable_file(path: &Path) -> Option<PathBuf> {
    if !path.is_file() {
        return None;
    }
    let encoded = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `encoded` is a live NUL-terminated path; access changes no state.
    if unsafe { libc::access(encoded.as_ptr(), libc::X_OK) } != 0 {
        return None;
    }
    // Command treats a bare relative filename as a PATH lookup, even for an
    // explicit override. Resolve it now so discovery and execution agree.
    path.canonicalize().ok()
}

#[cfg(target_os = "linux")]
fn persistent_storage_dir(root: &Path, plugin: &PluginId) -> Result<PathBuf, String> {
    create_private_dir(root)?;
    let mut encoded = String::with_capacity(plugin.0.len() * 2);
    for byte in plugin.0.as_bytes() {
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    // Preserve existing stores that fit one Linux filename. Longer, otherwise
    // valid owner identities need bounded components, not truncation (which
    // would merge owners). The non-hex names separate branch/profile roots
    // from encoded identity components, including prefix-related owners.
    let mut directory = root.to_owned();
    if encoded.len() <= 254 {
        directory.push(&encoded);
    } else {
        directory.push("_long");
        create_private_dir(&directory)?;
        let mut remaining = encoded.as_str();
        while !remaining.is_empty() {
            let (component, rest) = remaining.split_at(remaining.len().min(254));
            directory.push(component);
            create_private_dir(&directory)?;
            remaining = rest;
        }
        directory.push("profile");
    }
    create_private_dir(&directory)?;
    let absolute = directory.canonicalize().map_err(|error| {
        format!(
            "cannot resolve persistent web storage `{}`: {error}",
            directory.display()
        )
    })?;
    if absolute.as_os_str().as_encoded_bytes().contains(&0) {
        return Err("persistent web storage path contains a NUL byte".to_owned());
    }
    Ok(absolute)
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct PrivateEndpoint {
    directory: PathBuf,
    endpoint: Endpoint,
}

#[cfg(target_os = "linux")]
impl PrivateEndpoint {
    fn create() -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        for _ in 0..32 {
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let directory =
                std::env::temp_dir().join(format!("crikey-web-{}-{sequence}", std::process::id()));
            match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => {
                    let private = Self {
                        endpoint: Endpoint::UnixSocket(directory.join("host.sock")),
                        directory,
                    };
                    // Creation never grants other accounts access, but umask
                    // can also remove owner search permission. Restore 0700
                    // before binding; RAII removes the directory on failure.
                    create_private_dir(&private.directory)?;
                    return Ok(private);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "cannot create private web-host endpoint directory `{}`: {error}",
                        directory.display()
                    ));
                }
            }
        }
        Err("cannot allocate a unique private web-host endpoint directory".to_owned())
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    // Unlike the shared text endpoint spelling, this private argv boundary
    // must preserve native TMPDIR bytes through the host's args_os parser.
    fn argument(&self) -> OsString {
        let Endpoint::UnixSocket(path) = &self.endpoint else {
            unreachable!("the private Linux web-host endpoint is always a Unix socket");
        };
        let mut argument = OsString::with_capacity(b"unix:".len() + path.as_os_str().as_bytes().len());
        argument.push("unix:");
        argument.push(path);
        argument
    }
}

#[cfg(target_os = "linux")]
impl Drop for PrivateEndpoint {
    fn drop(&mut self) {
        if let Endpoint::UnixSocket(path) = &self.endpoint {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir(&self.directory);
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct MemoryPolicy {
    bytes: u64,
    hierarchies: Vec<CgroupHierarchy>,
}

#[cfg(target_os = "linux")]
impl MemoryPolicy {
    fn detect() -> Result<Self, String> {
        Self::from_hierarchies(physical_memory_bytes()?, CgroupHierarchy::detect()?)
    }

    fn from_hierarchies(physical: u64, hierarchies: Vec<CgroupHierarchy>) -> Result<Self, String> {
        let mut inherited: Option<u64> = None;
        // Read-only mounts can expose limits hidden above a writable delegated
        // bind mount. Keep every visible original-launcher limit even when a
        // different mount supplies the usable host parent.
        for hierarchy in &hierarchies {
            if let Some(limit) = inherited_memory_limit(&hierarchy.launcher, &hierarchy.root)? {
                inherited = Some(inherited.map_or(limit, |current| current.min(limit)));
            }
        }
        let bytes = web_memory_ceiling(physical, inherited)?;
        Ok(Self { bytes, hierarchies })
    }
}

#[cfg(target_os = "linux")]
fn web_memory_ceiling(physical: u64, inherited: Option<u64>) -> Result<u64, String> {
    let requested = MAX_WEB_MEMORY_BYTES.min(physical / 8);
    let effective = inherited.map_or(requested, |limit| requested.min(limit));
    if effective < MIN_WEB_MEMORY_BYTES {
        return Err(format!(
            "its effective memory ceiling is {} MiB, below the 256 MiB minimum",
            effective / (1024 * 1024)
        ));
    }
    Ok(effective)
}

#[cfg(target_os = "linux")]
fn physical_memory_bytes() -> Result<u64, String> {
    let text = std::fs::read_to_string("/proc/meminfo")
        .map_err(|error| format!("cannot read physical memory size: {error}"))?;
    let line = text
        .lines()
        .find(|line| line.starts_with("MemTotal:"))
        .ok_or_else(|| "physical memory size is absent from /proc/meminfo".to_owned())?;
    let kib = line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "physical memory size in /proc/meminfo has no value".to_owned())?
        .parse::<u64>()
        .map_err(|error| format!("physical memory size is invalid: {error}"))?;
    kib.checked_mul(1024)
        .ok_or_else(|| "physical memory size overflows bytes".to_owned())
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct CgroupHierarchy {
    root: PathBuf,
    launcher: PathBuf,
}

#[cfg(target_os = "linux")]
impl CgroupHierarchy {
    fn detect() -> Result<Vec<Self>, String> {
        let membership = std::fs::read_to_string("/proc/self/cgroup")
            .map_err(|error| format!("cannot read the launcher's cgroup: {error}"))?;
        let mounts = std::fs::read_to_string("/proc/self/mountinfo")
            .map_err(|error| format!("cannot read cgroup mount information: {error}"))?;
        Self::from_proc(&membership, &mounts)
    }

    fn from_proc(membership: &str, mounts: &str) -> Result<Vec<Self>, String> {
        let current = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .ok_or_else(|| "the launcher is not in a unified cgroup-v2 hierarchy".to_owned())?;
        let mut hierarchies = Vec::new();
        for line in mounts.lines() {
            let Some((fields, filesystem)) = line.split_once(" - ") else {
                continue;
            };
            if filesystem.split_whitespace().next() != Some("cgroup2") {
                continue;
            }
            let mut fields = fields.split_whitespace();
            let Some(mount_root) = fields.nth(3) else {
                continue;
            };
            let Some(mount_point) = fields.next() else {
                continue;
            };
            let mount_root = decode_mount_path(mount_root)?;
            let mount_point = decode_mount_path(mount_point)?;
            if let Ok(hierarchy) = Self::resolve(&mount_point, &mount_root, current) {
                hierarchies.push(hierarchy);
            }
        }
        if hierarchies.is_empty() {
            Err("no mounted cgroup-v2 hierarchy safely exposes the launcher's cgroup".to_owned())
        } else {
            Ok(hierarchies)
        }
    }

    fn resolve(mount_point: &Path, mount_root: &Path, current: &str) -> Result<Self, String> {
        // In a cgroup namespace procfs may report an unreachable /../ path.
        // Never normalize it, guess a host path, or traverse outside this mount.
        if !current.starts_with('/')
            || current.split('/').any(|part| matches!(part, "." | ".."))
            || !mount_root.is_absolute()
            || mount_root.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Err("the launcher's cgroup path is outside the visible hierarchy".to_owned());
        }
        let relative = Path::new(current)
            .strip_prefix(mount_root)
            .map_err(|_| "the launcher's cgroup is outside this cgroup mount".to_owned())?;
        let root = std::fs::canonicalize(mount_point)
            .map_err(|error| format!("cannot resolve the cgroup mount: {error}"))?;
        let launcher = root.join(relative);
        let resolved = std::fs::canonicalize(&launcher)
            .map_err(|error| format!("cannot resolve the launcher's cgroup: {error}"))?;
        if resolved != launcher || !resolved.starts_with(&root) {
            return Err("the launcher's cgroup escapes the visible cgroup mount".to_owned());
        }
        Ok(Self { root, launcher })
    }
}

#[cfg(target_os = "linux")]
fn decode_mount_path(value: &str) -> Result<PathBuf, String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            let escaped = bytes
                .get(index + 1..index + 4)
                .filter(|digits| digits.iter().all(|digit| matches!(digit, b'0'..=b'7')))
                .ok_or_else(|| "invalid escaped cgroup mount path".to_owned())?;
            let value = ((escaped[0] - b'0') as u16) * 64
                + ((escaped[1] - b'0') as u16) * 8
                + (escaped[2] - b'0') as u16;
            if value == 0 || value > u8::MAX as u16 {
                return Err("invalid escaped cgroup mount path".to_owned());
            }
            decoded.push(value as u8);
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Ok(PathBuf::from(OsString::from_vec(decoded)))
}

#[cfg(target_os = "linux")]
fn inherited_memory_limit(path: &Path, root: &Path) -> Result<Option<u64>, String> {
    if !path.starts_with(root) {
        return Err("the launcher's cgroup is outside the visible cgroup mount".to_owned());
    }
    let mut cursor = Some(path);
    let mut limit: Option<u64> = None;
    while let Some(directory) = cursor {
        let file = directory.join("memory.max");
        if file.is_file() {
            let value = std::fs::read_to_string(&file)
                .map_err(|error| format!("cannot read `{}`: {error}", file.display()))?;
            if value.trim() != "max" {
                let bytes = value.trim().parse::<u64>().map_err(|error| {
                    format!("invalid cgroup memory ceiling in `{}`: {error}", file.display())
                })?;
                limit = Some(limit.map_or(bytes, |current| current.min(bytes)));
            }
        } else if directory != root {
            return Err(format!(
                "required inherited memory controller `{}` is absent",
                file.display()
            ));
        }
        if directory == root {
            break;
        }
        cursor = directory.parent();
    }
    Ok(limit)
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct CgroupLease {
    path: PathBuf,
    attachment: File,
    kill: File,
}

#[cfg(target_os = "linux")]
impl CgroupLease {
    fn prepare() -> Result<Self, String> {
        let policy = MemoryPolicy::detect()?;
        Self::prepare_with(policy, Self::create_at)
    }

    fn prepare_with(
        policy: MemoryPolicy,
        mut create: impl FnMut(&Path, u64) -> Result<Self, String>,
    ) -> Result<Self, String> {
        let mut refusal = "no mounted hierarchy provides a delegated parent".to_owned();
        for hierarchy in &policy.hierarchies {
            match Self::select_parent(hierarchy, policy.bytes, &mut create) {
                Ok(lease) => return Ok(lease),
                Err(error) => refusal = error,
            }
        }
        Err(refusal)
    }

    fn select_parent(
        hierarchy: &CgroupHierarchy,
        bytes: u64,
        mut create: impl FnMut(&Path, u64) -> Result<Self, String>,
    ) -> Result<Self, String> {
        if !hierarchy.launcher.starts_with(&hierarchy.root) {
            return Err("the launcher's cgroup is outside the visible cgroup mount".to_owned());
        }
        let mut refusal = "no ancestor has memory enabled for children".to_owned();
        for parent in hierarchy
            .launcher
            .ancestors()
            .skip(1)
            .take_while(|parent| parent.starts_with(&hierarchy.root))
        {
            // An already enabled memory subtree avoids the no-internal-process
            // rule without moving the launcher or modifying other workloads.
            let candidate = (|| {
                let control = parent.join("cgroup.subtree_control");
                let enabled = std::fs::read_to_string(&control)
                    .map_err(|error| format!("cannot read `{}`: {error}", control.display()))?;
                if !enabled.split_whitespace().any(|name| name == "memory") {
                    return Err(format!("`{}` has no enabled memory subtree", parent.display()));
                }
                if parent != hierarchy.root {
                    let members = parent.join("cgroup.procs");
                    let processes = std::fs::read_to_string(&members)
                        .map_err(|error| format!("cannot read `{}`: {error}", members.display()))?;
                    if !processes.trim().is_empty() {
                        return Err(format!(
                            "`{}` contains processes and cannot parent the web cgroup",
                            parent.display()
                        ));
                    }
                }
                // Migration also requires write permission on cgroup.procs at
                // the common ancestor of the source and destination. Opening
                // it checks delegation without moving any existing process;
                // the child's pre-exec attachment is the final kernel check.
                let _migration_permission = Self::open_control(parent, "cgroup.procs")?;
                create(parent, bytes)
            })();
            match candidate {
                Ok(lease) => return Ok(lease),
                Err(error) => refusal = error,
            }
            if parent == hierarchy.root {
                break;
            }
        }
        Err(format!(
            "no usable memory-enabled delegated cgroup parent: {refusal}"
        ))
    }

    fn create_at(parent: &Path, bytes: u64) -> Result<Self, String> {
        Self::create_at_with(parent, bytes, Self::open_controls)
    }

    fn create_at_with(
        parent: &Path,
        bytes: u64,
        open_controls: impl FnOnce(&Path, u64) -> Result<Self, String>,
    ) -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("crikey-web-{}-{sequence}", std::process::id()));
        std::fs::create_dir(&path)
            .map_err(|error| format!("cannot create delegated cgroup `{}`: {error}", path.display()))?;
        match open_controls(&path, bytes) {
            Ok(lease) => Ok(lease),
            Err(error) => {
                std::fs::remove_dir(&path).map_err(|cleanup| {
                    format!(
                        "{error}; cannot remove unused cgroup `{}`: {cleanup}",
                        path.display()
                    )
                })?;
                Err(error)
            }
        }
    }

    fn open_controls(path: &Path, bytes: u64) -> Result<Self, String> {
        // Never create ordinary files in place of missing kernel controllers.
        // The private directory is still empty, so exercising cgroup.kill here
        // proves it is writable and supported without killing another process.
        let mut memory = Self::open_control(path, "memory.max")?;
        let attachment = Self::open_control(path, "cgroup.procs")?;
        let mut kill = Self::open_control(path, "cgroup.kill")?;
        write!(memory, "{bytes}").map_err(|error| {
            format!(
                "cannot enforce `{}/memory.max` at {bytes} bytes: {error}",
                path.display()
            )
        })?;
        kill.write_all(b"1")
            .map_err(|error| format!("cannot exercise `{}/cgroup.kill`: {error}", path.display()))?;
        Ok(Self {
            path: path.to_owned(),
            attachment,
            kill,
        })
    }

    fn open_control(path: &Path, name: &str) -> Result<File, String> {
        let control = path.join(name);
        File::options().write(true).open(&control).map_err(|error| {
            format!(
                "cannot open required `{}` for writing: {error}",
                control.display()
            )
        })
    }

    fn kill_tree(&mut self) {
        if let Err(error) = self.kill.write_all(b"1") {
            eprintln!(
                "cannot kill the supervised web-host cgroup `{}`: {error}",
                self.path.display()
            );
        }
    }
}

/// Runs between fork and exec, before the host can create engine descendants.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn attach_current_process(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: getpid and write are async-signal-safe; decimal formatting uses
    // only a stack buffer, never the allocator or inherited Rust locks.
    let mut pid = unsafe { libc::getpid() } as u32;
    let mut digits = [0_u8; 10];
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (pid % 10) as u8;
        pid /= 10;
        if pid == 0 {
            break;
        }
    }
    let bytes = &digits[start..];
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if written == bytes.len() as isize {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
impl Drop for CgroupLease {
    fn drop(&mut self) {
        self.kill_tree();
        for _ in 0..20 {
            if std::fs::remove_dir(&self.path).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct ProductionProcess {
    child: Child,
    cgroup: CgroupLease,
    terminated: bool,
}

#[cfg(target_os = "linux")]
impl ManagedProcess for ProductionProcess {
    fn try_wait(&mut self) -> Result<Option<ExitStatus>, String> {
        self.child.try_wait().map_err(|error| error.to_string())
    }

    fn terminate(&mut self) {
        if self.terminated {
            return;
        }
        terminate_child_tree(&mut self.child, &mut self.cgroup);
        self.terminated = true;
    }
}

#[cfg(target_os = "linux")]
impl Drop for ProductionProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn terminate_child_tree(child: &mut Child, cgroup: &mut CgroupLease) {
    terminate_child_tree_with(child, cgroup, |root| {
        // SAFETY: the caller has just observed this owned child still running
        // and has not reaped it. Even if it exits now, its PID cannot be reused
        // before the next try_wait. SIGTERM gives the host a graceful close.
        unsafe {
            libc::kill(-(root as i32), libc::SIGTERM);
        }
    });
}

#[cfg(target_os = "linux")]
fn terminate_child_tree_with(child: &mut Child, cgroup: &mut CgroupLease, signal_group: impl FnOnce(u32)) {
    // Only Ok(None) establishes that we still own the unreaped root identity.
    // An already-cached exit status or a wait error must bypass numeric signals.
    let mut root_exited = false;
    match child.try_wait() {
        Ok(None) => {
            signal_group(child.id());
            for _ in 0..PROCESS_TERMINATION_PASSES {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        root_exited = true;
                        break;
                    }
                    Ok(None) => std::thread::sleep(PROCESS_TERMINATION_POLL),
                    Err(_) => break,
                }
            }
        }
        Ok(Some(_)) => root_exited = true,
        Err(_) => {}
    }

    // The held dedicated controller, not remembered PIDs or process groups,
    // owns escalation. This is mandatory even after the root has been reaped:
    // detached descendants remain contained, including forks during SIGTERM.
    cgroup.kill_tree();
    if root_exited {
        return;
    }
    for _ in 0..PROCESS_TERMINATION_PASSES {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(PROCESS_TERMINATION_POLL);
    }
    // Never turn bounded manager teardown into an unbounded Child::wait when
    // the kernel cannot complete exit promptly (for example, uninterruptible I/O).
    eprintln!("supervised web-host root did not exit within the bounded teardown deadline");
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use std::io::{BufRead, BufReader};
    #[cfg(target_os = "linux")]
    use std::os::unix::process::ExitStatusExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Barrier;

    use super::*;
    use crikey_native_protocol::transport;
    #[cfg(target_os = "linux")]
    use crikey_native_protocol::Message;

    #[derive(Debug)]
    struct FakeProcess {
        terminated: Arc<AtomicUsize>,
    }

    impl ManagedProcess for FakeProcess {
        fn try_wait(&mut self) -> Result<Option<ExitStatus>, String> {
            Ok(None)
        }

        fn terminate(&mut self) {
            self.terminated.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct FakeSpawner {
        starts: Arc<AtomicUsize>,
        terminated: Arc<AtomicUsize>,
        peers: mpsc::Sender<Box<dyn Transport>>,
    }

    impl HostSpawner for FakeSpawner {
        fn spawn(&self, _request: &OpenRequest) -> Result<RunningHost, String> {
            self.starts.fetch_add(1, Ordering::AcqRel);
            let (launcher, host) = transport::pair();
            self.peers.send(host).expect("test host receiver is live");
            let mut launcher = launcher;
            launcher
                .set_read_timeout(Some(Duration::from_millis(5)))
                .expect("pair timeout");
            Ok(RunningHost {
                transport: launcher,
                process: Box::new(FakeProcess {
                    terminated: Arc::clone(&self.terminated),
                }),
            })
        }
    }
    type Harness = (
        WebHostSupervisor,
        mpsc::Receiver<Box<dyn Transport>>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    );

    fn harness() -> Harness {
        harness_with_wake(Arc::new(|| {}))
    }

    fn harness_with_wake(wake: WebWake) -> Harness {
        let starts = Arc::new(AtomicUsize::new(0));
        let terminated = Arc::new(AtomicUsize::new(0));
        let (peers, receiver) = mpsc::channel();
        let supervisor = WebHostSupervisor::with_spawner(
            PathBuf::from("/unused"),
            wake,
            Arc::new(FakeSpawner {
                starts: Arc::clone(&starts),
                terminated: Arc::clone(&terminated),
                peers,
            }),
        );
        (supervisor, receiver, starts, terminated)
    }

    fn open(supervisor: &mut WebHostSupervisor) -> u64 {
        supervisor
            .open(
                PluginId("native.test".to_owned()),
                2,
                1,
                "https://example.test".to_owned(),
                WebStorage::Ephemeral,
            )
            .expect("surface queues")
    }

    fn recv(host: &mut Box<dyn Transport>) -> Envelope {
        host.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("pair timeout");
        host.recv().expect("launcher command")
    }

    fn frame(surface_id: u64, generation: u64) -> WebFrame {
        WebFrame {
            surface_id,
            generation,
            pixel_width: 2,
            pixel_height: 1,
            rgba8: vec![generation as u8; 8],
            ..WebFrame::default()
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_endpoint_restores_owner_search_under_a_restrictive_umask() {
        const CHILD: &str = "CRIKEY_WEB_ENDPOINT_UMASK_TEST";
        if std::env::var_os(CHILD).is_some() {
            let private = PrivateEndpoint::create().expect("private endpoint under umask 0100");
            let directory = private.directory.clone();
            assert_eq!(
                std::fs::metadata(&directory)
                    .expect("endpoint directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700,
                "umask must not leave the host unable to traverse its endpoint directory",
            );
            let listener = Listener::bind(private.endpoint()).expect("owner can bind the private socket");
            drop(listener);
            drop(private);
            assert!(!directory.exists(), "the private endpoint directory is removed");
            return;
        }

        // umask is process-global. Set it in a shell that execs one isolated
        // test, never in this harness alongside concurrently running tests.
        let output = Command::new("sh")
            .arg("-c")
            .arg("umask 0100; exec \"$@\"")
            .arg("sh")
            .arg(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "web_host::tests::private_endpoint_restores_owner_search_under_a_restrictive_umask",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .expect("isolated restrictive-umask test starts");
        assert!(
            output.status.success(),
            "restrictive-umask subprocess failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn opening_preserves_a_first_frame_published_before_enqueue_returns() {
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let desired = AtomicU64::new(1);
        let wake: WebWake = Arc::new(|| {});
        publish_frame(&mailbox, &desired, &wake, frame(1, 1));
        queue_surface_open(&mailbox, &desired, 2, || {
            assert_eq!(desired.load(Ordering::Acquire), 2);
            assert!(lock_recover(&mailbox).latest_frame.is_none());
            // Model a host that answers synchronously as soon as Open is
            // queued: clearing after enqueue would erase its only frame.
            publish_frame(&mailbox, &desired, &wake, frame(2, 1));
            publish_caret(
                &mailbox,
                &desired,
                &wake,
                CaretArea {
                    surface_id: 2,
                    width: 1.0,
                    height: 2.0,
                    ..CaretArea::default()
                },
            );
            Ok(())
        })
        .expect("open queues");
        // Obsolete publishers may run after replacement, but cannot erase
        // the new owner's first frame or caret with data or a fatal error.
        publish_frame(&mailbox, &desired, &wake, frame(1, 9));
        publish_fatal(&mailbox, &desired, &wake, 1, "old failure".to_owned());
        let mailbox = lock_recover(&mailbox);
        assert_eq!(mailbox.latest_frame.as_ref().expect("first frame").surface_id, 2);
        assert_eq!(mailbox.latest_caret.expect("first caret").0, 2);
        assert!(mailbox.notices.is_empty());
    }

    struct BlockingSpawner {
        peers: mpsc::Sender<Box<dyn Transport>>,
        release: Arc<Barrier>,
        terminated: Arc<AtomicUsize>,
    }

    impl HostSpawner for BlockingSpawner {
        fn spawn(&self, _request: &OpenRequest) -> Result<RunningHost, String> {
            let (mut launcher, peer) = transport::pair();
            launcher
                .set_read_timeout(Some(Duration::from_millis(5)))
                .expect("pair read deadline");
            self.peers.send(peer).expect("test receiver");
            self.release.wait();
            Ok(RunningHost {
                transport: launcher,
                process: Box::new(FakeProcess {
                    terminated: Arc::clone(&self.terminated),
                }),
            })
        }
    }

    #[test]
    fn closing_during_startup_never_opens_the_obsolete_document() {
        let release = Arc::new(Barrier::new(2));
        let terminated = Arc::new(AtomicUsize::new(0));
        let (peers, receiver) = mpsc::channel();
        let mut supervisor = WebHostSupervisor::with_spawner(
            PathBuf::from("/unused"),
            Arc::new(|| {}),
            Arc::new(BlockingSpawner {
                peers,
                release: Arc::clone(&release),
                terminated: Arc::clone(&terminated),
            }),
        );
        open(&mut supervisor);
        let mut peer = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("startup begins");
        supervisor.close("dismissed during startup");
        release.wait();
        drop(supervisor);
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("pair timeout");
        assert!(matches!(peer.recv(), Err(ProtocolError::Closed)));
        assert_eq!(terminated.load(Ordering::Acquire), 1);
    }

    #[test]
    fn exhausted_surface_ids_are_never_reused() {
        let (mut supervisor, _, _, _) = harness();
        supervisor.next_surface_id = 0;
        for _ in 0..2 {
            let error = supervisor
                .open(
                    PluginId("native.test".to_owned()),
                    2,
                    1,
                    "https://example.test".to_owned(),
                    WebStorage::Ephemeral,
                )
                .expect_err("exhaustion remains terminal");
            assert!(error.contains("id space is exhausted"));
        }
        assert_eq!(supervisor.next_surface_id, 0);
    }

    #[test]
    fn stale_ui_events_do_not_fail_against_a_saturated_command_queue() {
        let (command, _receiver) = mpsc::sync_channel(0);
        let supervisor = WebHostSupervisor {
            command,
            mailbox: Arc::new(Mutex::new(Mailbox::default())),
            desired_surface: Arc::new(AtomicU64::new(2)),
            shutdown: Arc::new(AtomicBool::new(false)),
            next_surface_id: 3,
            #[cfg(target_os = "linux")]
            storage_root: PathBuf::from("/unused"),
            manager: None,
        };
        supervisor
            .navigate(1, "https://example.test".to_owned())
            .expect("old navigation is ignored");
        supervisor.resize(1, 0, 0).expect("old resize is ignored");
        supervisor
            .send_input(WebSurfaceInput::Focus {
                surface_id: 1,
                focused: false,
            })
            .expect("old input is ignored");
        supervisor
            .send_ime(WebImeEvent::Commit {
                surface_id: 1,
                text: "old composition".to_owned(),
            })
            .expect("old IME is ignored");
        assert!(
            supervisor
                .send_input(WebSurfaceInput::Focus {
                    surface_id: 2,
                    focused: true,
                })
                .is_err(),
            "current input still reports genuine backpressure"
        );
        assert_eq!(supervisor.desired_surface.load(Ordering::Acquire), 2);
    }

    #[test]
    fn repeating_a_declaration_keeps_the_current_document() {
        let (mut supervisor, peers, _, _) = harness();
        let surface_id = open(&mut supervisor);
        let mut peer = peers.recv_timeout(Duration::from_secs(1)).expect("host starts");
        assert!(matches!(recv(&mut peer).payload, Some(Payload::OpenSurface(_))));
        supervisor
            .navigate(surface_id, "https://example.test".to_owned())
            .expect("repeat queues");
        supervisor
            .navigate(surface_id, String::new())
            .expect("stay queues");
        supervisor
            .send_input(WebSurfaceInput::Focus {
                surface_id,
                focused: true,
            })
            .expect("input queues");
        assert!(matches!(recv(&mut peer).payload, Some(Payload::RawInput(_))));
        supervisor
            .navigate(surface_id, "https://example.test/next".to_owned())
            .expect("navigation queues");
        assert!(
            matches!(recv(&mut peer).payload, Some(Payload::Navigate(Navigate { url, .. })) if url == "https://example.test/next")
        );
    }

    #[cfg(target_os = "linux")]
    struct OneShotSpawner(Mutex<Option<RunningHost>>);

    #[cfg(target_os = "linux")]
    impl HostSpawner for OneShotSpawner {
        fn spawn(&self, _request: &OpenRequest) -> Result<RunningHost, String> {
            lock_recover(&self.0)
                .take()
                .ok_or_else(|| "already started".to_owned())
        }
    }

    #[cfg(target_os = "linux")]
    #[derive(Debug)]
    struct ExitedProcess(FakeProcess);

    #[cfg(target_os = "linux")]
    impl ManagedProcess for ExitedProcess {
        fn try_wait(&mut self) -> Result<Option<ExitStatus>, String> {
            Ok(Some(ExitStatus::from_raw(0)))
        }

        fn terminate(&mut self) {
            self.0.terminate();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_exited_host_keeps_its_buffered_terminal_diagnostic() {
        for (terminal, expected) in [
            (
                Some(Payload::Gone(Gone {
                    surface_id: 1,
                    reason: WebGoneCode::StartupFailed,
                    detail: "controlled engine startup failure".to_owned(),
                    ..Gone::default()
                })),
                "web host failed to start: controlled engine startup failure",
            ),
            (
                Some(Payload::LoadState(LoadState {
                    surface_id: 1,
                    state: WebLoadCode::Failed,
                    url: "https://example.test".to_owned(),
                    failure: "controlled document failure".to_owned(),
                    ..LoadState::default()
                })),
                "web page load failed for `https://example.test`: controlled document failure",
            ),
            (None, "web host exited unexpectedly (exit status: 0)"),
        ] {
            let (mut launcher, mut peer) = transport::pair();
            launcher
                .set_read_timeout(Some(Duration::from_millis(5)))
                .expect("bounded empty receive");
            peer.send(&Envelope {
                payload: Some(Payload::LoadState(LoadState {
                    surface_id: 1,
                    state: WebLoadCode::Started,
                    ..LoadState::default()
                })),
                ..Envelope::default()
            })
            .expect("nonterminal output precedes process exit");
            if let Some(terminal) = terminal {
                peer.send(&Envelope {
                    payload: Some(terminal),
                    ..Envelope::default()
                })
                .expect("terminal output was flushed before exit");
                peer.send(&Envelope {
                    payload: Some(Payload::WebFrame(frame(1, 9))),
                    ..Envelope::default()
                })
                .expect("a later frame cannot revive a terminal surface");
            }
            let terminated = Arc::new(AtomicUsize::new(0));
            let (wake_tx, wake_rx) = mpsc::channel();
            let mut supervisor = WebHostSupervisor::with_spawner(
                PathBuf::from("/unused"),
                Arc::new(move || {
                    let _ = wake_tx.send(());
                }),
                Arc::new(OneShotSpawner(Mutex::new(Some(RunningHost {
                    transport: launcher,
                    process: Box::new(ExitedProcess(FakeProcess {
                        terminated: Arc::clone(&terminated),
                    })),
                })))),
            );
            assert_eq!(open(&mut supervisor), 1);
            wake_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("the completed host publishes a terminal notice");
            let notices = supervisor.poll();
            drop(supervisor);
            assert_eq!(notices.len(), 1, "post-terminal frames are discarded");
            assert!(
                matches!(&notices[0], WebHostNotice::Fatal { surface_id: 1, message } if message == expected),
                "the original terminal diagnostic must survive process exit: {notices:?}",
            );
            assert_eq!(terminated.load(Ordering::Acquire), 1);
        }
    }

    #[cfg(target_os = "linux")]
    #[derive(Debug)]
    struct TrackedClosedPeer {
        inner: Box<dyn Transport>,
        live_writes: Arc<AtomicUsize>,
        reads: Arc<AtomicUsize>,
        closed: bool,
        resume_after_timeout: Option<(std::os::unix::net::UnixStream, Vec<u8>)>,
    }

    #[cfg(target_os = "linux")]
    impl Transport for TrackedClosedPeer {
        fn send(&mut self, envelope: &Envelope) -> Result<(), ProtocolError> {
            if self.closed {
                return Err(ProtocolError::Closed);
            }
            self.live_writes.fetch_add(1, Ordering::AcqRel);
            self.inner.send(envelope)
        }

        fn recv(&mut self) -> Result<Envelope, ProtocolError> {
            self.reads.fetch_add(1, Ordering::AcqRel);
            let result = self.inner.recv();
            if matches!(result, Err(ProtocolError::Timeout)) {
                if let Some((mut peer, remainder)) = self.resume_after_timeout.take() {
                    std::io::Write::write_all(&mut peer, &remainder).expect("complete timed-out frame");
                }
            }
            result
        }

        fn try_clone_handle(&self) -> Result<Box<dyn Transport>, ProtocolError> {
            Err(ProtocolError::Io("test transport cannot clone".to_owned()))
        }

        fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), ProtocolError> {
            self.inner.set_read_timeout(timeout)
        }

        fn close(&mut self) {
            self.closed = true;
            self.inner.close();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_queued_controls_preserve_only_the_buffered_current_terminal_diagnostic() {
        const DRAIN_LIMIT: usize = 64;
        for operation in ["navigate", "resize", "input", "ime"] {
            for terminal in ["failed", "gone", "none", "beyond-cap"] {
                // The pair's bounded 64-message channel cannot synchronously
                // preload a >64-envelope diagnostic prefix. Use real Unix
                // buffering for that small-byte, many-envelope boundary.
                let endpoint = PrivateEndpoint::create().expect("private buffered endpoint");
                let listener = Listener::bind(endpoint.endpoint()).expect("buffered listener");
                let (mut launcher, mut peer) = if terminal == "beyond-cap" {
                    let peer = transport::connect(endpoint.endpoint(), Some(Duration::from_secs(1)))
                        .expect("buffered Unix peer");
                    let launcher = listener
                        .accept(Some(Duration::from_secs(1)))
                        .expect("buffered launcher");
                    (launcher, peer)
                } else {
                    transport::pair()
                };
                launcher
                    .set_read_timeout(Some(Duration::from_millis(5)))
                    .expect("bounded receive");
                let prefix_count = if terminal == "beyond-cap" {
                    DRAIN_LIMIT + 1
                } else {
                    1
                };
                for _ in 0..prefix_count {
                    send_payload(
                        &mut peer,
                        Payload::LoadState(LoadState {
                            surface_id: 1,
                            state: WebLoadCode::Started,
                            ..LoadState::default()
                        }),
                    )
                    .expect("buffered Started");
                }
                send_payload(
                    &mut peer,
                    Payload::LoadState(LoadState {
                        surface_id: 2,
                        state: WebLoadCode::Failed,
                        failure: "obsolete owner must not replace the control error".to_owned(),
                        ..LoadState::default()
                    }),
                )
                .expect("buffered obsolete terminal");
                peer.send(&Envelope {
                    connection_id: 1,
                    ..Envelope::default()
                })
                .expect("buffered unknown output");
                if terminal != "none" {
                    let payload = if terminal == "gone" {
                        Payload::Gone(Gone {
                            surface_id: 1,
                            reason: WebGoneCode::StartupFailed,
                            detail: "specific startup failure".to_owned(),
                            ..Gone::default()
                        })
                    } else {
                        Payload::LoadState(LoadState {
                            surface_id: 1,
                            state: WebLoadCode::Failed,
                            url: "https://example.test/document".to_owned(),
                            failure: "specific native document failure".to_owned(),
                            ..LoadState::default()
                        })
                    };
                    send_payload(&mut peer, payload).expect("buffered terminal");
                }
                send_payload(&mut peer, Payload::WebFrame(frame(1, 99)))
                    .expect("post-terminal frame cannot revive the surface");
                drop(peer);

                let terminated = Arc::new(AtomicUsize::new(0));
                let writes = Arc::new(AtomicUsize::new(0));
                let reads = Arc::new(AtomicUsize::new(0));
                let mut active = Some(ActiveHost {
                    surface_id: 1,
                    pixel_width: 2,
                    pixel_height: 1,
                    url: "https://example.test".to_owned(),
                    latest_generation: 0,
                    runtime: RunningHost {
                        transport: Box::new(TrackedClosedPeer {
                            inner: launcher,
                            live_writes: Arc::clone(&writes),
                            reads: Arc::clone(&reads),
                            closed: false,
                            resume_after_timeout: None,
                        }),
                        process: Box::new(ExitedProcess(FakeProcess {
                            terminated: Arc::clone(&terminated),
                        })),
                    },
                });
                assert!(active
                    .as_mut()
                    .expect("active host")
                    .runtime
                    .process
                    .try_wait()
                    .expect("exited process")
                    .is_some());
                let mailbox = Arc::new(Mutex::new(Mailbox::default()));
                let desired = AtomicU64::new(1);
                let wake: WebWake = Arc::new(|| {});
                publish_frame(&mailbox, &desired, &wake, frame(1, 1));
                publish_caret(
                    &mailbox,
                    &desired,
                    &wake,
                    CaretArea {
                        surface_id: 1,
                        width: 1.0,
                        height: 1.0,
                        ..CaretArea::default()
                    },
                );
                let (control, fallback_prefix) = match operation {
                    "navigate" => (
                        Control::Navigate {
                            surface_id: 1,
                            url: "https://example.test/next".to_owned(),
                        },
                        "cannot navigate the web surface:",
                    ),
                    "resize" => (
                        Control::Resize {
                            surface_id: 1,
                            pixel_width: 3,
                            pixel_height: 2,
                        },
                        "cannot resize the web surface:",
                    ),
                    "ime" => (
                        Control::Input(Input {
                            surface_id: 1,
                            payload: Payload::ImeEvent(ImeEvent {
                                surface_id: 1,
                                kind: WebImeCode::Commit,
                                text: "文".to_owned(),
                                ..ImeEvent::default()
                            }),
                        }),
                        "web host input transport failed:",
                    ),
                    _ => (
                        Control::Input(Input {
                            surface_id: 1,
                            payload: Payload::RawInput(RawInput {
                                surface_id: 1,
                                kind: WebInputCode::FocusIn,
                                ..RawInput::default()
                            }),
                        }),
                        "web host input transport failed:",
                    ),
                };
                handle_control(
                    control,
                    &mut active,
                    &mailbox,
                    &desired,
                    &wake,
                    &OneShotSpawner(Mutex::new(None)),
                );
                assert!(active.is_none());
                assert_eq!(terminated.load(Ordering::Acquire), 1);
                assert_eq!(
                    writes.load(Ordering::Acquire),
                    1,
                    "never write after the failed control"
                );
                assert!(reads.load(Ordering::Acquire) <= DRAIN_LIMIT);
                let mailbox = lock_recover(&mailbox);
                assert!(mailbox.latest_frame.is_none());
                assert!(mailbox.latest_caret.is_none());
                assert_eq!(
                    mailbox.notices.len(),
                    1,
                    "no generic duplicate fatal: {mailbox:?}"
                );
                let WebHostNotice::Fatal { surface_id, message } = &mailbox.notices[0] else {
                    panic!("retirement must publish only its terminal failure");
                };
                assert_eq!(*surface_id, 1);
                match terminal {
                    "failed" => assert_eq!(
                        message,
                        "web page load failed for `https://example.test/document`: specific native document failure",
                    ),
                    "gone" => assert_eq!(message, "web host failed to start: specific startup failure"),
                    _ => assert!(message.starts_with(fallback_prefix), "{message}"),
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_control_resumes_partial_terminal_frames_until_the_diagnostic_deadline() {
        for split in [2, 6] {
            for resume in [true, false] {
                let endpoint = PrivateEndpoint::create().expect("partial-frame endpoint");
                let listener = Listener::bind(endpoint.endpoint()).expect("partial-frame listener");
                let mut peer = std::os::unix::net::UnixStream::connect(match endpoint.endpoint() {
                    Endpoint::UnixSocket(path) => path,
                    _ => unreachable!("Linux private endpoint is a Unix socket"),
                })
                .expect("raw partial-frame peer");
                let mut launcher = listener.accept(Some(Duration::from_secs(1))).expect("launcher");
                launcher
                    .set_read_timeout(Some(Duration::from_millis(1)))
                    .expect("real receive timeout");
                let mut framed = Vec::new();
                crikey_native_protocol::frame::write_frame(
                    &mut framed,
                    &Envelope {
                        payload: Some(Payload::LoadState(LoadState {
                            surface_id: 1,
                            state: WebLoadCode::Failed,
                            url: "https://example.test/partial".to_owned(),
                            failure: "specific resumed failure".to_owned(),
                            ..LoadState::default()
                        })),
                        ..Envelope::default()
                    }
                    .encode(),
                )
                .expect("framed terminal");
                std::io::Write::write_all(&mut peer, &framed[..split]).expect("partial prefix/body");
                peer.shutdown(std::net::Shutdown::Read)
                    .expect("peer no longer accepts controls");
                let (resume_after_timeout, _incomplete_peer) = if resume {
                    (Some((peer, framed[split..].to_vec())), None)
                } else {
                    (None, Some(peer))
                };
                let writes = Arc::new(AtomicUsize::new(0));
                let reads = Arc::new(AtomicUsize::new(0));
                let terminated = Arc::new(AtomicUsize::new(0));
                let mut active = Some(ActiveHost {
                    surface_id: 1,
                    pixel_width: 2,
                    pixel_height: 1,
                    url: "https://example.test".to_owned(),
                    latest_generation: 0,
                    runtime: RunningHost {
                        transport: Box::new(TrackedClosedPeer {
                            inner: launcher,
                            live_writes: Arc::clone(&writes),
                            reads: Arc::clone(&reads),
                            closed: false,
                            resume_after_timeout,
                        }),
                        process: Box::new(ExitedProcess(FakeProcess {
                            terminated: Arc::clone(&terminated),
                        })),
                    },
                });
                let mailbox = Arc::new(Mutex::new(Mailbox::default()));
                let desired = AtomicU64::new(1);
                let wake: WebWake = Arc::new(|| {});
                publish_frame(&mailbox, &desired, &wake, frame(1, 1));
                publish_caret(
                    &mailbox,
                    &desired,
                    &wake,
                    CaretArea {
                        surface_id: 1,
                        width: 1.0,
                        height: 1.0,
                        ..CaretArea::default()
                    },
                );
                let started = std::time::Instant::now();
                handle_control(
                    Control::Resize {
                        surface_id: 1,
                        pixel_width: 3,
                        pixel_height: 2,
                    },
                    &mut active,
                    &mailbox,
                    &desired,
                    &wake,
                    &OneShotSpawner(Mutex::new(None)),
                );
                assert!(
                    started.elapsed() < Duration::from_secs(2),
                    "partial reads must not hang teardown"
                );
                assert!(
                    reads.load(Ordering::Acquire) >= 2,
                    "a timeout must not discard retained reader state"
                );
                assert!(active.is_none());
                assert_eq!(terminated.load(Ordering::Acquire), 1);
                assert_eq!(writes.load(Ordering::Acquire), 1);
                let mailbox = lock_recover(&mailbox);
                assert!(mailbox.latest_frame.is_none());
                assert!(mailbox.latest_caret.is_none());
                assert_eq!(
                    mailbox.notices.len(),
                    1,
                    "never publish both generic and specific failures"
                );
                let WebHostNotice::Fatal { surface_id, message } = &mailbox.notices[0] else {
                    panic!("partial-frame retirement must publish one fatal");
                };
                assert_eq!(*surface_id, 1);
                if resume {
                    assert_eq!(
                        message,
                        "web page load failed for `https://example.test/partial`: specific resumed failure",
                    );
                } else {
                    assert!(message.starts_with("cannot resize the web surface:"), "{message}");
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[derive(Debug)]
    struct PartialWriteFailure {
        closed: bool,
        sends: Arc<AtomicUsize>,
        sent_after_error: Arc<AtomicBool>,
    }

    #[cfg(target_os = "linux")]
    impl Transport for PartialWriteFailure {
        fn send(&mut self, _envelope: &Envelope) -> Result<(), ProtocolError> {
            if self.closed {
                return Err(ProtocolError::Closed);
            }
            match self.sends.fetch_add(1, Ordering::AcqRel) {
                0 => Ok(()),
                1 => Err(ProtocolError::Timeout),
                _ => {
                    self.sent_after_error.store(true, Ordering::Release);
                    Err(ProtocolError::Timeout)
                }
            }
        }

        fn recv(&mut self) -> Result<Envelope, ProtocolError> {
            Err(ProtocolError::Timeout)
        }

        fn try_clone_handle(&self) -> Result<Box<dyn Transport>, ProtocolError> {
            Err(ProtocolError::Io("test transport cannot clone".to_owned()))
        }

        fn set_read_timeout(&mut self, _timeout: Option<Duration>) -> Result<(), ProtocolError> {
            Ok(())
        }

        fn close(&mut self) {
            self.closed = true;
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_write_is_closed_before_teardown_can_send_again() {
        let sends = Arc::new(AtomicUsize::new(0));
        let sent_after_error = Arc::new(AtomicBool::new(false));
        let terminated = Arc::new(AtomicUsize::new(0));
        let (wake_tx, wake_rx) = mpsc::channel();
        let mut supervisor = WebHostSupervisor::with_spawner(
            PathBuf::from("/unused"),
            Arc::new(move || {
                let _ = wake_tx.send(());
            }),
            Arc::new(OneShotSpawner(Mutex::new(Some(RunningHost {
                transport: Box::new(PartialWriteFailure {
                    closed: false,
                    sends: Arc::clone(&sends),
                    sent_after_error: Arc::clone(&sent_after_error),
                }),
                process: Box::new(FakeProcess {
                    terminated: Arc::clone(&terminated),
                }),
            })))),
        );
        let surface_id = open(&mut supervisor);
        supervisor
            .send_input(WebSurfaceInput::Focus {
                surface_id,
                focused: true,
            })
            .expect("input queues");
        wake_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("write error reports fatal");
        drop(supervisor);
        assert_eq!(sends.load(Ordering::Acquire), 2);
        assert!(!sent_after_error.load(Ordering::Acquire));
        assert_eq!(terminated.load(Ordering::Acquire), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stalled_unix_peer_fails_input_and_does_not_block_shutdown() {
        let endpoint = PrivateEndpoint::create().expect("private endpoint");
        let listener = Listener::bind(endpoint.endpoint()).expect("listener");
        let _stalled_peer =
            transport::connect(endpoint.endpoint(), Some(Duration::from_secs(1))).expect("peer");
        let mut launcher = listener.accept(Some(Duration::from_secs(1))).expect("launcher");
        launcher.set_read_timeout(Some(READ_POLL)).expect("read deadline");
        launcher
            .set_write_timeout(Some(WRITE_TIMEOUT))
            .expect("real Unix write deadline");
        let terminated = Arc::new(AtomicUsize::new(0));
        let (wake_tx, wake_rx) = mpsc::channel();
        let mut supervisor = WebHostSupervisor::with_spawner(
            PathBuf::from("/unused"),
            Arc::new(move || {
                let _ = wake_tx.send(());
            }),
            Arc::new(OneShotSpawner(Mutex::new(Some(RunningHost {
                transport: launcher,
                process: Box::new(FakeProcess {
                    terminated: Arc::clone(&terminated),
                }),
            })))),
        );
        let surface_id = open(&mut supervisor);
        supervisor
            .send_ime(WebImeEvent::Commit {
                surface_id,
                text: "x".repeat(2 * 1024 * 1024),
            })
            .expect("input queues without waiting for peer");
        wake_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("stalled write reports failure");
        assert!(supervisor.poll().iter().any(|notice| matches!(
            notice, WebHostNotice::Fatal { message, .. } if message.contains("input transport failed")
        )));
        let (finished_tx, finished_rx) = mpsc::channel();
        std::thread::spawn(move || {
            drop(supervisor);
            finished_tx.send(()).expect("shutdown receiver");
        });
        finished_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown is bounded");
        assert_eq!(terminated.load(Ordering::Acquire), 1);
    }

    #[cfg(target_os = "linux")]
    struct TestDirectory(PathBuf);

    #[cfg(target_os = "linux")]
    impl TestDirectory {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "crikey-web-audit-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).expect("test directory");
            Self(path)
        }

        fn cgroup(&self) -> CgroupLease {
            let path = self.0.join("cgroup");
            std::fs::create_dir(&path).expect("cgroup fixture");
            for name in ["memory.max", "cgroup.procs", "cgroup.kill"] {
                File::create(path.join(name)).expect("control fixture");
            }
            CgroupLease::open_controls(&path, MIN_WEB_MEMORY_BYTES).expect("fixture controls")
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(target_os = "linux")]
    fn assert_startup_refused(prepare: impl FnOnce() -> Result<CgroupLease, String>, reason: &str) {
        let executions = AtomicUsize::new(0);
        let result = start_cgroup_supervised(prepare, |_cgroup| {
            executions.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        let error = result.expect_err("supervision failure must refuse startup");
        assert!(
            error.starts_with("web surface refused: required cgroup-v2 supervision is unavailable:"),
            "{error}"
        );
        assert!(error.contains(reason), "{error}");
        assert_eq!(executions.load(Ordering::Relaxed), 0, "host must not execute");
    }

    #[cfg(target_os = "linux")]
    fn delegated_fixture(directory: &TestDirectory) -> CgroupHierarchy {
        let launcher = directory.0.join("launcher");
        std::fs::create_dir(&launcher).expect("launcher leaf");
        for (path, contents) in [
            (directory.0.join("memory.max"), "max"),
            (directory.0.join("cgroup.subtree_control"), "memory pids"),
            (directory.0.join("cgroup.procs"), ""),
            (launcher.join("memory.max"), "402653184"),
            (launcher.join("cgroup.subtree_control"), ""),
            (launcher.join("cgroup.procs"), "1234\n"),
        ] {
            std::fs::write(path, contents).expect("delegation fixture");
        }
        CgroupHierarchy::resolve(&directory.0, Path::new("/"), "/launcher").expect("visible hierarchy")
    }

    #[cfg(target_os = "linux")]
    fn create_fixture_lease(parent: &Path, bytes: u64) -> Result<CgroupLease, String> {
        CgroupLease::create_at_with(parent, bytes, |path, bytes| {
            for name in ["memory.max", "cgroup.procs", "cgroup.kill"] {
                File::create(path.join(name)).expect("private control fixture");
            }
            CgroupLease::open_controls(path, bytes)
        })
    }

    #[cfg(target_os = "linux")]
    fn remove_fixture_lease(lease: CgroupLease) {
        let path = lease.path.clone();
        for name in ["memory.max", "cgroup.procs", "cgroup.kill"] {
            std::fs::remove_file(path.join(name)).expect("remove fake control");
        }
        drop(lease);
        assert!(!path.exists(), "private leaf is removed");
    }

    #[cfg(target_os = "linux")]
    fn mounted_hierarchies_fixture(full: &TestDirectory, delegated: &TestDirectory) -> Vec<CgroupHierarchy> {
        let original = delegated_fixture(full);
        delegated_fixture(delegated);
        let subtree = full.0.join("delegation");
        std::fs::create_dir(&subtree).expect("delegated subtree");
        std::fs::rename(&original.launcher, subtree.join("launcher")).expect("nested launcher");
        for (name, contents) in [
            ("memory.max", "536870912"),
            ("cgroup.subtree_control", "memory"),
            ("cgroup.procs", ""),
        ] {
            std::fs::write(subtree.join(name), contents).expect("full-view subtree controls");
        }
        std::fs::write(full.0.join("memory.max"), MIN_WEB_MEMORY_BYTES.to_string())
            .expect("hidden ancestor limit");
        std::fs::write(delegated.0.join("memory.max"), "536870912").expect("same delegated subtree limit");
        let mounts = format!(
            "1 0 0:1 / {} ro - cgroup2 cgroup ro\n\
             2 0 0:1 /delegation {} rw - cgroup2 cgroup rw\n",
            full.0.display(),
            delegated.0.display(),
        );
        CgroupHierarchy::from_proc("0::/delegation/launcher\n", &mounts)
            .expect("both mount views expose the launcher")
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_later_writable_cgroup_mount_is_tried_without_losing_hidden_ancestor_limits() {
        let full = TestDirectory::new();
        let delegated = TestDirectory::new();
        let hierarchies = mounted_hierarchies_fixture(&full, &delegated);
        assert_eq!(hierarchies.len(), 2, "mount discovery retains both safe views");
        let policy = MemoryPolicy::from_hierarchies(8 * 1024 * 1024 * 1024, hierarchies)
            .expect("all original launcher limits are readable");
        assert_eq!(policy.bytes, MIN_WEB_MEMORY_BYTES);
        let mut attempted = Vec::new();
        let lease = CgroupLease::prepare_with(policy, |parent, bytes| {
            attempted.push(parent.to_owned());
            if parent.starts_with(&full.0) {
                Err("read-only cgroup mount".to_owned())
            } else {
                create_fixture_lease(parent, bytes)
            }
        })
        .expect("read-only first mount cannot hide the usable delegation");
        assert_eq!(
            attempted,
            vec![full.0.join("delegation"), full.0.clone(), delegated.0.clone()]
        );
        assert_eq!(lease.path.parent(), Some(delegated.0.as_path()));
        assert_eq!(
            std::fs::read_to_string(lease.path.join("memory.max")).expect("enforced ceiling"),
            MIN_WEB_MEMORY_BYTES.to_string(),
            "a hidden full-mount ancestor still caps the sibling host",
        );
        remove_fixture_lease(lease);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_hidden_below_floor_ancestor_refuses_every_mount_before_creation() {
        let full = TestDirectory::new();
        let delegated = TestDirectory::new();
        let hierarchies = mounted_hierarchies_fixture(&full, &delegated);
        std::fs::write(full.0.join("memory.max"), (MIN_WEB_MEMORY_BYTES - 1).to_string())
            .expect("below-floor hidden ancestor");
        assert_startup_refused(
            || {
                let policy = MemoryPolicy::from_hierarchies(8 * 1024 * 1024 * 1024, hierarchies)?;
                CgroupLease::prepare_with(policy, |_, _| {
                    panic!("no mount can bypass the original launcher floor")
                })
            },
            "below the 256 MiB minimum",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn populated_launcher_uses_an_existing_enabled_delegated_ancestor() {
        let directory = TestDirectory::new();
        let hierarchy = delegated_fixture(&directory);
        let executions = AtomicUsize::new(0);
        start_cgroup_supervised(
            || CgroupLease::select_parent(&hierarchy, MIN_WEB_MEMORY_BYTES, create_fixture_lease),
            |lease| {
                executions.fetch_add(1, Ordering::Relaxed);
                assert_eq!(lease.path.parent(), Some(hierarchy.root.as_path()));
                assert_ne!(lease.path.parent(), Some(hierarchy.launcher.as_path()));
                remove_fixture_lease(lease);
                Ok(())
            },
        )
        .expect("delegated sibling topology supports startup");
        assert_eq!(executions.load(Ordering::Relaxed), 1);
        assert_eq!(
            std::fs::read_to_string(hierarchy.launcher.join("cgroup.procs")).expect("launcher members"),
            "1234\n",
            "launcher is never moved"
        );
        assert_eq!(
            std::fs::read_to_string(hierarchy.root.join("cgroup.subtree_control")).expect("controllers"),
            "memory pids",
            "unrelated subtree controllers are never changed"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ancestor_selection_retains_the_original_launcher_memory_cap() {
        let directory = TestDirectory::new();
        let hierarchy = delegated_fixture(&directory);
        let inherited = inherited_memory_limit(&hierarchy.launcher, &hierarchy.root).expect("inherited cap");
        let ceiling = web_memory_ceiling(8 * 1024 * 1024 * 1024, inherited).expect("admitted ceiling");
        assert_eq!(ceiling, 384 * 1024 * 1024);
        let lease = CgroupLease::select_parent(&hierarchy, ceiling, create_fixture_lease)
            .expect("sibling retains launcher ceiling");
        assert_eq!(
            std::fs::read_to_string(lease.path.join("memory.max")).expect("private cap"),
            ceiling.to_string()
        );
        remove_fixture_lease(lease);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn no_memory_enabled_parent_refuses_without_execution_or_controller_changes() {
        let directory = TestDirectory::new();
        let hierarchy = delegated_fixture(&directory);
        std::fs::write(hierarchy.root.join("cgroup.subtree_control"), "pids").expect("disabled memory");
        assert_startup_refused(
            || {
                CgroupLease::select_parent(&hierarchy, MIN_WEB_MEMORY_BYTES, |_, _| {
                    panic!("disabled subtree cannot create a child")
                })
            },
            "no usable memory-enabled delegated cgroup parent",
        );
        assert_eq!(
            std::fs::read_to_string(hierarchy.root.join("cgroup.subtree_control")).expect("controllers"),
            "pids"
        );
        assert_eq!(
            std::fs::read_dir(&hierarchy.root).expect("root entries").count(),
            4
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_common_ancestor_migration_permission_refuses_without_creating_a_child() {
        let directory = TestDirectory::new();
        let hierarchy = delegated_fixture(&directory);
        // A directory cannot be opened as a writable control, including under
        // root, unlike a mode-bit permission fixture.
        let control = hierarchy.root.join("cgroup.procs");
        std::fs::remove_file(&control).expect("remove control");
        std::fs::create_dir(&control).expect("unwritable control fixture");
        assert_startup_refused(
            || {
                CgroupLease::select_parent(&hierarchy, MIN_WEB_MEMORY_BYTES, |_, _| {
                    panic!("missing migration permission cannot create a child")
                })
            },
            "cgroup.procs",
        );
        assert_eq!(
            std::fs::read_dir(&hierarchy.root).expect("root entries").count(),
            4
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ancestor_control_refusal_removes_the_unused_child_before_refusing_execution() {
        let directory = TestDirectory::new();
        let hierarchy = delegated_fixture(&directory);
        assert_startup_refused(
            || CgroupLease::select_parent(&hierarchy, MIN_WEB_MEMORY_BYTES, CgroupLease::create_at),
            "memory.max",
        );
        assert_eq!(
            std::fs::read_dir(&hierarchy.root).expect("root entries").count(),
            4
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn candidate_control_failure_is_cleaned_before_trying_the_next_ancestor() {
        let directory = TestDirectory::new();
        let hierarchy = delegated_fixture(&directory);
        let lower = hierarchy.root.join("lower");
        std::fs::create_dir(&lower).expect("lower delegated parent");
        std::fs::write(lower.join("cgroup.subtree_control"), "memory").expect("lower controllers");
        std::fs::write(lower.join("cgroup.procs"), "").expect("lower migration permission");
        let launcher = lower.join("launcher");
        std::fs::rename(&hierarchy.launcher, &launcher).expect("nested launcher leaf");
        let hierarchy = CgroupHierarchy {
            launcher,
            ..hierarchy
        };
        let lease = CgroupLease::select_parent(&hierarchy, MIN_WEB_MEMORY_BYTES, |parent, bytes| {
            if parent == lower {
                CgroupLease::create_at(parent, bytes)
            } else {
                assert_eq!(std::fs::read_dir(&lower).expect("lower entries").count(), 3);
                create_fixture_lease(parent, bytes)
            }
        })
        .expect("next usable ancestor");
        assert_eq!(lease.path.parent(), Some(hierarchy.root.as_path()));
        remove_fixture_lease(lease);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn no_internal_process_rule_skips_populated_nonroot_ancestors_and_the_launcher_leaf() {
        let directory = TestDirectory::new();
        let hierarchy = delegated_fixture(&directory);
        let lower = hierarchy.root.join("populated");
        std::fs::create_dir(&lower).expect("populated parent");
        std::fs::write(lower.join("cgroup.subtree_control"), "memory").expect("fake enabled memory");
        std::fs::write(lower.join("cgroup.procs"), "5678\n").expect("unrelated member");
        let launcher = lower.join("launcher");
        std::fs::rename(&hierarchy.launcher, &launcher).expect("nested launcher leaf");
        std::fs::write(launcher.join("cgroup.subtree_control"), "memory").expect("fake launcher controller");
        let hierarchy = CgroupHierarchy {
            launcher,
            ..hierarchy
        };
        let lease = CgroupLease::select_parent(&hierarchy, MIN_WEB_MEMORY_BYTES, |parent, bytes| {
            assert_eq!(
                parent, hierarchy.root,
                "neither populated nonroot directory is selected"
            );
            create_fixture_lease(parent, bytes)
        })
        .expect("empty delegated ancestor");
        assert_eq!(
            std::fs::read_to_string(lower.join("cgroup.procs")).expect("unrelated member retained"),
            "5678\n"
        );
        remove_fixture_lease(lease);
        let at_root = CgroupHierarchy {
            root: hierarchy.root.clone(),
            launcher: hierarchy.root.clone(),
        };
        assert_startup_refused(
            || {
                CgroupLease::select_parent(&at_root, MIN_WEB_MEMORY_BYTES, |_, _| {
                    panic!("selection must not cross the mount boundary")
                })
            },
            "no usable memory-enabled delegated cgroup parent",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn hierarchy_resolution_rejects_escape_and_respects_the_mounted_subtree() {
        let directory = TestDirectory::new();
        let hierarchy = delegated_fixture(&directory);
        assert!(CgroupHierarchy::resolve(&directory.0, Path::new("/"), "/../launcher").is_err());
        assert!(CgroupHierarchy::resolve(&directory.0, Path::new("/delegation"), "/other/launcher").is_err());
        let visible =
            CgroupHierarchy::resolve(&directory.0, Path::new("/delegation"), "/delegation/launcher")
                .expect("non-root mounted subtree");
        assert_eq!(visible.launcher, hierarchy.launcher);
        std::os::unix::fs::symlink(&hierarchy.launcher, directory.0.join("alias")).expect("path alias");
        assert!(CgroupHierarchy::resolve(&directory.0, Path::new("/"), "/alias").is_err());
        assert_eq!(
            decode_mount_path("/sys/fs/cgroup/delegated\\040tree").expect("escaped mount"),
            Path::new("/sys/fs/cgroup/delegated tree")
        );
        assert!(decode_mount_path("/sys/fs/cgroup/\\777").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires a writable cgroup-v2 delegation with memory already enabled for children"]
    fn real_delegated_cgroup_contains_and_kills_a_detached_child() {
        let launcher_membership = std::fs::read_to_string("/proc/self/cgroup").expect("launcher membership");
        let policy = MemoryPolicy::detect().expect("delegated memory policy");
        let cgroup = CgroupLease::prepare().expect("usable delegated ancestor");
        let path = cgroup.path.clone();
        assert_eq!(
            std::fs::read_to_string(path.join("memory.max"))
                .expect("enforced ceiling")
                .trim(),
            policy.bytes.to_string()
        );
        let mut command = contained_shell("setsid sh -c 'echo $$; exec sleep 30' & wait", &cgroup);
        command.stdout(Stdio::piped());
        let mut child = command.spawn().expect("contained controlled child");
        let stdout = child.stdout.take().expect("detached pid output");
        let mut process = ProductionProcess {
            child,
            cgroup,
            terminated: false,
        };
        let mut detached = String::new();
        BufReader::new(stdout)
            .read_line(&mut detached)
            .expect("detached child pid");
        let detached = detached.trim().parse::<u32>().expect("detached pid");
        let members = std::fs::read_to_string(path.join("cgroup.procs")).expect("private members");
        for pid in [process.child.id(), detached] {
            assert!(
                members.lines().any(|line| line.parse::<u32>() == Ok(pid)),
                "child {pid} must be inside the private cgroup: {members}"
            );
        }
        assert_eq!(
            std::fs::read_to_string("/proc/self/cgroup").expect("unchanged launcher membership"),
            launcher_membership,
            "the launcher is never moved"
        );
        // Exercise cgroup.kill itself, not process-group cleanup: setsid has
        // deliberately removed the descendant from the host process group.
        process.cgroup.kill_tree();
        assert_process_stopped(detached);
        assert_process_stopped(process.child.id());
        assert!(process.child.try_wait().expect("root is reaped").is_some());
        drop(process);
        assert!(!path.exists(), "private real cgroup is removed after teardown");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_delegation_refuses_startup_before_host_execution() {
        let directory = TestDirectory::new();
        assert_startup_refused(
            || CgroupLease::create_at(&directory.0.join("not-delegated"), MIN_WEB_MEMORY_BYTES),
            "cannot create delegated cgroup",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unavailable_memory_controller_removes_the_unused_private_cgroup() {
        let directory = TestDirectory::new();
        assert_startup_refused(
            || CgroupLease::create_at(&directory.0, MIN_WEB_MEMORY_BYTES),
            "memory.max",
        );
        assert_eq!(
            std::fs::read_dir(&directory.0)
                .expect("delegation fixture")
                .count(),
            0
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_required_controls_refuse_startup_without_creating_replacements() {
        for missing in ["memory.max", "cgroup.procs", "cgroup.kill"] {
            let directory = TestDirectory::new();
            for name in ["memory.max", "cgroup.procs", "cgroup.kill"] {
                if name != missing {
                    File::create(directory.0.join(name)).expect("control fixture");
                }
            }
            assert_startup_refused(
                || CgroupLease::open_controls(&directory.0, MIN_WEB_MEMORY_BYTES),
                missing,
            );
            assert!(
                !directory.0.join(missing).exists(),
                "missing controller stays missing"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unusable_memory_or_kill_controller_refuses_startup_before_host_execution() {
        for (failing, reason) in [
            ("memory.max", "cannot enforce"),
            ("cgroup.kill", "cannot exercise"),
        ] {
            let directory = TestDirectory::new();
            for name in ["memory.max", "cgroup.procs", "cgroup.kill"] {
                if name == failing {
                    std::os::unix::fs::symlink("/dev/full", directory.0.join(name))
                        .expect("always-failing controller fixture");
                } else {
                    File::create(directory.0.join(name)).expect("control fixture");
                }
            }
            assert_startup_refused(
                || CgroupLease::open_controls(&directory.0, MIN_WEB_MEMORY_BYTES),
                reason,
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_below_floor_cap_refuses_startup_before_host_execution() {
        assert_startup_refused(
            || {
                web_memory_ceiling(8 * 1024 * 1024 * 1024, Some(MIN_WEB_MEMORY_BYTES - 1))?;
                panic!("below-floor admission cannot reach cgroup creation");
            },
            "below the 256 MiB minimum",
        );
        assert_eq!(
            web_memory_ceiling(8 * 1024 * 1024 * 1024, Some(MIN_WEB_MEMORY_BYTES)),
            Ok(MIN_WEB_MEMORY_BYTES),
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unknown_inherited_cap_refuses_startup_before_host_execution() {
        let directory = TestDirectory::new();
        std::fs::write(directory.0.join("memory.max"), "invalid").expect("invalid inherited cap");
        assert_startup_refused(
            || {
                inherited_memory_limit(&directory.0, &directory.0)?;
                panic!("unknown inherited cap cannot reach cgroup creation");
            },
            "invalid cgroup memory ceiling",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn web_memory_ceiling_preserves_physical_and_absolute_limits() {
        assert_eq!(
            web_memory_ceiling(2 * 1024 * 1024 * 1024, None),
            Ok(MIN_WEB_MEMORY_BYTES)
        );
        assert_eq!(
            web_memory_ceiling(8 * 1024 * 1024 * 1024, None),
            Ok(MAX_WEB_MEMORY_BYTES)
        );
        assert!(web_memory_ceiling(1024 * 1024 * 1024, None).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[allow(unsafe_code)]
    fn failed_pre_exec_attachment_never_executes_the_host() {
        let directory = TestDirectory::new();
        let marker = directory.0.join("executed");
        let launcher_pid = std::process::id() as libc::pid_t;
        let mut command = Command::new("sh");
        command.arg("-c").arg("touch \"$1\"").arg("sh").arg(&marker);
        // SAFETY: production pre-exec setup uses only async-signal-safe calls;
        // the invalid descriptor deterministically rejects attachment.
        unsafe {
            command.pre_exec(move || prepare_host_process(launcher_pid, -1));
        }
        assert!(command.spawn().is_err(), "failed attachment must abort exec");
        assert!(!marker.exists(), "the executable must not run before attachment");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn discovery_skips_nonexecutables_and_resolves_override_identity() {
        let directory = TestDirectory::new();
        let sibling = directory.0.join("sibling");
        let on_path = directory.0.join("path");
        std::fs::create_dir(&sibling).expect("sibling directory");
        std::fs::create_dir(&on_path).expect("PATH directory");
        let name = host_file_name();
        std::fs::write(sibling.join(&name), "not executable").expect("nonexecutable sibling");
        let runnable = on_path.join(&name);
        std::fs::write(&runnable, "#!/bin/sh\nexit 0\n").expect("host fixture");
        std::fs::set_permissions(&runnable, std::fs::Permissions::from_mode(0o700)).expect("executable");
        let resolved = resolve_web_host_executable(
            None,
            Some(sibling.join("crikey")),
            Some(std::env::join_paths([&on_path]).expect("PATH")),
        )
        .expect("runnable PATH fallback");
        assert_eq!(resolved, runnable.canonicalize().expect("absolute host"));
        let current = std::env::current_dir().expect("current directory");
        let mut relative = PathBuf::new();
        for _ in current.ancestors().skip(1) {
            relative.push("..");
        }
        relative.push(runnable.strip_prefix("/").expect("absolute fixture path"));
        assert!(!relative.is_absolute());
        assert_eq!(
            resolve_web_host_executable(Some(relative.into_os_string()), None, None)
                .expect("relative override"),
            resolved,
        );
        assert!(resolve_web_host_executable(Some(sibling.join(&name).into_os_string()), None, None).is_err());
        assert!(resolve_web_host_executable(Some(OsString::new()), None, None).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires an installed web host and a writable cgroup-v2 delegation with memory enabled"]
    fn real_host_connects_through_a_non_utf8_temporary_directory() {
        const CHILD: &str = "CRIKEY_WEB_NATIVE_ENDPOINT_TEST";
        if std::env::var_os(CHILD).is_some() {
            let mut host = spawn_production(&OpenRequest {
                surface_id: 1,
                plugin: PluginId("native.endpoint-test".to_owned()),
                pixel_width: 2,
                pixel_height: 1,
                url: "https://example.test".to_owned(),
                storage: WebStorage::Ephemeral,
                storage_root: PathBuf::from("/unused"),
            })
            .expect("the real host connects to the exact native socket path");
            host.transport.close();
            host.process.terminate();
            return;
        }

        let directory = TestDirectory::new();
        let temporary = directory.0.join(OsString::from_vec(b"native-\xff".to_vec()));
        std::fs::create_dir(&temporary).expect("native temporary directory");
        // TMPDIR is process-global. Exercise the production spawner and real
        // host parser in an isolated harness rather than changing this process.
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "web_host::tests::real_host_connects_through_a_non_utf8_temporary_directory",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("TMPDIR", &temporary)
            .output()
            .expect("isolated native-path test starts");
        assert!(
            output.status.success(),
            "real host native-path connection failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            std::fs::read_dir(&temporary).expect("temporary entries").count(),
            0,
            "the private endpoint directory is removed after connection",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn persistent_storage_is_private_and_uses_the_full_owner_identity() {
        let directory = TestDirectory::new();
        let root = directory.0.join("web-storage");
        let native = PluginId("native.notes".to_owned());
        let modern = PluginId("modern.notes".to_owned());
        let native_path = persistent_storage_dir(&root, &native).expect("native storage");
        let modern_path = persistent_storage_dir(&root, &modern).expect("modern storage");
        assert_ne!(native_path, modern_path, "same suffix is not the same owner");
        assert_eq!(
            persistent_storage_dir(&root, &native).expect("reopened storage"),
            native_path,
        );
        let canonical_root = root.canonicalize().expect("storage root");
        assert_eq!(
            std::fs::metadata(&root)
                .expect("storage-root metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700,
            "owner directories cannot be replaced through a public storage root",
        );
        for path in [native_path, modern_path] {
            assert_eq!(path.parent(), Some(canonical_root.as_path()));
            assert_eq!(
                std::fs::metadata(path)
                    .expect("storage metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700,
                "durable cookies stay private to the launcher account",
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn persistent_storage_supports_long_and_prefix_related_owner_identities() {
        let directory = TestDirectory::new();
        let root = directory.0.join("web-storage");
        let owners = [
            PluginId(format!("native.{}", "a".repeat(120))),
            PluginId(format!("native.{}", "a".repeat(121))),
            PluginId(format!("native.{}b", "a".repeat(121))),
            PluginId(format!("native.{}c", "a".repeat(121))),
            PluginId(format!("modern.{}", "中".repeat(80))),
        ];
        let mut paths = Vec::new();
        for (index, owner) in owners.iter().enumerate() {
            let path = persistent_storage_dir(&root, owner).expect("valid long owner storage");
            assert!(!paths.contains(&path), "full owners must retain separate stores");
            std::fs::write(path.join("owner"), index.to_string()).expect("owner marker");
            paths.push(path);
        }
        let canonical_root = root.canonicalize().expect("storage root");
        assert_eq!(paths[0].parent(), Some(canonical_root.as_path()));
        for (index, owner) in owners.iter().enumerate() {
            let path = persistent_storage_dir(&root, owner).expect("reopened long owner storage");
            assert_eq!(path, paths[index]);
            for component in path
                .strip_prefix(&canonical_root)
                .expect("contained owner path")
                .components()
            {
                assert!(
                    component.as_os_str().as_bytes().len() <= 254,
                    "encoded components must fit Linux NAME_MAX",
                );
            }
            assert_eq!(
                std::fs::read_to_string(path.join("owner")).expect("retained isolated marker"),
                index.to_string(),
            );
            for ancestor in path
                .ancestors()
                .take_while(|ancestor| ancestor.starts_with(&canonical_root))
            {
                assert_eq!(
                    std::fs::metadata(ancestor)
                        .expect("private path metadata")
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700,
                );
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn persistent_storage_rejects_a_symlinked_long_owner_branch() {
        let directory = TestDirectory::new();
        let root = directory.0.join("web-storage");
        let owner = PluginId(format!("native.{}", "a".repeat(121)));
        let owned = persistent_storage_dir(&root, &owner).expect("long owner storage");
        let canonical_root = root.canonicalize().expect("storage root");
        let branches: Vec<_> = owned
            .ancestors()
            .take_while(|ancestor| ancestor.starts_with(&canonical_root))
            .map(Path::to_owned)
            .collect();
        let unrelated = directory.0.join("unrelated");
        std::fs::create_dir(&unrelated).expect("unrelated directory");
        std::fs::set_permissions(&unrelated, std::fs::Permissions::from_mode(0o750))
            .expect("unrelated permissions");
        std::fs::write(unrelated.join("marker"), "unrelated").expect("unrelated marker");
        for branch in branches {
            persistent_storage_dir(&root, &owner).expect("restored private owner path");
            std::fs::remove_dir_all(&branch).expect("empty private branch");
            std::os::unix::fs::symlink(&unrelated, &branch).expect("aliased private branch");
            let error = persistent_storage_dir(&root, &owner).expect_err("aliased branches are refused");
            assert!(error.contains("not a real directory"), "{error}");
            assert_eq!(
                std::fs::read_dir(&unrelated).expect("unrelated entries").count(),
                1
            );
            assert_eq!(
                std::fs::metadata(&unrelated)
                    .expect("unrelated metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o750,
            );
            assert_eq!(
                std::fs::read_to_string(unrelated.join("marker")).expect("untouched unrelated marker"),
                "unrelated",
            );
            std::fs::remove_file(&branch).expect("remove test alias before restoring path");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn persistent_storage_rejects_symlinked_owner_directories_without_modifying_the_target() {
        let directory = TestDirectory::new();
        let root = directory.0.join("web-storage");
        let owner = PluginId("native.notes".to_owned());
        let owned = persistent_storage_dir(&root, &owner).expect("initial owner store");
        std::fs::remove_dir(&owned).expect("empty owner store");
        let unrelated = directory.0.join("unrelated");
        std::fs::create_dir(&unrelated).expect("unrelated directory");
        std::fs::set_permissions(&unrelated, std::fs::Permissions::from_mode(0o750))
            .expect("unrelated permissions");
        std::fs::write(unrelated.join("marker"), "not a web store").expect("unrelated contents");
        std::os::unix::fs::symlink(&unrelated, &owned).expect("preexisting owner alias");
        let error = persistent_storage_dir(&root, &owner).expect_err("aliases cannot merge stores");
        assert!(error.contains("not a real directory"), "{error}");
        assert_eq!(
            std::fs::metadata(&unrelated)
                .expect("target metadata")
                .permissions()
                .mode()
                & 0o777,
            0o750,
            "refusal must not chmod an unrelated target",
        );
        assert_eq!(
            std::fs::read_to_string(unrelated.join("marker")).expect("untouched target"),
            "not a web store",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn persistent_storage_rejects_a_symlinked_root_before_creating_an_owner_store() {
        let directory = TestDirectory::new();
        let unrelated = directory.0.join("unrelated");
        std::fs::create_dir(&unrelated).expect("unrelated root target");
        let root = directory.0.join("web-storage");
        std::os::unix::fs::symlink(&unrelated, &root).expect("preexisting storage-root alias");
        let error = persistent_storage_dir(&root, &PluginId("native.notes".to_owned()))
            .expect_err("aliased roots cannot redirect durable cookies");
        assert!(error.contains("not a real directory"), "{error}");
        assert_eq!(
            std::fs::read_dir(&unrelated).expect("target entries").count(),
            0,
            "no plugin store is created outside the admitted root",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[allow(unsafe_code)]
    fn process_attachment_precedes_the_first_executable_instruction() {
        let directory = TestDirectory::new();
        let pid_file = directory.0.join("cgroup.procs");
        let attachment = File::create(&pid_file).expect("attachment fixture");
        let launcher_pid = std::process::id() as libc::pid_t;
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("test \"$(cat \"$1\")\" = \"$$\"")
            .arg("sh")
            .arg(&pid_file);
        // SAFETY: the callback uses only the async-signal-safe attachment
        // helper. A real cgroup is not required to prove pre-exec ordering.
        unsafe {
            command.pre_exec(move || prepare_host_process(launcher_pid, attachment.as_raw_fd()));
        }
        assert!(command.status().expect("child executes").success());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[allow(unsafe_code)]
    fn a_reparented_host_exits_before_executing() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("exit 42");
        // SAFETY: the callback runs the production async-signal-safe guard.
        // Parent pid 0 cannot match a live spawning process.
        unsafe {
            command.pre_exec(|| prepare_host_process(0, -1));
        }
        assert_eq!(command.status().expect("parent guard runs").code(), Some(127));
    }

    #[test]
    fn stale_surface_and_generation_frames_are_rejected() {
        let (wake_tx, wake_rx) = mpsc::channel();
        let (mut supervisor, peers, _, _) = harness_with_wake(Arc::new(move || {
            let _ = wake_tx.send(());
        }));
        let surface_id = open(&mut supervisor);
        let mut host = peers
            .recv_timeout(Duration::from_secs(1))
            .expect("fake host started");
        assert!(matches!(recv(&mut host).payload, Some(Payload::OpenSurface(_))));

        for (id, generation, rgba) in [
            (surface_id + 1, 1, vec![1; 8]),
            (surface_id, 2, vec![2; 8]),
            (surface_id, 1, vec![3; 8]),
        ] {
            host.send(&Envelope {
                payload: Some(Payload::WebFrame(WebFrame {
                    surface_id: id,
                    generation,
                    pixel_width: 2,
                    pixel_height: 1,
                    rgba8: rgba,
                    ..WebFrame::default()
                })),
                ..Envelope::default()
            })
            .expect("frame reaches supervisor");
        }
        host.send(&Envelope {
            payload: Some(Payload::CaretArea(CaretArea {
                surface_id,
                width: 1.0,
                height: 1.0,
                ..CaretArea::default()
            })),
            ..Envelope::default()
        })
        .expect("sentinel follows all frames");
        while lock_recover(&supervisor.mailbox).latest_caret.is_none() {
            wake_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("manager reaches sentinel");
        }
        let frames = supervisor
            .poll()
            .into_iter()
            .filter_map(|notice| match notice {
                WebHostNotice::Frame(frame) => Some(frame),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].generation, 2);
        assert_eq!(&*frames[0].rgba8, &[2; 8]);
    }

    #[test]
    fn resize_input_ime_and_close_cross_the_fake_transport() {
        let (mut supervisor, peers, _, terminated) = harness();
        let surface_id = open(&mut supervisor);
        let mut host = peers
            .recv_timeout(Duration::from_secs(1))
            .expect("fake host started");
        assert!(matches!(recv(&mut host).payload, Some(Payload::OpenSurface(_))));
        supervisor.resize(surface_id, 4, 3).expect("resize queues");
        supervisor
            .send_input(WebSurfaceInput::Focus {
                surface_id,
                focused: true,
            })
            .expect("input queues");
        supervisor
            .send_ime(WebImeEvent::Commit {
                surface_id,
                text: "é".to_owned(),
            })
            .expect("ime queues");
        assert!(matches!(recv(&mut host).payload, Some(Payload::Resize(_))));
        assert!(matches!(recv(&mut host).payload, Some(Payload::RawInput(_))));
        assert!(matches!(recv(&mut host).payload, Some(Payload::ImeEvent(_))));
        supervisor.close("test close");
        assert!(matches!(recv(&mut host).payload, Some(Payload::CloseSurface(_))));
        for _ in 0..100 {
            if terminated.load(Ordering::Acquire) != 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(terminated.load(Ordering::Acquire), 1);
    }

    #[test]
    fn replacement_terminates_the_old_host_before_starting_the_new_one() {
        let (mut supervisor, peers, starts, terminated) = harness();
        let first = open(&mut supervisor);
        let mut old_host = peers
            .recv_timeout(Duration::from_secs(1))
            .expect("old host started");
        assert!(matches!(
            recv(&mut old_host).payload,
            Some(Payload::OpenSurface(_))
        ));
        let second = open(&mut supervisor);
        assert_ne!(first, second);
        assert!(matches!(
            recv(&mut old_host).payload,
            Some(Payload::CloseSurface(_))
        ));
        let mut new_host = peers
            .recv_timeout(Duration::from_secs(1))
            .expect("new host started");
        assert!(matches!(
            recv(&mut new_host).payload,
            Some(Payload::OpenSurface(_))
        ));
        assert_eq!(starts.load(Ordering::Acquire), 2);
        assert_eq!(terminated.load(Ordering::Acquire), 1);
    }

    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    fn session_shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        // SAFETY: setsid is async-signal-safe and is the only pre-exec action.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
    }

    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    fn contained_shell(script: &str, cgroup: &CgroupLease) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        let attachment = cgroup.attachment.as_raw_fd();
        let launcher_pid = std::process::id() as libc::pid_t;
        // SAFETY: use the production async-signal-safe pre-exec helper so no
        // test descendant executes before entering the dedicated cgroup.
        unsafe {
            command.pre_exec(move || prepare_host_process(launcher_pid, attachment));
        }
        command
    }

    #[cfg(target_os = "linux")]
    fn process_is_running(pid: u32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        stat.rsplit_once(") ")
            .and_then(|(_, fields)| fields.split_whitespace().next())
            .is_some_and(|state| state != "Z" && state != "X")
    }

    #[cfg(target_os = "linux")]
    fn assert_process_stopped(pid: u32) {
        for _ in 0..100 {
            if !process_is_running(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("process {pid} survived tree cleanup");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires a writable cgroup-v2 delegation with memory already enabled for children"]
    fn exited_root_still_cleans_up_its_detached_cgroup_child() {
        let cgroup = CgroupLease::prepare().expect("usable delegated ancestor");
        let mut command = contained_shell("setsid sh -c 'echo $$; exec sleep 30' &", &cgroup);
        command.stdout(Stdio::piped());
        let mut child = command.spawn().expect("contained root starts");
        let stdout = child.stdout.take().expect("root stdout is piped");
        let mut process = ProductionProcess {
            child,
            cgroup,
            terminated: false,
        };
        let mut survivor = String::new();
        BufReader::new(stdout)
            .read_line(&mut survivor)
            .expect("survivor pid is written");
        let survivor = survivor.trim().parse::<u32>().expect("survivor pid");

        let mut status = None;
        for _ in 0..100 {
            status = process.try_wait().expect("root status is readable");
            if status.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(status.is_some(), "contained root must exit before teardown");
        assert!(!process.terminated, "observing root exit is not cleanup");
        assert!(process_is_running(survivor), "surviving child must outlive root");

        process.terminate();

        assert!(process.terminated);
        assert_process_stopped(survivor);
        assert!(process.child.try_wait().expect("root remains reaped").is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires a writable cgroup-v2 delegation with memory already enabled for children"]
    fn cgroup_cleanup_contains_a_late_detached_descendant() {
        let directory = TestDirectory::new();
        let late_pid_file = directory.0.join("late-detached.pid");
        let cgroup = CgroupLease::prepare().expect("usable delegated ancestor");
        let script = concat!(
            "close_host() {\n",
            "  setsid sh -c 'echo $$ > \"$1\"; exec sleep 30' sh \"$1\" >/dev/null 2>&1 &\n",
            "  sleep 0.05\n",
            "  exit 0\n",
            "}\n",
            "trap 'close_host \"$1\"' TERM\n",
            "printf 'ready\\n'\n",
            "while :; do :; done"
        );
        let mut command = contained_shell(script, &cgroup);
        command.arg("sh").arg(&late_pid_file).stdout(Stdio::piped());
        let mut child = command.spawn().expect("late-forking root starts");
        let stdout = child.stdout.take().expect("fake root stdout is piped");
        let mut process = ProductionProcess {
            child,
            cgroup,
            terminated: false,
        };
        let mut ready = String::new();
        BufReader::new(stdout)
            .read_line(&mut ready)
            .expect("fake root reports readiness");
        assert_eq!(ready.trim(), "ready");

        process.terminate();

        let late_pid = std::fs::read_to_string(&late_pid_file)
            .expect("SIGTERM trap records detached descendant")
            .trim()
            .parse::<u32>()
            .expect("detached descendant pid");
        assert_process_stopped(late_pid);
        assert!(process.child.try_wait().expect("root is reaped").is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn graceful_root_close_runs_before_cgroup_escalation() {
        let directory = TestDirectory::new();
        let closed_file = directory.0.join("closed");
        let mut command = session_shell(concat!(
            "trap 'printf closed > \"$1\"; exit 0' TERM\n",
            "printf 'ready\\n'\n",
            "while :; do :; done"
        ));
        command.arg("sh").arg(&closed_file).stdout(Stdio::piped());
        let mut child = command.spawn().expect("graceful root starts");
        let stdout = child.stdout.take().expect("root stdout is piped");
        let mut process = ProductionProcess {
            child,
            cgroup: directory.cgroup(),
            terminated: false,
        };
        let mut ready = String::new();
        BufReader::new(stdout)
            .read_line(&mut ready)
            .expect("root reports installed SIGTERM handler");
        assert_eq!(ready.trim(), "ready");

        process.terminate();

        assert_eq!(
            std::fs::read(&closed_file).expect("graceful close marker"),
            b"closed"
        );
        assert!(process
            .child
            .try_wait()
            .expect("root is reaped")
            .expect("root exited")
            .success());
        assert_eq!(
            std::fs::read(directory.0.join("cgroup/cgroup.kill")).expect("kill control"),
            b"11",
            "graceful root exit still escalates over the dedicated cgroup"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn already_reaped_root_bypasses_numeric_signaling_and_still_kills_cgroup() {
        let directory = TestDirectory::new();
        let mut cgroup = directory.cgroup();
        let mut child = session_shell("exit 0").spawn().expect("short-lived root starts");
        let status = child.wait().expect("root is reaped before teardown");
        assert!(status.success());

        // This seam substitutes every raw process/group operation in cleanup.
        // It never sends a real signal to a potentially reused/unrelated PID.
        terminate_child_tree_with(&mut child, &mut cgroup, |_| {
            panic!("an already-reaped root must never trigger numeric signaling");
        });

        assert_eq!(child.try_wait().expect("cached root status"), Some(status));
        assert_eq!(
            std::fs::read(directory.0.join("cgroup/cgroup.kill")).expect("kill control"),
            b"11",
            "already-reaped root must still kill detached cgroup members"
        );
    }
}
