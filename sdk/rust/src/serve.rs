//! Handshake and serving loop for native plugins (spec 16.3, 16.5, 16.7).
//!
//! The loop keeps protocol I/O in a small reader thread.  This is important for
//! cooperative cancellation: a plugin may be polling its context while it is
//! not currently inside a sink call, so a blocking transport read cannot be
//! allowed to starve the cancellation signal.
//!
//! Plugin callbacks are invoked behind an unwind boundary.  A callback panic is
//! reported as a failed request and the serving loop remains available for
//! later requests; a panic during startup or orderly shutdown is returned as an
//! SDK error because the plugin cannot safely continue in either state.  This
//! applies to the normal unwinding panic strategy; a plugin built with
//! `panic = "abort"` still terminates the process by design.

use std::collections::VecDeque;
use std::env;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex, Once};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crikey_core::{ActionId, CoreError, Item, ItemId, PageColor, PageFrame, PluginId, Result};
use crikey_native_protocol::message::{
    self, BatchState, Envelope, ErrorCode, EventKind, ExecuteOutcomeCode, Payload, StructuredError,
};
use crikey_native_protocol::transport::Transport;
use crikey_native_protocol::{Capabilities, Endpoint, ProtocolError, RequestId, PROTOCOL_VERSION};

use crate::{
    CancellationToken, CatalogSink, ExecuteOutcome, ExecuteRequest, LogLevel, PagePalette, PageRequest,
    Plugin, PluginContext, PluginEvent, PluginResource, Query, ResourceKind, SdkError, SuggestionSink,
};
use crikey_core::result_presentation::ResultPresentation;
use crikey_core::structured_arguments::{ArgumentSchema, SubmissionFailureCode, SubmissionOutcome};
use crikey_core::structured_arguments::{ExecutableKey, OwnedItem};
use crikey_core::structured_view::{
    RouteAdmission, RouteIdentity, TypedExecutionOutcome, ViewDescriptor, ViewExecutionContext, ViewReply,
    ViewRequestMode, STRUCTURED_VIEW_CAPABILITY,
};
use std::num::NonZeroU64;

static SENSITIVE: AtomicBool = AtomicBool::new(false);
static DIAGNOSTICS: Mutex<()> = Mutex::new(());
static PANIC_HOOK: Once = Once::new();

fn install_panic_policy() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _guard = DIAGNOSTICS.lock().unwrap_or_else(|e| e.into_inner());
            if !SENSITIVE.load(Ordering::Acquire) {
                previous(info);
            }
        }));
    });
}

fn mark_sensitive() {
    let _guard = DIAGNOSTICS.lock().unwrap_or_else(|e| e.into_inner());
    SENSITIVE.store(true, Ordering::Release);
}

fn diagnostic_detail(detail: &impl fmt::Display) -> String {
    let _guard = DIAGNOSTICS.lock().unwrap_or_else(|e| e.into_inner());
    if SENSITIVE.load(Ordering::Acquire) {
        "plugin callback failed".into()
    } else {
        detail.to_string()
    }
}

fn diagnostic_context(prefix: &str, detail: &impl fmt::Display) -> String {
    let _guard = DIAGNOSTICS.lock().unwrap_or_else(|e| e.into_inner());
    if SENSITIVE.load(Ordering::Acquire) {
        "plugin callback failed".into()
    } else {
        format!("{prefix}: {detail}")
    }
}

#[derive(Debug, Default)]
struct SchemaRegistry {
    catalog: ResultPresentation,
    suggestions: ResultPresentation,
    catalog_primary: std::collections::BTreeMap<ItemId, ActionId>,
    suggestion_primary: std::collections::BTreeMap<ItemId, ActionId>,
    route: Option<FrozenRoute>,
}

#[derive(Debug)]
struct FrozenRoute {
    context: ViewExecutionContext,
    descriptor: Arc<ViewDescriptor>,
}

impl FrozenRoute {
    fn resolve(
        &self,
        view: &ViewExecutionContext,
        owner: &PluginId,
        item: &str,
        action: Option<&str>,
    ) -> Result<(Item, Option<Arc<ArgumentSchema>>)> {
        let key = ExecutableKey {
            target: OwnedItem {
                owner: owner.clone(),
                item: ItemId(item.into()),
            },
            action: action.map(|a| ActionId(a.into())),
        };
        if &self.context != view {
            return Err(CoreError::Invalid("stale route descriptor".into()));
        }
        if matches!(&self.descriptor.view, crikey_core::structured_view::StandardView::Form(f) if f.submit != key)
        {
            return Err(CoreError::Invalid("route target is not selected".into()));
        }
        let (_, presentation) = self
            .descriptor
            .executable(&key)
            .ok_or_else(|| CoreError::Invalid("route executable unavailable".into()))?;
        let definition = self
            .descriptor
            .definition(&key.target)
            .ok_or_else(|| CoreError::Invalid("route item unavailable".into()))?;
        Ok((definition.item.clone(), presentation.schema.clone()))
    }
}

// SDK-only shape validation: this synthetic identity never establishes host authority.
fn local_admission(owner: &PluginId, view: &ViewExecutionContext) -> Result<RouteAdmission> {
    RouteAdmission::worker(
        RouteIdentity {
            owner: owner.clone(),
            route: view.route.clone(),
            incarnation: NonZeroU64::MIN,
        },
        view.surface(),
    )
    .map_err(|_| CoreError::Invalid("invalid route admission".into()))
}
struct Publication {
    metadata: ResultPresentation,
    owner: PluginId,
    items: std::collections::BTreeSet<crikey_core::structured_arguments::OwnedItem>,
    executables: std::collections::BTreeSet<crikey_core::structured_arguments::ExecutableKey>,
    primary: std::collections::BTreeMap<ItemId, ActionId>,
}

impl Default for Publication {
    fn default() -> Self {
        Self {
            metadata: ResultPresentation::default(),
            owner: PluginId(String::new()),
            items: Default::default(),
            executables: Default::default(),
            primary: Default::default(),
        }
    }
}

impl Publication {
    fn record(&mut self, items: &[Item]) {
        use crikey_core::structured_arguments::{ExecutableKey, OwnedItem};
        for item in items {
            if !self
                .metadata
                .items
                .iter()
                .any(|p| p.target.item == item.stable_id)
                && !self
                    .metadata
                    .schemas
                    .iter()
                    .any(|b| b.executable.target.item == item.stable_id)
            {
                continue;
            }
            let target = OwnedItem {
                owner: self.owner.clone(),
                item: item.stable_id.clone(),
            };
            self.items.insert(target.clone());
            if let Some(action) = item.actions.first() {
                self.primary
                    .insert(item.stable_id.clone(), action.action_id.clone());
            }
            if item.actions.is_empty() {
                self.executables.insert(ExecutableKey { target, action: None });
            } else {
                for action in &item.actions {
                    self.executables.insert(ExecutableKey {
                        target: target.clone(),
                        action: Some(action.action_id.clone()),
                    });
                }
            }
        }
    }
    fn validate(&self) -> Result<()> {
        self.metadata
            .validate(&self.owner, &self.items, &self.executables)
            .map_err(|_| CoreError::Invalid("invalid presentation binding".into()))
    }
}

