//! Red-first contract tests for the Rust native-plugin runtime (spec 16.7;
//! protocol clauses 16.5, 9.4, and 12.5).
//!
//! Every serving test supplies an explicit [`ServeConfig`]. The sole
//! `from_env` test respawns this test binary twice, with the child command's
//! environment owning the variables; the parent test process never mutates its
//! environment, avoiding process-wide environment races.

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use std::{env, process::Command, thread};

use crikey_core::{Category, CoreError, Item, Result};
use crikey_native_protocol::{
    message::{
        Cancel, Envelope, FlowControl, Handshake, HandshakeAck, Payload, ResultBatch, Shutdown,
        SuggestRequest,
    },
    transport::Transport,
    wire::UnknownFields,
    Capabilities, Endpoint, MAX_FRAME_BYTES, PROTOCOL_VERSION,
};
use crikey_plugin_sdk::{
    protocol, serve_on, ActionBuilder, CatalogSink, ExecuteRequest, ItemBuilder, LogLevel, Plugin,
    PluginContext, Query, SdkError, ServeConfig, SuggestionSink,
};

const FROM_ENV_CHILD: &str = "CRIKEY_SDK_FROM_ENV_CHILD_MODE";

/// Bounded observation of a value another thread publishes. Never a sleep used
/// as synchronisation: it yields and gives up after a hard cap, so a broken
/// implementation fails the assertion instead of hanging the suite.
fn poll_until(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..100_000 {
        if condition() {
            return true;
        }
        std::thread::yield_now();
    }
    false
}

fn explicit_config() -> ServeConfig {
    ServeConfig {
        plugin_id: "sdk.test".to_owned(),
        plugin_name: "SDK test plugin".to_owned(),
        plugin_version: "1.2.3".to_owned(),
        sdk_version: "sdk-test".to_owned(),
        capabilities: Capabilities {
            streaming_catalog: true,
            streaming_suggestions: true,
            cancellation: true,
            configuration_updates: true,
            events: true,
        },
        endpoint: Some(Endpoint::Stdio),
        session_token: Some("session-token-for-tests".to_owned()),
    }
}

fn item(stable_id: impl Into<String>, label: impl Into<String>) -> Item {
    ItemBuilder::new(stable_id, label).target("target").build()
}

fn envelope(request_id: u64, generation: u64, payload: Payload) -> Envelope {
    Envelope {
        connection_id: 1,
        request_id,
        generation,
        deadline_ms: 0,
        payload: Some(payload),
        unknown: UnknownFields::default(),
    }
}

fn ack(initial_credits: u32) -> Envelope {
    envelope(
        0,
        0,
        Payload::HandshakeAck(HandshakeAck {
            protocol_version: PROTOCOL_VERSION,
            host_capabilities: vec!["streaming".to_owned(), "cancellation".to_owned()],
            host_version: "test-host".to_owned(),
            accepted: true,
            reject_reason: String::new(),
            max_frame_bytes: MAX_FRAME_BYTES as u64,
            initial_credits,
            unknown: UnknownFields::default(),
        }),
    )
}

fn complete_handshake(host: &mut Box<dyn Transport>, initial_credits: u32) -> Handshake {
    let frame = host.recv().expect("plugin handshake");
    let handshake = match frame.payload {
        Some(Payload::Handshake(value)) => value,
        other => panic!("expected plugin handshake, got {other:?}"),
    };
    host.send(&ack(initial_credits))
        .expect("handshake acknowledgement");
    handshake
}

#[derive(Debug)]
struct NoTimeoutTransport {
    inner: Box<dyn Transport>,
    timeout_calls: Arc<AtomicUsize>,
}

impl NoTimeoutTransport {
    fn new(inner: Box<dyn Transport>, timeout_calls: Arc<AtomicUsize>) -> Self {
        Self { inner, timeout_calls }
    }
}

impl Transport for NoTimeoutTransport {
    fn send(&mut self, envelope: &Envelope) -> Result<(), protocol::ProtocolError> {
        self.inner.send(envelope)
    }

    fn recv(&mut self) -> Result<Envelope, protocol::ProtocolError> {
        self.inner.recv()
    }

    fn try_clone_handle(&self) -> Result<Box<dyn Transport>, protocol::ProtocolError> {
        Ok(Box::new(Self::new(
            self.inner.try_clone_handle()?,
            Arc::clone(&self.timeout_calls),
        )))
    }

    fn supports_read_timeout(&self) -> bool {
        false
    }

    fn set_read_timeout(&mut self, _timeout: Option<Duration>) -> Result<(), protocol::ProtocolError> {
        self.timeout_calls.fetch_add(1, Ordering::SeqCst);
        Err(protocol::ProtocolError::Malformed(
            "read timeouts are unsupported".to_owned(),
        ))
    }

