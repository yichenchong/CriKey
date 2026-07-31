//! In-process SDK harness for deterministic plugin tests (spec 16.7, 24.3).

use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

use crikey_core::{Item, PluginId};
use crikey_native_protocol::message::{self, Envelope, HandshakeAck, Payload};
use crikey_native_protocol::transport::Transport;
use crikey_native_protocol::{Capabilities, MAX_FRAME_BYTES, PROTOCOL_VERSION};

use crate::{serve_on, Plugin, SdkError, ServeConfig};

/// Terminal state folded by [`TestHarness::suggest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchStateKind {
    Final,
    Cancelled,
    Failed,
}

/// Results folded across a suggestion stream.
#[derive(Debug, Clone)]
pub struct HarnessSuggestions {
    pub items: Vec<Item>,
    pub state: BatchStateKind,
    /// Counts every result frame, including the terminal frame.
    pub batches: usize,
}

/// Drives an SDK plugin over the same framed in-memory transport used by the
/// native host tests (spec 16.5, 16.7).
#[derive(Debug)]
pub struct TestHarness {
    host: Option<Box<dyn Transport>>,
    worker: Option<JoinHandle<Result<(), SdkError>>>,
    info: crate::HandshakeInfo,
    plugin_id: PluginId,
    next_request: AtomicU64,
    cancel_latched: bool,
}

