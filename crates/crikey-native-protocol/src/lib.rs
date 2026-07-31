//! Native plugin IPC protocol (spec 16).
//!
//! The protocol is a hand-written proto3 codec carried in bounded, length-delimited
//! frames.  It is transport independent: Unix sockets, Windows named pipes and
//! inherited stdio all expose the same [`transport::Transport`] contract.

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Environment name carrying the host-created endpoint (spec 16.6).
pub const ENV_ENDPOINT: &str = "CRIKEY_PLUGIN_ENDPOINT";
/// Environment name carrying the per-process session token (spec 16.6).
pub const ENV_SESSION_TOKEN: &str = "CRIKEY_SESSION_TOKEN";
/// Environment name carrying the host-side plugin id (spec 16.6).
pub const ENV_PLUGIN_ID: &str = "CRIKEY_PLUGIN_ID";
/// Environment name carrying the negotiated protocol version (spec 16.6).
pub const ENV_PROTOCOL_VERSION: &str = "CRIKEY_PROTOCOL_VERSION";

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

impl Endpoint {
    /// Parses the stable endpoint spelling used in `CRIKEY_PLUGIN_ENDPOINT`.
    ///
    /// Accepted forms are `unix:/path`, `pipe:name`, and `stdio` (spec 16.2).
    pub fn parse(spec: &str) -> Result<Self, ProtocolError> {
        if spec == "stdio" {
            return Ok(Self::Stdio);
        }
        if let Some(path) = spec.strip_prefix("unix:") {
            if path.is_empty() {
                return Err(ProtocolError::Malformed("unix endpoint has no path".to_owned()));
            }
            return Ok(Self::UnixSocket(std::path::PathBuf::from(path)));
        }
        if let Some(name) = spec.strip_prefix("pipe:") {
            if name.is_empty() {
                return Err(ProtocolError::Malformed(
                    "named pipe endpoint has no name".to_owned(),
                ));
            }
            return Ok(Self::NamedPipe(name.to_owned()));
        }
        Err(ProtocolError::Malformed(format!(
            "invalid endpoint specification {spec:?}"
        )))
    }

    /// Returns the stable spelling accepted by [`Endpoint::parse`].
    pub fn to_spec(&self) -> String {
        match self {
            Self::UnixSocket(path) => format!("unix:{}", path.to_string_lossy()),
            Self::NamedPipe(name) => format!("pipe:{name}"),
            Self::Stdio => "stdio".to_owned(),
        }
    }
}

/// Failures at the framed protocol boundary.  A plugin cannot inject a panic
/// through any of these paths; malformed bytes are represented as data (spec
/// 16.3, 12.4).
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_BYTES} byte limit")]
    FrameTooLarge(usize),
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u32),
    #[error("malformed message: {0}")]
    Malformed(String),
    #[error("connection closed")]
    Closed,
    #[error("I/O error: {0}")]
    Io(String),
    #[error("I/O operation timed out")]
    Timeout,
    #[error("connection rejected: {0}")]
    Rejected(String),
}

/// Negotiated capability bits exposed by the native handshake (spec 16.3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub streaming_catalog: bool,
    pub streaming_suggestions: bool,
    pub cancellation: bool,
    pub configuration_updates: bool,
    pub events: bool,
}

/// A hand-written proto3 message with bounded, total decoding (spec 16.3).
pub trait Message: Sized + std::fmt::Debug {
    fn encode(&self) -> Vec<u8>;
    fn decode(bytes: &[u8]) -> Result<Self, ProtocolError>;
}

pub mod convert;
pub mod frame;
pub mod message;
pub mod transport;
pub mod wire;