    fn close(&mut self) {
        self.inner.close();
    }
}
#[derive(Debug)]
struct FailingTimeoutTransport {
    inner: Box<dyn Transport>,
    timeout_calls: Arc<AtomicUsize>,
}

impl FailingTimeoutTransport {
    fn new(inner: Box<dyn Transport>, timeout_calls: Arc<AtomicUsize>) -> Self {
        Self { inner, timeout_calls }
    }
}

impl Transport for FailingTimeoutTransport {
    fn send(&mut self, envelope: &Envelope) -> Result<(), protocol::ProtocolError> {
        self.inner.send(envelope)
    }

    fn recv(&mut self) -> Result<Envelope, protocol::ProtocolError> {
        self.inner.recv()
    }

    fn try_clone_handle(&self) -> Result<Box<dyn Transport>, protocol::ProtocolError> {
        Ok(Box::new(Self {
            inner: self.inner.try_clone_handle()?,
            timeout_calls: Arc::clone(&self.timeout_calls),
        }))
    }

    fn supports_read_timeout(&self) -> bool {
        true
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), protocol::ProtocolError> {
        if self.timeout_calls.fetch_add(1, Ordering::SeqCst) >= 2 {
            return Err(protocol::ProtocolError::Malformed(
                "read timeout setup failed".to_owned(),
            ));
        }
        self.inner.set_read_timeout(timeout)
    }

    fn close(&mut self) {
        self.inner.close();
    }
}

#[derive(Debug)]
struct FaultAfterHandshakeTransport {
    inner: Box<dyn Transport>,
    after_handshake: Arc<AtomicBool>,
    detail: &'static str,
}

impl FaultAfterHandshakeTransport {
    fn new(inner: Box<dyn Transport>, detail: &'static str) -> Self {
        Self {
            inner,
            after_handshake: Arc::new(AtomicBool::new(false)),
            detail,
        }
    }
}

impl Transport for FaultAfterHandshakeTransport {
    fn send(&mut self, envelope: &Envelope) -> Result<(), protocol::ProtocolError> {
        self.inner.send(envelope)
    }

    fn recv(&mut self) -> Result<Envelope, protocol::ProtocolError> {
        if self.after_handshake.swap(false, Ordering::AcqRel) {
            return Err(protocol::ProtocolError::Malformed(self.detail.to_owned()));
        }
        let envelope = self.inner.recv()?;
        self.after_handshake.store(true, Ordering::Release);
        Ok(envelope)
    }

    fn try_clone_handle(&self) -> Result<Box<dyn Transport>, protocol::ProtocolError> {
        Ok(Box::new(Self {
            inner: self.inner.try_clone_handle()?,
            after_handshake: Arc::clone(&self.after_handshake),
            detail: self.detail,
        }))
    }

    fn supports_read_timeout(&self) -> bool {
        self.inner.supports_read_timeout()
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), protocol::ProtocolError> {
        self.inner.set_read_timeout(timeout)
    }

    fn close(&mut self) {
        self.inner.close();
    }
}

#[derive(Clone, Copy)]
enum SuggestMode {
    Echo,
    Stream,
    Credit,
    Cancel,
    CancelIgnored,
    Log,
    Panic,
    FailOnce,
}

struct RuntimePlugin {
    mode: SuggestMode,
    calls: usize,
    credit_progress: Option<Arc<AtomicUsize>>,
    stop_observed: Option<Arc<AtomicUsize>>,
}

impl RuntimePlugin {
    fn new(mode: SuggestMode) -> Self {
        Self {
            mode,
            calls: 0,
            credit_progress: None,
            stop_observed: None,
        }
    }

    fn with_credit_progress(progress: Arc<AtomicUsize>) -> Self {
        Self {
            mode: SuggestMode::Credit,
            calls: 0,
            credit_progress: Some(progress),
            stop_observed: None,
        }
    }

    fn with_stop_observer(mode: SuggestMode, stop_observed: Arc<AtomicUsize>) -> Self {
        Self {
            mode,
            calls: 0,
            credit_progress: None,
            stop_observed: Some(stop_observed),
        }
    }
}

impl Plugin for RuntimePlugin {
    fn start(&mut self, _context: &dyn PluginContext) -> Result<()> {
        Ok(())
    }

    fn build_catalog(&mut self, _context: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        sink.finish()
    }