fn filtered_presentation(
    presentation: &ResultPresentation,
    typed: bool,
    visual: bool,
) -> Result<Option<message::ResultPresentation>, SdkError> {
    use crikey_core::result_presentation::{MAX_PRESENTATION_ITEMS, MAX_SCOPES, MAX_SECTIONS};
    if presentation.schemas.len() > MAX_PRESENTATION_ITEMS
        || presentation.items.len() > MAX_PRESENTATION_ITEMS
        || presentation.sections.len() > MAX_SECTIONS
        || presentation.scopes.len() > MAX_SCOPES
    {
        return Err(SdkError::Protocol("presentation exceeds limits".into()));
    }
    for binding in &presentation.schemas {
        binding
            .schema
            .validate()
            .map_err(|_| SdkError::Protocol("invalid argument schema".into()))?;
    }
    if !typed && !visual {
        return Ok(None);
    }
    let wire_presentation = ResultPresentation {
        sections: if visual {
            presentation.sections.clone()
        } else {
            Vec::new()
        },
        scopes: if visual {
            presentation.scopes.clone()
        } else {
            Vec::new()
        },
        items: if visual {
            presentation.items.clone()
        } else {
            Vec::new()
        },
        schemas: if typed {
            presentation.schemas.clone()
        } else {
            Vec::new()
        },
    };
    crikey_native_protocol::typed::to_presentation(wire_presentation)
        .map(Some)
        .map_err(SdkError::from)
}

/// Maximum number of decoded control frames retained while a plugin callback
/// is busy.  A full queue deliberately blocks the reader, which pauses IPC
/// reads rather than allowing unbounded memory growth (spec 12.4).
const READER_QUEUE_CAPACITY: usize = 64;
/// Maximum time spent waiting for a handshake acknowledgement.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Individual handshake reads use a short poll so the overall deadline is
/// enforceable on transports that support read timeouts.
const HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_LOG_RECORDS_PER_REQUEST: u32 = 256;
const MAX_LOG_BYTES_PER_REQUEST: usize = 64 * 1024;
const MAX_LOG_MESSAGE_BYTES: usize = 8 * 1024;

fn invoke_callback<T>(callback: impl FnOnce() -> Result<T>) -> std::result::Result<Result<T>, String> {
    catch_unwind(AssertUnwindSafe(|| {
        callback().map_err(|error| {
            if SENSITIVE.load(Ordering::Acquire) {
                CoreError::Invalid("plugin callback failed".into())
            } else {
                error
            }
        })
    }))
    .map_err(|panic| {
        let _guard = DIAGNOSTICS.lock().unwrap_or_else(|e| e.into_inner());
        if SENSITIVE.load(Ordering::Acquire) {
            return "plugin callback failed".to_owned();
        }
        let detail = if let Some(message) = panic.downcast_ref::<&str>() {
            (*message).to_owned()
        } else if let Some(message) = panic.downcast_ref::<String>() {
            message.clone()
        } else {
            "callback panicked with a non-string payload".to_owned()
        };
        format!("plugin callback panicked: {detail}")
    })
}

/// Connection and identity values advertised by a plugin (spec 16.3, 16.6).
#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// Stable plugin identity advertised during handshake.
    pub plugin_id: String,
    /// Human-readable plugin name.
    pub plugin_name: String,
    /// Plugin release version.
    pub plugin_version: String,
    /// SDK version advertised during handshake.
    pub sdk_version: String,
    /// Protocol capabilities this plugin implements.
    pub capabilities: Capabilities,
    /// Endpoint to connect, or `None` to read it from the environment.
    pub endpoint: Option<Endpoint>,
    /// Session token supplied by the host.
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
    /// Plugin identity received from the handshake.
    pub plugin_id: String,
    /// Human-readable plugin name.
    pub plugin_name: String,
    /// Plugin release version.
    pub plugin_version: String,
    /// SDK version advertised by the plugin.
    pub sdk_version: String,
    /// Negotiated protocol version.
    pub protocol_version: u32,
    /// Capabilities advertised by the plugin.
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
    install_panic_policy();
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
    let optional_capabilities = OptionalCapabilities {
        preview: plugin.supports_preview(),
        commands: plugin.supports_commands(),
        typed_arguments: plugin.supports_typed_arguments(),
        result_presentation: plugin.supports_result_presentation(),
        structured_views: plugin.supports_structured_views(),
    };
    let (ack, connection_id) = handshake(&mut *transport, &config, &session_token, &optional_capabilities)?;
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
    let mut context = RuntimeContext::new(
        PluginId(config.plugin_id.clone()),
        Arc::clone(&cancellation),
        Arc::clone(&transport),
        connection_id,
        0,
        0,
    );
    context.structured_views = plugin.supports_structured_views()
        && ack
            .host_capabilities
            .iter()
            .any(|s| s == STRUCTURED_VIEW_CAPABILITY);

    match invoke_callback(|| plugin.start(&context)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            stop_reader(&reader_stop, reader);
            return Err(SdkError::Protocol(diagnostic_context(
                "plugin start failed",
                &error,
            )));
        }
        Err(detail) => {
            stop_reader(&reader_stop, reader);
            return Err(SdkError::Protocol(diagnostic_context(
                "plugin start failed",
                &detail,
            )));
        }
    }
    if let Some(error) = context.take_log_error() {
        stop_reader(&reader_stop, reader);
        return Err(error);
    }

    let mut credits = CreditState {
        remaining: ack.initial_credits,
        paused: false,
    };
    let commands_negotiated = plugin.supports_commands()
        && ack
            .host_capabilities
            .iter()
            .any(|name| name == crikey_core::command::COMMANDS_CAPABILITY);
    let typed_negotiated =
        plugin.supports_typed_arguments() && ack.host_capabilities.iter().any(|s| s == "typed_arguments_v1");
    let presentation_negotiated = plugin.supports_result_presentation()
        && ack
            .host_capabilities
            .iter()
            .any(|s| s == "result_presentation_v1");
    let mut runtime = RequestRuntime {
        transport: &transport,
        input: &mut input,
        cancellation: &cancellation,
        credits: &mut credits,
        context: &context,
        metrics: &metrics,
        typed_negotiated,
        presentation_negotiated,
    };
    let result = serve_requests(
        plugin,
        &mut runtime,
        plugin_preview_negotiated(&ack),
        commands_negotiated,
    );
    stop_reader(&reader_stop, reader);
    result
}

struct OptionalCapabilities {
    preview: bool,
    commands: bool,
    typed_arguments: bool,
    result_presentation: bool,
    structured_views: bool,
}

fn handshake(
    transport: &mut dyn Transport,
    config: &ServeConfig,
    session_token: &str,
    optional: &OptionalCapabilities,
) -> Result<(message::HandshakeAck, u64), SdkError> {
    let handshake = message::Handshake {
        protocol_version: PROTOCOL_VERSION,
        plugin_id: config.plugin_id.clone(),
        plugin_version: config.plugin_version.clone(),
        capabilities: {
            let mut names = capability_names(&config.capabilities);
            if optional.preview {
                names.push(crikey_core::PREVIEW_CAPABILITY.to_owned());
            }
            if optional.commands {
                names.push(crikey_core::command::COMMANDS_CAPABILITY.to_owned());
            }
            if optional.typed_arguments {
                names.push("typed_arguments_v1".to_owned());
            }
            if optional.result_presentation {
                names.push("result_presentation_v1".to_owned());
            }
            if optional.structured_views {
                names.push(STRUCTURED_VIEW_CAPABILITY.to_owned());
            }
            names
        },
        session_token: session_token.to_owned(),
        plugin_name: config.plugin_name.clone(),
        sdk_version: config.sdk_version.clone(),
        unknown: Default::default(),
    };
    let timeout_supported = transport.supports_read_timeout();
    if timeout_supported {
        transport
            .set_read_timeout(Some(HANDSHAKE_READ_TIMEOUT))
            .map_err(SdkError::from)?;
    }
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
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let response = loop {
        match transport.recv() {
            Ok(response) => break response,
            Err(ProtocolError::Timeout) if Instant::now() < deadline => continue,
            Err(ProtocolError::Timeout) => {
                return Err(SdkError::Transport(
                    "handshake acknowledgement timed out".to_owned(),
                ));
            }
            Err(error) => return Err(SdkError::from(error)),
        }
    };
    if response.request_id != 0 || response.generation != 0 {
        return Err(SdkError::Protocol(format!(
            "handshake acknowledgement used request {} generation {}, expected zero",
            response.request_id, response.generation
        )));
    }
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
    if timeout_supported {
        transport.set_read_timeout(None).map_err(SdkError::from)?;
    }
    Ok((ack, connection_id))
}

