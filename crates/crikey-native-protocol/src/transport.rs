//! Transport-independent framed duplex I/O (spec 16.2).
//!
//! Unix sockets are exercised on this host. The Windows named-pipe arm below
//! uses the Win32 pipe APIs and is compile-verified only on this host; runtime
//! verification of that arm requires Windows.

use std::io::{self, Cursor, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
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
    /// Reports whether this transport can enforce a read timeout.
    ///
    /// A false result is normal for inherited stdio: callers may still set a
    /// timeout as a no-op, but must use another mechanism if they need a
    /// deadline.
    fn supports_read_timeout(&self) -> bool {
        false
    }
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), ProtocolError>;
    fn close(&mut self);
}

#[cfg(any(unix, windows))]
fn map_io(error: io::Error) -> ProtocolError {
    if error.kind() == io::ErrorKind::BrokenPipe {
        ProtocolError::Closed
    } else if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) {
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
    reader: frame::FrameReader,
    read_timeout: Option<Duration>,
}

impl IoTransport {
    fn new(stream: IoStream) -> Self {
        Self {
            stream,
            reader: frame::FrameReader::new(),
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
        self.reader.read_frame(&mut self.stream, &mut bytes)?;
        Envelope::decode(&bytes)
    }

    fn try_clone_handle(&self) -> Result<Box<dyn Transport>, ProtocolError> {
        let stream = self.stream.try_clone_handle()?;
        let mut cloned = Self::new(stream);
        cloned.set_timeout(self.read_timeout)?;
        Ok(Box::new(cloned))
    }
    fn supports_read_timeout(&self) -> bool {
        !matches!(&self.stream, IoStream::Stdio { .. } | IoStream::Closed)
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), ProtocolError> {
        self.set_timeout(timeout)
    }

    fn close(&mut self) {
        self.stream = IoStream::Closed;
    }
}

const PAIR_QUEUE_CAPACITY: usize = 64;

#[derive(Debug)]
struct PairSide {
    sender: Option<SyncSender<Vec<u8>>>,
    receiver: Arc<Mutex<Receiver<Vec<u8>>>>,
    read_timeout: Option<Duration>,
}

impl Transport for PairSide {
    fn send(&mut self, envelope: &Envelope) -> Result<(), ProtocolError> {
        let sender = self.sender.as_ref().ok_or(ProtocolError::Closed)?;
        let payload = envelope.encode();
        if payload.len() > crate::MAX_FRAME_BYTES {
            return Err(ProtocolError::FrameTooLarge(payload.len()));
        }
        let capacity = payload
            .len()
            .checked_add(4)
            .ok_or(ProtocolError::FrameTooLarge(payload.len()))?;
        let mut framed = Vec::with_capacity(capacity);
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
    fn supports_read_timeout(&self) -> bool {
        true
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
    let (left_sender, left_receiver) = mpsc::sync_channel(PAIR_QUEUE_CAPACITY);
    let (right_sender, right_receiver) = mpsc::sync_channel(PAIR_QUEUE_CAPACITY);
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
        // A target with neither Unix sockets nor named pipes has no listener
        // kind at all, so there is nothing to match on. `bind` refuses first,
        // which is why this is unreachable rather than merely unimplemented;
        // it exists because the crate must still compile for such a target.
        #[cfg(not(any(unix, windows)))]
        {
            let _ = timeout;
            return Err(ProtocolError::Malformed(
                "this platform has no host listener transport".to_owned(),
            ));
        }
        #[cfg(any(unix, windows))]
        match &self.kind {
            #[cfg(unix)]
            ListenerKind::Unix(listener) => {
                if let Some(timeout) = timeout {
                    listener.set_nonblocking(true).map_err(map_io)?;
                    let started = Instant::now();
                    let accepted = loop {
                        match listener.accept() {
                            Ok(value) => break Ok(value),
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                if started.elapsed() >= timeout {
                                    break Err(ProtocolError::Timeout);
                                }
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            Err(error) => break Err(map_io(error)),
                        }
                    };
                    listener.set_nonblocking(false).map_err(map_io)?;
                    let (stream, _) = accepted?;
                    // The accepted socket, not just the listener. Linux gives
                    // a fresh blocking socket here, but macOS and the BSDs
                    // hand back one that inherited O_NONBLOCK from the
                    // listener above, so every later `recv` on it returns
                    // `WouldBlock` and this layer reports that as `Timeout` -
                    // a live plugin looking like a silent one, on one family
                    // of platforms only.
                    stream.set_nonblocking(false).map_err(map_io)?;
                    return Ok(Box::new(IoTransport::new(IoStream::Unix(stream))));
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
    use std::ffi::{c_void, OsStr};
    use std::io::{self, Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::sync::Mutex;
    use std::thread;
    use std::time::{Duration, Instant};

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND,
        ERROR_INSUFFICIENT_BUFFER, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
        ERROR_PIPE_LISTENING, HANDLE, INVALID_HANDLE_VALUE, WIN32_ERROR,
    };
    use windows::Win32::Security::{
        AddAccessAllowedAce, GetLengthSid, GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor,
        SetSecurityDescriptorDacl, TokenUser, ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, PSECURITY_DESCRIPTOR,
        SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe, SetNamedPipeHandleState,
        PIPE_NOWAIT, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// `SECURITY_DESCRIPTOR_REVISION`, which the Win32 headers define as 1 and
    /// the `windows` crate does not re-export.
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

    /// How many bytes a pipe instance is still holding for a reader.
    ///
    /// Used to tell a client that connected, said its piece and left from one
    /// that connected and left with nothing to say: the first has a handshake
    /// worth draining, the second only a stale instance to re-arm. A query
    /// that fails answers zero, which routes the caller to the re-arm path
    /// rather than to a handle it could not inspect.
    fn buffered_bytes(handle: HANDLE) -> u32 {
        let mut available = 0_u32;
        // SAFETY: no output buffer is requested; `available` is valid writable
        // storage for the duration of the call.
        match unsafe { PeekNamedPipe(handle, None, 0, None, Some(&mut available as *mut u32), None) } {
            Ok(()) => available,
            Err(_) => 0,
        }
    }

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
        pub fn connect(name: &str, timeout: Option<Duration>) -> Result<Self, String> {
            let path = format!(r"\\.\pipe\{name}");
            let wide = wide(&path);
            let deadline = match timeout {
                Some(value) => Some(
                    Instant::now()
                        .checked_add(value)
                        .ok_or_else(|| "named pipe connect timeout is too large".to_owned())?,
                ),
                None => None,
            };
            loop {
                // SAFETY: `wide` is NUL-terminated and remains alive for the
                // synchronous call; a successful handle is owned by `PipeFile`.
                let result = unsafe {
                    CreateFileW(
                        PCWSTR(wide.as_ptr()),
                        FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                        FILE_SHARE_NONE,
                        None,
                        OPEN_EXISTING,
                        FILE_ATTRIBUTE_NORMAL,
                        None,
                    )
                };
                match result {
                    Ok(handle) => {
                        return Ok(Self {
                            handle,
                            read_timeout: None,
                        });
                    }
                    Err(error) => {
                        let retryable = matches!(
                            WIN32_ERROR::from_error(&error),
                            Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_PIPE_BUSY)
                        );
                        if timeout.is_none() || !retryable {
                            return Err(error.to_string());
                        }
                        let Some(deadline) = deadline else {
                            return Err("named pipe connect timeout is unavailable".to_owned());
                        };
                        let now = Instant::now();
                        if now >= deadline {
                            return Err("named pipe connect timed out".to_owned());
                        }
                        let remaining = deadline.duration_since(now);
                        thread::sleep(remaining.min(Duration::from_millis(1)));
                    }
                }
            }
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
            let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "named pipe read timeout is too large",
                )
            })?;
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
                if Instant::now() >= deadline {
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

    /// An explicit discretionary ACL naming this user and nobody else, kept
    /// alive together with the security descriptor that points into it.
    ///
    /// Null security attributes make `CreateNamedPipeW` apply the process
    /// token's *default* DACL. On an ordinary desktop that reaches beyond this
    /// user, and under some configurations as far as an anonymous logon. The
    /// listener offers a single instance, so a foreign process that opens it
    /// first occupies the only connection: the real plugin then finds the pipe
    /// busy while the host waits for a handshake the intruder cannot produce,
    /// and whatever the host writes before giving up is read by the intruder.
    /// The session token the host generates authenticates the peer once it is
    /// connected; it says nothing about who may open the object at all, so an
    /// explicit DACL is defence in depth alongside that token, not a
    /// replacement for it.
    ///
    /// Both buffers live in this one value because Win32 stores bare pointers:
    /// the descriptor points into the ACL and `SECURITY_ATTRIBUTES` points at
    /// the descriptor, and each must still be valid when `CreateNamedPipeW`
    /// reads it. `_acl` is therefore held for its address rather than its
    /// value; both allocations keep that address when this value is moved.
    struct PipeSecurity {
        _acl: Vec<u32>,
        descriptor: Box<SECURITY_DESCRIPTOR>,
    }

    impl PipeSecurity {
        fn current_user_only() -> Result<Self, String> {
            let token_user = current_token_user()?;
            // `TOKEN_USER` heads the buffer and its `Sid` points into the same
            // allocation, which is why the buffer is kept until the ACE below
            // has copied the SID.
            // SAFETY: `token_user` was filled by GetTokenInformation for
            // TokenUser and its `u64` elements give TOKEN_USER's alignment.
            let sid = unsafe { *token_user.as_ptr().cast::<TOKEN_USER>() }.User.Sid;
            // SAFETY: `sid` points into the live token-user buffer.
            let sid_length = unsafe { GetLengthSid(sid) } as usize;
            if sid_length == 0 {
                return Err("GetLengthSid rejected the token user SID".to_owned());
            }
            // ACCESS_ALLOWED_ACE already carries the first DWORD of the SID in
            // its SidStart member, so only the remaining SID bytes are extra.
            let acl_length =
                size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>() + sid_length;
            // u32 elements give the ACL the DWORD alignment Win32 requires of
            // it; a Vec<u8> would be aligned to one byte.
            let mut acl = vec![0_u32; acl_length.div_ceil(size_of::<u32>())];
            let acl_pointer = acl.as_mut_ptr().cast::<ACL>();
            // SAFETY: `acl` is DWORD-aligned storage of at least `acl_length`
            // bytes, which is exactly the length declared to InitializeAcl.
            unsafe { InitializeAcl(acl_pointer, acl_length as u32, ACL_REVISION) }
                .map_err(|error| format!("InitializeAcl failed: {error}"))?;
            // Read and write are all a plugin needs of the pipe. No other
            // trustee appears in the ACL, and an ACL with no matching ACE
            // denies, so every other account is refused by omission.
            let access = (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0;
            // SAFETY: the ACL was just initialised with room for exactly this
            // one ACE, and `sid` is valid for the duration of the call.
            unsafe { AddAccessAllowedAce(acl_pointer, ACL_REVISION, access, sid) }
                .map_err(|error| format!("AddAccessAllowedAce failed: {error}"))?;
            let mut descriptor = Box::new(SECURITY_DESCRIPTOR::default());
            let descriptor_pointer = PSECURITY_DESCRIPTOR((&raw mut *descriptor).cast());
            // SAFETY: `descriptor` is a correctly sized, uniquely owned
            // SECURITY_DESCRIPTOR.
            unsafe { InitializeSecurityDescriptor(descriptor_pointer, SECURITY_DESCRIPTOR_REVISION) }
                .map_err(|error| format!("InitializeSecurityDescriptor failed: {error}"))?;
            // `bdacldefaulted` is false: this DACL is a deliberate choice, and
            // saying otherwise would let Windows treat it as replaceable
            // inherited default.
            // SAFETY: the descriptor is initialised and the ACL it is given
            // outlives it, both being owned by the value returned below.
            unsafe {
                SetSecurityDescriptorDacl(descriptor_pointer, true, Some(acl_pointer.cast_const()), false)
            }
            .map_err(|error| format!("SetSecurityDescriptorDacl failed: {error}"))?;
            Ok(Self {
                _acl: acl,
                descriptor,
            })
        }

        fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: (&raw const *self.descriptor).cast_mut().cast::<c_void>(),
                // The listener handle is never handed to a child process.
                bInheritHandle: false.into(),
            }
        }
    }

    /// Returns a `TOKEN_USER` for this process's token, in `u64` storage so
    /// that the structure's interior pointer is correctly aligned.
    fn current_token_user() -> Result<Vec<u64>, String> {
        let mut token = HANDLE::default();
        // SAFETY: `token` is valid writable storage and the handle it receives
        // is closed on both exits below.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
            .map_err(|error| format!("OpenProcessToken failed: {error}"))?;
        let user = read_token_user(token);
        // SAFETY: `token` was opened just above and is closed exactly once.
        let _ = unsafe { CloseHandle(token) };
        user
    }

    fn read_token_user(token: HANDLE) -> Result<Vec<u64>, String> {
        let mut needed = 0_u32;
        // The documented size probe: with no buffer the call fails with
        // ERROR_INSUFFICIENT_BUFFER and writes the required length.
        // SAFETY: no output buffer is supplied and `needed` is writable.
        match unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut needed) } {
            Ok(()) => return Err("GetTokenInformation accepted a zero-length TokenUser buffer".to_owned()),
            Err(error) if WIN32_ERROR::from_error(&error) == Some(ERROR_INSUFFICIENT_BUFFER) => {}
            Err(error) => return Err(format!("GetTokenInformation could not size TokenUser: {error}")),
        }
        let mut buffer = vec![0_u64; (needed as usize).div_ceil(size_of::<u64>()).max(1)];
        // SAFETY: `buffer` holds at least `needed` bytes and is aligned for the
        // pointer TOKEN_USER contains.
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                needed,
                &mut needed,
            )
        }
        .map_err(|error| format!("GetTokenInformation failed for TokenUser: {error}"))?;
        Ok(buffer)
    }

