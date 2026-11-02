//! Lazy, one-surface supervisor for the out-of-process WPE host.
//!
//! The launcher thread only writes bounded mailboxes and polls completed
//! output. Executable discovery, process creation, endpoint acceptance, framed
//! I/O, and teardown all stay on the manager thread below.

use std::collections::VecDeque;
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
use std::time::Duration;

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
const MANAGER_IDLE_POLL: Duration = Duration::from_millis(25);
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
    Warning(String),
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
            WebHostNotice::Warning(_) => true,
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
        self.next_surface_id = self.next_surface_id.checked_add(1).unwrap_or(0);
        if surface_id == 0 {
            return Err("the web surface id space is exhausted".to_owned());
        }
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
        self.desired_surface.store(surface_id, Ordering::Release);
        if let Err(error) = self.command.try_send(Control::Open(request)) {
            self.desired_surface.store(0, Ordering::Release);
            return Err(queue_error("open a web surface", error));
        }
        let mut mailbox = lock_recover(&self.mailbox);
        mailbox.latest_frame = None;
        mailbox.latest_caret = None;
        Ok(surface_id)
    }

    pub(crate) fn navigate(&self, surface_id: u64, url: String) -> Result<(), String> {
        if url.is_empty() {
            return Ok(());
        }
        self.command
            .try_send(Control::Navigate { surface_id, url })
            .map_err(|error| queue_error("navigate the web surface", error))
    }

    pub(crate) fn resize(&self, surface_id: u64, pixel_width: u32, pixel_height: u32) -> Result<(), String> {
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
        let desired = self.desired_surface.load(Ordering::Acquire);
        let mut mailbox = lock_recover(&self.mailbox);
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
                WebHostNotice::Warning(_) => true,
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
    warning: Option<String>,
}

impl std::fmt::Debug for RunningHost {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("RunningHost")
            .field("transport", &self.transport)
            .field("warning", &self.warning)
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
            match host.runtime.process.try_wait() {
                Ok(Some(status)) => {
                    let surface_id = host.surface_id;
                    publish_fatal(
                        &mailbox,
                        &desired_surface,
                        &wake,
                        surface_id,
                        format!("web host exited unexpectedly ({status})"),
                    );
                    teardown(&mut active, "host exited");
                    continue;
                }
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
                Ok(None) => {}
            }

