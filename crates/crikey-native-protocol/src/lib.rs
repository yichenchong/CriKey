//! Native plugin IPC protocol (spec 16).
//!
//! Wire format: length-delimited, versioned binary messages over Windows named
//! pipes, Unix domain sockets, or stdio for development. The schema itself
//! lives in `sdk/protocol` and is transport independent.

pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum accepted frame size. Oversized frames are a protocol violation and
/// disconnect the plugin (spec 12.4).
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u64);

/// Transport selected for one plugin connection (spec 16.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    NamedPipe(String),
    UnixSocket(std::path::PathBuf),
    Stdio,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_BYTES} byte limit")]
    FrameTooLarge(usize),
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u32),
    #[error("malformed message: {0}")]
    Malformed(String),
    #[error("connection closed")]
    Closed,
}

/// Negotiated at handshake time (spec 16.3).
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub streaming_catalog: bool,
    pub streaming_suggestions: bool,
    pub cancellation: bool,
    pub configuration_updates: bool,
    pub events: bool,
}
