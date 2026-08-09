//! Adapts one [`Guest`] to the SDK's [`Plugin`] trait.
//!
//! This is the whole reason a WASM plugin is indistinguishable downstream from
//! a native one: `crikey-wasm-host` is an ordinary native plugin process, it
//! speaks the ordinary supervised native protocol through the published SDK,
//! and the only unusual thing about it is where the items come from.
//!
//! # Failure reporting
//!
//! A guest failure is returned as a plugin error for that one request, never
//! as silence and never as an empty successful answer. The distinction the
//! host cares about is preserved in the message: a deadline says the plugin
//! ran out of time, a trap says the plugin is broken. Both leave the process
//! alive and the next request servable.

use std::collections::BTreeMap;

use crikey_core::{CoreError, Result};
use crikey_plugin_sdk::{
    CatalogSink, ExecuteRequest as SdkExecuteRequest, LogLevel, Plugin, PluginContext, Query, SuggestionSink,
};

use crate::abi::{ExecuteRequest, SuggestRequest};
use crate::guest::{Guest, GuestError};
use crate::watchdog::Watchdog;

/// Items emitted per protocol batch. Matches the manifest model's own
/// `maximum-results-per-batch` default, so a wasm plugin streams at the same
/// granularity a native one does.
pub const BATCH_SIZE: usize = 50;

/// A WebAssembly module presented to the host as an ordinary native plugin.
#[derive(Debug)]
pub struct WasmPlugin {
    guest: Guest,
    watchdog: Watchdog,
}

impl WasmPlugin {
    /// Wraps a loaded guest, arming the wall-clock backstop from its
    /// configured hard deadline.
    pub fn new(guest: Guest) -> Self {
        let watchdog = Watchdog::spawn(guest.config().watchdog_window());
        Self { guest, watchdog }
    }

    /// [`Self::new`] with an injected watchdog, for tests that must not abort.
    pub fn with_watchdog(guest: Guest, watchdog: Watchdog) -> Self {
        Self { guest, watchdog }
    }

    /// Whether the module answers suggestion requests.
    pub fn answers_suggestions(&self) -> bool {
        self.guest.answers_suggestions()
    }

    /// Whether the module builds a catalog.
    pub fn builds_catalog(&self) -> bool {
        self.guest.builds_catalog()
    }

    fn drain_logs(&mut self, context: &dyn PluginContext) {
        for record in self.guest.take_logs() {
            context.log(record.level, &record.message);
        }
    }

    /// Turns a guest failure into a plugin error, naming which of the two
    /// stories it is.
    fn failure(error: GuestError) -> CoreError {
        if error.is_deadline() {
            CoreError::Invalid(format!("wasm plugin exceeded its deadline: {error}"))
        } else {
            CoreError::Invalid(format!("wasm plugin failed: {error}"))
        }
    }
}

impl Plugin for WasmPlugin {
    fn start(&mut self, context: &dyn PluginContext) -> Result<()> {
        // The module was validated, linked and instantiated before the serving
        // loop began, so there is nothing left to initialise. Anything the
        // guest said while starting is forwarded now.
        self.drain_logs(context);
        Ok(())
    }

    fn build_catalog(&mut self, context: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        if !self.guest.builds_catalog() {
            // Not an error: a suggestion-only plugin has an empty catalog and
            // must still terminate the stream.
            return sink.finish();
        }
        let items = {
            let _guard = self.watchdog.guard();
            self.guest.catalog()
        };
        self.drain_logs(context);
        let items = items.map_err(Self::failure)?;
        for batch in items.chunks(BATCH_SIZE) {
            sink.emit_batch(batch.to_vec())?;
        }
        sink.finish()
    }

    fn suggest(
        &mut self,
        query: Query,
        context: &dyn PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        if !self.guest.answers_suggestions() {
            return sink.finish();
        }
        let request = SuggestRequest {
            text: query.text,
            normalized: query.normalized,
            generation: query.generation,
            // The advisory budget the guest should aim at. Enforcement is fuel
            // and the watchdog, neither of which the guest can observe.
            deadline_ms: Some(query.deadline_ms.unwrap_or(self.guest.config().soft_deadline_ms)),
            selected_item_id: query.selected_item_id,
        };

        let items = {
            let _guard = self.watchdog.guard();
            self.guest.suggest(&request)
        };
        self.drain_logs(context);
        let items = items.map_err(Self::failure)?;

        for batch in items.chunks(BATCH_SIZE) {
            // A guest call is not interruptible, so cancellation is honoured
            // at the only point the host reaches: between batches, before the
            // rows cross the boundary.
            if sink.is_cancelled() {
                return sink.finish();
            }
            sink.emit_batch(batch.to_vec())?;
        }
        sink.finish()
    }

    fn execute(&mut self, request: SdkExecuteRequest, context: &dyn PluginContext) -> Result<()> {
        if !self.guest.executes_actions() {
            return Err(CoreError::Invalid(
                "wasm plugin exports no action entry point".to_owned(),
            ));
        }
        let outcome = {
            let _guard = self.watchdog.guard();
            self.guest.execute(&ExecuteRequest {
                item_id: request.item.0,
                action_id: request.action.map(|action| action.0),
                argument: request.argument,
            })
        };
        self.drain_logs(context);
        outcome.map_err(Self::failure)
    }

    fn stop(&mut self, context: &dyn PluginContext) -> Result<()> {
        self.drain_logs(context);
        Ok(())
    }

    /// Configuration is accepted and reported, not applied.
    ///
    /// The guest ABI has no configuration entry point in revision 1, so
    /// claiming a value was delivered would be a lie. Saying so once per
    /// publication is the honest alternative to silence (README invariant 7).
    fn on_configuration(
        &mut self,
        values: &BTreeMap<String, String>,
        context: &dyn PluginContext,
    ) -> Result<()> {
        if !values.is_empty() {
            context.log(
                LogLevel::Warn,
                &format!(
                    "{} configuration values were published but guest ABI {} has no \
                     configuration entry point, so none were delivered to the module",
                    values.len(),
                    crate::abi::ABI_VERSION
                ),
            );
        }
        Ok(())
    }
}