            match host.runtime.transport.recv() {
                Ok(envelope) => {
                    if let Some(reason) = handle_incoming(envelope, host, &mailbox, &desired_surface, &wake) {
                        teardown(&mut active, &reason);
                    }
                }
                Err(ProtocolError::Timeout) => {}
                Err(error) => {
                    let surface_id = host.surface_id;
                    publish_fatal(
                        &mailbox,
                        &desired_surface,
                        &wake,
                        surface_id,
                        format!("web host connection failed: {error}"),
                    );
                    teardown(&mut active, "connection failed");
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
            teardown(active, "surface was replaced");
            if desired_surface.load(Ordering::Acquire) != request.surface_id {
                return;
            }
            let surface_id = request.surface_id;
            let pixel_width = request.pixel_width;
            let pixel_height = request.pixel_height;
            let url = request.url.clone();
            match spawner.spawn(&request) {
                Ok(mut runtime) => {
                    if let Some(warning) = runtime.warning.take() {
                        publish_warning(mailbox, wake, warning);
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
                    publish_fatal(
                        mailbox,
                        desired_surface,
                        wake,
                        surface_id,
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
                    publish_fatal(
                        mailbox,
                        desired_surface,
                        wake,
                        surface_id,
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
                publish_fatal(
                    mailbox,
                    desired_surface,
                    wake,
                    host.surface_id,
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
    lock_recover(mailbox).latest_frame = Some(frame);
    wake();
}

fn publish_caret(
    mailbox: &Arc<Mutex<Mailbox>>,
    desired_surface: &AtomicU64,
    wake: &WebWake,
    caret: CaretArea,
) {
    if desired_surface.load(Ordering::Acquire) != caret.surface_id {
        return;
    }
    lock_recover(mailbox).latest_caret =
        Some((caret.surface_id, caret.x, caret.y, caret.width, caret.height));
    wake();
}

fn publish_warning(mailbox: &Arc<Mutex<Mailbox>>, wake: &WebWake, message: String) {
    lock_recover(mailbox).push_notice(WebHostNotice::Warning(message));
    wake();
}

fn publish_fatal(
    mailbox: &Arc<Mutex<Mailbox>>,
    desired_surface: &AtomicU64,
    wake: &WebWake,
    surface_id: u64,
    message: String,
) {
    if desired_surface.load(Ordering::Acquire) != surface_id {
        return;
    }
    let mut mailbox = lock_recover(mailbox);
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
#[allow(unsafe_code)]
fn spawn_production(request: &OpenRequest) -> Result<RunningHost, String> {
    use std::os::unix::process::CommandExt;

    let executable = web_host_executable()?;
    let storage_dir = match request.storage {
        WebStorage::Ephemeral => None,
        WebStorage::Persistent => Some(persistent_storage_dir(&request.storage_root, &request.plugin)?),
    };
    let endpoint = PrivateEndpoint::create()?;
    let listener = Listener::bind(endpoint.endpoint())
        .map_err(|error| format!("cannot bind the private web-host endpoint: {error}"))?;

    let memory = MemoryPolicy::detect()?;
    let mut cgroup = match memory {
        MemoryPolicy::Enforce { bytes } => match CgroupLease::create(bytes) {
            Ok(lease) => Some(lease),
            Err(error) => {
                let _ = error;
                None
            }
        },
        MemoryPolicy::Unavailable(_) => None,
    };
    let mut warning = match &memory {
        MemoryPolicy::Unavailable(reason) => {
            Some(format!("web host memory enforcement is unavailable: {reason}"))
        }
        MemoryPolicy::Enforce { .. } => None,
    };
    if matches!(memory, MemoryPolicy::Enforce { .. }) && cgroup.is_none() {
        warning = Some(
            "web host memory enforcement is unavailable: this cgroup-v2 session does not delegate a writable memory controller"
                .to_owned(),
        );
    }

    let mut command = Command::new(&executable);
    command
        .arg("--endpoint")
        .arg(endpoint.endpoint().to_spec())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(storage_dir) = storage_dir.as_ref() {
        command.arg("--storage-dir").arg(storage_dir);
    }
    // SAFETY: this closure executes after fork and before exec. It calls only
    // async-signal-safe libc functions, creates a new process group/session,
    // and arranges graceful SIGTERM when the launcher dies. The host installs
    // SIGTERM cleanup; manager teardown escalates over the whole tree.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() == 1 {
                libc::_exit(127);
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot spawn `{}`: {error}", executable.display()))?;
    let pid = child.id();
    if let Some(lease) = cgroup.as_mut() {
        if let Err(error) = lease.attach(pid) {
            warning = Some(format!("web host memory enforcement is unavailable: {error}"));
            cgroup = None;
        }
    }
    let mut transport = match listener.accept(Some(ACCEPT_TIMEOUT)) {
        Ok(transport) => transport,
        Err(error) => {
            terminate_child_tree(&mut child, cgroup.as_mut());
            return Err(format!(
                "web host did not connect to its private endpoint within {} ms: {error}",
                ACCEPT_TIMEOUT.as_millis()
            ));
        }
    };
    if let Err(error) = transport.set_read_timeout(Some(READ_POLL)) {
        transport.close();
        terminate_child_tree(&mut child, cgroup.as_mut());
        return Err(format!("cannot set the web-host receive deadline: {error}"));
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
        warning,
    })
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
    if let Some(configured) = std::env::var_os(ENV_WEB_HOST_OVERRIDE) {
        if configured.is_empty() {
            return Err(format!("{ENV_WEB_HOST_OVERRIDE} is set but empty"));
        }
        let path = PathBuf::from(configured);
        return path.is_file().then_some(path.clone()).ok_or_else(|| {
            format!(
                "{ENV_WEB_HOST_OVERRIDE} names `{}`, which is not an executable file",
                path.display()
            )
        });
    }

    let name = host_file_name();
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            let beside = directory.join(&name);
            if beside.is_file() {
                return Ok(beside);
            }
            // Cargo's test harnesses live in `target/<profile>/deps`; the host
            // built for the same profile lives one directory above.
            if directory.file_name().is_some_and(|part| part == "deps") {
                if let Some(profile) = directory.parent() {
                    let beside_profile = profile.join(&name);
                    if beside_profile.is_file() {
                        return Ok(beside_profile);
                    }
                }
            }
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join(&name);
            if candidate.is_file() {
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
fn persistent_storage_dir(root: &Path, plugin: &PluginId) -> Result<PathBuf, String> {
    let mut encoded = String::with_capacity(plugin.0.len() * 2);
    for byte in plugin.0.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    let directory = root.join(encoded);
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
fn create_private_dir(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(path)
        .map_err(|error| format!("cannot create private directory `{}`: {error}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        format!(
            "cannot restrict private directory `{}` to this account: {error}",
            path.display()
        )
    })
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
            match std::fs::create_dir(&directory) {
                Ok(()) => {
                    create_private_dir(&directory)?;
                    return Ok(Self {
                        endpoint: Endpoint::UnixSocket(directory.join("host.sock")),
                        directory,
                    });
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
enum MemoryPolicy {
    Enforce { bytes: u64 },
    Unavailable(String),
}

#[cfg(target_os = "linux")]
impl MemoryPolicy {
    fn detect() -> Result<Self, String> {
        let physical = match physical_memory_bytes() {
            Ok(bytes) => bytes,
            Err(error) => return Ok(Self::Unavailable(error)),
        };
        let requested = MAX_WEB_MEMORY_BYTES.min(physical / 8);
        let inherited = current_cgroup_path()
            .ok()
            .and_then(|path| inherited_memory_limit(&path).ok().flatten());
        let effective = inherited.map_or(requested, |limit| requested.min(limit));
        if effective < MIN_WEB_MEMORY_BYTES {
            return Err(format!(
                "web surface refused: its effective memory ceiling is {} MiB, below the 256 MiB minimum",
                effective / (1024 * 1024)
            ));
        }
        if !Path::new("/sys/fs/cgroup/cgroup.controllers").is_file() {
            return Ok(Self::Unavailable(
                "the unified cgroup-v2 hierarchy is not mounted".to_owned(),
            ));
        }
        Ok(Self::Enforce { bytes: effective })
    }
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
fn current_cgroup_path() -> Result<PathBuf, String> {
    let text = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|error| format!("cannot read the launcher's cgroup: {error}"))?;
    let relative = text
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| "the launcher is not in a unified cgroup-v2 hierarchy".to_owned())?;
    Ok(Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/')))
}

#[cfg(target_os = "linux")]
fn inherited_memory_limit(path: &Path) -> Result<Option<u64>, String> {
    let root = Path::new("/sys/fs/cgroup");
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
}

#[cfg(target_os = "linux")]
impl CgroupLease {
    fn create(bytes: u64) -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let parent = current_cgroup_path()?;
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("crikey-web-{}-{sequence}", std::process::id()));
        std::fs::create_dir(&path)
            .map_err(|error| format!("cannot create delegated cgroup `{}`: {error}", path.display()))?;
        let memory_max = path.join("memory.max");
        if let Err(error) = std::fs::write(&memory_max, bytes.to_string()) {
            let _ = std::fs::remove_dir(&path);
            return Err(format!(
                "cannot set `{}` to {bytes} bytes: {error}",
                memory_max.display()
            ));
        }
        Ok(Self { path })
    }

    fn attach(&mut self, pid: u32) -> Result<(), String> {
        let procs = self.path.join("cgroup.procs");
        std::fs::write(&procs, pid.to_string())
            .map_err(|error| format!("cannot attach pid {pid} through `{}`: {error}", procs.display()))
    }

    fn kill_tree(&mut self) {
        let kill = self.path.join("cgroup.kill");
        if kill.is_file() {
            let _ = std::fs::write(kill, "1");
        }
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
    cgroup: Option<CgroupLease>,
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
        terminate_child_tree(&mut self.child, self.cgroup.as_mut());
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
fn terminate_child_tree(child: &mut Child, mut cgroup: Option<&mut CgroupLease>) {
    let root = child.id();
    let mut observed = descendant_pids(root);
    // SAFETY: negative pid addresses the session/process group created in
    // `pre_exec`; SIGTERM is the host's graceful engine-destroy path.
    unsafe {
        libc::kill(-(root as i32), libc::SIGTERM);
    }
    for _ in 0..PROCESS_TERMINATION_PASSES {
        remember_descendants(root, &mut observed);
        let root_exited = child.try_wait().ok().flatten().is_some();
        if root_exited && !process_group_exists(root) && !observed.iter().copied().any(process_is_running) {
            break;
        }
        std::thread::sleep(PROCESS_TERMINATION_POLL);
    }

    // Descendants can fork and create a new session while the root is handling
    // SIGTERM. Re-scan from every process observed so far before each bounded
    // escalation pass; that retains ancestry even after the root exits.
    for _ in 0..PROCESS_TERMINATION_PASSES {
        let descendants = remember_descendants(root, &mut observed);
        if let Some(cgroup) = cgroup.as_deref_mut() {
            cgroup.kill_tree();
        }
        // SAFETY: every positive pid was discovered through procfs as part of
        // this dedicated host tree. Negative root addresses its process group.
        // ESRCH is expected when a process exits between discovery and signal.
        unsafe {
            for pid in observed.iter().rev() {
                libc::kill(*pid as i32, libc::SIGKILL);
            }
            libc::kill(-(root as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.try_wait();
        if descendants.is_empty()
            && !process_group_exists(root)
            && !observed.iter().copied().any(process_is_running)
        {
            break;
        }
        std::thread::sleep(PROCESS_TERMINATION_POLL);
    }
    let _ = child.wait();
}

#[cfg(target_os = "linux")]
fn remember_descendants(root: u32, observed: &mut Vec<u32>) -> Vec<u32> {
    let mut roots = Vec::with_capacity(observed.len() + 1);
    roots.push(root);
    roots.extend(observed.iter().copied());
    let descendants = descendant_pids_from(&roots);
    for pid in descendants.iter().copied() {
        if !observed.contains(&pid) {
            observed.push(pid);
        }
    }
    descendants
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn process_group_exists(root: u32) -> bool {
    // SAFETY: signal 0 only checks whether the dedicated process group exists.
    unsafe {
        libc::kill(-(root as i32), 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
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
fn descendant_pids(root: u32) -> Vec<u32> {
    descendant_pids_from(&[root])
}

#[cfg(target_os = "linux")]
fn descendant_pids_from(roots: &[u32]) -> Vec<u32> {
    let mut parents = std::collections::BTreeMap::<u32, Vec<u32>>::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(after_name) = stat.rsplit_once(") ").map(|(_, fields)| fields) else {
            continue;
        };
        let Some(parent) = after_name
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        parents.entry(parent).or_default().push(pid);
    }
    let mut output = Vec::new();
    let mut pending = VecDeque::from(roots.to_vec());
    let mut visited = roots.iter().copied().collect::<std::collections::BTreeSet<_>>();
    while let Some(parent) = pending.pop_front() {
        if let Some(children) = parents.remove(&parent) {
            for child in children {
                if visited.insert(child) {
                    output.push(child);
                    pending.push_back(child);
                }
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    use super::*;
    use crikey_native_protocol::transport;

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
                warning: None,
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
        let starts = Arc::new(AtomicUsize::new(0));
        let terminated = Arc::new(AtomicUsize::new(0));
        let (peers, receiver) = mpsc::channel();
        let supervisor = WebHostSupervisor::with_spawner(
            PathBuf::from("/unused"),
            Arc::new(|| {}),
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

    #[test]
    fn stale_surface_and_generation_frames_are_rejected() {
        let (mut supervisor, peers, _, _) = harness();
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
        std::thread::sleep(Duration::from_millis(40));
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
        use std::os::unix::process::CommandExt;

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
    fn exited_root_still_cleans_up_its_surviving_process_group() {
        use std::io::{BufRead, BufReader};

        let mut command = session_shell("sleep 30 & printf '%s\n' \"$!\"");
        command.stdout(Stdio::piped());
        let mut child = command.spawn().expect("fake root starts");
        let stdout = child.stdout.take().expect("fake root stdout is piped");
        let mut process = ProductionProcess {
            child,
            cgroup: None,
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
        assert!(status.is_some(), "fake root must exit before teardown");
        assert!(!process.terminated, "observing root exit is not cleanup");
        assert!(process_is_running(survivor), "surviving child must outlive root");

        process.terminate();

        assert!(process.terminated);
        assert_process_stopped(survivor);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tree_cleanup_discovers_a_late_detached_descendant() {
        use std::io::{BufRead, BufReader};

        static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);
        let late_pid_file = std::env::temp_dir().join(format!(
            "crikey-web-host-late-{}-{}",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&late_pid_file);
        let script = concat!(
            "trap 'setsid sleep 30 >/dev/null 2>&1 & echo $! > \"$1\"; ",
            "sleep 0.15' TERM\n",
            "printf 'ready\\n'\n",
            "while :; do sleep 1; done"
        );
        let mut command = session_shell(script);
        command.arg("sh").arg(&late_pid_file).stdout(Stdio::piped());
        let mut child = command.spawn().expect("late-forking root starts");
        let stdout = child.stdout.take().expect("fake root stdout is piped");
        let mut process = ProductionProcess {
            child,
            cgroup: None,
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
        let _ = std::fs::remove_file(&late_pid_file);
        assert_process_stopped(late_pid);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[allow(unsafe_code)]
    fn proc_descendant_walk_finds_a_real_grandchild_for_tree_cleanup() {
        let mut child = session_shell("sh -c 'sleep 30 & wait' & wait")
            .spawn()
            .expect("fake tree starts");
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            descendant_pids(child.id()).len() >= 2,
            "the fake executable must have a child and grandchild"
        );
        terminate_child_tree(&mut child, None);
        assert!(child.try_wait().expect("wait succeeds").is_some());
    }
}