    fn suggest(
        &mut self,
        _query: Query,
        context: &dyn PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()> {
        match self.mode {
            SuggestMode::Echo => {
                sink.emit_batch(vec![item("echo", "echo")])?;
                sink.finish()
            }
            SuggestMode::Stream => {
                for batch in 0..10 {
                    let items = (0..10)
                        .map(|offset| {
                            let index = batch * 10 + offset;
                            item(format!("item-{index}"), format!("label-{index}"))
                        })
                        .collect();
                    sink.emit_batch(items)?;
                }
                sink.finish()
            }
            SuggestMode::Credit => {
                sink.emit_batch(vec![item("first", "first")])?;
                if let Some(progress) = &self.credit_progress {
                    progress.fetch_add(1, Ordering::SeqCst);
                }
                sink.emit_batch(vec![item("second", "second")])?;
                if let Some(progress) = &self.credit_progress {
                    progress.fetch_add(1, Ordering::SeqCst);
                }
                sink.finish()
            }
            SuggestMode::Cancel => {
                sink.emit_batch(vec![item("before-cancel", "before cancel")])?;
                if sink
                    .emit_batch(vec![item("after-cancel", "after cancel")])
                    .is_err()
                    && sink.is_cancelled()
                {
                    return sink.finish();
                }
                sink.finish()
            }
            SuggestMode::CancelIgnored => {
                sink.emit_batch(vec![item("before-cancel", "before cancel")])?;
                for _ in 0..16 {
                    let _ = sink.emit_batch(vec![item("discarded", "discarded")]);
                }
                sink.finish()
            }
            SuggestMode::Log => {
                context.log(LogLevel::Info, "sdk runtime log");
                sink.finish()
            }
            SuggestMode::Panic => panic!("intentional suggest panic"),
            SuggestMode::FailOnce => {
                self.calls += 1;
                if self.calls == 1 {
                    return Err(CoreError::Invalid("intentional suggest failure".to_owned()));
                }
                sink.emit_batch(vec![item("reused", "worker remained alive")])?;
                sink.finish()
            }
        }
    }

    fn execute(&mut self, _request: ExecuteRequest, _context: &dyn PluginContext) -> Result<()> {
        Ok(())
    }

    fn stop(&mut self, _context: &dyn PluginContext) -> Result<()> {
        if let Some(stop_observed) = &self.stop_observed {
            stop_observed.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[test]
fn serve_config_from_env_reads_variables_and_rejects_missing_endpoint_in_isolated_children() {
    match env::var(FROM_ENV_CHILD).as_deref() {
        Ok("configured") => {
            let config = ServeConfig::from_env("fallback-id", "fallback-version")
                .expect("configured child environment");
            assert_eq!(config.plugin_id, "env.plugin");
            assert_eq!(config.session_token.as_deref(), Some("env-session"));
            assert_eq!(config.endpoint, Some(Endpoint::Stdio));
            return;
        }
        Ok("missing-endpoint") => {
            let error = match ServeConfig::from_env("fallback-id", "fallback-version") {
                Ok(_) => panic!("missing endpoint must reject configuration"),
                Err(error) => error,
            };
            match error {
                SdkError::Config(detail) => {
                    assert!(!detail.is_empty(), "missing endpoint diagnostic");
                }
                other => panic!("missing endpoint returned the wrong error: {other:?}"),
            }
            return;
        }
        _ => {}
    }

    let executable = env::current_exe().expect("current test executable");
    let child_args = [
        "serve_config_from_env_reads_variables_and_rejects_missing_endpoint_in_isolated_children",
        "--exact",
        "--nocapture",
    ];

    let configured = Command::new(&executable)
        .args(child_args)
        .env(FROM_ENV_CHILD, "configured")
        .env("CRIKEY_PLUGIN_ENDPOINT", "stdio")
        .env("CRIKEY_SESSION_TOKEN", "env-session")
        .env("CRIKEY_PLUGIN_ID", "env.plugin")
        .status()
        .expect("configured child process");
    assert!(configured.success(), "configured child exited with {configured}");

    let missing_endpoint = Command::new(&executable)
        .args(child_args)
        .env(FROM_ENV_CHILD, "missing-endpoint")
        .env_remove("CRIKEY_PLUGIN_ENDPOINT")
        .env("CRIKEY_SESSION_TOKEN", "env-session")
        .env("CRIKEY_PLUGIN_ID", "env.plugin")
        .status()
        .expect("missing-endpoint child process");
    assert!(
        missing_endpoint.success(),
        "missing-endpoint child exited with {missing_endpoint}"
    );
}

#[test]
fn serve_on_handshakes_and_stops_on_shutdown() {
    let (mut host, plugin_transport) = protocol::transport::pair();
    let join = thread::spawn(move || {
        let mut plugin = RuntimePlugin::new(SuggestMode::Echo);
        serve_on(&mut plugin, plugin_transport, explicit_config())
    });

    let handshake = complete_handshake(&mut host, 8);
    assert_eq!(handshake.plugin_id, "sdk.test");
    assert_eq!(handshake.protocol_version, PROTOCOL_VERSION);
    assert_eq!(handshake.plugin_name, "SDK test plugin");
    assert_eq!(handshake.plugin_version, "1.2.3");

    assert_eq!(handshake.sdk_version, "sdk-test");
    assert_eq!(handshake.session_token, "session-token-for-tests");
    for capability in [
        "streaming_catalog",
        "streaming_suggestions",
        "cancellation",
        "configuration_updates",
        "events",
    ] {
        assert!(
            handshake.capabilities.iter().any(|value| value == capability),
            "missing capability {capability}"
        );
    }

    host.send(&envelope(
        0,
        0,
        Payload::Shutdown(Shutdown {
            immediate: false,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("shutdown frame");
    join.join()
        .expect("serve thread did not panic")
        .expect("shutdown should end serving cleanly");
}
#[test]
fn serve_on_accepts_a_transport_without_read_timeout_support() {
    let timeout_calls = Arc::new(AtomicUsize::new(0));
    let (mut host, plugin_transport) = protocol::transport::pair();
    let plugin_transport = Box::new(NoTimeoutTransport::new(
        plugin_transport,
        Arc::clone(&timeout_calls),
    ));
    let join = thread::spawn(move || {
        let mut plugin = RuntimePlugin::new(SuggestMode::Echo);
        serve_on(&mut plugin, plugin_transport, explicit_config())
    });

    let handshake = complete_handshake(&mut host, 8);
    assert_eq!(handshake.plugin_id, "sdk.test");
    host.send(&envelope(
        0,
        0,
        Payload::Shutdown(Shutdown {
            immediate: false,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("shutdown frame");
    drop(host);
    join.join()
        .expect("serve thread did not panic")
        .expect("unsupported timeout must not abort the handshake");
    assert_eq!(
        timeout_calls.load(Ordering::SeqCst),
        0,
        "the SDK must not request an unsupported optional timeout",
    );
}

#[test]
fn serve_on_reports_reader_timeout_setup_failure() {
    let timeout_calls = Arc::new(AtomicUsize::new(0));
    let (mut host, plugin_transport) = protocol::transport::pair();
    let plugin_transport = Box::new(FailingTimeoutTransport::new(
        plugin_transport,
        Arc::clone(&timeout_calls),
    ));
    let join = thread::spawn(move || {
        let mut plugin = RuntimePlugin::new(SuggestMode::Echo);
        serve_on(&mut plugin, plugin_transport, explicit_config())
    });

    complete_handshake(&mut host, 8);
    let result = join.join().expect("serve thread did not panic");
    assert_eq!(
        result,
        Err(SdkError::Protocol(
            "malformed message: read timeout setup failed".to_owned(),
        )),
        "reader setup failures must reach the plugin caller",
    );
    assert_eq!(timeout_calls.load(Ordering::SeqCst), 3);
}

#[test]
fn serve_on_reports_a_truncated_frame_without_panicking() {
    let (mut host, plugin_transport) = protocol::transport::pair();
    let plugin_transport = Box::new(FaultAfterHandshakeTransport::new(
        plugin_transport,
        "truncated frame body",
    ));
    let join = thread::spawn(move || {
        let mut plugin = RuntimePlugin::new(SuggestMode::Echo);
        serve_on(&mut plugin, plugin_transport, explicit_config())
    });

    complete_handshake(&mut host, 8);
    let result = join.join().expect("serve thread did not panic");
    assert_eq!(
        result,
        Err(SdkError::Protocol(
            "malformed message: truncated frame body".to_owned(),
        )),
        "a truncated frame must be reported to the plugin caller",
    );
}

#[test]
fn serve_on_treats_eof_at_a_frame_boundary_as_clean_shutdown() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&stopped);
    let (mut host, plugin_transport) = protocol::transport::pair();
    let join = thread::spawn(move || {
        let mut plugin = RuntimePlugin::with_stop_observer(SuggestMode::Echo, observed);
        serve_on(&mut plugin, plugin_transport, explicit_config())
    });

    complete_handshake(&mut host, 8);
    drop(host);
    join.join()
        .expect("serve thread did not panic")
        .expect("EOF should end serving cleanly");
    assert_eq!(
        stopped.load(Ordering::SeqCst),
        1,
        "EOF must run the plugin stop callback before returning",
    );
}

#[test]
fn suggestion_results_echo_request_id_and_generation() {
    let (mut host, plugin_transport) = protocol::transport::pair();
    let join = thread::spawn(move || {
        let mut plugin = RuntimePlugin::new(SuggestMode::Echo);
        serve_on(&mut plugin, plugin_transport, explicit_config())
    });

    complete_handshake(&mut host, 8);
    let request_id = 73;
    let generation = 4_291;
    host.send(&envelope(
        request_id,
        generation,
        Payload::Suggest(SuggestRequest {
            text: "open report".to_owned(),
            normalized_text: "open report".to_owned(),
            selected_item_id: String::new(),
            max_items: 100,
            max_batches: 10,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("suggest request");

    let saw_terminal = loop {
        let frame = host.recv().expect("suggest result");
        assert_eq!(frame.request_id, request_id, "request id was not echoed");
        assert_eq!(frame.generation, generation, "generation was not echoed");
        let batch = match frame.payload {
            Some(Payload::Results(batch)) => batch,
            other => panic!("expected result batch, got {other:?}"),
        };
        if batch.state.as_i32() == 2 {
            break true;
        }
    };
    assert!(saw_terminal);

    host.send(&envelope(
        0,
        0,
        Payload::Shutdown(Shutdown {
            immediate: false,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("shutdown frame");
    join.join()
        .expect("serve thread did not panic")
        .expect("serve loop should survive a successful request");
}

#[test]
fn suggestion_sink_streams_one_hundred_items_in_multiple_batches() {
    let (mut host, plugin_transport) = protocol::transport::pair();
    let join = thread::spawn(move || {
        let mut plugin = RuntimePlugin::new(SuggestMode::Stream);
        serve_on(&mut plugin, plugin_transport, explicit_config())
    });

    complete_handshake(&mut host, 16);
    host.send(&envelope(
        8,
        9,
        Payload::Suggest(SuggestRequest {
            text: "stream".to_owned(),
            normalized_text: "stream".to_owned(),
            selected_item_id: String::new(),
            max_items: 100,
            max_batches: 20,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("stream request");

    let mut batches = Vec::new();
    loop {
        let frame = host.recv().expect("streamed result");
        let batch = match frame.payload {
            Some(Payload::Results(batch)) => batch,
            other => panic!("expected result batch, got {other:?}"),
        };
        let terminal = batch.state.as_i32() == 2;
        batches.push(batch);
        if terminal {
            break;
        }
    }

    assert!(batches.len() > 2, "streaming collapsed into too few batches");
    assert_eq!(batches.last().expect("terminal batch").state.as_i32(), 2);
    let items: Vec<_> = batches.iter().flat_map(|batch| batch.items.iter()).collect();
    assert_eq!(items.len(), 100);
    for (index, value) in items.iter().enumerate() {
        assert_eq!(value.stable_id, format!("item-{index}"));
        assert_eq!(value.label, format!("label-{index}"));
    }

    host.send(&envelope(
        0,
        0,
        Payload::Shutdown(Shutdown {
            immediate: false,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("shutdown frame");
    join.join()
        .expect("serve thread did not panic")
        .expect("streaming loop should shut down cleanly");
}
#[test]
fn context_log_reaches_host_with_the_active_request_identity() {
    let (mut host, plugin_transport) = protocol::transport::pair();
    let join = thread::spawn(move || {
        let mut plugin = RuntimePlugin::new(SuggestMode::Log);
        serve_on(&mut plugin, plugin_transport, explicit_config())
    });

    complete_handshake(&mut host, 1);
    let request_id = 31;
    let generation = 32;
    host.send(&envelope(
        request_id,
        generation,
        Payload::Suggest(SuggestRequest {
            text: "log".to_owned(),
            normalized_text: "log".to_owned(),
            selected_item_id: String::new(),
            max_items: 10,
            max_batches: 10,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("log request");

    host.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("configure bounded log observation");
    let log = host.recv().expect("context.log frame");
    host.set_read_timeout(None)
        .expect("restore blocking host receive");
    assert_eq!(log.connection_id, 1);
    assert_eq!(log.request_id, request_id);
    assert_eq!(log.generation, generation);
    let record = match log.payload {
        Some(Payload::Log(record)) => record,
        other => panic!("expected log frame, got {other:?}"),
    };
    assert_eq!(record.level.as_i32(), 3);
    assert_eq!(record.message, "sdk runtime log");

    let terminal = host.recv().expect("log request terminal frame");
    assert!(matches!(
        terminal.payload,
        Some(Payload::Results(batch)) if batch.state.as_i32() == 2
    ));
    host.send(&envelope(
        0,
        0,
        Payload::Shutdown(Shutdown {
            immediate: false,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("shutdown frame");
    join.join()
        .expect("serve thread did not panic")
        .expect("log callback should keep the loop alive");
}

#[test]
fn suggestion_sink_waits_for_flow_control_before_sending_a_second_batch() {
    let progress = Arc::new(AtomicUsize::new(0));
    let (mut host, plugin_transport) = protocol::transport::pair();
    let plugin_progress = Arc::clone(&progress);
    let join = thread::spawn(move || {
        let mut plugin = RuntimePlugin::with_credit_progress(plugin_progress);
        serve_on(&mut plugin, plugin_transport, explicit_config())
    });

    complete_handshake(&mut host, 1);
    let request_id = 12;
    let generation = 13;
    host.send(&envelope(
        request_id,
        generation,
        Payload::Suggest(SuggestRequest {
            text: "credit".to_owned(),
            normalized_text: "credit".to_owned(),
            selected_item_id: String::new(),
            max_items: 10,
            max_batches: 10,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("credit request");

    let first = host.recv().expect("first credited batch");
    // The plugin records progress AFTER `emit_batch` returns, so the frame can
    // legitimately reach the host a moment before the counter moves. Poll for
    // the observation instead of racing it; the load-bearing assertion is the
    // one below, that progress never reaches 2 while credit is withheld.
    assert!(
        poll_until(|| progress.load(Ordering::SeqCst) == 1),
        "the plugin never recorded sending its first batch",
    );
    let first_batch = match first.payload {
        Some(Payload::Results(batch)) => batch,
        other => panic!("expected first result batch, got {other:?}"),
    };
    assert_eq!(first_batch.items.len(), 1);
    assert_eq!(first_batch.items[0].stable_id, "first");

    // A bounded no-message observation is made before granting credit.  This
    // kills an implementation that queues the second batch before waiting.
    host.set_read_timeout(Some(Duration::from_millis(1)))
        .expect("configure bounded no-message observation");
    let observation = host.recv();
    assert!(
        matches!(observation, Err(protocol::ProtocolError::Timeout)),
        "second batch was queued before credit was granted: {observation:?}",
    );
    host.set_read_timeout(None)
        .expect("restore blocking host receive");

    host.send(&envelope(
        request_id,
        generation,
        Payload::Flow(FlowControl {
            credits: 1,
            paused: false,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("credit grant");
    let second = host.recv().expect("second credited batch");
    assert!(
        poll_until(|| progress.load(Ordering::SeqCst) == 2),
        "the plugin never recorded sending its second batch",
    );
    let second_batch = match second.payload {
        Some(Payload::Results(batch)) => batch,
        other => panic!("expected second result batch, got {other:?}"),
    };
    assert_eq!(second_batch.items[0].stable_id, "second");

    host.send(&envelope(
        request_id,
        generation,
        Payload::Flow(FlowControl {
            credits: 1,
            paused: false,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("terminal credit grant");
    let terminal = host.recv().expect("terminal credited batch");
    assert_eq!(
        match terminal.payload {
            Some(Payload::Results(batch)) => batch.state.as_i32(),
            other => panic!("expected terminal result batch, got {other:?}"),
        },
        2
    );

    host.send(&envelope(
        0,
        0,
        Payload::Shutdown(Shutdown {
            immediate: false,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("shutdown frame");
    join.join()
        .expect("serve thread did not panic")
        .expect("credit loop should shut down cleanly");
}

#[test]
fn cancel_frame_marks_sink_cancelled_and_emits_a_cancelled_terminal_batch() {
    let (mut host, plugin_transport) = protocol::transport::pair();
    let join = thread::spawn(move || {
        let mut plugin = RuntimePlugin::new(SuggestMode::Cancel);
        serve_on(&mut plugin, plugin_transport, explicit_config())
    });

    complete_handshake(&mut host, 1);
    let request_id = 91;
    let generation = 92;
    host.send(&envelope(
        request_id,
        generation,
        Payload::Suggest(SuggestRequest {
            text: "cancel".to_owned(),
            normalized_text: "cancel".to_owned(),
            selected_item_id: String::new(),
            max_items: 10,
            max_batches: 10,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("cancel request");

    let first = host.recv().expect("pre-cancel batch");
    assert!(matches!(first.payload, Some(Payload::Results(_))));
    host.send(&envelope(
        request_id,
        generation,
        Payload::Cancel(Cancel {
            reason: "new query superseded this request".to_owned(),
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("cancel frame");
    host.send(&envelope(
        request_id,
        generation,
        Payload::Flow(FlowControl {
            credits: 1,
            paused: false,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("credit for cancellation terminal");

    let terminal = host.recv().expect("cancelled terminal batch");
    let terminal_batch = match terminal.payload {
        Some(Payload::Results(batch)) => batch,
        other => panic!("expected cancelled result batch, got {other:?}"),
    };
    assert_eq!(terminal_batch.state.as_i32(), 3);

    host.send(&envelope(
        0,
        0,
        Payload::Shutdown(Shutdown {
            immediate: false,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("shutdown frame");
    join.join()
        .expect("serve thread did not panic")
        .expect("cancelled request should not kill the loop");
}

#[test]
fn cancelled_partial_batches_are_dropped_instead_of_resent() {
    let (mut host, plugin_transport) = protocol::transport::pair();
    let join = thread::spawn(move || {
        let mut plugin = RuntimePlugin::new(SuggestMode::CancelIgnored);
        serve_on(&mut plugin, plugin_transport, explicit_config())
    });

    complete_handshake(&mut host, 1);
    let request_id = 101;
    let generation = 102;
    host.send(&envelope(
        request_id,
        generation,
        Payload::Suggest(SuggestRequest {
            text: "cancel-ignored".to_owned(),
            normalized_text: "cancel-ignored".to_owned(),
            selected_item_id: String::new(),
            max_items: 10,
            max_batches: 10,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("cancel-ignored request");

    let first = host.recv().expect("pre-cancel batch");
    assert!(matches!(first.payload, Some(Payload::Results(_))));
    host.send(&envelope(
        request_id,
        generation,
        Payload::Cancel(Cancel {
            reason: "superseded".to_owned(),
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("cancel frame");
    host.send(&envelope(
        request_id,
        generation,
        Payload::Flow(FlowControl {
            credits: 1,
            paused: false,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("credit for terminal frame");

    let terminal = host.recv().expect("terminal frame");
    let terminal_batch = match terminal.payload {
        Some(Payload::Results(batch)) => batch,
        other => panic!("expected terminal result batch, got {other:?}"),
    };
    assert_eq!(
        terminal_batch.state.as_i32(),
        2,
        "cancel-ignored callback must send one final terminal frame"
    );
    assert!(
        terminal_batch.items.is_empty(),
        "cancelled partial items must not be replayed in the terminal credit"
    );

    host.send(&envelope(
        0,
        0,
        Payload::Shutdown(Shutdown {
            immediate: false,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("shutdown frame");
    join.join()
        .expect("serve thread did not panic")
        .expect("dropping cancelled partials must keep the loop alive");
}

#[test]
fn suggest_error_emits_failed_batch_and_keeps_the_loop_alive() {
    let (mut host, plugin_transport) = protocol::transport::pair();
    let join = thread::spawn(move || {
        let mut plugin = RuntimePlugin::new(SuggestMode::FailOnce);
        serve_on(&mut plugin, plugin_transport, explicit_config())
    });

    complete_handshake(&mut host, 8);
    let request = |request_id| {
        envelope(
            request_id,
            request_id + 100,
            Payload::Suggest(SuggestRequest {
                text: "failure".to_owned(),
                normalized_text: "failure".to_owned(),
                selected_item_id: String::new(),
                max_items: 10,
                max_batches: 10,
                unknown: UnknownFields::default(),
            }),
        )
    };

    host.send(&request(1)).expect("failing request");
    let failed = host.recv().expect("failed result batch");
    let failed_batch: ResultBatch = match failed.payload {
        Some(Payload::Results(batch)) => batch,
        other => panic!("expected failed result batch, got {other:?}"),
    };
    assert_eq!(failed_batch.state.as_i32(), 4);
    let error = failed_batch.error.expect("failed batch error");
    assert!(!error.message.is_empty());

    host.send(&request(2)).expect("follow-up request");
    // Items arrive on the frames `emit_batch` produced; `finish()` contributes
    // the terminal frame, which is empty when the plugin already flushed its
    // results. Accumulate across frames rather than indexing the terminal one.
    let mut follow_up_items: Vec<String> = Vec::new();
    let saw_follow_up_terminal = loop {
        let frame = host.recv().expect("follow-up result");
        let batch = match frame.payload {
            Some(Payload::Results(batch)) => batch,
            other => panic!("expected follow-up result batch, got {other:?}"),
        };
        follow_up_items.extend(batch.items.iter().map(|item| item.stable_id.clone()));
        if batch.state.as_i32() == 2 {
            break true;
        }
    };
    assert_eq!(
        follow_up_items,
        vec!["reused".to_owned()],
        "the loop must survive one plugin error and serve the next request",
    );
    assert!(saw_follow_up_terminal);

    host.send(&envelope(
        0,
        0,
        Payload::Shutdown(Shutdown {
            immediate: false,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("shutdown frame");
    join.join()
        .expect("serve thread did not panic")
        .expect("plugin failure should not kill the serving loop");
}

#[test]
fn suggest_panic_is_reported_as_failed_batch_and_worker_stays_alive() {
    let (mut host, plugin_transport) = protocol::transport::pair();
    let join = thread::spawn(move || {
        let mut plugin = RuntimePlugin::new(SuggestMode::Panic);
        serve_on(&mut plugin, plugin_transport, explicit_config())
    });
    complete_handshake(&mut host, 8);
    host.send(&envelope(
        200,
        201,
        Payload::Suggest(SuggestRequest {
            text: "panic".to_owned(),
            normalized_text: "panic".to_owned(),
            selected_item_id: String::new(),
            max_items: 10,
            max_batches: 10,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("panic request");
    let failed = host.recv().expect("panic failure batch");
    let batch = match failed.payload {
        Some(Payload::Results(batch)) => batch,
        other => panic!("expected failed result batch, got {other:?}"),
    };
    assert_eq!(batch.state.as_i32(), 4);
    assert!(
        batch
            .error
            .as_ref()
            .is_some_and(|error| error.message.contains("intentional suggest panic")),
        "panic detail should reach the host: {batch:?}"
    );
    host.send(&envelope(
        0,
        0,
        Payload::Shutdown(Shutdown {
            immediate: false,
            unknown: UnknownFields::default(),
        }),
    ))
    .expect("shutdown frame after panic");
    join.join()
        .expect("serve thread did not panic")
        .expect("a callback panic must not kill the serving loop");
}

#[test]
fn item_and_action_builders_apply_fields_and_core_defaults() {
    let default_action = ActionBuilder::new("default", "Default").build();
    assert_eq!(default_action.description, "");
    assert!(default_action.icon_reference.is_none());
    assert!(default_action.applicable_categories.is_empty());
    assert_eq!(
        default_action.execution_policy,
        crikey_core::ExecutionPolicy::Plugin
    );
    let mediated = ActionBuilder::new("mediated", "Mediated").host_mediated().build();
    assert_eq!(
        mediated.execution_policy,
        crikey_core::ExecutionPolicy::HostMediated
    );
    let default_item = ItemBuilder::new("default-stable", "Default").build();
    assert_eq!(default_item.target, "");
    assert_eq!(default_item.description, "");
    assert_eq!(
        default_item.category,
        Category::PluginDefined("plugin-defined".to_owned())
    );
    assert_eq!(default_item.score_hint, 0);
    assert!(default_item.search_terms.is_empty());
    assert!(default_item.metadata.is_empty());
    assert!(default_item.actions.is_empty());
    assert!(default_item.icon_reference.is_none());
    assert_eq!(
        default_item.argument_policy,
        crikey_core::ArgumentPolicy::Forbidden
    );
    assert_eq!(default_item.hit_policy, crikey_core::HitPolicy::Recorded);
    assert!(default_item.plugin_id.0.is_empty());

    let action = ActionBuilder::new("open", "Open")
        .description("Open the selected item")
        .icon("open-icon")
        .build();
    assert_eq!(action.action_id.0, "open");
    assert_eq!(action.label, "Open");
    assert_eq!(action.description, "Open the selected item");
    assert_eq!(action.icon_reference.as_deref(), Some("open-icon"));
    assert!(action.applicable_categories.is_empty());

    let built = ItemBuilder::new("stable", "A label")
        .target("https://example.test")
        .description("An item description")
        .category(Category::Url)
        .score_hint(17)
        .search_term("example")
        .search_term("test")
        .metadata("source", "sdk-test")
        .metadata("kind", "first")
        .metadata("kind", "url")
        .action(action)
        .build();

    assert_eq!(built.stable_id.0, "stable");
    assert_eq!(built.label, "A label");
    assert_eq!(built.target, "https://example.test");
    assert_eq!(built.description, "An item description");
    assert_eq!(built.category, Category::Url);
    assert_eq!(built.score_hint, 17);
    assert_eq!(built.search_terms, vec!["example".to_owned(), "test".to_owned()]);
    assert_eq!(built.metadata.get("source").map(String::as_str), Some("sdk-test"));
    assert_eq!(built.metadata.get("kind").map(String::as_str), Some("url"));
    assert_eq!(built.actions.len(), 1);
    assert_eq!(built.actions[0].action_id.0, "open");
    assert!(built.icon_reference.is_none());
    assert_eq!(built.argument_policy, crikey_core::ArgumentPolicy::Forbidden);
    assert_eq!(built.hit_policy, crikey_core::HitPolicy::Recorded);
    assert!(built.plugin_id.0.is_empty());
}
