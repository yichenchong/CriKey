//! Official Rust SDK for CriKey native plugins (spec 16.7).
//!
//! A native plugin is a supervised executable. It never loads into the CriKey
//! process, so it may use threads, SIMD, native libraries and memory-mapped
//! files freely inside its own process.

use crikey_core::{Item, PluginId, Result};
use crikey_native_protocol::RequestId;

/// A suggestion request delivered to the plugin.
#[derive(Debug, Clone)]
pub struct Query {
    pub request: RequestId,
    pub text: String,
    /// Deadline in milliseconds from receipt, when the host set one.
    pub deadline_ms: Option<u64>,
}

/// Cooperative cancellation handed to every request (spec 9.4).
///
/// Plugins should poll it before expensive work, inside long loops and before
/// emitting large batches. The host rejects stale results regardless.
pub trait CancellationToken: std::fmt::Debug {
    fn is_cancelled(&self) -> bool;
}

/// Per-request context: configuration, logging and cancellation.
pub trait PluginContext {
    fn plugin_id(&self) -> &PluginId;
    fn cancellation(&self) -> &dyn CancellationToken;
    fn log(&self, level: LogLevel, message: &str);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// Streaming sink for catalog construction. Batches, never one item per IPC
/// message (spec 16.5).
pub trait CatalogSink {
    fn emit_batch(&mut self, items: Vec<Item>) -> Result<()>;
    fn finish(&mut self) -> Result<()>;
}

/// Streaming sink for suggestions; supports partial and final batches.
pub trait SuggestionSink {
    fn emit_batch(&mut self, items: Vec<Item>) -> Result<()>;
    fn finish(&mut self) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct ExecuteRequest {
    pub request: RequestId,
    pub item: crikey_core::ItemId,
    pub action: Option<crikey_core::ActionId>,
    pub argument: Option<String>,
}

/// The trait a native plugin implements (spec 16.7).
pub trait Plugin {
    fn start(&mut self, context: &dyn PluginContext) -> Result<()>;

    fn build_catalog(&mut self, context: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()>;

    fn suggest(
        &mut self,
        query: Query,
        context: &dyn PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()>;

    fn execute(&mut self, request: ExecuteRequest, context: &dyn PluginContext) -> Result<()>;

    fn stop(&mut self, context: &dyn PluginContext) -> Result<()>;
}
