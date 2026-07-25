//! Native subprocess plugin host (spec 16.1, 16.6).
//!
//! Native plugins are never loaded into the CriKey process; the host launches
//! the executable, hands it an endpoint plus a session token, and supervises it.

use crikey_core::PluginId;
use crikey_native_protocol::Endpoint;

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub plugin: PluginId,
    pub executable: std::path::PathBuf,
    pub endpoint: Endpoint,
    pub session_token: String,
    /// Environment is restricted rather than inherited wholesale.
    pub environment: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    Clean,
    Crashed,
    Killed,
    ProtocolViolation,
}

pub trait NativeHost {
    fn launch(&mut self, spec: LaunchSpec) -> crikey_core::Result<()>;
    fn terminate(&mut self, plugin: &PluginId) -> crikey_core::Result<ExitKind>;
}
