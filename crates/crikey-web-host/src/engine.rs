//! Linux WPE adapter and framed host loop.
//!
//! Only this feature-gated module links the C shim. The GLib thread owns every
//! engine call. A blocking protocol reader and writer communicate with it
//! through bounded queues; a nonblocking socketpair is only a wakeup source,
//! so GLib never waits for the remainder of a partially received frame.

#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_void, CString};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crikey_native_protocol::message::{
    CaretArea, Envelope, Gone, LoadState, Payload, WebGoneCode, WebImeCode, WebLoadCode, WebStorageMode,
    MAX_WEB_FRAME_BYTES, MAX_WEB_FRAME_EDGE,
};
use crikey_native_protocol::{frame, Endpoint, Message, ProtocolError};

use crate::frame::FrameConverter;
use crate::ime::{ImeBridge, ImeOp};
use crate::input::EngineInput;
use crate::output::{take_frame_pixels, OutputBuffer};
use crate::state::{HostAction, HostState, OpenSpec, StateError};

const FIRST_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
const SHUTDOWN_FORCE_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_CAPACITY: usize = 64;
const COMMANDS_PER_DISPATCH: usize = 32;
// Page-controlled informational strings must not turn a bounded output queue
// into unbounded allocations or exceed the transport's frame limit.
const MAX_ENGINE_TEXT_BYTES: usize = 8_192;
const CK_WEB_INIT_OK: i32 = 0;
const CK_WEB_STORAGE_EPHEMERAL: i32 = 1;
const CK_WEB_STORAGE_PERSISTENT: i32 = 2;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("web host endpoint must be an absolute Unix socket path")]
    InvalidEndpoint,
    #[error("web host protocol failed: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("web host command was refused: {0}")]
    State(#[from] StateError),
    #[error("web host I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("could not start web-host thread: {0}")]
    Thread(io::Error),
    #[error("WPE initialization failed ({0})")]
    EngineInit(i32),
    #[error("WPE may be initialized only once per web-host process")]
    EngineAlreadyStarted,
    #[error("WPE refused to create the web surface")]
    SurfaceCreate,
    #[error("WPE persistent storage directory must be valid UTF-8")]
    StorageEncoding,
    #[error("web-host {0} thread did not stop after transport shutdown")]
    ThreadTimeout(&'static str),
}

#[derive(Debug)]
struct SocketTransport {
    stream: UnixStream,
    reader: frame::FrameReader,
}

impl SocketTransport {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            reader: frame::FrameReader::new(),
        }
    }

    fn try_clone(&self) -> Result<Self, io::Error> {
        self.stream.try_clone().map(Self::new)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), io::Error> {
        self.stream.set_read_timeout(timeout)
    }

    fn send(&mut self, envelope: &Envelope) -> Result<(), ProtocolError> {
        frame::write_frame(&mut self.stream, &envelope.encode())
    }

    fn recv(&mut self) -> Result<Envelope, ProtocolError> {
        self.recv_from(None)
    }

    fn recv_until(&mut self, deadline: Instant) -> Result<Envelope, ProtocolError> {
        self.recv_from(Some(deadline))
    }

    fn recv_from(&mut self, deadline: Option<Instant>) -> Result<Envelope, ProtocolError> {
        let mut bytes = Vec::new();
        let mut reader = DeadlineReader {
            stream: &mut self.stream,
            deadline,
        };
        self.reader.read_frame(&mut reader, &mut bytes)?;
        Envelope::decode(&bytes)
    }
}

struct DeadlineReader<'a> {
    stream: &'a mut UnixStream,
    deadline: Option<Instant>,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if let Some(deadline) = self.deadline {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "OpenSurface deadline expired"))?;
            self.stream.set_read_timeout(Some(remaining))?;
        }
        self.stream.read(bytes)
    }
}

#[derive(Debug)]
struct EngineLoop {
    active: bool,
}

impl EngineLoop {
    fn new() -> Result<Self, RunError> {
        // The shim, GLib default context and WPE loader have process-global,
        // thread-affine state. A safe public run() must not let concurrent or
        // later callers clean up another runtime or move WPE to a new thread.
        // A failed initialization can also leave library-global state behind;
        // retries require a fresh host process, just like normal page opens.
        static STARTED: AtomicBool = AtomicBool::new(false);
        if STARTED.swap(true, Ordering::Relaxed) {
            return Err(RunError::EngineAlreadyStarted);
        }
        let init = unsafe { ck_web_init() };
        if init != CK_WEB_INIT_OK {
            return Err(RunError::EngineInit(init));
        }
        Ok(Self { active: true })
    }

    fn cleanup(&mut self) {
        if self.active {
            unsafe { ck_web_cleanup() };
            self.active = false;
        }
    }
}

