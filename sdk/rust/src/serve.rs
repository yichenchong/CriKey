//! Handshake and serving loop for native plugins (spec 16.3, 16.5, 16.7).
//!
//! The loop keeps protocol I/O in a small reader thread.  This is important for
//! cooperative cancellation: a plugin may be polling its context while it is
//! not currently inside a sink call, so a blocking transport read cannot be
//! allowed to starve the cancellation signal.

use std::collections::VecDeque;
use std::env;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crikey_core::{ActionId, CoreError, Item, ItemId, PluginId, Result};
use crikey_native_protocol::message::{
    self, BatchState, Envelope, ErrorCode, EventKind, ExecuteOutcomeCode, Payload, StructuredError,
};
use crikey_native_protocol::transport::Transport;
use crikey_native_protocol::{Capabilities, Endpoint, ProtocolError, RequestId, PROTOCOL_VERSION};

use crate::{
    CancellationToken, CatalogSink, ExecuteRequest, LogLevel, Plugin, PluginContext, PluginEvent, Query,
    SdkError, SuggestionSink,
};

/// Maximum number of decoded control frames retained while a plugin callback
/// is busy.  A full queue deliberately blocks the reader, which pauses IPC
/// reads rather than allowing unbounded memory growth (spec 12.4).
const READER_QUEUE_CAPACITY: usize = 64;
const MAX_LOG_RECORDS_PER_REQUEST: u32 = 256;
const MAX_LOG_BYTES_PER_REQUEST: usize = 64 * 1024;
const MAX_LOG_MESSAGE_BYTES: usize = 8 * 1024;

/// Connection and identity values advertised by a plugin (spec 16.3, 16.6).
#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub plugin_id: String,
    pub plugin_name: String,
    pub plugin_version: String,
    pub sdk_version: String,
    pub capabilities: Capabilities,
    pub endpoint: Option<Endpoint>,
    pub session_token: Option<String>,
}

impl ServeConfig {
    /// Creates an explicit configuration.  The endpoint and session token are
    /// filled from the host environment when [`serve`] is called.
    pub fn new(plugin_id: &str, plugin_version: &str) -> Self {
        Self {
            plugin_id: plugin_id.to_owned(),
            plugin_name: plugin_id.to_owned(),
            plugin_version: plugin_version.to_owned(),
            sdk_version: env!("CARGO_PKG_VERSION").to_owned(),
            capabilities: Capabilities::default(),
            endpoint: None,
            session_token: None,
        }
    }

    /// Reads the endpoint, session token and optional host-selected plugin id
    /// from `CRIKEY_*` variables (spec 16.6).
    pub fn from_env(plugin_id: &str, plugin_version: &str) -> Result<Self, SdkError> {
        let endpoint_spec = env::var(crikey_native_protocol::ENV_ENDPOINT)
            .map_err(|_| SdkError::Config(format!("{} is not set", crikey_native_protocol::ENV_ENDPOINT)))?;
        let endpoint = Endpoint::parse(&endpoint_spec)
            .map_err(|error| SdkError::Config(format!("invalid plugin endpoint: {error}")))?;
        let session_token = env::var(crikey_native_protocol::ENV_SESSION_TOKEN).map_err(|_| {
            SdkError::Config(format!(
                "{} is not set",
                crikey_native_protocol::ENV_SESSION_TOKEN
            ))
        })?;
        if let Ok(version) = env::var(crikey_native_protocol::ENV_PROTOCOL_VERSION) {
            let version = version.parse::<u32>().map_err(|_| {
                SdkError::Config(format!(
                    "{} is not a protocol version",
                    crikey_native_protocol::ENV_PROTOCOL_VERSION
                ))
            })?;
            if version != PROTOCOL_VERSION {
                return Err(SdkError::Config(format!(
                    "unsupported protocol version {version}"
                )));
            }
        }
        let selected_id = env::var(crikey_native_protocol::ENV_PLUGIN_ID)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| plugin_id.to_owned());
        Ok(Self {
            plugin_id: selected_id.clone(),
            plugin_name: selected_id,
            plugin_version: plugin_version.to_owned(),
            sdk_version: env!("CARGO_PKG_VERSION").to_owned(),
            capabilities: Capabilities::default(),
            endpoint: Some(endpoint),
            session_token: Some(session_token),
        })
    }
}

/// Identity returned by [`crate::harness::TestHarness::handshake`].
#[derive(Debug, Clone)]
pub struct HandshakeInfo {
    pub plugin_id: String,
    pub plugin_name: String,
    pub plugin_version: String,
    pub sdk_version: String,
    pub protocol_version: u32,
    pub capabilities: Capabilities,
}

/// Connects to the configured endpoint, negotiates the protocol, and serves
/// until `Shutdown` or a clean EOF (spec 16.3, 16.7).
pub fn serve(plugin: &mut dyn Plugin, mut config: ServeConfig) -> Result<(), SdkError> {
    if config.endpoint.is_none() {
        let endpoint = env::var(crikey_native_protocol::ENV_ENDPOINT)
            .map_err(|_| SdkError::Config(format!("{} is not set", crikey_native_protocol::ENV_ENDPOINT)))?;
        config.endpoint = Some(
            Endpoint::parse(&endpoint)
                .map_err(|error| SdkError::Config(format!("invalid plugin endpoint: {error}")))?,
        );
    }
    if config.session_token.is_none() {
        config.session_token = Some(env::var(crikey_native_protocol::ENV_SESSION_TOKEN).map_err(|_| {
            SdkError::Config(format!(
                "{} is not set",
                crikey_native_protocol::ENV_SESSION_TOKEN
            ))
        })?);
    }
    let endpoint = config
        .endpoint
        .clone()
        .ok_or_else(|| SdkError::Config("plugin endpoint is missing".to_owned()))?;
    let transport = match endpoint {
        Endpoint::Stdio => crikey_native_protocol::transport::stdio(),
        endpoint => crikey_native_protocol::transport::connect(&endpoint, None).map_err(SdkError::from)?,
    };
    serve_on(plugin, transport, config)
}

