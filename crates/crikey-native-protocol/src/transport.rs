//! Transport-independent framed duplex I/O (spec 16.2).
//!
//! Unix sockets are exercised on this host. The Windows named-pipe arm below
//! uses the Win32 pipe APIs and is compile-verified only on this host; runtime
//! verification of that arm requires Windows.

use std::io::{self, Cursor, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use crate::frame;
use crate::message::Envelope;
use crate::{Endpoint, Message, ProtocolError};
/// A framed, transport-independent native protocol connection.
pub trait Transport: std::fmt::Debug + Send {
    fn send(&mut self, envelope: &Envelope) -> Result<(), ProtocolError>;
    fn recv(&mut self) -> Result<Envelope, ProtocolError>;
    /// Returns an independent handle to the same duplex connection.
    ///
    /// The clone is deliberately explicit so a blocking reader can coexist
    /// with a writer on another thread (spec 16.2).
    fn try_clone_handle(&self) -> Result<Box<dyn Transport>, ProtocolError>;
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), ProtocolError>;
    fn close(&mut self);
}

#[cfg(any(unix, windows))]
fn map_io(error: io::Error) -> ProtocolError {
    if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) {
        ProtocolError::Timeout
    } else {
        ProtocolError::Io(error.to_string())
    }
}

#[derive(Debug)]
enum IoStream {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    Stdio {
        reader: io::Stdin,
        writer: io::Stdout,
    },
    #[cfg(windows)]
    NamedPipe(windows_pipe::PipeFile),
    Closed,
}
impl IoStream {
    fn try_clone_handle(&self) -> Result<Self, ProtocolError> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.try_clone().map(Self::Unix).map_err(map_io),
            Self::Stdio { .. } => Ok(Self::Stdio {
                reader: io::stdin(),
                writer: io::stdout(),
            }),
            #[cfg(windows)]
            Self::NamedPipe(file) => file.try_clone().map(Self::NamedPipe).map_err(map_io),
            Self::Closed => Err(ProtocolError::Closed),
        }
    }
}

impl Read for IoStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(bytes),
            Self::Stdio { reader, .. } => reader.read(bytes),
            #[cfg(windows)]
            Self::NamedPipe(file) => file.read(bytes),
            Self::Closed => Ok(0),
        }
    }
}

impl Write for IoStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(bytes),
            Self::Stdio { writer, .. } => writer.write(bytes),
            #[cfg(windows)]
            Self::NamedPipe(file) => file.write(bytes),
            Self::Closed => Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed transport")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
            Self::Stdio { writer, .. } => writer.flush(),
            #[cfg(windows)]
            Self::NamedPipe(file) => file.flush(),
            Self::Closed => Ok(()),
        }
    }
}

#[derive(Debug)]
struct IoTransport {
    stream: IoStream,
    read_timeout: Option<Duration>,
}

impl IoTransport {
    fn new(stream: IoStream) -> Self {
        Self {
            stream,
            read_timeout: None,
        }
    }

    fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<(), ProtocolError> {
        #[cfg(unix)]
        if let IoStream::Unix(stream) = &self.stream {
            stream.set_read_timeout(timeout).map_err(map_io)?;
        }
        #[cfg(windows)]
        if let IoStream::NamedPipe(file) = &mut self.stream {
            file.set_read_timeout(timeout).map_err(map_io)?;
        }
        #[cfg(not(any(unix, windows)))]
        let _ = timeout;
        self.read_timeout = timeout;
        Ok(())
    }
}

impl Transport for IoTransport {
    fn send(&mut self, envelope: &Envelope) -> Result<(), ProtocolError> {
        if matches!(self.stream, IoStream::Closed) {
            return Err(ProtocolError::Closed);
        }
        frame::write_frame(&mut self.stream, &envelope.encode())
    }

    fn recv(&mut self) -> Result<Envelope, ProtocolError> {
        if matches!(self.stream, IoStream::Closed) {
            return Err(ProtocolError::Closed);
        }
        let mut bytes = Vec::new();
        frame::read_frame(&mut self.stream, &mut bytes)?;
        Envelope::decode(&bytes)
    }