impl Drop for EngineLoop {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Connects to the launcher's private endpoint and owns one WPE surface until
/// CloseSurface, transport loss, engine failure or supervised termination.
///
/// The process may enter WPE initialization only once. Concurrent or later
/// calls report StartupFailed without accessing the process-global engine.
pub fn run(endpoint: Endpoint, storage_dir: Option<PathBuf>) -> Result<(), RunError> {
    validate_endpoint(&endpoint)?;
    let Endpoint::UnixSocket(path) = &endpoint else {
        unreachable!("validate_endpoint accepts only Unix sockets");
    };
    let stream = UnixStream::connect(path)?;
    let control = stream.try_clone()?;
    let mut writer = SocketTransport::new(stream);
    writer.stream.set_write_timeout(Some(SHUTDOWN_GRACE))?;

    let first = match writer.recv_until(Instant::now() + FIRST_COMMAND_TIMEOUT) {
        Ok(first) => first,
        Err(error) => {
            let _ = send_direct_gone(
                &mut writer,
                0,
                WebGoneCode::ProtocolViolation,
                format!("failed to receive OpenSurface: {error}"),
            );
            return Err(RunError::Protocol(error));
        }
    };
    let hinted_surface_id = payload_surface_id(first.payload.as_ref());
    let mut state = match HostState::new(storage_dir) {
        Ok(state) => state,
        Err(error) => {
            let _ = send_direct_gone(
                &mut writer,
                hinted_surface_id,
                WebGoneCode::StartupFailed,
                error.to_string(),
            );
            return Err(RunError::State(error));
        }
    };
    let open = match state.receive(first) {
        Ok(HostAction::Open(open)) => open,
        Ok(_) => unreachable!("HostState only accepts OpenSurface first"),
        Err(error) => {
            let _ = send_direct_gone(
                &mut writer,
                hinted_surface_id,
                WebGoneCode::ProtocolViolation,
                error.to_string(),
            );
            return Err(RunError::State(error));
        }
    };
    // WebKit's directory and cookie APIs take UTF-8, not arbitrary Unix
    // filename bytes. Reject an unrepresentable profile rather than allowing
    // its path conversion to select default/shared storage. Ephemeral opens
    // carry no storage directory and remain independent of this setting.
    if open
        .storage_dir
        .as_deref()
        .is_some_and(|path| path.to_str().is_none())
    {
        let error = RunError::StorageEncoding;
        let _ = send_direct_gone(
            &mut writer,
            open.surface_id,
            WebGoneCode::StartupFailed,
            error.to_string(),
        );
        return Err(error);
    }

    let engine = match EngineLoop::new() {
        Ok(engine) => engine,
        Err(error) => {
            let _ = send_direct_gone(
                &mut writer,
                open.surface_id,
                WebGoneCode::StartupFailed,
                error.to_string(),
            );
            return Err(error);
        }
    };
    if let Some(path) = open.storage_dir.as_deref() {
        if let Err(error) = std::fs::create_dir_all(path) {
            let _ = send_direct_gone(
                &mut writer,
                open.surface_id,
                WebGoneCode::StartupFailed,
                format!("could not create persistent storage directory: {error}"),
            );
            return Err(RunError::Io(error));
        }
    }

    writer.set_read_timeout(None)?;
    writer.stream.set_write_timeout(None)?;
    let reader = writer.try_clone()?;
    let (command_tx, command_rx) = mpsc::sync_channel(COMMAND_CAPACITY);
    let (terminal_tx, terminal_rx) = mpsc::channel();
    let (wake_read, wake_write) = UnixStream::pair()?;
    wake_read.set_nonblocking(true)?;
    wake_write.set_nonblocking(true)?;

    let output = Arc::new(OutputBuffer::new());
    let (recycle_tx, recycle_rx) = mpsc::sync_channel(2);
    let reader_worker = spawn_reader(reader, command_tx, terminal_tx.clone(), wake_write.try_clone()?)?;
    let writer_worker = match spawn_writer(writer, Arc::clone(&output), recycle_tx, terminal_tx, wake_write) {
        Ok(worker) => worker,
        Err(error) => {
            let _ = control.shutdown(Shutdown::Both);
            let _ = finish_reader(reader_worker);
            return Err(error);
        }
    };

    let mut runtime = Box::new(Runtime::new(
        state,
        command_rx,
        terminal_rx,
        wake_read,
        recycle_rx,
        Arc::clone(&output),
        engine,
    ));
    if let Err(error) = runtime.create_surface(&open) {
        runtime.emit_gone(WebGoneCode::StartupFailed, error.to_string());
        runtime.shutdown_engine();
        shutdown_workers(&control, &output, reader_worker, writer_worker)?;
        return Err(error);
    }

    let user = (&mut *runtime as *mut Runtime).cast::<c_void>();
    unsafe {
        ck_web_watch_fd(runtime.wake_read.as_raw_fd(), Some(on_commands_ready), user);
        ck_web_watch_termination(
            Some(on_termination),
            Rc::as_ptr(&runtime.callbacks).cast_mut().cast(),
        );
        if !runtime.callbacks.terminating.get() {
            ck_web_run();
        }
    }

    runtime.shutdown_engine();
    shutdown_workers(&control, &output, reader_worker, writer_worker)
}

fn validate_endpoint(endpoint: &Endpoint) -> Result<(), RunError> {
    match endpoint {
        Endpoint::UnixSocket(path) if path.is_absolute() => Ok(()),
        _ => Err(RunError::InvalidEndpoint),
    }
}

fn send_direct_gone(
    transport: &mut SocketTransport,
    surface_id: u64,
    reason: WebGoneCode,
    detail: String,
) -> Result<(), ProtocolError> {
    transport.send(&envelope(Payload::Gone(Gone {
        surface_id,
        reason,
        detail,
        ..Default::default()
    })))
}

fn envelope(payload: Payload) -> Envelope {
    Envelope {
        payload: Some(payload),
        ..Default::default()
    }
}

fn payload_surface_id(payload: Option<&Payload>) -> u64 {
    match payload {
        Some(Payload::OpenSurface(value)) => value.surface_id,
        Some(Payload::RawInput(value)) => value.surface_id,
        Some(Payload::ImeEvent(value)) => value.surface_id,
        Some(Payload::Navigate(value)) => value.surface_id,
        Some(Payload::Resize(value)) => value.surface_id,
        Some(Payload::CloseSurface(value)) => value.surface_id,
        Some(Payload::WebFrame(value)) => value.surface_id,
        Some(Payload::CaretArea(value)) => value.surface_id,
        Some(Payload::LoadState(value)) => value.surface_id,
        Some(Payload::Gone(value)) => value.surface_id,
        _ => 0,
    }
}

#[derive(Debug)]
enum TerminalEvent {
    ProtocolFault(String),
    Disconnected(String),
}

#[derive(Debug)]
struct ReaderWorker {
    thread: JoinHandle<()>,
    done: Receiver<()>,
}

#[derive(Debug)]
struct WriterWorker {
    thread: JoinHandle<Result<(), ProtocolError>>,
    done: Receiver<()>,
}

fn write_wake_byte(mut write: impl FnMut() -> io::Result<usize>) -> io::Result<()> {
    loop {
        match write() {
            Ok(1) => return Ok(()),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "transport wakeup write returned zero bytes",
                ));
            }
            Ok(_) => unreachable!("single-byte wakeup write exceeded its buffer"),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

fn signal_loop(wake: &mut UnixStream) {
    if write_wake_byte(|| wake.write(&[1])).is_err() {
        // Closing the shared socket endpoint leaves a persistent HUP for GLib,
        // including when this failure races loop startup. Only the loop thread
        // may access the C loop: cleanup can otherwise race a worker's quit.
        let _ = wake.shutdown(Shutdown::Both);
    }
}

fn spawn_reader(
    mut reader: SocketTransport,
    commands: SyncSender<Envelope>,
    terminal: Sender<TerminalEvent>,
    mut wake: UnixStream,
) -> Result<ReaderWorker, RunError> {
    let (done_tx, done) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("crikey-web-reader".to_owned())
        .spawn(move || {
            loop {
                match reader.recv() {
                    Ok(envelope) => match commands.try_send(envelope) {
                        Ok(()) => signal_loop(&mut wake),
                        Err(TrySendError::Full(_)) => {
                            let _ = terminal.send(TerminalEvent::ProtocolFault(
                                "web-host command queue capacity exceeded".to_owned(),
                            ));
                            signal_loop(&mut wake);
                            break;
                        }
                        Err(TrySendError::Disconnected(_)) => break,
                    },
                    Err(error @ (ProtocolError::Closed | ProtocolError::Io(_))) => {
                        let _ = terminal.send(TerminalEvent::Disconnected(error.to_string()));
                        signal_loop(&mut wake);
                        break;
                    }
                    Err(error) => {
                        let _ = terminal.send(TerminalEvent::ProtocolFault(error.to_string()));
                        signal_loop(&mut wake);
                        break;
                    }
                }
            }
            let _ = done_tx.send(());
        })
        .map_err(RunError::Thread)?;
    Ok(ReaderWorker { thread, done })
}

fn spawn_writer(
    mut writer: SocketTransport,
    output: Arc<OutputBuffer>,
    recycle: SyncSender<Vec<u8>>,
    terminal: Sender<TerminalEvent>,
    mut wake: UnixStream,
) -> Result<WriterWorker, RunError> {
    let (done_tx, done) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("crikey-web-writer".to_owned())
        .spawn(move || {
            let result = loop {
                let Some(mut next) = output.pop() else {
                    break Ok(());
                };
                if let Err(error) = writer.send(&next) {
                    let _ = terminal.send(TerminalEvent::Disconnected(error.to_string()));
                    signal_loop(&mut wake);
                    break Err(error);
                }
                if let Some(rgba) = take_frame_pixels(&mut next) {
                    let _ = recycle.try_send(rgba);
                }
            };
            let _ = done_tx.send(());
            result
        })
        .map_err(RunError::Thread)?;
    Ok(WriterWorker { thread, done })
}

fn shutdown_workers(
    control: &UnixStream,
    output: &OutputBuffer,
    reader: ReaderWorker,
    writer: WriterWorker,
) -> Result<(), RunError> {
    output.close();
    match writer.done.recv_timeout(SHUTDOWN_GRACE) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
        Err(RecvTimeoutError::Timeout) => {
            let _ = control.shutdown(Shutdown::Both);
            wait_for_completion(&writer.done, "writer")?;
        }
    }
    let _ = control.shutdown(Shutdown::Both);
    wait_for_completion(&reader.done, "reader")?;