/// Runs the serving loop over an already-established transport.  The harness
/// uses this form to drive a plugin in-process (spec 16.7).
pub fn serve_on(
    plugin: &mut dyn Plugin,
    transport: Box<dyn Transport>,
    config: ServeConfig,
) -> Result<(), SdkError> {
    let session_token = match config.session_token.clone() {
        Some(token) => token,
        None => env::var(crikey_native_protocol::ENV_SESSION_TOKEN).map_err(|_| {
            SdkError::Config(format!(
                "{} is not set",
                crikey_native_protocol::ENV_SESSION_TOKEN
            ))
        })?,
    };
    let mut transport = transport;
    let (ack, connection_id) = handshake(&mut *transport, &config, &session_token)?;
    let reader_transport = transport.try_clone_handle().ok();
    let transport = Arc::new(Mutex::new(transport));
    let metrics = Arc::new(RuntimeMetrics::default());
    let cancellation = Arc::new(CancellationState::default());
    let reader_stop = Arc::new(AtomicBool::new(false));
    let (reader_tx, reader_rx) = mpsc::sync_channel(READER_QUEUE_CAPACITY);
    let reader = spawn_reader(
        Arc::clone(&transport),
        reader_transport,
        reader_tx,
        Arc::clone(&cancellation),
        Arc::clone(&reader_stop),
        Arc::clone(&metrics),
    );
    let mut input = RuntimeInput::new(reader_rx, Arc::clone(&metrics));
    let context = RuntimeContext::new(
        PluginId(config.plugin_id.clone()),
        Arc::clone(&cancellation),
        Arc::clone(&transport),
        connection_id,
        0,
        0,
    );

    if let Err(error) = plugin.start(&context) {
        stop_reader(&reader_stop, reader);
        return Err(SdkError::Protocol(format!("plugin start failed: {error}")));
    }
    if let Some(error) = context.take_log_error() {
        stop_reader(&reader_stop, reader);
        return Err(error);
    }

    let mut credits = CreditState {
        remaining: ack.initial_credits,
        paused: false,
    };
    let result = serve_requests(
        plugin,
        &transport,
        &mut input,
        &cancellation,
        &mut credits,
        &context,
        &metrics,
    );
    stop_reader(&reader_stop, reader);
    result
}

fn handshake(
    transport: &mut dyn Transport,
    config: &ServeConfig,
    session_token: &str,
) -> Result<(message::HandshakeAck, u64), SdkError> {
    let handshake = message::Handshake {
        protocol_version: PROTOCOL_VERSION,
        plugin_id: config.plugin_id.clone(),
        plugin_version: config.plugin_version.clone(),
        capabilities: capability_names(&config.capabilities),
        session_token: session_token.to_owned(),
        plugin_name: config.plugin_name.clone(),
        sdk_version: config.sdk_version.clone(),
        unknown: Default::default(),
    };
    send_direct(
        transport,
        Envelope {
            connection_id: 0,
            request_id: 0,
            generation: 0,
            deadline_ms: 0,
            payload: Some(Payload::Handshake(handshake)),
            unknown: Default::default(),
        },
    )?;
    let response = transport.recv().map_err(SdkError::from)?;
    let connection_id = response.connection_id;
    let ack = match response.payload {
        Some(Payload::HandshakeAck(ack)) => ack,
        Some(other) => {
            return Err(SdkError::Protocol(format!(
                "expected handshake acknowledgement, got {}",
                other.kind()
            )))
        }
        None => {
            return Err(SdkError::Protocol(
                "handshake acknowledgement was empty".to_owned(),
            ))
        }
    };
    if !ack.accepted {
        let reason = if ack.reject_reason.is_empty() {
            "host rejected the plugin".to_owned()
        } else {
            ack.reject_reason
        };
        return Err(SdkError::Rejected(reason));
    }
    if connection_id == 0 {
        return Err(SdkError::Protocol(
            "host handshake acknowledgement omitted connection id".to_owned(),
        ));
    }
    if ack.protocol_version != PROTOCOL_VERSION {
        return Err(SdkError::Protocol(format!(
            "host selected unsupported protocol version {}",
            ack.protocol_version
        )));
    }
    Ok((ack, connection_id))
}

fn capability_names(capabilities: &Capabilities) -> Vec<String> {
    let mut names = Vec::new();
    if capabilities.streaming_catalog {
        names.push("streaming_catalog".to_owned());
    }
    if capabilities.streaming_suggestions {
        names.push("streaming_suggestions".to_owned());
    }
    if capabilities.cancellation {
        names.push("cancellation".to_owned());
    }
    if capabilities.configuration_updates {
        names.push("configuration_updates".to_owned());
    }
    if capabilities.events {
        names.push("events".to_owned());
    }
    names
}

fn send_direct(transport: &mut dyn Transport, envelope: Envelope) -> Result<(), SdkError> {
    transport.send(&envelope).map_err(SdkError::from)
}