    fn try_clone_handle(&self) -> Result<Box<dyn Transport>, ProtocolError> {
        let stream = self.stream.try_clone_handle()?;
        let mut cloned = Self::new(stream);
        cloned.set_timeout(self.read_timeout)?;
        Ok(Box::new(cloned))
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), ProtocolError> {
        self.set_timeout(timeout)
    }

    fn close(&mut self) {
        self.stream = IoStream::Closed;
    }
}

#[derive(Debug)]
struct PairSide {
    sender: Option<Sender<Vec<u8>>>,
    receiver: Arc<Mutex<Receiver<Vec<u8>>>>,
    read_timeout: Option<Duration>,
}

impl Transport for PairSide {
    fn send(&mut self, envelope: &Envelope) -> Result<(), ProtocolError> {
        let sender = self.sender.as_ref().ok_or(ProtocolError::Closed)?;
        let payload = envelope.encode();
        let mut framed = Vec::with_capacity(payload.len().saturating_add(4));
        frame::write_frame(&mut framed, &payload)?;
        sender.send(framed).map_err(|_| ProtocolError::Closed)
    }

    fn recv(&mut self) -> Result<Envelope, ProtocolError> {
        let framed = {
            let receiver = self
                .receiver
                .lock()
                .map_err(|_| ProtocolError::Io("pair receiver lock poisoned".to_owned()))?;
            match self.read_timeout {
                Some(timeout) => receiver.recv_timeout(timeout).map_err(|error| match error {
                    RecvTimeoutError::Timeout => ProtocolError::Timeout,
                    RecvTimeoutError::Disconnected => ProtocolError::Closed,
                })?,
                None => receiver.recv().map_err(|_| ProtocolError::Closed)?,
            }
        };
        let mut cursor = Cursor::new(framed);
        let mut payload = Vec::new();
        frame::read_frame(&mut cursor, &mut payload)?;
        Envelope::decode(&payload)
    }

    fn try_clone_handle(&self) -> Result<Box<dyn Transport>, ProtocolError> {
        let sender = self.sender.as_ref().ok_or(ProtocolError::Closed)?.clone();
        Ok(Box::new(Self {
            sender: Some(sender),
            receiver: Arc::clone(&self.receiver),
            read_timeout: self.read_timeout,
        }))
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), ProtocolError> {
        self.read_timeout = timeout;
        Ok(())
    }

    fn close(&mut self) {
        self.sender = None;
    }
}

/// In-memory duplex pair used by SDK harnesses and deterministic tests.
pub fn pair() -> (Box<dyn Transport>, Box<dyn Transport>) {
    let (left_sender, left_receiver) = mpsc::channel();
    let (right_sender, right_receiver) = mpsc::channel();
    let left = PairSide {
        sender: Some(left_sender),
        receiver: Arc::new(Mutex::new(right_receiver)),
        read_timeout: None,
    };
    let right = PairSide {
        sender: Some(right_sender),
        receiver: Arc::new(Mutex::new(left_receiver)),
        read_timeout: None,
    };
    (Box::new(left), Box::new(right))
}

/// Host-side endpoint listener.
#[derive(Debug)]
pub struct Listener {
    endpoint: Endpoint,
    kind: ListenerKind,
}

#[derive(Debug)]
enum ListenerKind {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener),
    #[cfg(windows)]
    NamedPipe(windows_pipe::PipeListener),
}

