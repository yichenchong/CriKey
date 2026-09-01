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

pub use builder::{ActionBuilder, ItemBuilder, PageBuilder, PageRect};
pub use crikey_core::{
    NodeRole, NodeShape, PageColor, PageError, PageFrame, PageInput, PageInputKind, PageNode,
};
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
            protocol::ProtocolError::Rejected(detail) => Self::Rejected(detail),
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

/// What the host should do once an action has run (spec 10.4, 32.2).
///
/// An action that opened a page has not finished the user's task, it has
/// handed the user a surface, and the host has to be told which one: the
/// launcher stays open on the named page instead of dismissing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ExecuteOutcome {
    /// The action did its work and the host may dismiss the launcher.
    #[default]
    Completed,
    /// The action opened a page; the host draws it by asking the plugin for
    /// frames under this identifier.
    ShowPage {
        /// The page the plugin will draw. Chosen by the plugin and echoed
        /// back in every [`PageRequest`], so one plugin can own several.
        page_id: String,
    },
}

impl ExecuteOutcome {
    /// Reports that this action opened `page_id`.
    pub fn show_page(page_id: impl Into<String>) -> Self {
        Self::ShowPage {
            page_id: page_id.into(),
        }
    }
}

/// Lets a plugin whose action merely ran keep writing the outcome it has
/// always written, so extending the vocabulary costs existing authors nothing.
impl From<()> for ExecuteOutcome {
    fn from((): ()) -> Self {
        Self::Completed
    }
}

/// The launcher's own colours, handed to a page so it can match the theme it
/// is drawn inside (spec 32.3).
///
/// A page never reads the host's configuration, so this is the only way its
/// surface can look like part of the launcher rather than pasted onto it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagePalette {
    /// Background of the page area.
    pub surface: PageColor,
    /// Body text on `surface`.
    pub text: PageColor,
    /// Selection and primary-control colour.
    pub accent: PageColor,
    /// Secondary text and hairlines.
    pub muted: PageColor,
}

/// One host request for a page frame (spec 32.3).
///
/// Pages are host-driven exactly like suggestions: the plugin is asked for a
/// frame and answers once. It never pushes, so a page cannot repaint the
/// user's screen at a moment the host did not choose.
#[derive(Debug, Clone)]
pub struct PageRequest {
    /// The page the plugin named when its action returned
    /// [`ExecuteOutcome::ShowPage`].
    pub page_id: String,
    /// Monotonic per page. The frame answering it carries it back, and the
    /// host drops any frame answering an older one.
    pub generation: u64,
    /// Viewport width in logical pixels.
    pub width: u32,
    /// Viewport height in logical pixels.
    pub height: u32,
    /// Everything the user did since the previous frame, in order. The host
    /// has already hit-tested each event, so [`PageInput::node_id`] names the
    /// node it landed on.
    pub events: Vec<PageInput>,
    /// Whether the page currently owns the keyboard.
    pub focused: bool,
    /// The launcher's colours for this frame.
    pub palette: PagePalette,
}

/// What a host [`ResourceRequest`] is asking for (spec 16.4).
///
/// [`ResourceRequest`]: protocol::message::ResourceRequest
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// Icon pixels for an item the plugin published.
    Icon,
    /// An opaque file shipped inside the plugin package.
    File,
    /// Configuration data owned by the plugin.
    Configuration,
    /// A kind this SDK release does not know. A plugin should decline it
    /// rather than guess: the host will read the answer as the kind it asked
    /// for, not the kind the plugin assumed.
    Unknown,
}

/// Bytes a plugin serves for one resource reference (spec 16.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginResource {
    /// The payload itself. The host enforces its own size ceiling, so a
    /// plugin that serves something enormous gets it dropped, not rendered.
    pub content: Vec<u8>,
    /// An IANA media type such as `image/png`, or empty when unknown. The
    /// host sniffs the content regardless; this is a hint, never a promise.
    pub media_type: String,
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

    /// Executes one selected item or action and reports what the host should
    /// do next (spec 10.4, 32.2).
    ///
    /// This is the method the host calls; the default forwards to
    /// [`Plugin::execute`] and reports [`ExecuteOutcome::Completed`], which is
    /// exactly what every plugin written before pages existed already meant.
    /// A plugin that opens a page overrides this one instead, because Rust
    /// cannot widen the `Ok(())` those authors already wrote into a richer
    /// outcome and silently changing what their action reports would be worse
    /// than asking the page-drawing minority for one more method.
    fn execute_outcome(
        &mut self,
        request: ExecuteRequest,
        context: &dyn PluginContext,
    ) -> Result<ExecuteOutcome> {
        self.execute(request, context).map(ExecuteOutcome::from)
    }

    /// Draws one frame of a page the plugin opened (spec 32.3).
    ///
    /// The default closes the page. A plugin that returned
    /// [`ExecuteOutcome::ShowPage`] without implementing this has opened a
    /// surface nobody owns, and closing it immediately is the only honest
    /// answer: the alternative leaves the user looking at an empty rectangle
    /// they cannot dismiss except by escaping the launcher.
    fn page(&mut self, request: PageRequest, _context: &dyn PluginContext) -> Result<PageFrame> {
        Ok(PageFrame {
            generation: request.generation,
            close: true,
            ..PageFrame::default()
        })
    }

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

    /// Serves one resource the host asked for (spec 16.4).
    ///
    /// `Ok(None)` means "I do not have that", which is the default and is not
    /// an error: a plugin that publishes no icons of its own never needs to
    /// implement this. The host bounds both the wait and the payload size, so
    /// an implementation must return promptly and must not stream.
    fn resource(
        &mut self,
        _kind: ResourceKind,
        _reference: &str,
        _context: &dyn PluginContext,
    ) -> Result<Option<PluginResource>> {
        Ok(None)
    }
}
