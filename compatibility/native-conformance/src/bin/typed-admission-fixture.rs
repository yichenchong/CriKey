//! Real SDK consumer for provider pre-delivery admission races.
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crikey_core::result_presentation::{ExecutableSchemaBinding, ResultPresentation};
use crikey_core::structured_arguments::*;
use crikey_core::structured_view::TypedExecutionOutcome;
use crikey_core::{CoreError, ItemId, Result};
use crikey_native_protocol::message::{Envelope, Payload};
use crikey_native_protocol::transport::Transport;
use crikey_native_protocol::{Endpoint, ProtocolError};
use crikey_plugin_sdk::{
    serve_on, CatalogSink, ExecuteRequest, ItemBuilder, Plugin, PluginContext, Query, ServeConfig,
    SuggestionSink, TypedExecuteRequest,
};

#[derive(Debug)]
struct Witness(Box<dyn Transport>);
impl Witness {
    fn record(envelope: Envelope) -> std::result::Result<Envelope, ProtocolError> {
        if matches!(envelope.payload.as_ref(), Some(Payload::TypedExecuteRequest(_))) {
            // Observe physical receipt before SDK validation or callback admission.
            // Do not persist the values themselves.
            std::fs::write("typed-received", b"received")
                .map_err(|error| ProtocolError::Io(error.to_string()))?;
        }
        Ok(envelope)
    }
}
impl Transport for Witness {
    fn send(&mut self, envelope: &Envelope) -> std::result::Result<(), ProtocolError> {
        self.0.send(envelope)
    }
    fn recv(&mut self) -> std::result::Result<Envelope, ProtocolError> {
        Self::record(self.0.recv()?)
    }
    fn recv_with_diagnostics(
        &mut self,
        sensitive: &AtomicBool,
    ) -> std::result::Result<Envelope, ProtocolError> {
        Self::record(self.0.recv_with_diagnostics(sensitive)?)
    }
    fn try_clone_handle(&self) -> std::result::Result<Box<dyn Transport>, ProtocolError> {
        Ok(Box::new(Self(self.0.try_clone_handle()?)))
    }
    fn supports_read_timeout(&self) -> bool {
        self.0.supports_read_timeout()
    }
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> std::result::Result<(), ProtocolError> {
        self.0.set_read_timeout(timeout)
    }
    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> std::result::Result<(), ProtocolError> {
        self.0.set_write_timeout(timeout)
    }
    fn close(&mut self) {
        self.0.close();
    }
}

struct Worker;
impl Plugin for Worker {
    fn supports_typed_arguments(&self) -> bool {
        true
    }
    fn supports_commands(&self) -> bool {
        true
    }
    fn commands(&mut self, _: &dyn PluginContext) -> Result<crikey_plugin_sdk::PluginCommands> {
        Ok(crikey_plugin_sdk::PluginCommands::Declarations(vec![
            crikey_plugin_sdk::CommandBuilder::new("login", "Login", "typed-login").build()?,
        ]))
    }
    fn supports_result_presentation(&self) -> bool {
        true
    }
    fn catalog_presentation(&mut self, context: &dyn PluginContext) -> Result<ResultPresentation> {
        Ok(ResultPresentation {
            sections: vec![crikey_core::result_presentation::Section {
                id: crikey_core::result_presentation::SectionId("accounts".into()),
                label: "Accounts".into(),
            }],
            scopes: vec![crikey_core::result_presentation::Scope {
                id: crikey_core::result_presentation::ScopeId("private".into()),
                label: "Private".into(),
            }],
            schemas: vec![ExecutableSchemaBinding {
                executable: ExecutableKey {
                    target: OwnedItem {
                        owner: context.plugin_id().clone(),
                        item: ItemId("typed-login".into()),
                    },
                    action: None,
                },
                schema: Arc::new(ArgumentSchema {
                    fields: vec![ArgumentField {
                        id: FieldId("password".into()),
                        label: "Password".into(),
                        required: true,
                        placeholder: None,
                        validation_message: None,
                        kind: FieldKind::Password {
                            min_bytes: 1,
                            max_bytes: 128,
                        },
                    }],
                }),
            }],
            ..Default::default()
        })
    }
    fn start(&mut self, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn stop(&mut self, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn build_catalog(&mut self, context: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        if std::path::Path::new("typed-discovery-hold").exists() {
            std::fs::write("typed-discovery-entered", b"entered").unwrap();
            let end = Instant::now() + Duration::from_secs(20);
            while !std::path::Path::new("typed-discovery-release").exists() {
                if context.cancellation().is_cancelled() || Instant::now() >= end {
                    return Err(CoreError::Cancelled);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        sink.emit_batch(vec![ItemBuilder::new("typed-login", "Typed login").build()])?;
        sink.finish()
    }
    fn suggest(&mut self, _: Query, _: &dyn PluginContext, sink: &mut dyn SuggestionSink) -> Result<()> {
        sink.emit_batch(vec![ItemBuilder::new("scope-item", "Scope item").build()])?;
        sink.finish()
    }
    fn suggestion_presentation(&mut self, _: &Query, _: &dyn PluginContext) -> Result<ResultPresentation> {
        Ok(ResultPresentation {
            scopes: ["private", "other"]
                .into_iter()
                .map(|id| crikey_core::result_presentation::Scope {
                    id: crikey_core::result_presentation::ScopeId(id.into()),
                    label: id.into(),
                })
                .collect(),
            ..Default::default()
        })
    }
    fn execute(&mut self, _: ExecuteRequest, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn execute_typed(
        &mut self,
        _: TypedExecuteRequest,
        _: &dyn PluginContext,
    ) -> Result<TypedExecutionOutcome> {
        std::fs::write("typed-executed", b"executed").unwrap();
        Ok(SubmissionOutcome::Completed.into())
    }
}
fn main() {
    let endpoint = Endpoint::parse(&std::env::var(crikey_native_protocol::ENV_ENDPOINT).unwrap()).unwrap();
    let transport = match endpoint {
        Endpoint::Stdio => crikey_native_protocol::transport::stdio(),
        endpoint => crikey_native_protocol::transport::connect(&endpoint, None).unwrap(),
    };
    serve_on(
        &mut Worker,
        Box::new(Witness(transport)),
        ServeConfig::new("typed", "1.0.0"),
    )
    .unwrap();
}