impl Listener {
    /// Binds a host endpoint. Stdio is plugin-side only (spec 16.2).
    pub fn bind(endpoint: &Endpoint) -> Result<Self, ProtocolError> {
        match endpoint {
            Endpoint::Stdio => Err(ProtocolError::Malformed(
                "stdio is not a host listener endpoint".to_owned(),
            )),
            Endpoint::UnixSocket(path) => bind_unix(endpoint, path),
            Endpoint::NamedPipe(name) => bind_named_pipe(endpoint, name),
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Accepts one connection, returning `Timeout` when the optional bound expires.
    pub fn accept(&self, timeout: Option<Duration>) -> Result<Box<dyn Transport>, ProtocolError> {
        match &self.kind {
            #[cfg(unix)]
            ListenerKind::Unix(listener) => {
                if let Some(timeout) = timeout {
                    listener.set_nonblocking(true).map_err(map_io)?;
                    let deadline = Instant::now().checked_add(timeout);
                    loop {
                        match listener.accept() {
                            Ok((stream, _)) => return Ok(Box::new(IoTransport::new(IoStream::Unix(stream)))),
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                if deadline.is_some_and(|at| Instant::now() >= at) {
                                    return Err(ProtocolError::Timeout);
                                }
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            Err(error) => return Err(map_io(error)),
                        }
                    }
                }
                let (stream, _) = listener.accept().map_err(map_io)?;
                Ok(Box::new(IoTransport::new(IoStream::Unix(stream))))
            }
            #[cfg(windows)]
            ListenerKind::NamedPipe(listener) => {
                let stream = listener.accept(timeout)?;
                Ok(Box::new(IoTransport::new(IoStream::NamedPipe(stream))))
            }
        }
    }
}

#[cfg(unix)]
fn bind_unix(endpoint: &Endpoint, path: &std::path::Path) -> Result<Listener, ProtocolError> {
    std::os::unix::net::UnixListener::bind(path)
        .map(|listener| Listener {
            endpoint: endpoint.clone(),
            kind: ListenerKind::Unix(listener),
        })
        .map_err(map_io)
}

#[cfg(not(unix))]
fn bind_unix(_endpoint: &Endpoint, _path: &std::path::Path) -> Result<Listener, ProtocolError> {
    Err(ProtocolError::Malformed(
        "unix sockets are not available on this platform".to_owned(),
    ))
}

#[cfg(unix)]
fn connect_unix(
    path: &std::path::Path,
    timeout: Option<Duration>,
) -> Result<Box<dyn Transport>, ProtocolError> {
    let stream = std::os::unix::net::UnixStream::connect(path).map_err(map_io)?;
    let mut transport = IoTransport::new(IoStream::Unix(stream));
    transport.set_timeout(timeout)?;
    Ok(Box::new(transport))
}

#[cfg(not(unix))]
fn connect_unix(
    _path: &std::path::Path,
    _timeout: Option<Duration>,
) -> Result<Box<dyn Transport>, ProtocolError> {
    Err(ProtocolError::Malformed(
        "unix sockets are not available on this platform".to_owned(),
    ))
}

#[cfg(unix)]
impl Drop for Listener {
    fn drop(&mut self) {
        if let Endpoint::UnixSocket(path) = &self.endpoint {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(not(unix))]
impl Drop for Listener {
    fn drop(&mut self) {}
}

/// Plugin-side connection to a host endpoint.
pub fn connect(endpoint: &Endpoint, timeout: Option<Duration>) -> Result<Box<dyn Transport>, ProtocolError> {
    match endpoint {
        Endpoint::Stdio => Err(ProtocolError::Malformed(
            "use transport::stdio for the stdio endpoint".to_owned(),
        )),
        Endpoint::UnixSocket(path) => connect_unix(path, timeout),
        Endpoint::NamedPipe(name) => connect_named_pipe(name, timeout),
    }
}

/// Plugin-side transport over inherited stdin/stdout.
pub fn stdio() -> Box<dyn Transport> {
    Box::new(IoTransport::new(IoStream::Stdio {
        reader: io::stdin(),
        writer: io::stdout(),
    }))
}

#[cfg(not(windows))]
fn bind_named_pipe(_endpoint: &Endpoint, _name: &str) -> Result<Listener, ProtocolError> {
    Err(ProtocolError::Malformed("named pipes require Windows".to_owned()))
}

#[cfg(not(windows))]
fn connect_named_pipe(_name: &str, _timeout: Option<Duration>) -> Result<Box<dyn Transport>, ProtocolError> {
    Err(ProtocolError::Malformed("named pipes require Windows".to_owned()))
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_pipe {
    use std::ffi::OsStr;
    use std::io::{self, Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::sync::Mutex;
    use std::thread;
    use std::time::{Duration, Instant};

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, ERROR_BROKEN_PIPE, ERROR_NO_DATA,
        ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, HANDLE, INVALID_HANDLE_VALUE, WIN32_ERROR,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PeekNamedPipe, SetNamedPipeHandleState, PIPE_NOWAIT,
        PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    fn io_error(error: windows::core::Error) -> io::Error {
        match WIN32_ERROR::from_error(&error) {
            Some(ERROR_BROKEN_PIPE) => io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()),
            Some(ERROR_NO_DATA) => io::Error::new(io::ErrorKind::WouldBlock, error.to_string()),
            _ => io::Error::other(error.to_string()),
        }
    }

    #[derive(Debug)]
    pub struct PipeFile {
        handle: HANDLE,
        read_timeout: Option<Duration>,
    }
    // SAFETY: Win32 pipe handles are kernel-owned, transferable values; this
    // wrapper has unique ownership and all I/O is synchronized through &mut.
    unsafe impl Send for PipeFile {}

    impl PipeFile {
        pub fn connect(name: &str) -> Result<Self, String> {
            let path = format!(r"\\.\pipe\{name}");
            let wide = wide(&path);
            // SAFETY: `wide` is NUL-terminated and remains alive for the
            // synchronous call; the returned handle is owned by `PipeFile`.
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )
            }
            .map_err(|error| error.to_string())?;
            Ok(Self {
                handle,
                read_timeout: None,
            })
        }

        pub fn try_clone(&self) -> Result<Self, io::Error> {
            let mut target = HANDLE::default();
            // SAFETY: both process handles are pseudo-handles valid for this
            // call, and `target` is writable storage for the duplicate.
            unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    self.handle,
                    GetCurrentProcess(),
                    &mut target,
                    0,
                    false,
                    DUPLICATE_SAME_ACCESS,
                )
            }
            .map_err(io_error)?;
            Ok(Self {
                handle: target,
                read_timeout: self.read_timeout,
            })
        }

        pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
            let mode = if timeout.is_some() { PIPE_NOWAIT } else { PIPE_WAIT };
            // SAFETY: the mode pointer remains valid for the synchronous call
            // and `self.handle` is owned by this wrapper.
            unsafe { SetNamedPipeHandleState(self.handle, Some(&mode), None, None) }.map_err(io_error)?;
            self.read_timeout = timeout;
            Ok(())
        }

        fn wait_for_data(&self) -> io::Result<()> {
            let Some(timeout) = self.read_timeout else {
                return Ok(());
            };
            let deadline = Instant::now().checked_add(timeout);
            loop {
                let mut available = 0_u32;
                // SAFETY: querying availability does not provide an output
                // buffer; `available` is valid writable storage.
                match unsafe {
                    PeekNamedPipe(self.handle, None, 0, None, Some(&mut available as *mut u32), None)
                } {
                    Ok(()) => {}
                    Err(error) if WIN32_ERROR::from_error(&error) == Some(ERROR_NO_DATA) => {}
                    Err(error) => return Err(io_error(error)),
                }
                if available != 0 {
                    return Ok(());
                }
                if deadline.is_some_and(|at| Instant::now() >= at) {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "named pipe read timed out",
                    ));
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    impl Drop for PipeFile {
        fn drop(&mut self) {
            if !self.handle.is_invalid() {
                // SAFETY: this RAII wrapper owns the valid handle and drops it
                // exactly once.
                let _ = unsafe { CloseHandle(self.handle) };
            }
        }
    }

    impl Read for PipeFile {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if bytes.is_empty() {
                return Ok(0);
            }
            loop {
                self.wait_for_data()?;
                let mut count = 0_u32;
                // SAFETY: `bytes` is a valid mutable slice for the synchronous
                // ReadFile call and `count` points to writable storage.
                match unsafe { ReadFile(self.handle, Some(bytes), Some(&mut count as *mut u32), None) } {
                    Ok(()) => return Ok(count as usize),
                    Err(error)
                        if self.read_timeout.is_some()
                            && WIN32_ERROR::from_error(&error) == Some(ERROR_NO_DATA) =>
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => return Err(io_error(error)),
                }
            }
        }
    }

    impl Write for PipeFile {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut count = 0_u32;
            // SAFETY: `bytes` is a valid immutable slice for the synchronous
            // WriteFile call and `count` points to writable storage.
            unsafe { WriteFile(self.handle, Some(bytes), Some(&mut count as *mut u32), None) }
                .map(|()| count as usize)
                .map_err(io_error)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Debug)]
    pub struct PipeListener(Mutex<Option<HANDLE>>);

