use crikey_core::{CoreError, Result};
use crikey_plugin_sdk::{
    harness::TestHarness, CatalogSink, ExecuteRequest, Plugin, PluginContext, PreviewContent, PreviewOutcome,
    PreviewText, Query, ServeConfig, SuggestionSink,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct PreviewPlugin {
    enabled: bool,
    calls: Arc<AtomicUsize>,
}
impl Plugin for PreviewPlugin {
    fn start(&mut self, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn stop(&mut self, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn build_catalog(&mut self, _: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        sink.finish()
    }
    fn suggest(&mut self, _: Query, _: &dyn PluginContext, sink: &mut dyn SuggestionSink) -> Result<()> {
        sink.finish()
    }
    fn execute(&mut self, _: ExecuteRequest, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn supports_preview(&self) -> bool {
        self.enabled
    }
    fn preview(&mut self, item: &str, _: &dyn PluginContext) -> Result<PreviewOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match item {
            "empty" => Ok(PreviewOutcome::Empty),
            "error" => Err(CoreError::Invalid("preview failed".into())),
            "panic" => panic!("preview panic"),
            "oversized" => Ok(PreviewOutcome::Content(PreviewContent {
                text: Some(PreviewText::Plain("x".repeat(256 * 1024 + 1))),
                ..Default::default()
            })),
            _ => Ok(PreviewOutcome::Content(PreviewContent {
                text: Some(PreviewText::Plain(item.into())),
                ..Default::default()
            })),
        }
    }
}

#[test]
fn callback_is_selected_on_demand_and_failures_preserve_runtime() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut harness = TestHarness::start(
        PreviewPlugin {
            enabled: true,
            calls: calls.clone(),
        },
        ServeConfig::new("preview-test", "1.0.0"),
    )
    .unwrap();
    harness.catalog().unwrap();
    harness.suggest("ordinary query").unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let PreviewOutcome::Content(content) = harness.preview("selected", 71).unwrap() else {
        panic!("missing content")
    };
    assert_eq!(content.text, Some(PreviewText::Plain("selected".into())));
    assert_eq!(harness.preview("empty", 72).unwrap(), PreviewOutcome::Empty);
    for item in ["error", "panic", "oversized"] {
        assert!(harness.preview(item, 73).is_err());
    }
    assert!(matches!(
        harness.preview("still alive", 74).unwrap(),
        PreviewOutcome::Content(_)
    ));
    harness.shutdown().unwrap();
}

#[test]
fn absent_capability_never_calls_preview_callback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut harness = TestHarness::start(
        PreviewPlugin {
            enabled: false,
            calls: calls.clone(),
        },
        ServeConfig::new("old-worker", "1.0.0"),
    )
    .unwrap();
    assert_eq!(
        harness.preview("selected", 1).unwrap(),
        PreviewOutcome::Unsupported
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    harness.suggest("ordinary query remains usable").unwrap();
    harness.shutdown().unwrap();
}

#[test]
fn unknown_host_capabilities_do_not_grant_preview() {
    use crikey_plugin_sdk::protocol::{
        message::{Envelope, HandshakeAck, Payload, PreviewRequest, Shutdown},
        transport::pair,
        PROTOCOL_VERSION,
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let plugin_calls = calls.clone();
    let (mut host, transport) = pair();
    host.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let worker = std::thread::spawn(move || {
        let mut config = ServeConfig::new("negotiation", "1.0.0");
        config.session_token = Some("test-session".into());
        crikey_plugin_sdk::serve_on(
            &mut PreviewPlugin {
                enabled: true,
                calls: plugin_calls,
            },
            transport,
            config,
        )
    });
    let Some(Payload::Handshake(handshake)) = host.recv().unwrap().payload else {
        panic!("no handshake")
    };
    assert!(handshake.capabilities.iter().any(|s| s == "preview_v1"));
    host.send(&Envelope {
        connection_id: 1,
        payload: Some(Payload::HandshakeAck(HandshakeAck {
            accepted: true,
            protocol_version: PROTOCOL_VERSION,
            max_frame_bytes: crikey_plugin_sdk::protocol::MAX_FRAME_BYTES as u64,
            host_capabilities: vec!["preview_v999".into()],
            ..Default::default()
        })),
        ..Default::default()
    })
    .unwrap();
    host.send(&Envelope {
        connection_id: 1,
        request_id: 9,
        generation: 17,
        payload: Some(Payload::PreviewRequest(PreviewRequest {
            item_id: "selected".into(),
            ..Default::default()
        })),
        ..Default::default()
    })
    .unwrap();
    let response = host.recv().unwrap();
    assert_eq!((response.request_id, response.generation), (9, 17));
    let Some(Payload::PreviewResponse(response)) = response.payload else {
        panic!("no preview response")
    };
    assert_eq!(response.status, 3);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    host.send(&Envelope {
        connection_id: 1,
        payload: Some(Payload::Shutdown(Shutdown::default())),
        ..Default::default()
    })
    .unwrap();
    worker.join().unwrap().unwrap();
}