fn plugin_preview_negotiated(ack: &message::HandshakeAck) -> bool {
    ack.host_capabilities
        .iter()
        .any(|s| s == crikey_core::PREVIEW_CAPABILITY)
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
                if transport.supports_read_timeout() {
                    if let Err(error) = transport.set_read_timeout(Some(Duration::from_millis(10))) {
                        cancellation.cancel((0, 0));
                        let _ = sender.send(ReadEvent::Failed(Box::new(SdkError::from(error))));
                        return;
                    }
                }
            }
            ReaderTransport::Shared(transport) => {
                let mut locked = match transport.lock() {
                    Ok(locked) => locked,
                    Err(_) => {
                        cancellation.cancel((0, 0));
                        let _ = sender.send(ReadEvent::Failed(Box::new(SdkError::Transport(
                            "transport lock poisoned".to_owned(),
                        ))));
                        return;
                    }
                };
                if locked.supports_read_timeout() {
                    if let Err(error) = locked.set_read_timeout(Some(Duration::from_millis(10))) {
                        cancellation.cancel((0, 0));
                        let _ = sender.send(ReadEvent::Failed(Box::new(SdkError::from(error))));
                        return;
                    }
                }
            }
        }
        while !stop.load(Ordering::Acquire) {
            let received = match &mut reader {
                ReaderTransport::Independent(transport) => transport.recv(),
                ReaderTransport::Shared(transport) => match transport.lock() {
                    Ok(mut locked) => locked.recv(),
                    Err(_) => {
                        cancellation.cancel((0, 0));
                        let _ = sender.send(ReadEvent::Failed(Box::new(SdkError::Transport(
                            "transport lock poisoned".to_owned(),
                        ))));
                        break;
                    }
                },
            };
            match received {
                Ok(envelope) => {
                    if matches!(&envelope.payload, Some(Payload::TypedExecuteRequest(_))) {
                        mark_sensitive();
                    }
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
                    cancellation.cancel((0, 0));
                    let _ = sender.send(ReadEvent::Closed);
                    break;
                }
                Err(error) => {
                    cancellation.cancel((0, 0));
                    let _ = sender.send(ReadEvent::Failed(Box::new(SdkError::from(error))));
                    break;
                }
            }
        }
    })
}