    let writer_result = match writer.thread.join() {
        Ok(result) => result.map_err(RunError::Protocol),
        Err(_) => Err(RunError::Thread(io::Error::other(
            "web-host writer thread panicked",
        ))),
    };
    let reader_result = match reader.thread.join() {
        Ok(()) => Ok(()),
        Err(_) => Err(RunError::Thread(io::Error::other(
            "web-host reader thread panicked",
        ))),
    };
    writer_result.and(reader_result)
}

fn finish_reader(reader: ReaderWorker) -> Result<(), RunError> {
    wait_for_completion(&reader.done, "reader")?;
    match reader.thread.join() {
        Ok(()) => Ok(()),
        Err(_) => Err(RunError::Thread(io::Error::other(
            "web-host reader thread panicked",
        ))),
    }
}

fn wait_for_completion(done: &Receiver<()>, name: &'static str) -> Result<(), RunError> {
    match done.recv_timeout(SHUTDOWN_FORCE_TIMEOUT) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => Ok(()),
        Err(RecvTimeoutError::Timeout) => Err(RunError::ThreadTimeout(name)),
    }
}

#[derive(Debug)]
struct Runtime {
    state: HostState,
    surface: Option<NonNull<CkWebSurface>>,
    commands: Receiver<Envelope>,
    terminal: Receiver<TerminalEvent>,
    wake_read: UnixStream,
    callbacks: Rc<CallbackState>,
    ime: ImeBridge,
    engine: EngineLoop,
}

