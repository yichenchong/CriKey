//! Recoverable callbacks must not discard the physical connection or cancellation token.
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
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
enum Failure {
    Error,
    Panic,
    Cancel,
}

struct Fixture {
    failure: Option<Failure>,
    entered: mpsc::Sender<()>,
    cancel_typed: bool,
}
impl Fixture {
    fn presentation(&mut self, context: &dyn PluginContext) -> Result<ResultPresentation> {
        match self.failure.take() {
            Some(Failure::Error) => return Err(CoreError::Invalid("private-presentation".into())),
            Some(Failure::Panic) => panic!("private-presentation"),
            Some(Failure::Cancel) => {
                self.entered.send(()).unwrap();
                wait_for_cancel(context)?;
                return Err(CoreError::Cancelled);
            }
            None => {}
        }
        Ok(ResultPresentation {
            schemas: vec![ExecutableSchemaBinding {
                executable: ExecutableKey {
                    target: OwnedItem {
                        owner: context.plugin_id().clone(),
                        item: ItemId("typed".into()),
                    },
                    action: None,
                },
                schema: Arc::new(ArgumentSchema::default()),
            }],
            ..Default::default()
        })
    }
}
fn wait_for_cancel(context: &dyn PluginContext) -> Result<()> {
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        if context.cancellation().is_cancelled() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(1));
    }
    Err(CoreError::Invalid("callback did not observe cancellation".into()))
}
impl Plugin for Fixture {
    fn start(&mut self, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn stop(&mut self, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn supports_typed_arguments(&self) -> bool {
        true
    }
    fn catalog_presentation(&mut self, context: &dyn PluginContext) -> Result<ResultPresentation> {
        self.presentation(context)
    }
    fn suggestion_presentation(
        &mut self,
        _: &Query,
        context: &dyn PluginContext,
    ) -> Result<ResultPresentation> {
        self.presentation(context)
    }
    fn build_catalog(&mut self, _: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        sink.emit_batch(vec![ItemBuilder::new("typed", "Typed").build()])?;
        sink.finish()
    }
    fn suggest(&mut self, _: Query, _: &dyn PluginContext, sink: &mut dyn SuggestionSink) -> Result<()> {
        sink.emit_batch(vec![ItemBuilder::new("typed", "Typed").build()])?;
        sink.finish()
    }
    fn execute(&mut self, _: ExecuteRequest, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn execute_typed(
        &mut self,
        _: TypedExecuteRequest,
        context: &dyn PluginContext,
    ) -> Result<TypedExecutionOutcome> {
        self.entered.send(()).unwrap();
        if self.cancel_typed {
            self.cancel_typed = false;
            wait_for_cancel(context)?;
        }
        // Completion racing with cancellation must not become a successful
        // submission merely because the callback returns normally.
        Ok(SubmissionOutcome::Completed.into())
    }
}
fn frame(id: u64, payload: Payload) -> Envelope {
    Envelope {
        connection_id: 1,
        request_id: id,
        generation: 7,
        deadline_ms: 0,
        payload: Some(payload),
        unknown: Default::default(),
    }
}
type RunningFixture = (
    Box<dyn Transport>,
    thread::JoinHandle<std::result::Result<(), SdkError>>,
    mpsc::Receiver<()>,
);

fn start(failure: Option<Failure>, cancel_typed: bool) -> RunningFixture {
    let (mut host, transport) = protocol::transport::pair();
    let (entered, receiver) = mpsc::channel();
    let mut fixture = Fixture {
        failure,
        entered,
        cancel_typed,
    };
    let worker = thread::spawn(move || {
        let mut config = ServeConfig::new("typed.recovery", "1");
        config.session_token = Some("token".into());
        serve_on(&mut fixture, transport, config)
    });
    assert!(matches!(
        host.recv().unwrap().payload,
        Some(Payload::Handshake(_))
    ));
    let mut ack = frame(
        0,
        Payload::HandshakeAck(message::HandshakeAck {
            accepted: true,
            protocol_version: protocol::PROTOCOL_VERSION,
            host_capabilities: vec!["typed_arguments_v1".into()],
            initial_credits: 32,
            ..Default::default()
        }),
    );
    ack.generation = 0;
    host.send(&ack).unwrap();
    (host, worker, receiver)
}
fn stop(host: &mut dyn Transport, worker: thread::JoinHandle<std::result::Result<(), SdkError>>) {
    host.send(&frame(99, Payload::Shutdown(Default::default())))
        .unwrap();
    worker.join().unwrap().unwrap();
}
fn request(suggestions: bool) -> Payload {
    if suggestions {
        Payload::Suggest(message::SuggestRequest {
            text: "test".into(),
            normalized_text: "test".into(),
            max_items: 10,
            max_batches: 10,
            ..Default::default()
        })
    } else {
        Payload::CatalogRequest(Default::default())
    }
}
fn terminal(host: &mut dyn Transport, id: u64, suggestions: bool, failed: bool, cancelled: bool) {
    loop {
        let reply = host.recv().unwrap();
        assert_eq!(reply.request_id, id);
        assert!(!format!("{reply:?}").contains("private-presentation"));
        if suggestions {
            let Some(Payload::Results(batch)) = reply.payload else {
                panic!("results")
            };
            if batch.state.as_i32() != 1 {
                assert_eq!(
                    batch.state.as_i32(),
                    if cancelled {
                        3
                    } else if failed {
                        4
                    } else {
                        2
                    }
                );
                assert_eq!(batch.error.is_some(), failed && !cancelled);
                if failed {
                    assert!(batch.presentation.is_none());
                }
                return;
            }
        } else {
            let Some(Payload::CatalogBatch(batch)) = reply.payload else {
                panic!("catalog")
            };
            if batch.done {
                assert_eq!(batch.error.is_some(), failed);
                if failed {
                    assert!(batch.presentation.is_none());
                }
                return;
            }
        }
    }
}
fn typed() -> Payload {
    Payload::TypedExecuteRequest(
        protocol::typed::to_request(
            &ItemId("typed".into()),
            None,
            ArgumentSchema::default().fingerprint().unwrap(),
            ArgumentValues::default(),
        )
        .unwrap(),
    )
}
fn typed_outcome(host: &mut dyn Transport) -> TypedExecutionOutcome {
    let Some(Payload::TypedExecuteResponse(response)) = host.recv().unwrap().payload else {
        panic!("typed response")
    };
    protocol::typed::from_response(response, &ArgumentSchema::default()).unwrap()
}

#[test]
fn presentation_failures_keep_same_connection_usable() {
    for suggestions in [false, true] {
        for failure in [Failure::Error, Failure::Panic, Failure::Cancel] {
            let (mut host, worker, entered) = start(Some(failure), false);
            // Enter sensitive mode before presentation callbacks: panic/error details must stay private.
            host.send(&frame(1, typed())).unwrap();
            assert_eq!(
                typed_outcome(&mut *host),
                SubmissionOutcome::Failed(SubmissionFailureCode::Unavailable).into()
            );
            host.send(&frame(2, request(suggestions))).unwrap();
            if matches!(failure, Failure::Cancel) {
                entered.recv_timeout(Duration::from_secs(3)).unwrap();
                host.send(&frame(2, Payload::Cancel(Default::default()))).unwrap();
            }
            terminal(
                &mut *host,
                2,
                suggestions,
                true,
                matches!(failure, Failure::Cancel),
            );
            // A failed publication must not leave schema authority behind.
            host.send(&frame(3, typed())).unwrap();
            assert_eq!(
                typed_outcome(&mut *host),
                SubmissionOutcome::Failed(SubmissionFailureCode::Unavailable).into()
            );
            host.send(&frame(4, request(suggestions))).unwrap();
            terminal(&mut *host, 4, suggestions, false, false);
            host.send(&frame(5, typed())).unwrap();
            assert_eq!(typed_outcome(&mut *host), SubmissionOutcome::Completed.into());
            stop(&mut *host, worker);
        }
    }
}

#[test]
fn typed_callback_observes_cancel_without_host_timeout() {
    let (mut host, worker, entered) = start(None, true);
    host.send(&frame(1, request(false))).unwrap();
    terminal(&mut *host, 1, false, false, false);
    host.send(&frame(2, typed())).unwrap();
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    host.send(&frame(2, Payload::Cancel(Default::default()))).unwrap();
    assert_eq!(
        typed_outcome(&mut *host),
        SubmissionOutcome::Failed(SubmissionFailureCode::Cancelled).into()
    );
    host.send(&frame(3, typed())).unwrap();
    assert_eq!(typed_outcome(&mut *host), SubmissionOutcome::Completed.into());
    stop(&mut *host, worker);
}

#[test]
fn already_cancelled_typed_request_never_enters_callback() {
    let (mut host, worker, entered) = start(None, false);
    // The catalog round trip is a barrier: the runtime has processed this
    // cancellation before the later request begins, without timing assumptions.
    host.send(&frame(2, Payload::Cancel(Default::default()))).unwrap();
    host.send(&frame(1, request(false))).unwrap();
    terminal(&mut *host, 1, false, false, false);
    host.send(&frame(2, typed())).unwrap();
    assert_eq!(
        typed_outcome(&mut *host),
        SubmissionOutcome::Failed(SubmissionFailureCode::Cancelled).into()
    );
    assert!(matches!(entered.try_recv(), Err(mpsc::TryRecvError::Empty)));
    host.send(&frame(3, typed())).unwrap();
    assert_eq!(typed_outcome(&mut *host), SubmissionOutcome::Completed.into());
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    stop(&mut *host, worker);
}
