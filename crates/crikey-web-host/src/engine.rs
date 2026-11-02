//! Linux WPE adapter and framed host loop.
//!
//! Only this feature-gated module links the C shim. The GLib thread owns every
//! engine call. A blocking protocol reader and writer communicate with it
//! through bounded queues; a nonblocking socketpair is only a wakeup source,
//! so GLib never waits for the remainder of a partially received frame.

#![allow(unsafe_code)]

use std::ffi::{c_char, c_void, CStr, CString};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
    #[error("WPE refused to create the web surface")]
    SurfaceCreate,
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
        let mut bytes = Vec::new();
        self.reader.read_frame(&mut self.stream, &mut bytes)?;
        Envelope::decode(&bytes)
    }
}

#[derive(Debug)]
struct EngineLoop {
    active: bool,
}

impl EngineLoop {
    fn new() -> Self {
        Self { active: true }
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
pub fn run(endpoint: Endpoint, storage_dir: Option<PathBuf>) -> Result<(), RunError> {
    validate_endpoint(&endpoint)?;
    let Endpoint::UnixSocket(path) = &endpoint else {
        unreachable!("validate_endpoint accepts only Unix sockets");
    };
    let stream = UnixStream::connect(path)?;
    let control = stream.try_clone()?;
    let mut writer = SocketTransport::new(stream);
    writer.set_read_timeout(Some(FIRST_COMMAND_TIMEOUT))?;

    let first = match writer.recv() {
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
    let mut state = HostState::new(storage_dir)?;
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

    let init = unsafe { ck_web_init() };
    if init != CK_WEB_INIT_OK {
        let _ = send_direct_gone(
            &mut writer,
            open.surface_id,
            WebGoneCode::StartupFailed,
            format!("WPE initialization failed ({init})"),
        );
        return Err(RunError::EngineInit(init));
    }
    let engine = EngineLoop::new();
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
        ck_web_watch_termination(Some(on_termination), user);
        ck_web_run();
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
        // including when this failure races loop startup. Quitting also wakes
        // and stops an already-running loop immediately.
        let _ = wake.shutdown(Shutdown::Both);
        unsafe { ck_web_quit() };
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
    recycle: Receiver<Vec<u8>>,
    output: Arc<OutputBuffer>,
    frame_converter: FrameConverter,
    ime: ImeBridge,
    engine: EngineLoop,
    gone_sent: bool,
    terminating: bool,
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
        Self {
            state,
            surface: None,
            commands,
            terminal,
            wake_read,
            recycle,
            output,
            frame_converter: FrameConverter::new(),
            ime: ImeBridge::new(),
            engine,
            gone_sent: false,
            terminating: false,
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
            WebStorageMode::ModeUnspecified => {
                return Err(RunError::State(StateError::UnspecifiedStorageMode));
            }
        };
        let callbacks = CkWebCallbacks {
            user: (self as *mut Runtime).cast::<c_void>(),
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
        if self.terminating {
            return;
        }
        let mut bytes = [0_u8; COMMANDS_PER_DISPATCH];
        let count = match self.wake_read.read(&mut bytes) {
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
            if self.terminating {
                break;
            }
        }
    }

    fn apply(&mut self, action: HostAction) {
        if self.terminating {
            return;
        }
        if !matches!(action, HostAction::Ime(_)) {
            let ops = self.ime.flush();
            self.apply_ime_ops(ops);
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
                    modifiers,
                } => ck_web_surface_axis(surface.as_ptr(), time_ms, x, y, delta_x, delta_y, modifiers),
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

    fn push(&mut self, payload: Payload) {
        if let Some(rgba) = self.output.push(envelope(payload)) {
            self.frame_converter.recycle(rgba);
        }
    }

    fn emit_gone(&mut self, reason: WebGoneCode, detail: String) {
        if self.gone_sent {
            return;
        }
        self.gone_sent = true;
        self.push(Payload::Gone(Gone {
            surface_id: self.state.surface_id().unwrap_or(0),
            reason,
            detail,
            ..Default::default()
        }));
    }

    fn protocol_fault(&mut self, detail: String) {
        self.emit_gone(WebGoneCode::ProtocolViolation, detail);
        self.quit();
    }

    fn disconnect(&mut self, detail: String) {
        self.emit_gone(WebGoneCode::Closed, detail);
        self.quit();
    }

    fn quit(&mut self) {
        if !self.terminating {
            self.terminating = true;
            unsafe { ck_web_quit() };
        }
    }

    fn destroy_surface(&mut self) {
        if let Some(surface) = self.surface.take() {
            unsafe { ck_web_surface_destroy(surface.as_ptr()) };
        }
    }

    fn shutdown_engine(&mut self) {
        self.terminating = true;
        self.destroy_surface();
        self.engine.cleanup();
    }

    fn take_recycled_frame(&mut self) {
        while let Ok(rgba) = self.recycle.try_recv() {
            self.frame_converter.recycle(rgba);
        }
    }

    fn receive_frame(&mut self, bgra: *const u8, width: u32, height: u32, stride: u32) {
        if self.terminating || bgra.is_null() {
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
        let surface_id = self.state.surface_id().unwrap_or(0);
        match self
            .frame_converter
            .convert(surface_id, pixels, width, height, stride)
        {
            Ok(frame) => self.push(Payload::WebFrame(frame)),
            Err(error) => self.engine_frame_fault(&error.to_string()),
        }
    }

    fn engine_frame_fault(&mut self, detail: &str) {
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
    if let Some(runtime) = (user as *mut Runtime).as_mut() {
        if runtime.terminating {
            return;
        }
        runtime.emit_gone(WebGoneCode::Closed, "web host terminated".to_owned());
        runtime.quit();
    }
}

unsafe extern "C" fn on_frame(user: *mut c_void, bgra: *const u8, width: u32, height: u32, stride: u32) {
    if let Some(runtime) = (user as *mut Runtime).as_mut() {
        runtime.receive_frame(bgra, width, height, stride);
    }
}

unsafe extern "C" fn on_caret(user: *mut c_void, x: i32, y: i32, width: i32, height: i32) {
    if let Some(runtime) = (user as *mut Runtime).as_mut() {
        if runtime.terminating {
            return;
        }
        runtime.push(Payload::CaretArea(CaretArea {
            surface_id: runtime.state.surface_id().unwrap_or(0),
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
    if let Some(runtime) = (user as *mut Runtime).as_mut() {
        if runtime.terminating {
            return;
        }
        let state = WebLoadCode::from_i32(state);
        if state == WebLoadCode::StateUnspecified {
            runtime.protocol_fault("engine reported an unknown load state".to_owned());
            return;
        }
        runtime.push(Payload::LoadState(LoadState {
            surface_id: runtime.state.surface_id().unwrap_or(0),
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
    if let Some(runtime) = (user as *mut Runtime).as_mut() {
        if runtime.terminating {
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
        String::new()
    } else {
        CStr::from_ptr(value).to_string_lossy().into_owned()
    }
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
    use std::io;

    use super::write_wake_byte;

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
}