impl Runtime {
    fn new(
        state: HostState,
        commands: Receiver<Envelope>,
        terminal: Receiver<TerminalEvent>,
        wake_read: UnixStream,
        recycle: Receiver<Vec<u8>>,
        output: Arc<OutputBuffer>,
        engine: EngineLoop,
    ) -> Self {
        let surface_id = state.surface_id().unwrap_or(0);
        Self {
            state,
            surface: None,
            commands,
            terminal,
            wake_read,
            callbacks: Rc::new(CallbackState::new(surface_id, recycle, output)),
            ime: ImeBridge::new(),
            engine,
        }
    }

    fn create_surface(&mut self, open: &OpenSpec) -> Result<(), RunError> {
        let url = CString::new(open.url.as_bytes()).map_err(|_| RunError::State(StateError::InvalidUrl))?;
        let storage = match open.storage_dir.as_deref() {
            Some(path) => Some(
                CString::new(path.as_os_str().as_bytes())
                    .map_err(|_| RunError::State(StateError::InvalidStorageDirectory))?,
            ),
            None => None,
        };
        let storage_mode = match open.storage_mode {
            WebStorageMode::Ephemeral => CK_WEB_STORAGE_EPHEMERAL,
            WebStorageMode::Persistent => CK_WEB_STORAGE_PERSISTENT,
            WebStorageMode::ModeUnspecified => CK_WEB_STORAGE_EPHEMERAL,
        };
        let callbacks = CkWebCallbacks {
            user: Rc::as_ptr(&self.callbacks).cast_mut().cast::<c_void>(),
            frame: Some(on_frame),
            caret: Some(on_caret),
            load_state: Some(on_load_state),
            gone: Some(on_gone),
        };
        let surface = unsafe {
            ck_web_surface_new(
                open.pixel_width,
                open.pixel_height,
                storage_mode,
                storage.as_ref().map_or(std::ptr::null(), |path| path.as_ptr()),
                &callbacks,
            )
        };
        self.surface = NonNull::new(surface);
        let Some(surface) = self.surface else {
            return Err(RunError::SurfaceCreate);
        };
        unsafe { ck_web_surface_load_uri(surface.as_ptr(), url.as_ptr()) };
        Ok(())
    }

    fn process_commands(&mut self) {
        if self.callbacks.terminating.get() {
            return;
        }
        let mut bytes = [0_u8; COMMANDS_PER_DISPATCH];
        let ready = loop {
            match self.wake_read.read(&mut bytes) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result,
            }
        };
        let count = match ready {
            Ok(0) => {
                self.disconnect("transport wakeup closed".to_owned());
                return;
            }
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => 0,
            Err(error) => {
                self.disconnect(format!("transport wakeup failed: {error}"));
                return;
            }
        };
        match self.terminal.try_recv() {
            Ok(TerminalEvent::ProtocolFault(detail)) => {
                self.protocol_fault(detail);
                return;
            }
            Ok(TerminalEvent::Disconnected(detail)) => {
                self.disconnect(detail);
                return;
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        }
        for _ in 0..count {
            match self.commands.try_recv() {
                Ok(envelope) => match self.state.receive(envelope) {
                    Ok(action) => self.apply(action),
                    Err(error) => self.protocol_fault(error.to_string()),
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.disconnect("transport reader stopped".to_owned());
                    break;
                }
            }
            if self.callbacks.terminating.get() {
                break;
            }
        }
    }

    fn apply(&mut self, action: HostAction) {
        if self.callbacks.terminating.get() {
            return;
        }
        let ops = match &action {
            HostAction::Navigate(_)
            | HostAction::Close { .. }
            | HostAction::Input(EngineInput::Focus(false)) => self.ime.cancel(),
            HostAction::Ime(_) => Vec::new(),
            _ => self.ime.flush(),
        };
        self.apply_ime_ops(ops);
        if self.callbacks.terminating.get() {
            return;
        }
        let Some(surface) = self.surface else {
            self.protocol_fault("command arrived without a live surface".to_owned());
            return;
        };
        match action {
            HostAction::Open(_) => self.protocol_fault("duplicate OpenSurface action".to_owned()),
            HostAction::Navigate(url) => match CString::new(url) {
                Ok(url) => unsafe { ck_web_surface_load_uri(surface.as_ptr(), url.as_ptr()) },
                Err(_) => self.protocol_fault("navigation URL contains a NUL byte".to_owned()),
            },
            HostAction::Resize {
                pixel_width,
                pixel_height,
            } => unsafe {
                ck_web_surface_resize(surface.as_ptr(), pixel_width, pixel_height);
            },
            HostAction::Input(input) => self.apply_input(surface, input),
            HostAction::Ime(event) => {
                let ops = match event.kind {
                    WebImeCode::Preedit => self.ime.preedit(
                        event.text,
                        event
                            .has_cursor
                            .then_some((event.cursor_begin_chars, event.cursor_end_chars)),
                    ),
                    WebImeCode::Commit => self.ime.commit(event.text),
                    WebImeCode::KindUnspecified => {
                        self.protocol_fault("IME kind is unspecified".to_owned());
                        return;
                    }
                };
                self.apply_ime_ops(ops);
            }
            HostAction::Close { reason } => {
                let detail = if reason.is_empty() {
                    "surface closed by launcher".to_owned()
                } else {
                    reason
                };
                self.emit_gone(WebGoneCode::Closed, detail);
                self.quit();
            }
        }
    }

