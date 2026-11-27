//! Typed submissions retain exact authority and permanently redact diagnostics.
use crikey_core::result_presentation::{ExecutableSchemaBinding, ResultPresentation};
use crikey_core::structured_arguments::*;
use crikey_core::{CoreError, ItemId, Result};
use crikey_plugin_sdk::{
    protocol::{
        self,
        message::{self, Envelope, Payload},
        transport::Transport,
    },
    *,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::thread;

fn schema() -> ArgumentSchema {
    ArgumentSchema {
        fields: vec![ArgumentField {
            id: FieldId("password".into()),
            label: "Password".into(),
            required: true,
            placeholder: None,
            validation_message: Some("Enter your password".into()),
            kind: FieldKind::Password {
                min_bytes: 1,
                max_bytes: 128,
            },
        }],
    }
}

struct Fixture {
    typed: Arc<AtomicUsize>,
    legacy: Arc<AtomicUsize>,
}
impl Plugin for Fixture {
    fn supports_typed_arguments(&self) -> bool {
        true
    }
    fn supports_result_presentation(&self) -> bool {
        true
    }
    fn catalog_presentation(&mut self, context: &dyn PluginContext) -> Result<ResultPresentation> {
        Ok(ResultPresentation {
            schemas: vec![ExecutableSchemaBinding {
                executable: ExecutableKey {
                    target: OwnedItem {
                        owner: context.plugin_id().clone(),
                        item: ItemId("login".into()),
                    },
                    action: None,
                },
                schema: Arc::new(schema()),
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
    fn build_catalog(&mut self, _: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        sink.emit_batch(vec![ItemBuilder::new("login", "Login").build()])?;
        sink.finish()
    }
    fn suggest(&mut self, _: Query, _: &dyn PluginContext, sink: &mut dyn SuggestionSink) -> Result<()> {
        sink.finish()
    }
    fn execute(&mut self, _: ExecuteRequest, context: &dyn PluginContext) -> Result<()> {
        self.legacy.fetch_add(1, Ordering::SeqCst);
        context.log(LogLevel::Error, "private-password");
        panic!("private-password")
    }
    fn execute_typed(
        &mut self,
        request: TypedExecuteRequest,
        context: &dyn PluginContext,
    ) -> Result<SubmissionOutcome> {
        self.typed.fetch_add(1, Ordering::SeqCst);
        assert!(request.action.is_none());
        context.log(LogLevel::Error, "private-password");
        let _ = thread::spawn(|| panic!("private-password background")).join();
        match request.values.0.get(&FieldId("password".into())) {
            Some(StructuredValue::Password(password)) if password.expose_plaintext() == "reject" => {
                Ok(SubmissionOutcome::InvalidFields(vec![SubmissionFieldFailure {
                    field: FieldId("attacker-field".into()),
                    code: SubmissionFieldCode::Rejected,
                }]))
            }
            _ => Err(CoreError::Invalid("private-password".into())),
        }
    }
}

fn frame(id: u64, payload: Payload) -> Envelope {
    Envelope {
        connection_id: 1,
        request_id: id,
        generation: 0,
        deadline_ms: 0,
        payload: Some(payload),
        unknown: Default::default(),
    }
}
type RunningFixture = (
    Box<dyn Transport>,
    thread::JoinHandle<Result<(), SdkError>>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
);

fn start(host_caps: &[&str]) -> RunningFixture {
    let (mut host, transport) = protocol::transport::pair();
    let typed = Arc::new(AtomicUsize::new(0));
    let legacy = Arc::new(AtomicUsize::new(0));
    let mut fixture = Fixture {
        typed: typed.clone(),
        legacy: legacy.clone(),
    };
    let worker = thread::spawn(move || {
        let mut config = ServeConfig::new("typed.test", "1");
        config.session_token = Some("token".into());
        serve_on(&mut fixture, transport, config)
    });
    let handshake = host.recv().unwrap();
    let Some(Payload::Handshake(handshake)) = handshake.payload else {
        panic!("handshake")
    };
    assert!(handshake.capabilities.iter().any(|s| s == "typed_arguments_v1"));
    assert!(handshake
        .capabilities
        .iter()
        .any(|s| s == "result_presentation_v1"));
    host.send(&frame(
        0,
        Payload::HandshakeAck(message::HandshakeAck {
            accepted: true,
            protocol_version: protocol::PROTOCOL_VERSION,
            host_capabilities: host_caps.iter().map(|s| (*s).into()).collect(),
            initial_credits: 32,
            ..Default::default()
        }),
    ))
    .unwrap();
    (host, worker, typed, legacy)
}
fn catalog(host: &mut dyn Transport) -> Option<message::ResultPresentation> {
    host.send(&frame(1, Payload::CatalogRequest(Default::default())))
        .unwrap();
    let mut metadata = None;
    loop {
        let Some(Payload::CatalogBatch(batch)) = host.recv().unwrap().payload else {
            panic!("catalog")
        };
        metadata = batch.presentation.or(metadata);
        if batch.done {
            assert!(batch.error.is_none());
            return metadata;
        }
    }
}
fn stop(host: &mut dyn Transport, worker: thread::JoinHandle<Result<(), SdkError>>) {
    host.send(&frame(99, Payload::Shutdown(Default::default())))
        .unwrap();
    worker.join().unwrap().unwrap();
}
fn typed_request(password: &str, fingerprint: [u8; 32]) -> message::TypedExecuteRequest {
    let mut values = ArgumentValues::default();
    values.0.insert(
        FieldId("password".into()),
        StructuredValue::Password(Password::new(password.into()).unwrap()),
    );
    protocol::typed::to_request(&ItemId("login".into()), None, fingerprint, values).unwrap()
}
fn outcome(host: &mut dyn Transport, request: message::TypedExecuteRequest) -> SubmissionOutcome {
    host.send(&frame(3, Payload::TypedExecuteRequest(request)))
        .unwrap();
    let response = host.recv().unwrap();
    assert!(!format!("{response:?}").contains("private-password"));
    let Some(Payload::TypedExecuteResponse(response)) = response.payload else {
        panic!("typed response")
    };
    protocol::typed::from_response(response, &schema()).unwrap()
}

#[test]
fn typed_authority_fingerprint_hostility_and_process_lifetime_privacy() {
    // Panic hooks and the SDK's Once-installed privacy policy are process-wide.
    // A sibling serving test must not initialize that policy before this test
    // installs its observer, or this observer would replace the privacy wrapper.
    const CHILD: &str = "CRIKEY_SDK_TYPED_PRIVACY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "typed_authority_fingerprint_hostility_and_process_lifetime_privacy",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .status()
            .expect("isolated typed privacy consumer");
        assert!(
            status.success(),
            "isolated typed privacy consumer exited with {status}"
        );
        return;
    }
    let panic_hooks = Arc::new(AtomicUsize::new(0));
    let observed_hooks = Arc::clone(&panic_hooks);
    std::panic::set_hook(Box::new(move |_| {
        observed_hooks.fetch_add(1, Ordering::SeqCst);
    }));
    // Old hosts still populate the private schema registry; legacy never runs it.
    let (mut host, worker, _, legacy) = start(&[]);
    assert!(catalog(&mut *host).is_none());
    host.send(&frame(
        2,
        Payload::Execute(message::ExecuteRequest {
            item_id: "login".into(),
            ..Default::default()
        }),
    ))
    .unwrap();
    let Some(Payload::ExecuteResult(result)) = host.recv().unwrap().payload else {
        panic!("legacy refusal")
    };
    assert!(result.error.is_some());
    assert_eq!(legacy.load(Ordering::SeqCst), 0);
    stop(&mut *host, worker);

    // Typed schemas negotiate without visual presentation.
    let (mut host, worker, typed, legacy) = start(&["typed_arguments_v1"]);
    let metadata = catalog(&mut *host).unwrap();
    assert_eq!(metadata.schemas.len(), 1);
    assert!(metadata.sections.is_empty());
    let hash = schema().fingerprint().unwrap();
    let mut stale = hash;
    stale[0] ^= 1;
    assert_eq!(
        outcome(&mut *host, typed_request("private-password", stale)),
        SubmissionOutcome::Failed(SubmissionFailureCode::SchemaMismatch)
    );
    assert_eq!(typed.load(Ordering::SeqCst), 0);
    let mut hostile = typed_request("private-password", hash);
    hostile.values.0.insert(
        FieldId("unknown".into()),
        StructuredValue::Password(Password::new("private-password".into()).unwrap()),
    );
    assert_eq!(
        outcome(&mut *host, hostile),
        SubmissionOutcome::Failed(SubmissionFailureCode::ProtocolViolation)
    );
    assert_eq!(typed.load(Ordering::SeqCst), 0);
    assert_eq!(
        outcome(&mut *host, typed_request("private-password", hash)),
        SubmissionOutcome::Failed(SubmissionFailureCode::Failed)
    );
    assert_eq!(
        outcome(&mut *host, typed_request("reject", hash)),
        SubmissionOutcome::Failed(SubmissionFailureCode::Failed)
    );
    assert_eq!(typed.load(Ordering::SeqCst), 2);
    // A later unrelated callback cannot leak through log/error/panic diagnostics.
    host.send(&frame(
        4,
        Payload::Execute(message::ExecuteRequest {
            item_id: "untyped".into(),
            ..Default::default()
        }),
    ))
    .unwrap();
    let response = host.recv().unwrap();
    assert!(!format!("{response:?}").contains("private-password"));
    assert!(matches!(response.payload, Some(Payload::ExecuteResult(_))));
    assert_eq!(legacy.load(Ordering::SeqCst), 1);
    stop(&mut *host, worker);

    // Presentation alone must not grant typed execution.
    let (mut host, worker, typed, _) = start(&["result_presentation_v1"]);
    assert!(catalog(&mut *host).unwrap().schemas.is_empty());
    assert_eq!(
        outcome(&mut *host, typed_request("private-password", hash)),
        SubmissionOutcome::Failed(SubmissionFailureCode::Unsupported)
    );
    assert_eq!(typed.load(Ordering::SeqCst), 0);
    stop(&mut *host, worker);
    assert_eq!(
        panic_hooks.load(Ordering::SeqCst),
        0,
        "background and later callback hooks must stay suppressed"
    );
}

fn boundary_rows() -> Vec<crikey_core::Item> {
    (0..16)
        .map(|index| {
            let id = if index == 0 {
                "login".into()
            } else {
                format!("row-{index}")
            };
            ItemBuilder::new(id, "Large row")
                .description("x".repeat(480 * 1024))
                .build()
        })
        .collect()
}

struct BoundaryFixture;

impl BoundaryFixture {
    fn presentation(context: &dyn PluginContext) -> ResultPresentation {
        use crikey_core::result_presentation::{Section, SectionId};
        ResultPresentation {
            sections: (0..128)
                .map(|index| Section {
                    id: SectionId(format!("section-{index}")),
                    label: "s".repeat(8192),
                })
                .collect(),
            schemas: vec![ExecutableSchemaBinding {
                executable: ExecutableKey {
                    target: OwnedItem {
                        owner: context.plugin_id().clone(),
                        item: ItemId("login".into()),
                    },
                    action: None,
                },
                schema: Arc::new(schema()),
            }],
            ..Default::default()
        }
    }
}

impl Plugin for BoundaryFixture {
    fn supports_typed_arguments(&self) -> bool {
        true
    }
    fn supports_result_presentation(&self) -> bool {
        true
    }
    fn start(&mut self, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn stop(&mut self, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn catalog_presentation(&mut self, context: &dyn PluginContext) -> Result<ResultPresentation> {
        Ok(Self::presentation(context))
    }
    fn suggestion_presentation(
        &mut self,
        _: &Query,
        context: &dyn PluginContext,
    ) -> Result<ResultPresentation> {
        Ok(Self::presentation(context))
    }
    fn build_catalog(&mut self, _: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        sink.emit_batch(boundary_rows())?;
        sink.finish()
    }
    fn suggest(&mut self, _: Query, _: &dyn PluginContext, sink: &mut dyn SuggestionSink) -> Result<()> {
        sink.emit_batch(boundary_rows())?;
        sink.finish()
    }
    fn execute(&mut self, _: ExecuteRequest, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn execute_typed(&mut self, _: TypedExecuteRequest, _: &dyn PluginContext) -> Result<SubmissionOutcome> {
        Ok(SubmissionOutcome::Completed)
    }
}

#[test]
fn large_rows_and_presentation_use_independent_decodable_frames() {
    for suggestions in [false, true] {
        let (mut host, transport) = protocol::transport::pair();
        host.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let worker = thread::spawn(move || {
            let mut config = ServeConfig::new("typed.test", "1");
            config.session_token = Some("token".into());
            serve_on(&mut BoundaryFixture, transport, config)
        });
        assert!(matches!(
            host.recv().unwrap().payload,
            Some(Payload::Handshake(_))
        ));
        host.send(&frame(
            0,
            Payload::HandshakeAck(message::HandshakeAck {
                accepted: true,
                protocol_version: protocol::PROTOCOL_VERSION,
                host_capabilities: vec!["typed_arguments_v1".into(), "result_presentation_v1".into()],
                initial_credits: 1,
                ..Default::default()
            }),
        ))
        .unwrap();
        let request = if suggestions {
            Payload::Suggest(message::SuggestRequest::default())
        } else {
            Payload::CatalogRequest(Default::default())
        };
        host.send(&frame(1, request)).unwrap();

        let mut presentation = None;
        let mut rows = Vec::new();
        let mut sequence = 0;
        loop {
            let envelope = host.recv().unwrap();
            assert_eq!((envelope.request_id, envelope.generation), (1, 0));
            message::validate_outbound(&envelope).unwrap();
            let (items, metadata, done, actual_sequence, error) = match envelope.payload.unwrap() {
                Payload::CatalogBatch(batch) if !suggestions => (
                    batch.items,
                    batch.presentation,
                    batch.done,
                    batch.sequence,
                    batch.error,
                ),
                Payload::Results(batch) if suggestions => {
                    assert_eq!(batch.state.as_i32(), if sequence == 2 { 2 } else { 1 });
                    (
                        batch.items,
                        batch.presentation,
                        batch.state.as_i32() == 2,
                        batch.sequence,
                        batch.error,
                    )
                }
                other => panic!("unexpected batch: {other:?}"),
            };
            assert_eq!(actual_sequence, sequence);
            assert!(error.is_none());
            if sequence == 0 {
                assert!(!done, "metadata alone must not finalize publication");
                assert!(items.is_empty());
                let metadata = metadata.expect("standalone presentation");
                assert_eq!(metadata.sections.len(), 128);
                assert_eq!(metadata.schemas.len(), 1);
                assert!(protocol::typed::presentation_charge(&metadata) > message::BATCH_OVERHEAD_RESERVE);
                presentation = Some(metadata);
            } else {
                assert!(metadata.is_none(), "sidecar must be emitted exactly once");
                rows.extend(items);
            }
            if done {
                assert_eq!(sequence, 2);
                break;
            }
            sequence += 1;
            // Each sidecar, item batch and terminal frame consumes its own credit.
            host.send(&frame(
                0,
                Payload::Flow(message::FlowControl {
                    credits: 1,
                    ..Default::default()
                }),
            ))
            .unwrap();
        }
        assert_eq!(rows.len(), 16);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(
                row.stable_id,
                if index == 0 {
                    "login".into()
                } else {
                    format!("row-{index}")
                }
            );
            assert_eq!(row.description, "x".repeat(480 * 1024));
        }
        assert_eq!(message::max_decodable_items(&rows), rows.len());
        // Both independent payloads are valid, but combining them exceeds the
        // decoder's 8 MiB budget even though the item-only chunker accepts all rows.
        let combined = if suggestions {
            Payload::Results(message::ResultBatch {
                items: rows,
                presentation,
                ..Default::default()
            })
        } else {
            Payload::CatalogBatch(message::CatalogBatch {
                items: rows,
                presentation,
                ..Default::default()
            })
        };
        assert!(message::validate_outbound(&frame(1, combined)).is_err());
        assert_eq!(
            outcome(
                &mut *host,
                typed_request("valid", schema().fingerprint().unwrap())
            ),
            SubmissionOutcome::Completed,
            "successful terminal publication installs exact typed authority"
        );
        stop(&mut *host, worker);
    }
}