impl TestHarness {
    /// Starts a plugin and completes the SDK handshake before returning.
    pub fn start(plugin: impl Plugin + Send + 'static, config: ServeConfig) -> Result<Self, SdkError> {
        let (mut host, plugin_transport) = crate::protocol::transport::pair();
        let mut worker_config = config.clone();
        if worker_config.session_token.is_none() {
            worker_config.session_token = Some("sdk-harness-session".to_owned());
        }
        let worker = thread::spawn(move || {
            let mut plugin = plugin;
            serve_on(&mut plugin, plugin_transport, worker_config)
        });
        let handshake = host.recv().map_err(SdkError::from)?;
        let handshake = match handshake.payload {
            Some(Payload::Handshake(handshake)) => handshake,
            Some(payload) => {
                let _ = worker.join();
                return Err(SdkError::Protocol(format!(
                    "expected plugin handshake, got {}",
                    payload.kind()
                )));
            }
            None => {
                let _ = worker.join();
                return Err(SdkError::Protocol("plugin handshake was empty".to_owned()));
            }
        };
        let info = crate::HandshakeInfo {
            plugin_id: handshake.plugin_id.clone(),
            plugin_name: handshake.plugin_name,
            plugin_version: handshake.plugin_version,
            sdk_version: handshake.sdk_version,
            protocol_version: handshake.protocol_version,
            capabilities: capabilities_from_names(&handshake.capabilities),
        };
        host.send(&Envelope {
            connection_id: 1,
            request_id: 0,
            generation: 0,
            deadline_ms: 0,
            payload: Some(Payload::HandshakeAck(HandshakeAck {
                protocol_version: PROTOCOL_VERSION,
                host_capabilities: vec!["streaming".to_owned(), "cancellation".to_owned()],
                host_version: "sdk-test-harness".to_owned(),
                accepted: true,
                reject_reason: String::new(),
                max_frame_bytes: MAX_FRAME_BYTES as u64,
                initial_credits: 8,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        })
        .map_err(SdkError::from)?;
        Ok(Self {
            host: Some(host),
            worker: Some(worker),
            plugin_id: PluginId(info.plugin_id.clone()),
            info,
            next_request: AtomicU64::new(1),
            cancel_latched: false,
        })
    }

    pub fn handshake(&self) -> &crate::HandshakeInfo {
        &self.info
    }

    /// Requests and folds the complete plugin catalog.
    pub fn catalog(&mut self) -> Result<Vec<Item>, SdkError> {
        let request_id = self.next_request();
        self.send(Envelope {
            connection_id: 1,
            request_id,
            generation: 0,
            deadline_ms: 0,
            payload: Some(Payload::CatalogRequest(message::CatalogRequest {
                max_items: 0,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        })?;
        let mut items = Vec::new();
        loop {
            let envelope = self.recv()?;
            match envelope.payload {
                Some(Payload::CatalogBatch(batch)) => {
                    if let Some(error) = batch.error {
                        return Err(SdkError::Protocol(format!("catalog failed: {}", error.message)));
                    }
                    items.extend(
                        batch.items.iter().map(|item| {
                            crikey_native_protocol::convert::from_proto_item(&self.plugin_id, item)
                        }),
                    );
                    self.grant_credit(request_id, 0)?;
                    if batch.done {
                        return Ok(items);
                    }
                }
                Some(Payload::Error(error)) => {
                    return Err(SdkError::Protocol(error.message));
                }
                Some(payload) => {
                    return Err(SdkError::Protocol(format!(
                        "expected catalog batch, got {}",
                        payload.kind()
                    )))
                }
                None => return Err(SdkError::Protocol("empty catalog response".to_owned())),
            }
        }
    }

    /// Requests suggestions and clears any previously latched cancellation.
    pub fn suggest(&mut self, text: &str) -> Result<HarnessSuggestions, SdkError> {
        self.cancel_latched = false;
        self.suggest_inner(text, false)
    }

    /// Requests suggestions while preserving the cancellation latch set by
    /// [`TestHarness::cancel`] (contract §11.10).
    pub fn suggest_with_cancel_latched(&mut self, text: &str) -> Result<HarnessSuggestions, SdkError> {
        self.suggest_inner(text, true)
    }

    /// Latches cancellation for the next explicit cancelled suggestion call.
    pub fn cancel(&mut self) {
        self.cancel_latched = true;
    }

    pub fn execute(
        &mut self,
        item: &str,
        action: Option<&str>,
        argument: Option<&str>,
    ) -> Result<(), SdkError> {
        let request_id = self.next_request();
        self.send(Envelope {
            connection_id: 1,
            request_id,
            generation: 0,
            deadline_ms: 0,
            payload: Some(Payload::Execute(message::ExecuteRequest {
                item_id: item.to_owned(),
                action_id: action.unwrap_or_default().to_owned(),
                argument: argument.unwrap_or_default().to_owned(),
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        })?;
        let envelope = self.recv()?;
        match envelope.payload {
            Some(Payload::ExecuteResult(result)) if result.outcome.as_i32() == 1 => Ok(()),
            Some(Payload::ExecuteResult(result)) => {
                let detail = result
                    .error
                    .map(|error| error.message)
                    .unwrap_or_else(|| "plugin execution failed".to_owned());
                Err(SdkError::Protocol(detail))
            }
            Some(payload) => Err(SdkError::Protocol(format!(
                "expected execute result, got {}",
                payload.kind()
            ))),
            None => Err(SdkError::Protocol("empty execute response".to_owned())),
        }
    }

    /// Stops the plugin and joins its serving thread.
    pub fn shutdown(mut self) {
        if let Some(host) = self.host.as_mut() {
            let _ = host.send(&Envelope {
                connection_id: 1,
                request_id: 0,
                generation: 0,
                deadline_ms: 0,
                payload: Some(Payload::Shutdown(message::Shutdown {
                    immediate: false,
                    unknown: Default::default(),
                })),
                unknown: Default::default(),
            });
        }
        self.join_before_close();
    }

    fn suggest_inner(&mut self, text: &str, preserve_latch: bool) -> Result<HarnessSuggestions, SdkError> {
        let request_id = self.next_request();
        let generation = request_id;
        if preserve_latch && self.cancel_latched {
            self.send(Envelope {
                connection_id: 1,
                request_id,
                generation,
                deadline_ms: 0,
                payload: Some(Payload::Cancel(message::Cancel {
                    reason: "harness cancellation".to_owned(),
                    unknown: Default::default(),
                })),
                unknown: Default::default(),
            })?;
        }
        self.send(Envelope {
            connection_id: 1,
            request_id,
            generation,
            deadline_ms: 0,
            payload: Some(Payload::Suggest(message::SuggestRequest {
                text: text.to_owned(),
                normalized_text: text.trim().to_lowercase(),
                selected_item_id: String::new(),
                max_items: 0,
                max_batches: 0,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        })?;
        let mut items = Vec::new();
        let mut batches = 0;
        loop {
            let envelope = self.recv()?;
            match envelope.payload {
                Some(Payload::Results(batch)) => {
                    batches += 1;
                    items.extend(
                        batch.items.iter().map(|item| {
                            crikey_native_protocol::convert::from_proto_item(&self.plugin_id, item)
                        }),
                    );
                    let terminal_state = match batch.state.as_i32() {
                        1 => None,
                        2 => Some(BatchStateKind::Final),
                        3 => Some(BatchStateKind::Cancelled),
                        4 => Some(BatchStateKind::Failed),
                        state => {
                            return Err(SdkError::Protocol(format!(
                                "invalid suggestion batch state {state}"
                            )));
                        }
                    };
                    self.grant_credit(request_id, generation)?;
                    if let Some(state) = terminal_state {
                        return Ok(HarnessSuggestions {
                            items,
                            state,
                            batches,
                        });
                    }
                }
                Some(Payload::Error(error)) => return Err(SdkError::Protocol(error.message)),
                Some(payload) => {
                    return Err(SdkError::Protocol(format!(
                        "expected result batch, got {}",
                        payload.kind()
                    )))
                }
                None => return Err(SdkError::Protocol("empty suggestion response".to_owned())),
            }
        }
    }

    fn next_request(&self) -> u64 {
        self.next_request.fetch_add(1, Ordering::Relaxed)
    }

    fn send(&mut self, envelope: Envelope) -> Result<(), SdkError> {
        self.host
            .as_mut()
            .ok_or_else(|| SdkError::Transport("harness is shut down".to_owned()))?
            .send(&envelope)
            .map_err(SdkError::from)
    }

    fn recv(&mut self) -> Result<Envelope, SdkError> {
        self.host
            .as_mut()
            .ok_or_else(|| SdkError::Transport("harness is shut down".to_owned()))?
            .recv()
            .map_err(SdkError::from)
    }

    fn grant_credit(&mut self, request_id: u64, generation: u64) -> Result<(), SdkError> {
        self.send(Envelope {
            connection_id: 1,
            request_id,
            generation,
            deadline_ms: 0,
            payload: Some(Payload::Flow(message::FlowControl {
                credits: 1,
                paused: false,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        })
    }

    fn close_and_join(&mut self) {
        if let Some(host) = self.host.take() {
            drop(host);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    fn join_before_close(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(host) = self.host.take() {
            drop(host);
        }
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        self.close_and_join();
    }
}

fn capabilities_from_names(names: &[String]) -> Capabilities {
    Capabilities {
        streaming_catalog: names.iter().any(|name| name == "streaming_catalog"),
        streaming_suggestions: names.iter().any(|name| name == "streaming_suggestions"),
        cancellation: names.iter().any(|name| name == "cancellation"),
        configuration_updates: names.iter().any(|name| name == "configuration_updates"),
        events: names.iter().any(|name| name == "events"),
    }
}