    #[derive(Debug)]
    pub struct PipeListener(Mutex<Option<HANDLE>>);

    impl PipeListener {
        pub fn bind(name: &str) -> Result<Self, String> {
            let path = format!(r"\\.\pipe\{name}");
            let wide = wide(&path);
            let security = PipeSecurity::current_user_only()?;
            let attributes = security.attributes();
            // PIPE_NOWAIT lets accept poll for a caller instead of blocking
            // forever; the accepted handle is switched back to PIPE_WAIT.
            // PIPE_REJECT_REMOTE_CLIENTS keeps the listener local: this
            // endpoint exists for a child on this machine, and a remote opener
            // could otherwise take the single instance before it.
            // SAFETY: `wide` is NUL-terminated and `security` owns the
            // descriptor `attributes` points at; both outlive this synchronous
            // call. A successful handle is placed in the mutex.
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(wide.as_ptr()),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    8 * 1024 * 1024,
                    8 * 1024 * 1024,
                    0,
                    Some(&attributes),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err("CreateNamedPipeW returned an invalid handle".to_owned());
            }
            Ok(Self(Mutex::new(Some(handle))))
        }

        /// Accepts one client, leaving the listener listening if it times out.
        ///
        /// The server handle stays in the slot for the whole polling loop and
        /// moves into a [`PipeFile`] only once a client is actually connected.
        /// Taking it up front meant that returning `Timeout` dropped and closed
        /// the only listening handle, so the very next `accept` answered
        /// `Closed`: one plugin slow to connect turned a retryable timeout into
        /// a dead endpoint, and the caller read that as "the plugin never
        /// started" rather than "the host threw its pipe away". The Unix
        /// listener keeps its socket across a timeout; this is the same
        /// contract.
        pub fn accept(&self, timeout: Option<Duration>) -> Result<PipeFile, crate::ProtocolError> {
            let mut guard = self
                .0
                .lock()
                .map_err(|_| crate::ProtocolError::Io("named-pipe listener lock poisoned".to_owned()))?;
            // `HANDLE` is a copyable handle value, so this is a borrow of the
            // slot's handle, not ownership of it: the slot still closes it
            // unless one of the paths below explicitly takes over.
            let handle = *guard.as_ref().ok_or(crate::ProtocolError::Closed)?;
            let deadline = match timeout {
                Some(value) => Some(Instant::now().checked_add(value).ok_or(
                    crate::ProtocolError::Malformed("named pipe accept timeout is too large".to_owned()),
                )?),
                None => None,
            };
            // Whether the next `ConnectNamedPipe` answer is the acknowledgement
            // that a just-disconnected instance is listening again rather than
            // news of a client. On a non-blocking pipe the first call after
            // `DisconnectNamedPipe` succeeds immediately to say exactly that,
            // and taking it for a connection would hand out a handle nobody is
            // on the other end of.
            let mut arming = false;
            loop {
                // SAFETY: the slot owns this valid server handle and holds it
                // alive through each non-blocking ConnectNamedPipe call.
                match unsafe { ConnectNamedPipe(handle, None) } {
                    Ok(()) if arming => {
                        arming = false;
                        if deadline.is_some_and(|at| Instant::now() >= at) {
                            return Err(crate::ProtocolError::Timeout);
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                    Ok(()) => break,
                    Err(error) => match WIN32_ERROR::from_error(&error) {
                        Some(ERROR_PIPE_CONNECTED) => break,
                        // A client connected and closed again before this poll
                        // came round. Win32 is explicit that a good connection
                        // exists only after `ERROR_PIPE_CONNECTED`, so this
                        // handle is not treated as one: it is only handed on
                        // when the departed client actually left bytes in the
                        // instance buffer, which is the plugin handshake this
                        // host exists to read. With nothing buffered there is
                        // nothing to salvage, so the instance is disconnected -
                        // the documented recovery - and polling continues for
                        // the next client instead of wedging on a stale one.
                        Some(ERROR_NO_DATA) => {
                            if buffered_bytes(handle) > 0 {
                                break;
                            }
                            // SAFETY: the slot still owns this valid server
                            // handle; disconnecting re-arms the instance and
                            // leaves it listening for the next client.
                            if unsafe { DisconnectNamedPipe(handle) }.is_err() {
                                *guard = None;
                                // SAFETY: removed from the slot just above, so
                                // this closes it exactly once.
                                let _ = unsafe { CloseHandle(handle) };
                                return Err(crate::ProtocolError::Io(
                                    "a closed client could not be disconnected from the pipe".to_owned(),
                                ));
                            }
                            // The next `ConnectNamedPipe` will report success
                            // for the re-arm itself, not for a client.
                            arming = true;
                            if deadline.is_some_and(|at| Instant::now() >= at) {
                                return Err(crate::ProtocolError::Timeout);
                            }
                            thread::sleep(Duration::from_millis(1));
                        }
                        Some(ERROR_PIPE_LISTENING) => {
                            if deadline.is_some_and(|at| Instant::now() >= at) {
                                // The slot still holds the handle, so the next
                                // accept resumes polling this same instance.
                                return Err(crate::ProtocolError::Timeout);
                            }
                            thread::sleep(Duration::from_millis(1));
                        }
                        // Any other ConnectNamedPipe failure is terminal: the
                        // listener is in an unknown state and must not be
                        // polled again, so the handle is released here and the
                        // emptied slot makes every later accept answer Closed.
                        _ => {
                            *guard = None;
                            // SAFETY: the handle was just removed from the slot,
                            // which is the only other owner, so this closes it
                            // exactly once.
                            let _ = unsafe { CloseHandle(handle) };
                            return Err(crate::ProtocolError::Io(error.to_string()));
                        }
                    },
                }
            }
            // A client is connected, so the single instance now belongs to the
            // accepted file. Emptying the slot before building it keeps exactly
            // one owner, including on the failure below, where `file` drops.
            *guard = None;
            let file = PipeFile {
                handle,
                read_timeout: None,
            };
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

    /// Windows-only because the object under test is a Win32 security
    /// descriptor: off Windows this module does not exist, so no Linux suite
    /// can say anything about who may open the pipe.
    #[cfg(test)]
    mod tests {
        use super::{current_token_user, PipeSecurity, FILE_GENERIC_READ, FILE_GENERIC_WRITE, TOKEN_USER};
        use windows::Win32::Security::{
            EqualSid, GetAce, GetSecurityDescriptorDacl, ACCESS_ALLOWED_ACE, PSECURITY_DESCRIPTOR, PSID,
        };

        /// `ACCESS_ALLOWED_ACE_TYPE` from the Win32 headers, which the
        /// `windows` crate does not re-export.
        const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

        /// Every weakening of this descriptor is a way back to the default
        /// DACL: an absent DACL grants everyone, a defaulted one is the
        /// inherited default this exists to replace, an extra ACE is an extra
        /// trustee, and a wrong SID is somebody else's account.
        #[test]
        fn the_pipe_dacl_grants_this_user_and_nobody_else() {
            let security = PipeSecurity::current_user_only().expect("the pipe DACL must be buildable");
            let descriptor = PSECURITY_DESCRIPTOR((&raw const *security.descriptor).cast_mut().cast());
            let mut present = windows::core::BOOL(0);
            let mut dacl = std::ptr::null_mut();
            let mut defaulted = windows::core::BOOL(0);
            // SAFETY: `security` owns an initialised descriptor and the three
            // outputs are valid writable storage.
            unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) }
                .expect("the descriptor must carry a readable DACL");
            assert!(present.as_bool(), "an absent DACL grants every account access");
            assert!(!dacl.is_null(), "a NULL DACL grants every account access");
            assert!(
                !defaulted.as_bool(),
                "the DACL must be reported as deliberate, not as a replaceable default"
            );
            // SAFETY: `dacl` points into the ACL `security` owns.
            let ace_count = unsafe { (*dacl).AceCount };
            assert_eq!(ace_count, 1, "exactly one trustee may appear in the pipe DACL");

            let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
            // SAFETY: the ACL is valid and index 0 exists, as just asserted.
            unsafe { GetAce(dacl, 0, &mut ace) }.expect("the single ACE must be readable");
            // SAFETY: an ACE of type ACCESS_ALLOWED_ACE_TYPE, which the next
            // assertion confirms, has exactly this layout.
            let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            assert_eq!(
                ace.Header.AceType, ACCESS_ALLOWED_ACE_TYPE,
                "the single ACE must be an access-allowed ACE"
            );
            assert_eq!(
                ace.Mask,
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                "the plugin needs read and write on the pipe and nothing more"
            );

            let user = current_token_user().expect("the token user must be readable");
            // SAFETY: the buffer was filled for TokenUser and is aligned for it.
            let expected = unsafe { *user.as_ptr().cast::<TOKEN_USER>() }.User.Sid;
            let granted = PSID((&raw const ace.SidStart).cast_mut().cast());
            assert!(
                // SAFETY: both SIDs point into live, valid storage.
                unsafe { EqualSid(granted, expected) }.is_ok(),
                "the granted trustee must be this process's own user"
            );
        }
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
fn connect_named_pipe(name: &str, timeout: Option<Duration>) -> Result<Box<dyn Transport>, ProtocolError> {
    let file = windows_pipe::PipeFile::connect(name, timeout).map_err(ProtocolError::Io)?;
    Ok(Box::new(IoTransport::new(IoStream::NamedPipe(file))))
}