    fn apply_input(&mut self, surface: NonNull<CkWebSurface>, input: EngineInput) {
        unsafe {
            match input {
                EngineInput::Key {
                    time_ms,
                    keysym,
                    hardware_keycode,
                    pressed,
                    repeat,
                    modifiers,
                } => ck_web_surface_key(
                    surface.as_ptr(),
                    time_ms,
                    keysym,
                    hardware_keycode,
                    i32::from(pressed),
                    i32::from(repeat),
                    modifiers,
                ),
                EngineInput::PointerMotion {
                    time_ms,
                    x,
                    y,
                    modifiers,
                } => ck_web_surface_pointer_motion(surface.as_ptr(), time_ms, x, y, modifiers),
                EngineInput::PointerButton {
                    time_ms,
                    x,
                    y,
                    button,
                    pressed,
                    modifiers,
                } => ck_web_surface_pointer_button(
                    surface.as_ptr(),
                    time_ms,
                    x,
                    y,
                    button,
                    u32::from(pressed),
                    modifiers,
                ),
                EngineInput::Axis {
                    time_ms,
                    x,
                    y,
                    delta_x,
                    delta_y,
                    precise,
                    modifiers,
                } => ck_web_surface_axis(
                    surface.as_ptr(),
                    time_ms,
                    x,
                    y,
                    delta_x,
                    delta_y,
                    i32::from(precise),
                    modifiers,
                ),
                EngineInput::Focus(focused) => {
                    ck_web_surface_focus(surface.as_ptr(), i32::from(focused));
                }
            }
        }
    }

    fn apply_ime_ops(&mut self, ops: Vec<ImeOp>) {
        let Some(surface) = self.surface else {
            return;
        };
        for op in ops {
            if self.callbacks.terminating.get() {
                break;
            }
            unsafe {
                match op {
                    ImeOp::SetPreedit { text, cursor_chars } => {
                        if let Ok(text) = CString::new(text) {
                            ck_web_surface_set_preedit(surface.as_ptr(), text.as_ptr(), cursor_chars);
                        } else {
                            self.protocol_fault("IME preedit contains a NUL byte".to_owned());
                            return;
                        }
                    }
                    ImeOp::PreeditStarted => ck_web_surface_preedit_started(surface.as_ptr()),
                    ImeOp::PreeditChanged => ck_web_surface_preedit_changed(surface.as_ptr()),
                    ImeOp::PreeditFinished => ck_web_surface_preedit_finished(surface.as_ptr()),
                    ImeOp::Commit(text) => {
                        if let Ok(text) = CString::new(text) {
                            ck_web_surface_commit(surface.as_ptr(), text.as_ptr());
                        } else {
                            self.protocol_fault("IME commit contains a NUL byte".to_owned());
                            return;
                        }
                    }
                }
            }
        }
    }

    fn emit_gone(&self, reason: WebGoneCode, detail: String) {
        self.callbacks.emit_gone(reason, detail);
    }

    fn protocol_fault(&mut self, detail: String) {
        self.emit_gone(WebGoneCode::ProtocolViolation, detail);
        self.quit();
    }

    fn disconnect(&mut self, detail: String) {
        self.emit_gone(WebGoneCode::Closed, detail);
        self.quit();
    }

    fn quit(&self) {
        self.callbacks.quit();
    }

    fn destroy_surface(&mut self) {
        if let Some(surface) = self.surface.take() {
            unsafe { ck_web_surface_destroy(surface.as_ptr()) };
        }
    }

    fn shutdown_engine(&mut self) {
        self.callbacks.terminating.set(true);
        let _ = self.ime.cancel();
        self.destroy_surface();
        self.engine.cleanup();
    }
}

// Surface callbacks may be emitted synchronously by any WPE call. They must
// never reconstruct a second &mut Runtime while a command is borrowing it.
// Rc gives the C userdata a stable shared allocation independent of Runtime's
// exclusive command borrow; only the frame converter needs interior mutation.
#[derive(Debug)]
struct CallbackState {
    surface_id: u64,
    recycle: Receiver<Vec<u8>>,
    output: Arc<OutputBuffer>,
    frame_converter: RefCell<FrameConverter>,
    gone_sent: Cell<bool>,
    terminating: Cell<bool>,
}

impl CallbackState {
    fn new(surface_id: u64, recycle: Receiver<Vec<u8>>, output: Arc<OutputBuffer>) -> Self {
        Self {
            surface_id,
            recycle,
            output,
            frame_converter: RefCell::new(FrameConverter::new()),
            gone_sent: Cell::new(false),
            terminating: Cell::new(false),
        }
    }

    fn push(&self, payload: Payload) {
        if let Some(rgba) = self.output.push(envelope(payload)) {
            self.frame_converter.borrow_mut().recycle(rgba);
        }
    }

    fn emit_gone(&self, reason: WebGoneCode, mut detail: String) {
        if self.gone_sent.replace(true) {
            return;
        }
        truncate_text(&mut detail);
        self.push(Payload::Gone(Gone {
            surface_id: self.surface_id,
            reason,
            detail,
            ..Default::default()
        }));
    }

    fn protocol_fault(&self, detail: String) {
        self.emit_gone(WebGoneCode::ProtocolViolation, detail);
        self.quit();
    }

    fn quit(&self) {
        if !self.terminating.replace(true) {
            unsafe { ck_web_quit() };
        }
    }

    fn take_recycled_frame(&self) {
        while let Ok(rgba) = self.recycle.try_recv() {
            self.frame_converter.borrow_mut().recycle(rgba);
        }
    }