#[derive(Debug)]
enum ReadEvent {
    Envelope(Box<Envelope>),
    Closed,
    Failed(Box<SdkError>),
}

enum ReaderTransport {
    Independent(Box<dyn Transport>),
    Shared(Arc<Mutex<Box<dyn Transport>>>),
}

#[derive(Debug, Default)]
struct RuntimeMetrics {
    queued: AtomicU32,
    in_flight: AtomicU32,
}

impl RuntimeMetrics {
    fn enter(&self) -> InFlightGuard<'_> {
        self.in_flight.store(1, Ordering::Release);
        InFlightGuard {
            metric: &self.in_flight,
        }
    }

    fn queue_depth(&self, pending: usize) -> u32 {
        let queued = self.queued.load(Ordering::Acquire);
        let pending = u32::try_from(pending).unwrap_or(u32::MAX);
        queued.saturating_add(pending)
    }
}

struct InFlightGuard<'a> {
    metric: &'a AtomicU32,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.metric.store(0, Ordering::Release);
    }
}

fn spawn_reader(
    writer_transport: Arc<Mutex<Box<dyn Transport>>>,
    reader_transport: Option<Box<dyn Transport>>,
    sender: mpsc::SyncSender<ReadEvent>,
    cancellation: Arc<CancellationState>,
    stop: Arc<AtomicBool>,
    metrics: Arc<RuntimeMetrics>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        // Prefer an independent read handle so a blocking recv can never hold
        // the writer mutex.  The shared fallback is retained for third-party
        // transports that predate `try_clone_handle`; those transports must
        // provide a finite read timeout themselves.
        let mut reader = match reader_transport {
            Some(reader) => ReaderTransport::Independent(reader),
            None => ReaderTransport::Shared(writer_transport),
        };
        match &mut reader {
            ReaderTransport::Independent(transport) => {
                let _ = transport.set_read_timeout(Some(Duration::from_millis(10)));
            }
            ReaderTransport::Shared(transport) => {
                if let Ok(mut locked) = transport.lock() {
                    let _ = locked.set_read_timeout(Some(Duration::from_millis(10)));
                }
            }
        }
        while !stop.load(Ordering::Acquire) {
            let received = match &mut reader {
                ReaderTransport::Independent(transport) => transport.recv(),
                ReaderTransport::Shared(transport) => match transport.lock() {
                    Ok(mut locked) => locked.recv(),
                    Err(_) => {
                        let _ = sender.send(ReadEvent::Failed(Box::new(SdkError::Transport(
                            "transport lock poisoned".to_owned(),
                        ))));
                        break;
                    }
                },
            };
            match received {
                Ok(envelope) => {
                    if let Some(Payload::Cancel(_)) = envelope.payload {
                        cancellation.cancel((envelope.request_id, envelope.generation));
                    }
                    metrics.queued.fetch_add(1, Ordering::AcqRel);
                    if sender.send(ReadEvent::Envelope(Box::new(envelope))).is_err() {
                        metrics.queued.fetch_sub(1, Ordering::AcqRel);
                        break;
                    }
                }
                Err(ProtocolError::Timeout) => continue,
                Err(ProtocolError::Closed) => {
                    let _ = sender.send(ReadEvent::Closed);
                    break;
                }
                Err(error) => {
                    let _ = sender.send(ReadEvent::Failed(Box::new(SdkError::from(error))));
                    break;
                }
            }
        }
    })
}

fn stop_reader(stop: &Arc<AtomicBool>, reader: JoinHandle<()>) {
    stop.store(true, Ordering::Release);
    // The reader may be blocked in inherited stdio, which has no portable
    // read-timeout operation.  It is deliberately detached; process teardown
    // reaps it, while a normal EOF/Closed frame lets it finish by itself.
    drop(reader);
}

#[derive(Debug)]
struct RuntimeInput {
    receiver: mpsc::Receiver<ReadEvent>,
    metrics: Arc<RuntimeMetrics>,
    pending: VecDeque<Envelope>,
    closed: bool,
    shutdown: bool,
}

impl RuntimeInput {
    fn new(receiver: mpsc::Receiver<ReadEvent>, metrics: Arc<RuntimeMetrics>) -> Self {
        Self {
            receiver,
            metrics,
            pending: VecDeque::new(),
            closed: false,
            shutdown: false,
        }
    }

    fn queue_depth(&self) -> u32 {
        self.metrics.queue_depth(self.pending.len())
    }

    fn next(&mut self) -> Result<Envelope, SdkError> {
        if let Some(envelope) = self.pending.pop_front() {
            return Ok(envelope);
        }
        self.next_raw()
    }

    fn next_raw(&mut self) -> Result<Envelope, SdkError> {
        match self.receiver.recv() {
            Ok(ReadEvent::Envelope(envelope)) => {
                self.metrics.queued.fetch_sub(1, Ordering::AcqRel);
                Ok(*envelope)
            }
            Ok(ReadEvent::Closed) => {
                self.closed = true;
                Err(SdkError::Transport("connection closed".to_owned()))
            }
            Ok(ReadEvent::Failed(error)) => Err(*error),
            Err(_) => {
                self.closed = true;
                Err(SdkError::Transport("connection closed".to_owned()))
            }
        }
    }

    fn stash(&mut self, envelope: Envelope) {
        self.pending.push_back(envelope);
    }
    fn mark_shutdown(&mut self, envelope: Envelope) {
        self.shutdown = true;
        self.stash(envelope);
    }
}

#[derive(Debug, Default)]
struct CreditState {
    remaining: u32,
    paused: bool,
}

