//! Stream-facing native host values (spec 12.3-12.5, 24.3).

use crikey_core::Item;

/// Maximum number of diagnostic log records retained for one call.
///
/// A plugin can send empty records at no byte cost, so the byte limit alone
/// must never determine the size of the returned vector (spec 12.4).
pub const MAX_LOG_RECORDS: usize = 4096;

/// Maximum number of decoded envelopes retained by the reader.
pub const READER_QUEUE_CAPACITY: usize = 64;

/// Maximum encoded size of envelopes retained by the reader queue.
///
/// The frame limit is eight MiB, but the queue is deliberately much smaller
/// than `READER_QUEUE_CAPACITY * MAX_FRAME_BYTES` so a peer cannot park a
/// large decoded payload in every slot (spec 12.4).
pub const READER_QUEUE_MAX_BYTES: usize = 16 * 1024 * 1024;

/// A protocol envelope observed at the host/plugin boundary.
///
/// The ring is bounded to [`OBSERVATION_CAPACITY`] entries; observations are
/// diagnostic evidence, not an unbounded event log (spec 26.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolObservation {
    pub direction: &'static str,
    pub kind: &'static str,
    pub request_id: u64,
    pub connection_id: u64,
    pub generation: u64,
}

/// Maximum number of protocol observations retained by a worker.
pub const OBSERVATION_CAPACITY: usize = 256;

/// Evidence that a plugin response did not echo the active request identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoMismatch {
    pub request_id: bool,
    pub generation: bool,
    pub reason: String,
}

/// Terminal state folded from one or more result batches (spec 12.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchState {
    Final,
    Cancelled,
    Failed,
}

/// Structured error reported by a plugin callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginError {
    pub message: String,
    pub detail: String,
}

/// Suggestions accumulated across a streamed call.
#[derive(Debug, Clone)]
pub struct Suggestions {
    pub items: Vec<Item>,
    pub state: BatchState,
    pub log: Vec<String>,
    /// Present exactly when [`state`](Self::state) is [`BatchState::Failed`].
    pub error: Option<PluginError>,
    /// Number of result-batch frames consumed, including the terminal frame.
    pub batches: usize,
    /// True when a host aggregate limit stopped the stream early.
    pub truncated: bool,
}

/// Outcome of one plugin action execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecuteOutcome {
    Ok,
    Failed(PluginError),
    Unsupported,
    /// The action asked the host to open the named plugin-drawn page
    /// (spec 32.2). The id is the plugin's own handle for the surface; every
    /// later frame request quotes it back, so an empty one names nothing the
    /// plugin could serve and is refused as a protocol violation instead.
    ShowPage {
        page_id: String,
    },
}

/// Health report returned by a native plugin (spec 24.3, 26.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthSnapshot {
    pub healthy: bool,
    pub memory_bytes: u64,
    pub queue_depth: u32,
    pub in_flight: u32,
    pub detail: String,
}

/// Stream counters exposed to diagnostics (spec 24.3, 26.1).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StreamDiagnostics {
    pub batches: u64,
    pub items: u64,
    pub bytes: u64,
    pub credits_granted: u64,
    pub truncated_calls: u64,
    pub rejected_stale: u64,
    pub dropped_obsolete: u64,
    pub peak_queue_depth: u32,
}

/// One host-to-plugin suggestion request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSuggestRequest {
    pub generation: u64,
    pub text: String,
    pub normalized: String,
    pub selected_item_id: Option<String>,
}

/// One host-to-plugin request for the next frame of an open page (spec 32.3).
///
/// The host drives the page: a plugin never pushes a frame, so this request is
/// the only thing that can produce one. `events` carries everything the user
/// did since the previous request, already hit-tested by the host, which is
/// what lets a burst of pointer motion cost one round trip rather than one
/// each.
#[derive(Debug, Clone, PartialEq)]
pub struct NativePageRequest {
    pub page_id: String,
    /// Monotonic per page. The worker echoes it, and a frame answering an
    /// older generation is dropped rather than drawn.
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub events: Vec<crikey_core::PageInput>,
    /// Whether the launcher window itself holds focus, so a page can dim its
    /// caret rather than pretend it is being typed into.
    pub focused: bool,
    /// Host palette as `0xRRGGBBAA`. Zero states no colour, leaving the
    /// plugin its own default rather than painting everything transparent.
    pub colour_surface: u32,
    pub colour_text: u32,
    pub colour_accent: u32,
    pub colour_muted: u32,
}

/// Combines a plugin error message and detail for host diagnostics.
pub(crate) fn error_detail(error: &PluginError) -> String {
    if error.detail.is_empty() {
        error.message.clone()
    } else if error.message.is_empty() {
        error.detail.clone()
    } else {
        format!("{}: {}", error.message, error.detail)
    }
}
