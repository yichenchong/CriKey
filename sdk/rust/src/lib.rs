//! Official Rust SDK for CriKey native plugins (spec 16.7).
//!
//! The SDK depends on [`crikey_core`], [`crikey_native_protocol`] and the
//! launcher manifest model used by the packaging validator. A plugin is an
//! ordinary executable; the serving loop in [`serve`] keeps its protocol
//! boundary separate from plugin code.

use std::collections::BTreeMap;

use crikey_core::{Item, PluginId, Result};
use crikey_native_protocol::RequestId;

mod builder;
mod serve;

pub mod bench;
pub mod harness;
pub mod packaging;

pub use builder::{ActionBuilder, ItemBuilder};
pub use crikey_native_protocol as protocol;
pub use serve::{serve, serve_on, HandshakeInfo, ServeConfig};

/// Errors raised by the SDK boundary (spec 16.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SdkError {
    /// The peer sent a malformed or otherwise invalid protocol message.
    Protocol(String),
    /// The transport could not be opened or no longer carries messages.
    Transport(String),
    /// Plugin configuration is incomplete or invalid.
    Config(String),
    /// The host explicitly rejected the plugin during negotiation.
    Rejected(String),
}

impl std::fmt::Display for SdkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(detail) => write!(formatter, "protocol error: {detail}"),
            Self::Transport(detail) => write!(formatter, "transport error: {detail}"),
            Self::Config(detail) => write!(formatter, "configuration error: {detail}"),
            Self::Rejected(detail) => write!(formatter, "plugin rejected: {detail}"),
        }
    }
}

impl std::error::Error for SdkError {}

impl From<protocol::ProtocolError> for SdkError {
    fn from(error: protocol::ProtocolError) -> Self {
        match error {
            protocol::ProtocolError::Closed => Self::Transport("connection closed".to_owned()),
            protocol::ProtocolError::Timeout => Self::Transport("transport timed out".to_owned()),
            other => Self::Protocol(other.to_string()),
        }
    }
}

/// A suggestion request delivered to a plugin (spec 8.1, 12.1).
#[derive(Debug, Clone)]
pub struct Query {
    /// The host request identity echoed on every result frame.
    pub request: RequestId,
    /// Raw query text supplied by the host.
    pub text: String,
    /// Normalized query text supplied by the host.
    pub normalized: String,
    /// Query deadline in milliseconds from receipt, when supplied.
    pub deadline_ms: Option<u64>,
    /// Monotonic query generation used for stale-result rejection.
    pub generation: u64,
    /// Item selected by the user, for argument suggestions.
    pub selected_item_id: Option<String>,
}

/// Cooperative cancellation handed to every request (spec 9.4, 16.7).
///
/// Plugins should poll it before expensive work, inside long loops and before
/// emitting large batches.  The host rejects stale results regardless.
pub trait CancellationToken: std::fmt::Debug {
    /// Returns whether the host cancelled the active request.
    fn is_cancelled(&self) -> bool;
}

/// Per-request context: identity, configuration-independent logging and
/// cancellation (spec 16.7).
pub trait PluginContext {
    /// Identity of the plugin serving this request.
    fn plugin_id(&self) -> &PluginId;
    /// Cooperative cancellation state for the active request.
    fn cancellation(&self) -> &dyn CancellationToken;
    /// Sends a bounded diagnostic record to the host.
    fn log(&self, level: LogLevel, message: &str);
}

/// Log severity accepted by [`PluginContext::log`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Unrecoverable plugin or protocol failure.
    Error,
    /// Recoverable warning.
    Warn,
    /// Informational message.
    Info,
    /// Debugging detail.
    Debug,
    /// Very verbose tracing detail.
    Trace,
}

/// Event delivered by the optional [`Plugin::on_event`] callback.
#[derive(Debug, Clone)]
pub struct PluginEvent {
    /// Event kind defined by the native protocol.
    pub kind: protocol::message::EventKind,
    /// String attributes attached to the event.
    pub attributes: BTreeMap<String, String>,
    /// Protocol-specific event flags.
    pub flags: u64,
}

/// Streaming sink for catalog construction.  Batches, never one item per IPC
/// message (spec 16.5).
pub trait CatalogSink {
    /// Sends one bounded catalog batch.
    fn emit_batch(&mut self, items: Vec<Item>) -> Result<()>;
    /// Sends the catalog terminal frame.
    fn finish(&mut self) -> Result<()>;
}

/// Streaming sink for suggestions; supports partial and final batches.
pub trait SuggestionSink {
    /// Sends one partial suggestion batch.
    fn emit_batch(&mut self, items: Vec<Item>) -> Result<()>;
    /// Sends a final or cancelled terminal frame.
    fn finish(&mut self) -> Result<()>;
    /// Returns the current cooperative cancellation state (spec 9.4).
    fn is_cancelled(&self) -> bool;
}

/// Action execution request delivered by the host (spec 10.4).
#[derive(Debug, Clone)]
pub struct ExecuteRequest {
    /// Host request identity.
    pub request: RequestId,
    /// Item selected for execution.
    pub item: crikey_core::ItemId,
    /// Optional item action.
    pub action: Option<crikey_core::ActionId>,
    /// Optional user argument.
    pub argument: Option<String>,
}

/// The trait a native plugin implements (spec 16.7).
pub trait Plugin {
    /// Initializes plugin state before requests are served.
    fn start(&mut self, context: &dyn PluginContext) -> Result<()>;

    /// Builds and streams the complete catalog.
    fn build_catalog(&mut self, context: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()>;

    /// Handles one suggestion request.
    fn suggest(
        &mut self,
        query: Query,
        context: &dyn PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()>;

    /// Executes one selected item or action.
    fn execute(&mut self, request: ExecuteRequest, context: &dyn PluginContext) -> Result<()>;

    /// Stops plugin state during orderly shutdown.
    fn stop(&mut self, context: &dyn PluginContext) -> Result<()>;
    /// Receives a complete or delta configuration publication (spec 21.4).
    fn on_configuration(
        &mut self,
        _values: &BTreeMap<String, String>,
        _context: &dyn PluginContext,
    ) -> Result<()> {
        Ok(())
    }

    /// Receives a host event publication (spec 14.6, 18.7).
    fn on_event(&mut self, _event: &PluginEvent, _context: &dyn PluginContext) -> Result<()> {
        Ok(())
    }

    /// Notifies the plugin that the host activated it (spec 13.2).
    fn on_activated(&mut self, _context: &dyn PluginContext) -> Result<()> {
        Ok(())
    }

    /// Notifies the plugin that the host deactivated it (spec 13.2).
    fn on_deactivated(&mut self, _context: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
}