impl CreditState {
    fn grant(&mut self, flow: &message::FlowControl) {
        self.remaining = self.remaining.saturating_add(flow.credits);
        self.paused = flow.paused;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CancellationKey {
    request_id: u64,
    generation: u64,
}

#[derive(Debug, Default)]
struct CancellationState {
    active: Mutex<ActiveCancellation>,
}

#[derive(Debug, Default)]
struct ActiveCancellation {
    current: Option<CancellationKey>,
    cancelled: bool,
    observed: bool,
    pending: VecDeque<CancellationKey>,
}

impl CancellationState {
    fn begin(&self, key: CancellationKey) {
        if let Ok(mut active) = self.active.lock() {
            active.current = Some(key);
            active.cancelled = false;
            active.observed = false;
            if let Some(position) = active.pending.iter().position(|pending| *pending == key) {
                active.pending.remove(position);
                active.cancelled = true;
            }
        }
    }

    fn end(&self) {
        if let Ok(mut active) = self.active.lock() {
            active.current = None;
            active.cancelled = false;
            active.observed = false;
        }
    }

    fn cancel(&self, key: (u64, u64)) {
        let key = CancellationKey {
            request_id: key.0,
            generation: key.1,
        };
        if let Ok(mut active) = self.active.lock() {
            if active.current == Some(key) || (key.request_id == 0 && key.generation == 0) {
                active.cancelled = true;
            } else if active.pending.len() < 256 {
                active.pending.push_back(key);
            }
        }
    }

    fn is_cancelled(&self) -> bool {
        self.active
            .lock()
            .map(|mut active| {
                if active.cancelled {
                    active.observed = true;
                }
                active.cancelled
            })
            .unwrap_or(true)
    }

    fn cancellation_flag(&self) -> bool {
        self.active.lock().map(|active| active.cancelled).unwrap_or(true)
    }

    fn was_observed(&self) -> bool {
        self.active.lock().map(|active| active.observed).unwrap_or(true)
    }
}

#[derive(Debug, Clone)]
struct RuntimeCancellation {
    state: Arc<CancellationState>,
}

impl CancellationToken for RuntimeCancellation {
    fn is_cancelled(&self) -> bool {
        self.state.is_cancelled()
    }
}

#[derive(Debug, Default)]
struct LogState {
    records: u32,
    bytes: usize,
    error: Option<SdkError>,
}

#[derive(Debug)]
struct RuntimeContext {
    plugin_id: PluginId,
    cancellation: RuntimeCancellation,
    transport: Arc<Mutex<Box<dyn Transport>>>,
    connection_id: u64,
    request_id: u64,
    generation: u64,
    log_state: Arc<Mutex<LogState>>,
}

impl RuntimeContext {
    fn new(
        plugin_id: PluginId,
        state: Arc<CancellationState>,
        transport: Arc<Mutex<Box<dyn Transport>>>,
        connection_id: u64,
        request_id: u64,
        generation: u64,
    ) -> Self {
        Self {
            plugin_id,
            cancellation: RuntimeCancellation { state },
            transport,
            connection_id,
            request_id,
            generation,
            log_state: Arc::new(Mutex::new(LogState::default())),
        }
    }

    fn for_request(&self, request_id: u64, generation: u64) -> Self {
        Self {
            plugin_id: self.plugin_id.clone(),
            cancellation: self.cancellation.clone(),
            transport: Arc::clone(&self.transport),
            connection_id: self.connection_id,
            request_id,
            generation,
            log_state: Arc::new(Mutex::new(LogState::default())),
        }
    }

    fn take_log_error(&self) -> Option<SdkError> {
        self.log_state.lock().ok()?.error.take()
    }
}

fn bounded_log_message(message: &str) -> String {
    if message.len() <= MAX_LOG_MESSAGE_BYTES {
        return message.to_owned();
    }
    let mut end = MAX_LOG_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    message[..end].to_owned()
}

fn protocol_log_level(level: LogLevel) -> message::LogLevel {
    message::LogLevel::from_i32(match level {
        LogLevel::Error => 1,
        LogLevel::Warn => 2,
        LogLevel::Info => 3,
        LogLevel::Debug => 4,
        LogLevel::Trace => 5,
    })
}

impl PluginContext for RuntimeContext {
    fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }

    fn cancellation(&self) -> &dyn CancellationToken {
        &self.cancellation
    }

    fn log(&self, level: LogLevel, message: &str) {
        let message = bounded_log_message(message);
        let Ok(mut state) = self.log_state.lock() else {
            return;
        };
        if state.records >= MAX_LOG_RECORDS_PER_REQUEST
            || state.bytes.saturating_add(message.len()) > MAX_LOG_BYTES_PER_REQUEST
        {
            return;
        }
        state.records = state.records.saturating_add(1);
        state.bytes = state.bytes.saturating_add(message.len());
        drop(state);

        let envelope = Envelope {
            connection_id: self.connection_id,
            request_id: self.request_id,
            generation: self.generation,
            deadline_ms: 0,
            payload: Some(Payload::Log(message::LogRecord {
                level: protocol_log_level(level),
                message,
                timestamp_ms: 0,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        };
        let result = match self.transport.lock() {
            Ok(mut transport) => transport.send(&envelope).map_err(SdkError::from),
            Err(_) => Err(SdkError::Transport("transport lock poisoned".to_owned())),
        };
        if let Err(error) = result {
            if let Ok(mut state) = self.log_state.lock() {
                if state.error.is_none() {
                    state.error = Some(error);
                }
            }
        }
    }
}

fn serve_requests(
    plugin: &mut dyn Plugin,
    transport: &Arc<Mutex<Box<dyn Transport>>>,
    input: &mut RuntimeInput,
    cancellation: &Arc<CancellationState>,
    credits: &mut CreditState,
    context: &RuntimeContext,
    metrics: &RuntimeMetrics,
) -> Result<(), SdkError> {
    loop {
        let envelope = match input.next() {
            Ok(envelope) => envelope,
            Err(_error) if input.closed => return Ok(()),
            Err(error) => return Err(error),
        };
        let request_id = envelope.request_id;
        let generation = envelope.generation;
        let deadline_ms = envelope.deadline_ms;
        let payload = match envelope.payload {
            Some(payload) => payload,
            None => continue,
        };
        let request_context = context.for_request(request_id, generation);
        match payload {
            Payload::Shutdown(_) => {
                let _in_flight = metrics.enter();
                plugin
                    .stop(&request_context)
                    .map_err(|error| SdkError::Protocol(format!("plugin stop failed: {error}")))?;
                if let Some(error) = request_context.take_log_error() {
                    return Err(error);
                }
                return Ok(());
            }
            Payload::Flow(flow) => credits.grant(&flow),
            Payload::Cancel(_) => {
                cancellation.cancel((request_id, generation));
            }
            Payload::Suggest(request) => {
                let mut runtime = RequestRuntime {
                    transport,
                    input,
                    cancellation,
                    credits,
                    context: &request_context,
                    metrics,
                };
                handle_suggest(plugin, request, request_id, generation, deadline_ms, &mut runtime)?;
                if let Some(error) = request_context.take_log_error() {
                    return Err(error);
                }
            }
            Payload::CatalogRequest(request) => {
                let mut runtime = RequestRuntime {
                    transport,
                    input,
                    cancellation,
                    credits,
                    context: &request_context,
                    metrics,
                };
                handle_catalog(plugin, request_id, generation, request, &mut runtime)?;
                if let Some(error) = request_context.take_log_error() {
                    return Err(error);
                }
            }
            Payload::Execute(request) => {
                handle_execute(
                    plugin,
                    request,
                    request_id,
                    generation,
                    transport,
                    &request_context,
                    metrics,
                )?;
                if let Some(error) = request_context.take_log_error() {
                    return Err(error);
                }
            }
            Payload::Configuration(configuration) => {
                let callback_result = {
                    let _in_flight = metrics.enter();
                    plugin.on_configuration(&configuration.values, &request_context)
                };
                if let Some(error) = request_context.take_log_error() {
                    return Err(error);
                }
                if let Err(error) = callback_result {
                    send_error(
                        transport,
                        request_context.connection_id,
                        request_id,
                        generation,
                        &format!("configuration callback failed: {error}"),
                    )?;
                }
            }
            Payload::Event(event) => {
                let plugin_event = PluginEvent {
                    kind: EventKind::from_i32(event.kind.as_i32()),
                    attributes: event.attributes,
                    flags: event.flags,
                };
                let callback_result = {
                    let _in_flight = metrics.enter();
                    plugin.on_event(&plugin_event, &request_context)
                };
                if let Some(error) = request_context.take_log_error() {
                    return Err(error);
                }
                if let Err(error) = callback_result {
                    send_error(
                        transport,
                        request_context.connection_id,
                        request_id,
                        generation,
                        &format!("event callback failed: {error}"),
                    )?;
                }
            }
            Payload::Lifecycle(lifecycle) => {
                let callback = {
                    let _in_flight = metrics.enter();
                    match lifecycle.kind.as_i32() {
                        1 => plugin.start(&request_context),
                        2 => plugin.stop(&request_context),
                        3 => plugin.on_activated(&request_context),
                        4 => plugin.on_deactivated(&request_context),
                        _ => Err(CoreError::Invalid("unsupported lifecycle kind".to_owned())),
                    }
                };
                let (ok, error) = match callback {
                    Ok(()) => (true, None),
                    Err(error) => (false, Some(structured_error(&error.to_string(), request_id))),
                };
                if let Some(log_error) = request_context.take_log_error() {
                    return Err(log_error);
                }
                send_envelope(
                    transport,
                    Envelope {
                        connection_id: request_context.connection_id,
                        request_id,
                        generation,
                        deadline_ms: 0,
                        payload: Some(Payload::LifecycleAck(message::LifecycleAck {
                            kind: lifecycle.kind,
                            ok,
                            error,
                            unknown: Default::default(),
                        })),
                        unknown: Default::default(),
                    },
                )?;
            }
            Payload::HealthCheck(check) => {
                send_envelope(
                    transport,
                    Envelope {
                        connection_id: request_context.connection_id,
                        request_id,
                        generation,
                        deadline_ms: 0,
                        payload: Some(Payload::HealthReport(message::HealthReport {
                            nonce: check.nonce,
                            healthy: true,
                            // The SDK has no portable process-memory observer;
                            // zero is the proto3 absent value and detail names
                            // the metric as unavailable rather than claiming a
                            // measurement.
                            memory_bytes: 0,
                            queue_depth: input.queue_depth(),
                            in_flight: metrics.in_flight.load(Ordering::Acquire),
                            detail: "memory_bytes=unavailable".to_owned(),
                            unknown: Default::default(),
                        })),
                        unknown: Default::default(),
                    },
                )?;
            }
            _ => {
                return Err(SdkError::Protocol(format!(
                    "unsupported request payload {}",
                    payload.kind()
                )));
            }
        }
        if input.shutdown {
            let _in_flight = metrics.enter();
            plugin
                .stop(&request_context)
                .map_err(|error| SdkError::Protocol(format!("plugin stop failed: {error}")))?;
            if let Some(error) = request_context.take_log_error() {
                return Err(error);
            }
            return Ok(());
        }
    }
}

struct RequestRuntime<'a> {
    transport: &'a Arc<Mutex<Box<dyn Transport>>>,
    input: &'a mut RuntimeInput,
    cancellation: &'a Arc<CancellationState>,
    credits: &'a mut CreditState,
    context: &'a RuntimeContext,
    metrics: &'a RuntimeMetrics,
}

fn handle_suggest(
    plugin: &mut dyn Plugin,
    request: message::SuggestRequest,
    request_id: u64,
    generation: u64,
    deadline_ms: u64,
    runtime: &mut RequestRuntime<'_>,
) -> Result<(), SdkError> {
    let transport = runtime.transport;
    let input = &mut *runtime.input;
    let cancellation = runtime.cancellation;
    let credits = &mut *runtime.credits;
    let context = runtime.context;
    let metrics = runtime.metrics;
    let key = CancellationKey {
        request_id,
        generation,
    };
    cancellation.begin(key);
    let query = Query {
        request: RequestId(request_id),
        text: request.text,
        normalized: request.normalized_text,
        deadline_ms: (deadline_ms != 0).then_some(deadline_ms),
        generation,
        selected_item_id: if request.selected_item_id.is_empty() {
            None
        } else {
            Some(request.selected_item_id)
        },
    };
    let mut sink = SuggestionSinkImpl::new(
        context.connection_id,
        request_id,
        generation,
        transport,
        input,
        cancellation,
        credits,
    );
    let callback_result = {
        let _in_flight = metrics.enter();
        plugin.suggest(query, context, &mut sink)
    };
    if sink.shutdown_received() {
        cancellation.end();
        return Ok(());
    }
    let cancelled_error = sink.take_cancelled_error();
    let observed = cancellation.was_observed();
    let result = if let Some(error) = sink.take_transport_error() {
        Err(error)
    } else if sink.terminal_sent() {
        Ok(())
    } else if observed {
        sink.finish_cancelled()
            .map_err(|error| SdkError::Protocol(format!("cancelled suggestion could not finish: {error}")))
    } else if cancelled_error {
        sink.finish_final()
            .map_err(|error| SdkError::Protocol(format!("ignored cancellation could not finish: {error}")))
    } else if let Err(error) = callback_result {
        sink.finish_failed(&error.to_string()).map_err(|finish_error| {
            SdkError::Protocol(format!("failed suggestion could not finish: {finish_error}"))
        })
    } else {
        sink.finish_final().map_err(|finish_error| {
            SdkError::Protocol(format!("suggestion could not finish: {finish_error}"))
        })
    };
    let shutdown = sink.shutdown_received();
    cancellation.end();
    if shutdown {
        Ok(())
    } else {
        result
    }
}
fn handle_catalog(
    plugin: &mut dyn Plugin,
    request_id: u64,
    generation: u64,
    _request: message::CatalogRequest,
    runtime: &mut RequestRuntime<'_>,
) -> Result<(), SdkError> {
    let transport = runtime.transport;
    let input = &mut *runtime.input;
    let cancellation = runtime.cancellation;
    let credits = &mut *runtime.credits;
    let context = runtime.context;
    let metrics = runtime.metrics;
    let key = CancellationKey {
        request_id,
        generation,
    };
    cancellation.begin(key);
    let mut sink = CatalogSinkImpl::new(
        context.connection_id,
        request_id,
        generation,
        transport,
        input,
        cancellation,
        credits,
    );
    let callback_result = {
        let _in_flight = metrics.enter();
        plugin.build_catalog(context, &mut sink)
    };
    if sink.shutdown_received() {
        cancellation.end();
        return Ok(());
    }
    let result = if let Some(error) = sink.take_transport_error() {
        Err(error)
    } else if sink.terminal_sent() {
        Ok(())
    } else {
        sink.finish(callback_result.err().map(|error| error.to_string()))
            .map_err(|error| SdkError::Protocol(format!("catalog could not finish: {error}")))
    };
    let shutdown = sink.shutdown_received();
    cancellation.end();
    if shutdown {
        Ok(())
    } else {
        result
    }
}

fn handle_execute(
    plugin: &mut dyn Plugin,
    request: message::ExecuteRequest,
    request_id: u64,
    generation: u64,
    transport: &Arc<Mutex<Box<dyn Transport>>>,
    context: &RuntimeContext,
    metrics: &RuntimeMetrics,
) -> Result<(), SdkError> {
    let plugin_request = ExecuteRequest {
        request: RequestId(request_id),
        item: ItemId(request.item_id),
        action: if request.action_id.is_empty() {
            None
        } else {
            Some(ActionId(request.action_id))
        },
        argument: if request.argument.is_empty() {
            None
        } else {
            Some(request.argument)
        },
    };
    let callback = {
        let _in_flight = metrics.enter();
        plugin.execute(plugin_request, context)
    };
    let (outcome, error) = match callback {
        Ok(()) => (ExecuteOutcomeCode::from_i32(1), None),
        Err(error) => (
            ExecuteOutcomeCode::from_i32(2),
            Some(structured_error(&error.to_string(), request_id)),
        ),
    };
    send_envelope(
        transport,
        Envelope {
            connection_id: context.connection_id,
            request_id,
            generation,
            deadline_ms: 0,
            payload: Some(Payload::ExecuteResult(message::ExecuteResult {
                outcome,
                error,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        },
    )
}

fn send_error(
    transport: &Arc<Mutex<Box<dyn Transport>>>,
    connection_id: u64,
    request_id: u64,
    generation: u64,
    detail: &str,
) -> Result<(), SdkError> {
    send_envelope(
        transport,
        Envelope {
            connection_id,
            request_id,
            generation,
            deadline_ms: 0,
            payload: Some(Payload::Error(structured_error(detail, request_id))),
            unknown: Default::default(),
        },
    )
}

fn structured_error(detail: &str, request_id: u64) -> StructuredError {
    StructuredError {
        code: ErrorCode::from_i32(2),
        message: if detail.is_empty() {
            "plugin callback failed".to_owned()
        } else {
            detail.to_owned()
        },
        detail: detail.to_owned(),
        request_id,
        unknown: Default::default(),
    }
}

fn send_envelope(transport: &Arc<Mutex<Box<dyn Transport>>>, envelope: Envelope) -> Result<(), SdkError> {
    let mut locked = transport
        .lock()
        .map_err(|_| SdkError::Transport("transport lock poisoned".to_owned()))?;
    locked.send(&envelope).map_err(SdkError::from)
}

struct SuggestionSinkImpl<'a> {
    connection_id: u64,
    request_id: u64,
    generation: u64,
    transport: &'a Arc<Mutex<Box<dyn Transport>>>,
    input: &'a mut RuntimeInput,
    cancellation: &'a Arc<CancellationState>,
    credits: &'a mut CreditState,
    sequence: u64,
    terminal: bool,
    transport_error: Option<SdkError>,
    cancelled_error: bool,
}

impl<'a> SuggestionSinkImpl<'a> {
    fn new(
        connection_id: u64,
        request_id: u64,
        generation: u64,
        transport: &'a Arc<Mutex<Box<dyn Transport>>>,
        input: &'a mut RuntimeInput,
        cancellation: &'a Arc<CancellationState>,
        credits: &'a mut CreditState,
    ) -> Self {
        Self {
            connection_id,
            request_id,
            generation,
            transport,
            input,
            cancellation,
            credits,
            sequence: 0,
            terminal: false,
            transport_error: None,
            cancelled_error: false,
        }
    }

    fn terminal_sent(&self) -> bool {
        self.terminal
    }
    fn shutdown_received(&self) -> bool {
        self.input.shutdown
    }

    fn take_transport_error(&mut self) -> Option<SdkError> {
        self.transport_error.take()
    }
    fn take_cancelled_error(&mut self) -> bool {
        std::mem::take(&mut self.cancelled_error)
    }

    fn wait_for_credit(&mut self, _terminal: bool) -> Result<(), CoreError> {
        if self.input.shutdown {
            return Err(CoreError::Cancelled);
        }
        while self.credits.remaining == 0 || self.credits.paused {
            let envelope = self.input.next_raw().map_err(|error| {
                self.transport_error = Some(error.clone());
                CoreError::Invalid("transport closed while waiting for credit".to_owned())
            })?;
            match envelope.payload {
                Some(Payload::Flow(flow)) => self.credits.grant(&flow),
                Some(Payload::Cancel(_)) => {
                    self.cancellation
                        .cancel((envelope.request_id, envelope.generation));
                }
                Some(Payload::Shutdown(_)) => {
                    self.input.mark_shutdown(envelope);
                    return Err(CoreError::Cancelled);
                }
                _ => self.input.stash(envelope),
            }
            if self.input.shutdown {
                return Err(CoreError::Cancelled);
            }
        }
        Ok(())
    }

    fn send_batch(
        &mut self,
        state: BatchState,
        items: Vec<Item>,
        error: Option<StructuredError>,
    ) -> Result<()> {
        let partial = state.as_i32() == 1;
        if partial && self.cancellation.cancellation_flag() {
            self.cancelled_error = true;
            return Err(CoreError::Cancelled);
        }
        self.wait_for_credit(partial)?;
        if partial && self.cancellation.cancellation_flag() {
            self.cancelled_error = true;
            return Err(CoreError::Cancelled);
        }
        self.send_batch_wire(state, items, error)
    }

    fn send_batch_wire(
        &mut self,
        state: BatchState,
        items: Vec<Item>,
        error: Option<StructuredError>,
    ) -> Result<()> {
        let proto_items = items
            .iter()
            .map(crikey_native_protocol::convert::to_proto_item)
            .collect();
        let envelope = Envelope {
            connection_id: self.connection_id,
            request_id: self.request_id,
            generation: self.generation,
            deadline_ms: 0,
            payload: Some(Payload::Results(message::ResultBatch {
                state,
                items: proto_items,
                sequence: self.sequence,
                error,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        };
        send_envelope(self.transport, envelope).map_err(|error| {
            self.transport_error = Some(error.clone());
            CoreError::Invalid("failed to send suggestion batch".to_owned())
        })?;
        self.credits.remaining = self.credits.remaining.saturating_sub(1);
        self.sequence = self.sequence.saturating_add(1);
        Ok(())
    }

    fn finish_final(&mut self) -> Result<()> {
        if self.terminal || self.shutdown_received() {
            return Ok(());
        }
        if self.shutdown_received() {
            return Ok(());
        }
        if let Err(error) = self.send_batch(BatchState::from_i32(2), Vec::new(), None) {
            if self.shutdown_received() {
                return Ok(());
            }
            return Err(error);
        }
        self.terminal = true;
        Ok(())
    }

    fn finish_cancelled(&mut self) -> Result<()> {
        if self.terminal || self.shutdown_received() {
            return Ok(());
        }
        if let Err(error) = self.send_batch(BatchState::from_i32(3), Vec::new(), None) {
            if self.shutdown_received() {
                return Ok(());
            }
            return Err(error);
        }
        self.terminal = true;
        Ok(())
    }

    fn finish_failed(&mut self, detail: &str) -> Result<()> {
        if self.terminal || self.shutdown_received() {
            return Ok(());
        }
        if let Err(error) = self.send_batch(
            BatchState::from_i32(4),
            Vec::new(),
            Some(structured_error(detail, self.request_id)),
        ) {
            if self.shutdown_received() {
                return Ok(());
            }
            return Err(error);
        }
        self.terminal = true;
        Ok(())
    }
}

impl SuggestionSink for SuggestionSinkImpl<'_> {
    fn emit_batch(&mut self, items: Vec<Item>) -> Result<()> {
        self.send_batch(BatchState::from_i32(1), items, None)
    }

    fn finish(&mut self) -> Result<()> {
        if self.cancellation.was_observed() {
            self.finish_cancelled()
        } else {
            self.finish_final()
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

struct CatalogSinkImpl<'a> {
    connection_id: u64,
    request_id: u64,
    generation: u64,
    transport: &'a Arc<Mutex<Box<dyn Transport>>>,
    input: &'a mut RuntimeInput,
    cancellation: &'a Arc<CancellationState>,
    credits: &'a mut CreditState,
    sequence: u64,
    terminal: bool,
    transport_error: Option<SdkError>,
}

impl<'a> CatalogSinkImpl<'a> {
    fn new(
        connection_id: u64,
        request_id: u64,
        generation: u64,
        transport: &'a Arc<Mutex<Box<dyn Transport>>>,
        input: &'a mut RuntimeInput,
        cancellation: &'a Arc<CancellationState>,
        credits: &'a mut CreditState,
    ) -> Self {
        Self {
            connection_id,
            request_id,
            generation,
            transport,
            input,
            cancellation,
            credits,
            sequence: 0,
            terminal: false,
            transport_error: None,
        }
    }

    fn terminal_sent(&self) -> bool {
        self.terminal
    }
    fn shutdown_received(&self) -> bool {
        self.input.shutdown
    }

    fn take_transport_error(&mut self) -> Option<SdkError> {
        self.transport_error.take()
    }

    fn wait_for_credit(&mut self) -> Result<(), CoreError> {
        if self.input.shutdown {
            return Err(CoreError::Cancelled);
        }
        while self.credits.remaining == 0 || self.credits.paused {
            let envelope = self.input.next_raw().map_err(|error| {
                self.transport_error = Some(error.clone());
                CoreError::Invalid("transport closed while waiting for credit".to_owned())
            })?;
            match envelope.payload {
                Some(Payload::Flow(flow)) => self.credits.grant(&flow),
                Some(Payload::Cancel(_)) => {
                    self.cancellation
                        .cancel((envelope.request_id, envelope.generation));
                    return Err(CoreError::Cancelled);
                }
                Some(Payload::Shutdown(_)) => {
                    self.input.mark_shutdown(envelope);
                    return Err(CoreError::Cancelled);
                }
                _ => self.input.stash(envelope),
            }
            if self.input.shutdown {
                return Err(CoreError::Cancelled);
            }
        }
        Ok(())
    }

    fn send(&mut self, items: Vec<Item>, done: bool, error: Option<StructuredError>) -> Result<()> {
        self.wait_for_credit()?;
        let proto_items = items
            .iter()
            .map(crikey_native_protocol::convert::to_proto_item)
            .collect();
        let envelope = Envelope {
            connection_id: self.connection_id,
            request_id: self.request_id,
            generation: self.generation,
            deadline_ms: 0,
            payload: Some(Payload::CatalogBatch(message::CatalogBatch {
                items: proto_items,
                done,
                sequence: self.sequence,
                error,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        };
        let send_result = send_envelope(self.transport, envelope).map_err(|error| {
            self.transport_error = Some(error.clone());
            CoreError::Invalid("failed to send catalog batch".to_owned())
        });
        if send_result.is_ok() {
            self.credits.remaining = self.credits.remaining.saturating_sub(1);
            self.sequence = self.sequence.saturating_add(1);
        }
        send_result
    }

    fn finish(&mut self, failure: Option<String>) -> Result<()> {
        if self.terminal {
            return Ok(());
        }
        let error = failure.map(|detail| structured_error(&detail, self.request_id));
        self.send(Vec::new(), true, error)?;
        self.terminal = true;
        Ok(())
    }
}

impl CatalogSink for CatalogSinkImpl<'_> {
    fn emit_batch(&mut self, items: Vec<Item>) -> Result<()> {
        self.send(items, false, None)
    }

    fn finish(&mut self) -> Result<()> {
        self.finish(None)
    }
}

impl fmt::Debug for SuggestionSinkImpl<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SuggestionSink")
            .field("request_id", &self.request_id)
            .field("generation", &self.generation)
            .field("sequence", &self.sequence)
            .field("terminal", &self.terminal)
            .finish()
    }
}

impl fmt::Debug for CatalogSinkImpl<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CatalogSink")
            .field("request_id", &self.request_id)
            .field("generation", &self.generation)
            .field("sequence", &self.sequence)
            .field("terminal", &self.terminal)
            .finish()
    }
}