    fn receive_frame(&self, bgra: *const u8, width: u32, height: u32, stride: u32) {
        if self.terminating.get() || bgra.is_null() {
            return;
        }
        self.take_recycled_frame();
        let row_bytes = match usize::try_from(width).ok().and_then(|width| width.checked_mul(4)) {
            Some(row_bytes) => row_bytes,
            None => {
                self.engine_frame_fault("exported frame row size overflowed");
                return;
            }
        };
        let required = match usize::try_from(height)
            .ok()
            .and_then(|height| height.checked_sub(1))
            .and_then(|rows| rows.checked_mul(stride as usize))
            .and_then(|prefix| prefix.checked_add(row_bytes))
        {
            Some(required) => required,
            None => {
                self.engine_frame_fault("exported frame mapping size overflowed");
                return;
            }
        };
        if (stride as usize) < row_bytes || required > isize::MAX as usize {
            self.engine_frame_fault("exported frame stride or mapping size is invalid");
            return;
        }
        // Refuse impossible geometry before forming a slice from the borrowed
        // mapping. The C shim has already verified stride and owns the mapping
        // for this callback's duration.
        let pixels = match (width, height) {
            (0, _) | (_, 0) => {
                self.engine_frame_fault("exported frame has an empty dimension");
                return;
            }
            (w, h)
                if w > MAX_WEB_FRAME_EDGE
                    || h > MAX_WEB_FRAME_EDGE
                    || (w as usize) * (h as usize) * 4 > MAX_WEB_FRAME_BYTES =>
            {
                self.engine_frame_fault("exported frame is outside protocol bounds");
                return;
            }
            _ => unsafe { std::slice::from_raw_parts(bgra, required) },
        };
        let converted =
            self.frame_converter
                .borrow_mut()
                .convert(self.surface_id, pixels, width, height, stride);
        match converted {
            Ok(frame) => self.push(Payload::WebFrame(frame)),
            Err(error) => self.engine_frame_fault(&error.to_string()),
        }
    }

    fn engine_frame_fault(&self, detail: &str) {
        self.emit_gone(WebGoneCode::Crashed, detail.to_owned());
        self.quit();
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.shutdown_engine();
    }
}

unsafe extern "C" fn on_commands_ready(user: *mut c_void) {
    if let Some(runtime) = (user as *mut Runtime).as_mut() {
        runtime.process_commands();
    }
}

unsafe extern "C" fn on_termination(user: *mut c_void) {
    if let Some(runtime) = (user as *const CallbackState).as_ref() {
        if runtime.terminating.get() {
            return;
        }
        runtime.emit_gone(WebGoneCode::Closed, "web host terminated".to_owned());
        runtime.quit();
    }
}

unsafe extern "C" fn on_frame(user: *mut c_void, bgra: *const u8, width: u32, height: u32, stride: u32) {
    if let Some(runtime) = (user as *const CallbackState).as_ref() {
        runtime.receive_frame(bgra, width, height, stride);
    }
}

unsafe extern "C" fn on_caret(user: *mut c_void, x: i32, y: i32, width: i32, height: i32) {
    if let Some(runtime) = (user as *const CallbackState).as_ref() {
        if runtime.terminating.get() {
            return;
        }
        runtime.push(Payload::CaretArea(CaretArea {
            surface_id: runtime.surface_id,
            x: x as f32,
            y: y as f32,
            width: width.max(0) as f32,
            height: height.max(0) as f32,
            ..Default::default()
        }));
    }
}

unsafe extern "C" fn on_load_state(
    user: *mut c_void,
    state: i32,
    url: *const c_char,
    failure: *const c_char,
    can_go_back: i32,
    can_go_forward: i32,
    title: *const c_char,
) {
    if let Some(runtime) = (user as *const CallbackState).as_ref() {
        if runtime.terminating.get() {
            return;
        }
        let state = WebLoadCode::from_i32(state);
        if state == WebLoadCode::StateUnspecified {
            runtime.protocol_fault("engine reported an unknown load state".to_owned());
            return;
        }
        runtime.push(Payload::LoadState(LoadState {
            surface_id: runtime.surface_id,
            state,
            url: borrowed_c_string(url),
            failure: borrowed_c_string(failure),
            can_go_back: can_go_back != 0,
            can_go_forward: can_go_forward != 0,
            title: borrowed_c_string(title),
            ..Default::default()
        }));
    }
}

unsafe extern "C" fn on_gone(user: *mut c_void, reason: i32, detail: *const c_char) {
    if let Some(runtime) = (user as *const CallbackState).as_ref() {
        if runtime.terminating.get() {
            return;
        }
        let reason = WebGoneCode::from_i32(reason);
        let reason = if reason == WebGoneCode::ReasonUnspecified {
            WebGoneCode::Crashed
        } else {
            reason
        };
        runtime.emit_gone(reason, borrowed_c_string(detail));
        runtime.quit();
    }
}

unsafe fn borrowed_c_string(value: *const c_char) -> String {
    if value.is_null() {
        return String::new();
    }
    let mut length = 0;
    while length < MAX_ENGINE_TEXT_BYTES && *value.add(length) != 0 {
        length += 1;
    }
    let bytes = std::slice::from_raw_parts(value.cast::<u8>(), length);
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    truncate_text(&mut text);
    text
}