    impl PipeListener {
        pub fn bind(name: &str) -> Result<Self, String> {
            let path = format!(r"\\.\pipe\{name}");
            let wide = wide(&path);
            // PIPE_NOWAIT lets accept poll for a caller instead of blocking
            // forever; the accepted handle is switched back to PIPE_WAIT.
            // SAFETY: `wide` is NUL-terminated and remains alive for the
            // synchronous call; a successful handle is placed in the mutex.
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(wide.as_ptr()),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT,
                    1,
                    8 * 1024 * 1024,
                    8 * 1024 * 1024,
                    0,
                    None,
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err("CreateNamedPipeW returned an invalid handle".to_owned());
            }
            Ok(Self(Mutex::new(Some(handle))))
        }

        pub fn accept(&self, timeout: Option<Duration>) -> Result<PipeFile, crate::ProtocolError> {
            let mut guard = self
                .0
                .lock()
                .map_err(|_| crate::ProtocolError::Io("named-pipe listener lock poisoned".to_owned()))?;
            let handle = guard.take().ok_or(crate::ProtocolError::Closed)?;
            let file = PipeFile {
                handle,
                read_timeout: None,
            };
            let deadline = timeout.and_then(|value| Instant::now().checked_add(value));
            loop {
                // SAFETY: `file` owns the valid server handle and keeps it
                // alive through each non-blocking ConnectNamedPipe call.
                match unsafe { ConnectNamedPipe(file.handle, None) } {
                    Ok(()) => break,
                    Err(error) => match WIN32_ERROR::from_error(&error) {
                        Some(ERROR_PIPE_CONNECTED) => break,
                        Some(ERROR_PIPE_LISTENING) => {
                            if deadline.is_some_and(|at| Instant::now() >= at) {
                                return Err(crate::ProtocolError::Timeout);
                            }
                            thread::sleep(Duration::from_millis(1));
                        }
                        _ => return Err(crate::ProtocolError::Io(error.to_string())),
                    },
                }
            }
            // Restore blocking reads for the accepted connection. The
            // timeout path above is implemented by PeekNamedPipe polling.
            // SAFETY: `file.handle` is a connected named-pipe server handle;
            // the mode pointer remains valid for the synchronous call.
            unsafe { SetNamedPipeHandleState(file.handle, Some(&PIPE_WAIT), None, None) }
                .map_err(|error| crate::ProtocolError::Io(error.to_string()))?;
            Ok(file)
        }
    }

    impl Drop for PipeListener {
        fn drop(&mut self) {
            if let Ok(mut guard) = self.0.lock() {
                if let Some(handle) = guard.take() {
                    // SAFETY: the listener mutex owns this handle and no
                    // accepted PipeFile can reference a handle still inside it.
                    let _ = unsafe { CloseHandle(handle) };
                }
            }
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }
}

#[cfg(windows)]
fn bind_named_pipe(endpoint: &Endpoint, name: &str) -> Result<Listener, ProtocolError> {
    let kind = windows_pipe::PipeListener::bind(name).map_err(ProtocolError::Io)?;
    Ok(Listener {
        endpoint: endpoint.clone(),
        kind: ListenerKind::NamedPipe(kind),
    })
}

#[cfg(windows)]
fn connect_named_pipe(name: &str, _timeout: Option<Duration>) -> Result<Box<dyn Transport>, ProtocolError> {
    let file = windows_pipe::PipeFile::connect(name).map_err(ProtocolError::Io)?;
    Ok(Box::new(IoTransport::new(IoStream::NamedPipe(file))))
}