fn stop_reader(stop: &Arc<AtomicBool>, reader: JoinHandle<()>) {
    stop.store(true, Ordering::Release);
    // Normal transports poll their read timeout and finish promptly.  Join
    // those readers so repeated in-process harness runs do not accumulate
    // threads.  Inherited stdio has no portable read-timeout operation; keep
    // that exceptional reader detached rather than hanging shutdown forever.
    let deadline = Instant::now() + Duration::from_millis(100);
    while !reader.is_finished() && Instant::now() < deadline {
        thread::yield_now();
    }
    if reader.is_finished() {
        let _ = reader.join();
    } else {
        drop(reader);
    }
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

    fn next_raw_until(&mut self, deadline: Instant) -> Result<Option<Envelope>, SdkError> {
        match self
            .receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(ReadEvent::Envelope(envelope)) => {
                self.metrics.queued.fetch_sub(1, Ordering::AcqRel);
                Ok(Some(*envelope))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Ok(ReadEvent::Failed(error)) => Err(*error),
            Ok(ReadEvent::Closed) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.closed = true;
                Err(SdkError::Transport("connection closed".into()))
            }
        }
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

    fn stash(&mut self, envelope: Envelope) -> std::result::Result<(), SdkError> {
        if self.pending.len() >= READER_QUEUE_CAPACITY {
            return Err(SdkError::Protocol(
                "input queue exhausted while waiting for flow control".to_owned(),
            ));
        }
        self.pending.push_back(envelope);
        Ok(())
    }

    fn mark_shutdown(&mut self) {
        self.shutdown = true;
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
            if key.request_id == 0 && key.generation == 0 {
                if active.current.is_some() {
                    active.cancelled = true;
                }
            } else if active.current == Some(key) {
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
    schemas: Arc<Mutex<SchemaRegistry>>,
    structured_views: bool,
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
            schemas: Arc::new(Mutex::new(SchemaRegistry::default())),
            structured_views: false,
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
            schemas: Arc::clone(&self.schemas),
            structured_views: self.structured_views,
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

/// Maps the wire enum onto the SDK's, keeping a kind this release does not
/// know distinguishable from one it does rather than folding it onto `Icon`.
fn resource_kind(kind: &message::ResourceKind) -> ResourceKind {
    match kind.as_i32() {
        1 => ResourceKind::Icon,
        2 => ResourceKind::File,
        3 => ResourceKind::Configuration,
        _ => ResourceKind::Unknown,
    }
}

impl PluginContext for RuntimeContext {
    fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }

    fn cancellation(&self) -> &dyn CancellationToken {
        &self.cancellation
    }

    fn log(&self, level: LogLevel, message: &str) {
        let _guard = DIAGNOSTICS.lock().unwrap_or_else(|e| e.into_inner());
        if SENSITIVE.load(Ordering::Acquire) {
            return;
        }
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
    runtime: &mut RequestRuntime<'_>,
    preview_negotiated: bool,
    commands_negotiated: bool,
) -> Result<(), SdkError> {
    let transport = runtime.transport;
    let input = &mut *runtime.input;
    let cancellation = runtime.cancellation;
    let credits = &mut *runtime.credits;
    let context = runtime.context;
    let metrics = runtime.metrics;
    let typed_negotiated = runtime.typed_negotiated;
    let presentation_negotiated = runtime.presentation_negotiated;
    loop {
        let envelope = match input.next() {
            Ok(envelope) => envelope,
            Err(_error) if input.closed => {
                stop_plugin(plugin, context, metrics, false)?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let request_id = envelope.request_id;
        let generation = envelope.generation;
        let deadline_ms = envelope.deadline_ms;
        let payload = match envelope.payload {
            Some(payload) => payload,
            None => {
                send_error(
                    transport,
                    context.connection_id,
                    request_id,
                    generation,
                    "request payload is missing",
                )?;
                continue;
            }
        };
        let request_context = context.for_request(request_id, generation);
        match payload {
            Payload::Shutdown(_) => {
                stop_plugin(plugin, &request_context, metrics, true)?;
                return Ok(());
            }
            Payload::Flow(flow) => credits.grant(&flow),
            Payload::Cancel(_) => {
                cancellation.cancel((request_id, generation));
            }
            Payload::StructuredViewRequest(request) => {
                let mut runtime = RequestRuntime {
                    transport,
                    input,
                    cancellation,
                    credits,
                    context: &request_context,
                    metrics,
                    typed_negotiated,
                    presentation_negotiated,
                };
                handle_structured_view(plugin, request, deadline_ms, &mut runtime)?;
            }
            Payload::CommandsRequest(_) => {
                cancellation.begin(CancellationKey {
                    request_id,
                    generation,
                });
                let started = Instant::now();
                let callback = if !commands_negotiated {
                    Ok(Ok(crate::PluginCommands::Unsupported))
                } else if request_context.cancellation().is_cancelled() {
                    Ok(Ok(crate::PluginCommands::Cancelled))
                } else {
                    let _in_flight = metrics.enter();
                    invoke_callback(|| plugin.commands(&request_context))
                };
                let result = match callback {
                    Ok(Ok(outcome)) => crikey_native_protocol::commands::to_response(outcome),
                    Ok(Err(error)) => Err(diagnostic_detail(&error)),
                    Err(detail) => Err(detail),
                };
                let mut response = result.unwrap_or_else(|detail| commands_failure(&detail));
                if request_context.cancellation().is_cancelled() {
                    response = message::CommandsResponse {
                        status: 3,
                        ..Default::default()
                    };
                } else if deadline_ms != 0 && started.elapsed().as_millis() >= u128::from(deadline_ms) {
                    response = commands_failure("commands deadline exceeded");
                }
                send_envelope(
                    transport,
                    Envelope {
                        connection_id: request_context.connection_id,
                        request_id,
                        generation,
                        deadline_ms: 0,
                        payload: Some(Payload::CommandsResponse(response)),
                        unknown: Default::default(),
                    },
                )?;
                cancellation.end();
                if let Some(error) = request_context.take_log_error() {
                    return Err(error);
                }
            }
            Payload::PreviewRequest(request) => {
                cancellation.begin(CancellationKey {
                    request_id,
                    generation,
                });
                let started = Instant::now();
                let callback = if !preview_negotiated || !plugin.supports_preview() {
                    Ok(Ok(crikey_core::PreviewOutcome::Unsupported))
                } else if request.item_id.is_empty() || request.item_id.len() > 256 {
                    Ok(Err(CoreError::Invalid(
                        "preview item identity exceeds bounds".into(),
                    )))
                } else {
                    let _in_flight = metrics.enter();
                    invoke_callback(|| plugin.preview(&request.item_id, &request_context))
                };
                let mut response = match callback {
                    Ok(Ok(outcome)) => match crikey_native_protocol::preview::to_response(outcome) {
                        Ok(response) => response,
                        Err(error) => preview_failure(&diagnostic_detail(&error)),
                    },
                    Ok(Err(error)) => preview_failure(&diagnostic_detail(&error)),
                    Err(detail) => preview_failure(&detail),
                };
                if request_context.cancellation().is_cancelled() {
                    response = message::PreviewResponse {
                        status: 4,
                        ..Default::default()
                    };
                } else if deadline_ms != 0 && started.elapsed().as_millis() >= u128::from(deadline_ms) {
                    response = preview_failure("preview deadline exceeded");
                }
                send_envelope(
                    transport,
                    Envelope {
                        connection_id: request_context.connection_id,
                        request_id,
                        generation,
                        deadline_ms: 0,
                        payload: Some(Payload::PreviewResponse(response)),
                        unknown: Default::default(),
                    },
                )?;
                if let Some(error) = request_context.take_log_error() {
                    return Err(error);
                }
                cancellation.end();
            }
            Payload::Suggest(request) => {
                let mut runtime = RequestRuntime {
                    transport,
                    input,
                    cancellation,
                    credits,
                    context: &request_context,
                    metrics,
                    typed_negotiated,
                    presentation_negotiated,
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
                    typed_negotiated,
                    presentation_negotiated,
                };
                handle_catalog(plugin, request_id, generation, request, &mut runtime)?;
                if let Some(error) = request_context.take_log_error() {
                    return Err(error);
                }
            }
            Payload::TypedExecuteRequest(request) => {
                cancellation.begin(CancellationKey {
                    request_id,
                    generation,
                });
                handle_typed_execute(
                    plugin,
                    request,
                    &request_context,
                    metrics,
                    typed_negotiated,
                    deadline_ms,
                )?;
                cancellation.end();
            }
            Payload::Execute(request) => {
                cancellation.begin(CancellationKey {
                    request_id,
                    generation,
                });
                handle_execute(plugin, request, &request_context, metrics, deadline_ms)?;
                cancellation.end();
                if let Some(error) = request_context.take_log_error() {
                    return Err(error);
                }
            }
            Payload::PageRequest(request) => {
                handle_page(
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
                    invoke_callback(|| plugin.on_configuration(&configuration.values, &request_context))
                };
                if let Some(error) = request_context.take_log_error() {
                    return Err(error);
                }
                let callback_error = match callback_result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(diagnostic_detail(&error)),
                    Err(detail) => Some(detail),
                };
                if let Some(error) = callback_error {
                    send_error(
                        transport,
                        request_context.connection_id,
                        request_id,
                        generation,
                        &diagnostic_context("configuration callback failed", &error),
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
                    invoke_callback(|| plugin.on_event(&plugin_event, &request_context))
                };
                if let Some(error) = request_context.take_log_error() {
                    return Err(error);
                }
                let callback_error = match callback_result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(diagnostic_detail(&error)),
                    Err(detail) => Some(detail),
                };
                if let Some(error) = callback_error {
                    send_error(
                        transport,
                        request_context.connection_id,
                        request_id,
                        generation,
                        &diagnostic_context("event callback failed", &error),
                    )?;
                }
            }
            Payload::Lifecycle(lifecycle) => {
                if matches!(lifecycle.kind.as_i32(), 2 | 4) {
                    context
                        .schemas
                        .lock()
                        .map_err(|_| SdkError::Protocol("route registry unavailable".into()))?
                        .route = None;
                }
                let callback = {
                    let _in_flight = metrics.enter();
                    invoke_callback(|| match lifecycle.kind.as_i32() {
                        1 => plugin.start(&request_context),
                        2 => plugin.stop(&request_context),
                        3 => plugin.on_activated(&request_context),
                        4 => plugin.on_deactivated(&request_context),
                        _ => Err(CoreError::Invalid("unsupported lifecycle kind".to_owned())),
                    })
                };
                let (ok, error) = match callback {
                    Ok(Ok(())) => (true, None),
                    Ok(Err(error)) => (
                        false,
                        Some(structured_error(&diagnostic_detail(&error), request_id)),
                    ),
                    Err(detail) => (false, Some(structured_error(&detail, request_id))),
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
            Payload::ResourceRequest(request) => {
                let kind = resource_kind(&request.kind);
                let reference = request.reference;
                let callback_result = {
                    let _in_flight = metrics.enter();
                    invoke_callback(|| plugin.resource(kind, &reference, &request_context))
                };
                if let Some(error) = request_context.take_log_error() {
                    return Err(error);
                }
                // A refusal is answered, never dropped. The host is holding a
                // deadline open for this reference; letting it expire would
                // cost the user the wait and tell the host nothing it could
                // not already assume.
                let (resource, error) = match callback_result {
                    Ok(Ok(resource)) => (resource, None),
                    Ok(Err(error)) => (
                        None,
                        Some(structured_error(&diagnostic_detail(&error), request_id)),
                    ),
                    Err(detail) => (None, Some(structured_error(&detail, request_id))),
                };
                let (found, content, media_type) = match resource {
                    Some(PluginResource { content, media_type }) => (true, content, media_type),
                    None => (false, Vec::new(), String::new()),
                };
                send_envelope(
                    transport,
                    Envelope {
                        connection_id: request_context.connection_id,
                        request_id,
                        generation,
                        deadline_ms: 0,
                        payload: Some(Payload::ResourceResponse(message::ResourceResponse {
                            reference,
                            found,
                            content,
                            media_type,
                            error,
                            unknown: Default::default(),
                        })),
                        unknown: Default::default(),
                    },
                )?;
            }
            _ => {
                send_error(
                    transport,
                    request_context.connection_id,
                    request_id,
                    generation,
                    &format!("unsupported request payload {}", payload.kind()),
                )?;
            }
        }
        if input.shutdown {
            stop_plugin(plugin, &request_context, metrics, true)?;
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
    typed_negotiated: bool,
    presentation_negotiated: bool,
}

fn commands_failure(detail: &str) -> message::CommandsResponse {
    let _guard = DIAGNOSTICS.lock().unwrap_or_else(|e| e.into_inner());
    let detail = if SENSITIVE.load(Ordering::Acquire) {
        "plugin callback failed"
    } else {
        detail
    };
    let mut end = detail.len().min(8192);
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    message::CommandsResponse {
        status: 4,
        detail: detail[..end].to_owned(),
        ..Default::default()
    }
}

fn preview_failure(detail: &str) -> message::PreviewResponse {
    let _guard = DIAGNOSTICS.lock().unwrap_or_else(|e| e.into_inner());
    let detail = if SENSITIVE.load(Ordering::Acquire) {
        "plugin callback failed"
    } else {
        detail
    };
    let mut end = detail.len().min(crikey_core::preview::MAX_PREVIEW_VALUE_BYTES);
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    message::PreviewResponse {
        status: 5,
        detail: detail[..end].to_owned(),
        ..Default::default()
    }
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
        selected_scope_id: if runtime.presentation_negotiated {
            request.selected_scope_id
        } else {
            None
        },
    };
    {
        let mut registry = context
            .schemas
            .lock()
            .map_err(|_| SdkError::Protocol("schema registry unavailable".into()))?;
        registry.suggestions = ResultPresentation::default();
        registry.suggestion_primary.clear();
    }
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
        invoke_callback(|| {
            let presentation = plugin.suggestion_presentation(&query, context)?;
            let wire = filtered_presentation(
                &presentation,
                runtime.typed_negotiated,
                runtime.presentation_negotiated,
            )
            .map_err(|_| CoreError::Invalid("invalid presentation".into()))?;
            sink.publication = Publication {
                metadata: presentation,
                owner: context.plugin_id.clone(),
                ..Default::default()
            };
            sink.presentation = wire;
            plugin.suggest(query, context, &mut sink)
        })
    };
    if sink.shutdown_received() {
        cancellation.end();
        return Ok(());
    }
    let cancelled_error = sink.take_cancelled_error();
    let observed = cancellation.was_observed();
    let callback_error = match callback_result {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(diagnostic_detail(&error)),
        Err(detail) => Some(detail),
    };
    let succeeded = callback_error.is_none() && !observed && !cancelled_error;
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
    } else if let Some(error) = callback_error {
        sink.finish_failed(&error).map_err(|finish_error| {
            SdkError::Protocol(format!("failed suggestion could not finish: {finish_error}"))
        })
    } else {
        sink.finish_final().map_err(|finish_error| {
            SdkError::Protocol(format!("suggestion could not finish: {finish_error}"))
        })
    };
    if result.is_ok() && succeeded && sink.publication.validate().is_ok() {
        context
            .schemas
            .lock()
            .map_err(|_| SdkError::Protocol("schema registry unavailable".into()))?
            .suggestions = std::mem::take(&mut sink.publication.metadata);
        context
            .schemas
            .lock()
            .map_err(|_| SdkError::Protocol("schema registry unavailable".into()))?
            .suggestion_primary = std::mem::take(&mut sink.publication.primary);
    }
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
    {
        let mut registry = context
            .schemas
            .lock()
            .map_err(|_| SdkError::Protocol("schema registry unavailable".into()))?;
        registry.catalog = ResultPresentation::default();
        registry.catalog_primary.clear();
    }
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
        invoke_callback(|| {
            let presentation = plugin.catalog_presentation(context)?;
            let wire = filtered_presentation(
                &presentation,
                runtime.typed_negotiated,
                runtime.presentation_negotiated,
            )
            .map_err(|_| CoreError::Invalid("invalid presentation".into()))?;
            sink.publication = Publication {
                metadata: presentation,
                owner: context.plugin_id.clone(),
                ..Default::default()
            };
            sink.presentation = wire;
            plugin.build_catalog(context, &mut sink)
        })
    };
    if sink.shutdown_received() {
        cancellation.end();
        return Ok(());
    }
    let callback_error = match callback_result {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(diagnostic_detail(&error)),
        Err(detail) => Some(detail),
    };
    let succeeded = callback_error.is_none();
    let result = if let Some(error) = sink.take_transport_error() {
        Err(error)
    } else if sink.terminal_sent() {
        Ok(())
    } else {
        sink.finish(callback_error)
            .map_err(|error| SdkError::Protocol(format!("catalog could not finish: {error}")))
    };
    if result.is_ok() && succeeded && sink.publication.validate().is_ok() {
        context
            .schemas
            .lock()
            .map_err(|_| SdkError::Protocol("schema registry unavailable".into()))?
            .catalog = std::mem::take(&mut sink.publication.metadata);
        context
            .schemas
            .lock()
            .map_err(|_| SdkError::Protocol("schema registry unavailable".into()))?
            .catalog_primary = std::mem::take(&mut sink.publication.primary);
    }
    let shutdown = sink.shutdown_received();
    cancellation.end();
    if shutdown {
        Ok(())
    } else {
        result
    }
}

fn handle_structured_view(
    plugin: &mut dyn Plugin,
    request: message::StructuredViewRequest,
    deadline_ms: u64,
    runtime: &mut RequestRuntime<'_>,
) -> Result<(), SdkError> {
    let context = runtime.context;
    // Refusal control replies echo the route, so never copy an unbounded
    // malformed identity into that credit-exempt response.
    if request.route_id.len() > 256 {
        return send_error(
            runtime.transport,
            context.connection_id,
            context.request_id,
            context.generation,
            "invalid view request",
        );
    }
    let started = Instant::now();
    runtime.cancellation.begin(CancellationKey {
        request_id: context.request_id,
        generation: context.generation,
    });
    let mut response = message::StructuredViewResponse {
        route_id: request.route_id.clone(),
        logical_view: request.logical_view,
        surface: request.surface,
        status: 4,
        ..Default::default()
    };
    let mut frozen = None;
    let prepared = (|| -> Result<()> {
        if !context.structured_views {
            response.status = 2;
            return Ok(());
        }
        let request = crikey_native_protocol::structured_view::from_request(request, &context.plugin_id)
            .map_err(|_| CoreError::Invalid("invalid view request".into()))?;
        let revision = NonZeroU64::new(context.request_id)
            .ok_or_else(|| CoreError::Invalid("invalid view revision".into()))?;
        let mut registry = context
            .schemas
            .lock()
            .map_err(|_| CoreError::Invalid("route registry unavailable".into()))?;
        if request.mode == ViewRequestMode::Refresh
            && !registry.route.as_ref().is_some_and(|r| {
                r.context.route == request.route
                    && r.context.session == request.session
                    && Some(r.context.revision) == request.previous_revision
            })
        {
            return Err(CoreError::Invalid("stale view refresh".into()));
        }
        if request.mode == ViewRequestMode::Open {
            registry.route = None;
        }
        drop(registry);
        if context.cancellation().is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        let descriptor = {
            let _in_flight = runtime.metrics.enter();
            match invoke_callback(|| plugin.structured_view(&request, context)) {
                Ok(result) => result?,
                Err(_) => return Err(CoreError::Invalid("view callback failed".into())),
            }
        };
        let view = ViewExecutionContext {
            route: request.route.clone(),
            session: request.session,
            revision,
        };
        let admission = local_admission(&context.plugin_id, &view)?;
        let descriptor = Arc::new(descriptor);
        ViewReply {
            context: view.clone(),
            descriptor: Arc::clone(&descriptor),
        }
        .validate(&request, revision, &admission)
        .map_err(|_| CoreError::Invalid("invalid view descriptor".into()))?;
        response.descriptor = Some(Box::new(
            crikey_native_protocol::structured_view::to_descriptor(&descriptor, &admission)
                .map_err(|_| CoreError::Invalid("invalid view descriptor".into()))?,
        ));
        response.status = 1;
        frozen = Some(FrozenRoute {
            context: view,
            descriptor,
        });
        Ok(())
    })();
    if prepared.is_err() {
        response.status = 4;
        response.descriptor = None;
        frozen = None;
    }
    let mut envelope = Envelope {
        connection_id: context.connection_id,
        request_id: context.request_id,
        generation: context.generation,
        deadline_ms: 0,
        payload: Some(Payload::StructuredViewResponse(response)),
        unknown: Default::default(),
    };
    // Preflight the complete envelope before waiting for credit or publishing
    // authority. A valid Core descriptor can still exceed the wire budget.
    if message::validate_outbound(&envelope).is_err() {
        if let Some(Payload::StructuredViewResponse(response)) = envelope.payload.as_mut() {
            response.status = 4;
            response.descriptor = None;
        }
        frozen = None;
    }
    let Some(Payload::StructuredViewResponse(response)) = envelope.payload.as_mut() else {
        unreachable!("structured view response envelope");
    };
    let mut sink = SuggestionSinkImpl::new(
        context.connection_id,
        context.request_id,
        context.generation,
        runtime.transport,
        runtime.input,
        runtime.cancellation,
        runtime.credits,
    );
    let deadline = (deadline_ms != 0).then(|| started + Duration::from_millis(deadline_ms));
    // Descriptor-free terminal refusals are bounded control replies. Only a
    // successful descriptor waits for and consumes an ordinary producer credit.
    if response.status == 1 && sink.wait_for_credit_until(deadline, true).is_err() {
        if let Some(error) = sink.take_transport_error() {
            return Err(error);
        }
        response.status = 3;
    }
    if context.cancellation().is_cancelled() {
        response.status = 3;
    } else if deadline_ms != 0 && started.elapsed().as_millis() >= u128::from(deadline_ms) {
        response.status = 5;
    }
    if response.status != 1 {
        response.descriptor = None;
        frozen = None;
    }
    let consumes_credit = response.status == 1;
    send_envelope(runtime.transport, envelope)?;
    if consumes_credit {
        sink.credits.remaining -= 1;
    }
    if let Some(frozen) = frozen {
        context
            .schemas
            .lock()
            .map_err(|_| SdkError::Protocol("route registry unavailable".into()))?
            .route = Some(frozen);
    }
    runtime.cancellation.end();
    if let Some(error) = context.take_log_error() {
        return Err(error);
    }
    Ok(())
}

fn handle_typed_execute(
    plugin: &mut dyn Plugin,
    request: message::TypedExecuteRequest,
    context: &RuntimeContext,
    metrics: &RuntimeMetrics,
    negotiated: bool,
    deadline_ms: u64,
) -> Result<(), SdkError> {
    let request_id = context.request_id;
    let generation = context.generation;
    let transport = &context.transport;
    mark_sensitive();
    let started = Instant::now();
    let view = request
        .view
        .clone()
        .map(crikey_native_protocol::structured_view::from_context)
        .transpose();
    let route = match &view {
        Ok(Some(view)) if context.structured_views => context.schemas.lock().ok().and_then(|registry| {
            registry.route.as_ref().and_then(|r| {
                r.resolve(
                    view,
                    &context.plugin_id,
                    &request.item_id,
                    request.action_id.as_deref(),
                )
                .ok()
            })
        }),
        _ => None,
    };
    let schema = {
        let registry = context
            .schemas
            .lock()
            .map_err(|_| SdkError::Protocol("schema registry unavailable".into()))?;
        if request.view.is_some() {
            route.as_ref().and_then(|(_, schema)| {
                schema
                    .as_ref()
                    .map(|schema| (Arc::clone(schema), schema.fingerprint().ok()))
            })
        } else {
            let mut selected = None;
            for binding in registry
                .suggestions
                .schemas
                .iter()
                .chain(&registry.catalog.schemas)
            {
                if binding.executable.target.owner == context.plugin_id
                    && binding.executable.target.item.0 == request.item_id
                    && binding.executable.action.as_ref().map(|a| &a.0) == request.action_id.as_ref()
                {
                    // Both root publications remain authoritative, independently
                    // of the separately resolved view namespace.
                    let fingerprint = binding.schema.fingerprint().ok();
                    if fingerprint.as_ref().map(|v| v.as_slice())
                        == Some(request.expected_fingerprint.as_slice())
                    {
                        selected = Some((binding, fingerprint));
                        break;
                    }
                    selected.get_or_insert((binding, fingerprint));
                }
            }
            selected.map(|(binding, fingerprint)| (Arc::clone(&binding.schema), fingerprint))
        }
    };
    let empty = ArgumentSchema::default();
    let failed = |code| TypedExecutionOutcome::from(SubmissionOutcome::Failed(code));
    let mut outcome = if !negotiated
        || !plugin.supports_typed_arguments()
        || (request.view.is_some() && !context.structured_views)
    {
        failed(SubmissionFailureCode::Unsupported)
    } else if context.cancellation().is_cancelled() {
        failed(SubmissionFailureCode::Cancelled)
    } else if let Some((schema, fingerprint)) = schema.as_ref() {
        if fingerprint.as_ref().map(|v| v.as_slice()) != Some(request.expected_fingerprint.as_slice()) {
            failed(SubmissionFailureCode::SchemaMismatch)
        } else if schema.validate_values(&request.values).is_err() {
            failed(SubmissionFailureCode::ProtocolViolation)
        } else {
            let expected_fingerprint =
                fingerprint.ok_or_else(|| SdkError::Protocol("invalid schema".into()))?;
            let _in_flight = metrics.enter();
            match invoke_callback(|| match (view, route) {
                (Ok(Some(view)), Some((item, _))) => plugin.execute_view_typed(
                    crate::ViewTypedExecuteRequest {
                        request: RequestId(request_id),
                        view,
                        item,
                        action: request.action_id.map(ActionId),
                        expected_fingerprint,
                        values: request.values,
                    },
                    context,
                ),
                (Ok(None), _) => plugin.execute_typed(
                    crate::TypedExecuteRequest {
                        request: RequestId(request_id),
                        item: ItemId(request.item_id),
                        action: request.action_id.map(ActionId),
                        expected_fingerprint,
                        values: request.values,
                    },
                    context,
                ),
                _ => Err(CoreError::Invalid("route unavailable".into())),
            }) {
                Ok(Ok(outcome)) if outcome.validate(Some(schema)).is_ok() => outcome,
                _ => failed(SubmissionFailureCode::Failed),
            }
        }
    } else {
        failed(SubmissionFailureCode::Unavailable)
    };
    if matches!(outcome, TypedExecutionOutcome::Navigate(_)) && !context.structured_views {
        outcome = failed(SubmissionFailureCode::Unsupported);
    }
    if context.cancellation().is_cancelled() {
        outcome = failed(SubmissionFailureCode::Cancelled);
    } else if deadline_ms != 0 && started.elapsed().as_millis() >= u128::from(deadline_ms) {
        outcome = failed(SubmissionFailureCode::Timeout);
    }
    // Navigation is a proposal, not host publication. Retain the exact current
    // descriptor until a subsequent admitted Open/Refresh replaces it.
    let response_schema = schema
        .as_ref()
        .map(|(schema, _)| schema.as_ref())
        .unwrap_or(&empty);
    let response =
        crikey_native_protocol::typed::to_response(outcome, response_schema).map_err(SdkError::from)?;
    send_envelope(
        transport,
        Envelope {
            connection_id: context.connection_id,
            request_id,
            generation,
            deadline_ms: 0,
            payload: Some(Payload::TypedExecuteResponse(response)),
            unknown: Default::default(),
        },
    )
}

fn handle_execute(
    plugin: &mut dyn Plugin,
    request: message::ExecuteRequest,
    context: &RuntimeContext,
    metrics: &RuntimeMetrics,
    deadline_ms: u64,
) -> Result<(), SdkError> {
    let request_id = context.request_id;
    let generation = context.generation;
    let transport = &context.transport;
    let started = Instant::now();
    let view = request
        .view
        .map(crikey_native_protocol::structured_view::from_context)
        .transpose();
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
    let mut callback = {
        let _in_flight = metrics.enter();
        if context.cancellation().is_cancelled() {
            Ok(Err(CoreError::Cancelled))
        } else {
            match view {
                Ok(Some(view)) if context.structured_views => {
                    let resolved = context.schemas.lock().ok().and_then(|registry| {
                        registry.route.as_ref().and_then(|route| {
                            route
                                .resolve(
                                    &view,
                                    &context.plugin_id,
                                    &plugin_request.item.0,
                                    plugin_request.action.as_ref().map(|a| a.0.as_str()),
                                )
                                .ok()
                        })
                    });
                    match resolved {
                        Some((item, None))
                            if match item.argument_policy {
                                crikey_core::item::ArgumentPolicy::Forbidden => {
                                    plugin_request.argument.is_none()
                                }
                                crikey_core::item::ArgumentPolicy::Required => {
                                    plugin_request.argument.as_ref().is_some_and(|v| !v.is_empty())
                                }
                                crikey_core::item::ArgumentPolicy::Optional => true,
                            } =>
                        {
                            invoke_callback(|| {
                                plugin.execute_view(
                                    crate::ViewExecuteRequest {
                                        request: plugin_request.request,
                                        view,
                                        item,
                                        action: plugin_request.action,
                                        argument: plugin_request.argument,
                                    },
                                    context,
                                )
                            })
                        }
                        _ => Ok(Err(CoreError::Invalid(
                            "route execution unavailable or requires typed input".into(),
                        ))),
                    }
                }
                Ok(None) => {
                    let guarded = context
                        .schemas
                        .lock()
                        .map_err(|_| CoreError::Invalid("schema registry unavailable".into()))
                        .map(|registry| {
                            let resolved = plugin_request
                                .action
                                .as_ref()
                                .or_else(|| registry.suggestion_primary.get(&plugin_request.item))
                                .or_else(|| registry.catalog_primary.get(&plugin_request.item));
                            registry
                                .catalog
                                .schemas
                                .iter()
                                .chain(&registry.suggestions.schemas)
                                .any(|binding| {
                                    binding.executable.target.item == plugin_request.item
                                        && binding.executable.action.as_ref() == resolved
                                })
                        });
                    match guarded {
                        Ok(false) => invoke_callback(|| plugin.execute_outcome(plugin_request, context)),
                        _ => Ok(Err(CoreError::Invalid("typed arguments required".into()))),
                    }
                }
                _ => Ok(Err(CoreError::Invalid(
                    "structured views unsupported or invalid".into(),
                ))),
            }
        }
    };
    if context.cancellation().is_cancelled() {
        callback = Ok(Err(CoreError::Cancelled));
    } else if deadline_ms != 0 && started.elapsed().as_millis() >= u128::from(deadline_ms) {
        callback = Ok(Err(CoreError::Invalid("execution deadline exceeded".into())));
    }
    let mut navigation = None;
    let (outcome, page_id, error) = match callback {
        Ok(Ok(ExecuteOutcome::Completed)) => (ExecuteOutcomeCode::from_i32(1), String::new(), None),
        // An action that opened a page reports the page rather than
        // completion: the host keeps the launcher open on that surface, so
        // folding this onto the OK outcome would dismiss the window the page
        // was about to be drawn in.
        Ok(Ok(ExecuteOutcome::ShowPage { page_id })) => (ExecuteOutcomeCode::from_i32(4), page_id, None),
        Ok(Ok(ExecuteOutcome::Navigate(route))) => {
            match crikey_native_protocol::structured_view::to_navigation(&route) {
                Ok(value) if context.structured_views => {
                    navigation = Some(value);
                    (ExecuteOutcomeCode::from_i32(5), String::new(), None)
                }
                _ => (
                    ExecuteOutcomeCode::from_i32(2),
                    String::new(),
                    Some(structured_error(
                        "structured navigation unsupported or invalid",
                        request_id,
                    )),
                ),
            }
        }
        Ok(Err(error)) => (
            ExecuteOutcomeCode::from_i32(2),
            String::new(),
            Some(structured_error(&diagnostic_detail(&error), request_id)),
        ),
        Err(detail) => (
            ExecuteOutcomeCode::from_i32(2),
            String::new(),
            Some(structured_error(&detail, request_id)),
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
                page_id,
                navigation,
                unknown: Default::default(),
            })),
            unknown: Default::default(),
        },
    )
}

/// Answers one host request for a page frame (spec 32.3).
///
/// The reply always carries the requested generation and always carries a
/// frame. A page whose plugin failed is closed rather than left as a surface
/// the user cannot dismiss, and the failure is logged instead of raised: a
/// broken page must not cost the connection every other request on it.
fn handle_page(
    plugin: &mut dyn Plugin,
    request: message::PageRequest,
    request_id: u64,
    generation: u64,
    transport: &Arc<Mutex<Box<dyn Transport>>>,
    context: &RuntimeContext,
    metrics: &RuntimeMetrics,
) -> Result<(), SdkError> {
    let page_generation = request.generation;
    let plugin_request = PageRequest {
        page_id: request.page_id,
        generation: page_generation,
        width: request.width,
        height: request.height,
        events: request
            .events
            .iter()
            .map(crikey_native_protocol::convert::from_proto_page_input)
            .collect(),
        focused: request.focused,
        palette: PagePalette {
            surface: PageColor::from_u32(request.colour_surface),
            text: PageColor::from_u32(request.colour_text),
            accent: PageColor::from_u32(request.colour_accent),
            muted: PageColor::from_u32(request.colour_muted),
        },
    };
    let callback = {
        let _in_flight = metrics.enter();
        invoke_callback(|| plugin.page(plugin_request, context))
    };
    let mut frame = match callback {
        Ok(Ok(frame)) => frame,
        Ok(Err(error)) => closing_frame(context, page_generation, &diagnostic_detail(&error)),
        Err(detail) => closing_frame(context, page_generation, &detail),
    };
    // The host drops a frame whose generation does not match the request it
    // answers, so the SDK stamps it rather than trusting a plugin to copy a
    // number it has no reason to care about: a page silently refusing to
    // repaint is far harder to diagnose than one that never drew at all.
    frame.generation = page_generation;
    // Refused frames are reported here, at the plugin that built them, and not
    // only at the host that rejects them: the host knows the frame is invalid
    // but nothing about which line of plugin code produced it.
    if let Err(error) = frame.validate() {
        frame = closing_frame(
            context,
            page_generation,
            &diagnostic_context("invalid page frame", &error),
        );
    }
    send_envelope(
        transport,
        Envelope {
            connection_id: context.connection_id,
            request_id,
            generation,
            deadline_ms: 0,
            payload: Some(Payload::PageFrame(
                crikey_native_protocol::convert::to_proto_page_frame(&frame),
            )),
            unknown: Default::default(),
        },
    )
}

/// The frame a failed page answers with: nothing drawn, page closed, reason
/// logged where the plugin author will see it.
fn closing_frame(context: &RuntimeContext, generation: u64, detail: &str) -> PageFrame {
    context.log(
        LogLevel::Error,
        &diagnostic_context("page callback failed", &detail),
    );
    PageFrame {
        generation,
        close: true,
        ..PageFrame::default()
    }
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
    let _guard = DIAGNOSTICS.lock().unwrap_or_else(|e| e.into_inner());
    let detail = if SENSITIVE.load(Ordering::Acquire) {
        "plugin callback failed"
    } else {
        detail
    };
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

fn send_envelope(transport: &Arc<Mutex<Box<dyn Transport>>>, mut envelope: Envelope) -> Result<(), SdkError> {
    let _guard = DIAGNOSTICS.lock().unwrap_or_else(|e| e.into_inner());
    if SENSITIVE.load(Ordering::Acquire) {
        let error = match envelope.payload.as_mut() {
            Some(Payload::Log(_)) => return Ok(()),
            Some(Payload::Error(error)) => Some(error),
            Some(Payload::ExecuteResult(result)) => result.error.as_mut(),
            Some(Payload::LifecycleAck(result)) => result.error.as_mut(),
            Some(Payload::ResourceResponse(result)) => result.error.as_mut(),
            Some(Payload::CatalogBatch(result)) => result.error.as_mut(),
            Some(Payload::Results(result)) => result.error.as_mut(),
            Some(Payload::CommandsResponse(result)) => {
                if !result.detail.is_empty() {
                    result.detail = "plugin callback failed".into();
                }
                None
            }
            Some(Payload::PreviewResponse(result)) => {
                if !result.detail.is_empty() {
                    result.detail = "plugin callback failed".into();
                }
                None
            }
            _ => None,
        };
        if let Some(error) = error {
            error.message = "plugin callback failed".into();
            error.detail.clear();
        }
    }
    message::validate_outbound(&envelope).map_err(SdkError::from)?;
    let mut locked = transport
        .lock()
        .map_err(|_| SdkError::Transport("transport lock poisoned".to_owned()))?;
    locked.send(&envelope).map_err(SdkError::from)
}

fn stop_plugin(
    plugin: &mut dyn Plugin,
    context: &RuntimeContext,
    metrics: &RuntimeMetrics,
    propagate_log_error: bool,
) -> Result<(), SdkError> {
    let callback = {
        let _in_flight = metrics.enter();
        invoke_callback(|| plugin.stop(context))
    };
    match callback {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            return Err(SdkError::Protocol(diagnostic_context(
                "plugin stop failed",
                &error,
            )));
        }
        Err(detail) => {
            return Err(SdkError::Protocol(diagnostic_context(
                "plugin stop failed",
                &detail,
            )));
        }
    }
    if propagate_log_error {
        if let Some(error) = context.take_log_error() {
            return Err(error);
        }
    } else {
        let _ = context.take_log_error();
    }
    Ok(())
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
    presentation: Option<message::ResultPresentation>,
    publication: Publication,
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
            presentation: None,
            publication: Publication::default(),
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
        self.wait_for_credit_until(None, false)
    }

    fn wait_for_credit_until(
        &mut self,
        deadline: Option<Instant>,
        cancellable: bool,
    ) -> Result<(), CoreError> {
        loop {
            if self.input.shutdown
                || (cancellable && self.cancellation.cancellation_flag())
                || deadline.is_some_and(|deadline| Instant::now() >= deadline)
            {
                return Err(CoreError::Cancelled);
            }
            if self.credits.remaining != 0 && !self.credits.paused {
                return Ok(());
            }
            let received = match deadline {
                Some(deadline) => self.input.next_raw_until(deadline),
                None => self.input.next_raw().map(Some),
            };
            let envelope = received
                .map_err(|error| {
                    self.transport_error = Some(error.clone());
                    CoreError::Invalid("transport closed while waiting for credit".to_owned())
                })?
                .ok_or(CoreError::Cancelled)?;
            match envelope.payload {
                Some(Payload::Flow(flow)) => self.credits.grant(&flow),
                Some(Payload::Cancel(_)) => {
                    self.cancellation
                        .cancel((envelope.request_id, envelope.generation));
                }
                Some(Payload::Shutdown(_)) => {
                    self.input.mark_shutdown();
                    return Err(CoreError::Cancelled);
                }
                _ => {
                    self.input.stash(envelope).map_err(|error| {
                        self.transport_error = Some(error.clone());
                        CoreError::Invalid(error.to_string())
                    })?;
                }
            }
        }
    }

    /// `ResultBatch` carries the same repeated `Item` field as `CatalogBatch`
    /// and is bounded the same way: by what the receiver can decode, not by
    /// `MAX_FRAME_BYTES`. Pending presentation is sent as its own partial
    /// frame before allocating item chunks: its independent cap does not fit
    /// inside the item chunker's overhead reserve. Neither that frame nor an
    /// item chunk grants authority before successful terminal validation.
    fn send_batch(
        &mut self,
        state: BatchState,
        items: Vec<Item>,
        error: Option<StructuredError>,
    ) -> Result<()> {
        if !items.is_empty() && self.presentation.is_some() {
            self.send_one(BatchState::from_i32(1), Vec::new(), None)?;
        }
        let proto_items: Vec<message::Item> = items
            .iter()
            .map(crikey_native_protocol::convert::to_proto_item)
            .collect();
        let mut remaining = proto_items.as_slice();
        loop {
            let take = if remaining.is_empty() {
                0
            } else {
                message::max_decodable_items(remaining)
            };
            let (chunk, rest) = remaining.split_at(take);
            remaining = rest;
            let last = remaining.is_empty();
            self.send_one(state, chunk.to_vec(), if last { error.clone() } else { None })?;
            if last {
                return Ok(());
            }
        }
    }

    fn send_one(
        &mut self,
        state: BatchState,
        proto_items: Vec<message::Item>,
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
                presentation: self.presentation.take(),
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
        if self.publication.validate().is_err() {
            return self.finish_failed("invalid presentation binding");
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
        self.publication.record(&items);
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
    presentation: Option<message::ResultPresentation>,
    publication: Publication,
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
            presentation: None,
            publication: Publication::default(),
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
                    if self.cancellation.cancellation_flag() {
                        return Err(CoreError::Cancelled);
                    }
                }
                Some(Payload::Shutdown(_)) => {
                    self.input.mark_shutdown();
                    return Err(CoreError::Cancelled);
                }
                _ => {
                    self.input.stash(envelope).map_err(|error| {
                        self.transport_error = Some(error.clone());
                        CoreError::Invalid(error.to_string())
                    })?;
                }
            }
            if self.input.shutdown {
                return Err(CoreError::Cancelled);
            }
        }
        Ok(())
    }

    /// Emits `items` as one or more `CatalogBatch` frames.
    ///
    /// Splitting is not an optimisation. A batch is bounded by what the
    /// receiver can decode, not by `MAX_FRAME_BYTES`, and the two are far
    /// apart for repeated items; a plugin that handed the whole catalog to
    /// one frame would produce a frame the host accepts on the wire and then
    /// refuses while decoding, and would be disconnected for it.
    /// Pending presentation is sent in a standalone nonterminal frame before
    /// allocating item chunks, keeping its independent budget out of the item
    /// chunker's overhead reserve. The ordinary credit and sequence accounting
    /// applies, and publication authority still waits for terminal validation.
    fn send(&mut self, items: Vec<Item>, done: bool, error: Option<StructuredError>) -> Result<()> {
        if !items.is_empty() && self.presentation.is_some() {
            self.send_one(Vec::new(), false, None)?;
        }
        let proto_items: Vec<message::Item> = items
            .iter()
            .map(crikey_native_protocol::convert::to_proto_item)
            .collect();
        let mut remaining = proto_items.as_slice();
        loop {
            let take = if remaining.is_empty() {
                0
            } else {
                message::max_decodable_items(remaining)
            };
            let (chunk, rest) = remaining.split_at(take);
            remaining = rest;
            let last = remaining.is_empty();
            self.send_one(
                chunk.to_vec(),
                done && last,
                if last { error.clone() } else { None },
            )?;
            if last {
                return Ok(());
            }
        }
    }

    fn send_one(
        &mut self,
        proto_items: Vec<message::Item>,
        done: bool,
        error: Option<StructuredError>,
    ) -> Result<()> {
        self.wait_for_credit()?;
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
                presentation: self.presentation.take(),
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
        if failure.is_none() {
            self.publication.validate()?;
        }
        let error = failure.map(|detail| structured_error(&detail, self.request_id));
        self.send(Vec::new(), true, error)?;
        self.terminal = true;
        Ok(())
    }
}

impl CatalogSink for CatalogSinkImpl<'_> {
    fn emit_batch(&mut self, items: Vec<Item>) -> Result<()> {
        self.publication.record(&items);
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