fn truncate_text(text: &mut String) {
    let mut end = text.len().min(MAX_ENGINE_TEXT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

#[repr(C)]
struct CkWebSurface {
    _private: [u8; 0],
}

#[repr(C)]
struct CkWebCallbacks {
    user: *mut c_void,
    frame: Option<unsafe extern "C" fn(*mut c_void, *const u8, u32, u32, u32)>,
    caret: Option<unsafe extern "C" fn(*mut c_void, i32, i32, i32, i32)>,
    load_state:
        Option<unsafe extern "C" fn(*mut c_void, i32, *const c_char, *const c_char, i32, i32, *const c_char)>,
    gone: Option<unsafe extern "C" fn(*mut c_void, i32, *const c_char)>,
}

extern "C" {
    fn ck_web_init() -> i32;
    fn ck_web_cleanup();
    fn ck_web_surface_new(
        width: u32,
        height: u32,
        storage_mode: i32,
        storage_dir: *const c_char,
        callbacks: *const CkWebCallbacks,
    ) -> *mut CkWebSurface;
    fn ck_web_surface_load_uri(surface: *mut CkWebSurface, uri: *const c_char);
    fn ck_web_surface_resize(surface: *mut CkWebSurface, width: u32, height: u32);
    fn ck_web_surface_key(
        surface: *mut CkWebSurface,
        time_ms: u32,
        keysym: u32,
        hardware_keycode: u32,
        pressed: i32,
        repeat: i32,
        modifiers: u32,
    );
    fn ck_web_surface_pointer_motion(
        surface: *mut CkWebSurface,
        time_ms: u32,
        x: i32,
        y: i32,
        modifiers: u32,
    );
    fn ck_web_surface_pointer_button(
        surface: *mut CkWebSurface,
        time_ms: u32,
        x: i32,
        y: i32,
        button: u32,
        state: u32,
        modifiers: u32,
    );
    fn ck_web_surface_axis(
        surface: *mut CkWebSurface,
        time_ms: u32,
        x: i32,
        y: i32,
        delta_x: f64,
        delta_y: f64,
        precise: i32,
        modifiers: u32,
    );
    fn ck_web_surface_focus(surface: *mut CkWebSurface, focused: i32);
    fn ck_web_surface_set_preedit(surface: *mut CkWebSurface, text: *const c_char, cursor_chars: u32);
    fn ck_web_surface_preedit_started(surface: *mut CkWebSurface);
    fn ck_web_surface_preedit_changed(surface: *mut CkWebSurface);
    fn ck_web_surface_preedit_finished(surface: *mut CkWebSurface);
    fn ck_web_surface_commit(surface: *mut CkWebSurface, text: *const c_char);
    fn ck_web_surface_destroy(surface: *mut CkWebSurface);
    fn ck_web_watch_fd(fd: i32, ready: Option<unsafe extern "C" fn(*mut c_void)>, user: *mut c_void);
    fn ck_web_watch_termination(ready: Option<unsafe extern "C" fn(*mut c_void)>, user: *mut c_void);
    fn ck_web_run();
    fn ck_web_quit();
}

#[cfg(test)]
mod tests {
    use std::ffi::{c_char, c_void, CString};
    use std::io::{self, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::ptr;
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::{Duration, Instant};

    use crikey_native_protocol::message::{OpenSurface, Payload, WebGoneCode, WebLoadCode, WebStorageMode};
    use crikey_native_protocol::{Endpoint, ProtocolError};

    use crate::output::OutputBuffer;

    use super::{
        borrowed_c_string, ck_web_surface_destroy, ck_web_surface_new, envelope, on_load_state, run,
        write_wake_byte, CallbackState, CkWebCallbacks, DeadlineReader, EngineLoop, RunError,
        SocketTransport, CK_WEB_STORAGE_EPHEMERAL, MAX_ENGINE_TEXT_BYTES,
    };

    extern "C" {
        fn g_error_new_literal(domain: u32, code: i32, message: *const c_char) -> *mut c_void;
        fn g_error_free(error: *mut c_void);
        fn webkit_media_error_quark() -> u32;
        fn webkit_network_error_quark() -> u32;
        fn webkit_policy_error_quark() -> u32;
        fn ck_web_on_load_failed(
            view: *mut c_void,
            event: i32,
            uri: *mut c_char,
            error: *mut c_void,
            surface: *mut c_void,
        ) -> i32;
    }

    fn assert_native_load_failure_callbacks() {
        let uri = CString::new("https://example.test/native-load").unwrap();
        let detail = CString::new("native error regression").unwrap();
        let cases = unsafe {
            [
                (webkit_media_error_quark(), 204, false),
                (webkit_media_error_quark(), 999, true),
                (webkit_network_error_quark(), 302, false),
                (webkit_network_error_quark(), 399, true),
                (webkit_policy_error_quark(), 102, false),
                (webkit_policy_error_quark(), 100, true),
            ]
        };
        for (domain, code, reports_failure) in cases {
            let (_, recycle) = mpsc::channel();
            let output = Arc::new(OutputBuffer::new());
            let callbacks = CallbackState::new(7, recycle, Arc::clone(&output));
            let native_callbacks = CkWebCallbacks {
                user: ptr::from_ref(&callbacks).cast_mut().cast(),
                frame: None,
                caret: None,
                load_state: Some(on_load_state),
                gone: None,
            };
            unsafe {
                let surface =
                    ck_web_surface_new(320, 200, CK_WEB_STORAGE_EPHEMERAL, ptr::null(), &native_callbacks);
                assert!(!surface.is_null(), "create real WPE surface for native callback");
                let error = g_error_new_literal(domain, code, detail.as_ptr());
                assert!(!error.is_null());
                assert_eq!(
                    ck_web_on_load_failed(ptr::null_mut(), 0, uri.as_ptr().cast_mut(), error, surface.cast(),),
                    1,
                );
                g_error_free(error);
                ck_web_surface_destroy(surface);
            }
            output.close();
            let update = output.pop();
            if reports_failure {
                let Some(Payload::LoadState(state)) = update.and_then(|update| update.payload) else {
                    panic!("native error {domain}/{code} did not report Failed");
                };
                assert_eq!(state.state, WebLoadCode::Failed);
                assert_eq!(state.url, "https://example.test/native-load");
                assert_eq!(state.failure, "native error regression");
            } else {
                assert!(
                    update.is_none(),
                    "expected control-flow error {domain}/{code} leaked {update:?}"
                );
            }
        }
    }

    #[test]
    fn wpe_process_lifetime_and_native_load_failure_boundaries() {
        let engine = EngineLoop::new().expect("initialize WPE for this process");
        assert_native_load_failure_callbacks();
        let socket =
            std::env::temp_dir().join(format!("crikey-web-engine-reuse-{}.sock", std::process::id()));
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = Endpoint::UnixSocket(socket.clone());
        let worker = thread::spawn(move || run(endpoint, None));
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "second runtime connection deadline expired"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("second runtime connection failed: {error}"),
            }
        };
        let mut transport = SocketTransport::new(stream);
        transport
            .send(&envelope(Payload::OpenSurface(OpenSurface {
                surface_id: 7,
                pixel_width: 320,
                pixel_height: 200,
                url: "http://127.0.0.1/".to_owned(),
                storage_mode: WebStorageMode::Ephemeral,
                ..OpenSurface::default()
            })))
            .unwrap();
        let response = transport
            .recv_until(Instant::now() + Duration::from_secs(5))
            .unwrap();
        let Some(Payload::Gone(gone)) = response.payload else {
            panic!("expected structured startup rejection");
        };
        assert_eq!(gone.surface_id, 7);
        assert_eq!(gone.reason, WebGoneCode::StartupFailed);
        assert!(gone.detail.contains("only once"), "{}", gone.detail);
        assert!(matches!(
            worker.join().unwrap(),
            Err(RunError::EngineAlreadyStarted)
        ));
        drop(engine);
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn wake_write_retries_interruption_until_the_byte_is_written() {
        let mut attempts = 0;
        let result = write_wake_byte(|| {
            attempts += 1;
            if attempts < 3 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok(1)
            }
        });

        assert!(result.is_ok());
        assert_eq!(attempts, 3);
    }

    #[test]
    fn wake_write_accepts_a_full_nonblocking_socket() {
        let mut attempts = 0;
        let result = write_wake_byte(|| {
            attempts += 1;
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        });

        assert!(result.is_ok());
        assert_eq!(attempts, 1);
    }

    #[test]
    fn wake_write_returns_unexpected_failures() {
        let result = write_wake_byte(|| Err(io::Error::from(io::ErrorKind::BrokenPipe)));

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn wake_write_rejects_a_zero_length_write() {
        let result = write_wake_byte(|| Ok(0));

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WriteZero);
    }

    #[test]
    fn startup_deadline_is_not_extended_by_a_drip_fed_partial_frame() {
        let (mut peer, stream) = UnixStream::pair().unwrap();
        peer.write_all(&[0, 0, 0, 20]).unwrap();
        let drip = thread::spawn(move || {
            for _ in 0..20 {
                thread::sleep(Duration::from_millis(5));
                if peer.write_all(&[0]).is_err() {
                    break;
                }
            }
        });
        let mut transport = SocketTransport::new(stream);
        assert!(matches!(
            transport.recv_until(Instant::now() + Duration::from_millis(20)),
            Err(ProtocolError::Timeout)
        ));
        drop(transport);
        drip.join().unwrap();
    }

    #[test]
    fn expired_startup_deadline_does_not_consume_already_available_bytes() {
        let (mut peer, mut stream) = UnixStream::pair().unwrap();
        peer.write_all(b"ab").unwrap();
        let mut reader = DeadlineReader {
            stream: &mut stream,
            deadline: Some(Instant::now() - Duration::from_millis(1)),
        };
        let mut byte = [0];
        assert_eq!(
            reader.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        stream.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [b'a']);
    }

    #[test]
    fn callback_metadata_is_bounded_before_copying_and_preserves_utf8() {
        let (_, recycle) = mpsc::channel();
        let output = Arc::new(OutputBuffer::new());
        let callbacks = CallbackState::new(7, recycle, Arc::clone(&output));
        let title = CString::new("é".repeat(MAX_ENGINE_TEXT_BYTES)).unwrap();
        unsafe {
            on_load_state(
                ptr::from_ref(&callbacks).cast_mut().cast(),
                WebLoadCode::Finished as i32,
                title.as_ptr(),
                title.as_ptr(),
                0,
                0,
                title.as_ptr(),
            );
        }
        let Some(Payload::LoadState(state)) = output.pop().unwrap().payload else {
            panic!("expected load state");
        };
        assert_eq!(state.surface_id, 7);
        for value in [state.title, state.url, state.failure] {
            assert!(value.len() <= MAX_ENGINE_TEXT_BYTES);
            assert!(value.chars().all(|character| character == 'é'));
        }
        assert!(unsafe { borrowed_c_string(ptr::null()) }.is_empty());
    }

    #[test]
    fn terminal_detail_is_bounded_and_callbacks_cannot_replace_it() {
        let (_, recycle) = mpsc::channel();
        let output = Arc::new(OutputBuffer::new());
        let callbacks = CallbackState::new(7, recycle, Arc::clone(&output));
        callbacks.emit_gone(WebGoneCode::Closed, "界".repeat(MAX_ENGINE_TEXT_BYTES));
        callbacks.emit_gone(WebGoneCode::Crashed, "second cause".to_owned());
        let Some(Payload::Gone(gone)) = output.pop().unwrap().payload else {
            panic!("expected terminal Gone");
        };
        assert_eq!(gone.reason, WebGoneCode::Closed);
        assert!(gone.detail.len() <= MAX_ENGINE_TEXT_BYTES);
        assert!(gone.detail.chars().all(|character| character == '界'));
    }
}
