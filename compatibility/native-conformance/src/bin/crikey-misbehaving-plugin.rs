//! Deliberately non-conforming native plugin used by host safety tests.
//!
//! Unlike `crikey-conformance-plugin`, this binary does not use the SDK serve
//! loop. It hand-drives the frozen protocol so the host can be tested against
//! invalid versions, invalid tokens, oversized frames, credit floods and a
//! peer that never answers (spec 12.3, 12.4, 16.3).

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use crikey_native_protocol::frame;
use crikey_native_protocol::message::{
    BatchState, CatalogBatch, Envelope, Handshake, Item, LifecycleAck, LifecycleKind, LogLevel, LogRecord,
    Payload, ResultBatch,
};
use crikey_native_protocol::transport::{self, Transport};
use crikey_native_protocol::wire::UnknownFields;
use crikey_native_protocol::{
    Endpoint, ProtocolError, ENV_ENDPOINT, ENV_SESSION_TOKEN, MAX_FRAME_BYTES, PROTOCOL_VERSION,
};

const MODE_ENV: &str = "CRIKEY_CONFORMANCE_MODE";
const MODE_FILE: &str = "conformance-mode";
const PLUGIN_ID: &str = "misbehaving";
const PLUGIN_NAME: &str = "CriKey Native Misbehaving Fixture";
const LOG_MESSAGE_BYTES: usize = 64 * 1024;
const PLUGIN_VERSION: &str = "1.0.0";

#[derive(Debug, Clone)]
enum Mode {
    Oversized,
    Flood,
    LogFlood,
    StderrFlood,
    PartialNoTerminal,
    ControlWitness,
    DuplicateSequence,
    TerminalItems,
    BadVersion(u32),
    BadToken,
    Hang,
}

fn candidate(value: Option<String>) -> Option<String> {
    value
        .map(|mode| mode.trim().to_owned())
        .filter(|mode| !mode.is_empty())
}

fn selected_mode() -> String {
    let from_file = || {
        let path: PathBuf = env::current_dir().ok()?.join(MODE_FILE);
        fs::read_to_string(path).ok()
    };

    candidate(env::var(MODE_ENV).ok())
        .or_else(|| candidate(env::args().nth(1)))
        .or_else(|| candidate(from_file()))
        .unwrap_or_else(|| "echo".to_owned())
}
fn parse_mode(spec: &str) -> Mode {
    match spec {
        "oversized" => Mode::Oversized,
        "flood" => Mode::Flood,
        "log-flood" => Mode::LogFlood,
        "stderr-flood" => Mode::StderrFlood,
        "partial-no-terminal" => Mode::PartialNoTerminal,
        "control-witness" => Mode::ControlWitness,
        "duplicate-sequence" => Mode::DuplicateSequence,
        "terminal-items" => Mode::TerminalItems,
        "bad-token" => Mode::BadToken,
        "hang" => Mode::Hang,
        _ if spec.starts_with("bad-version:") => Mode::BadVersion(
            spec.strip_prefix("bad-version:")
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(PROTOCOL_VERSION + 1),
        ),
        _ => Mode::Hang,
    }
}

fn required(name: &str) -> Result<String, ProtocolError> {
    env::var(name).map_err(|error| ProtocolError::Malformed(format!("{name}: {error}")))
}

fn envelope(connection_id: u64, request_id: u64, generation: u64, payload: Payload) -> Envelope {
    Envelope {
        connection_id,
        request_id,
        generation,
        deadline_ms: 0,
        payload: Some(payload),
        unknown: UnknownFields::default(),
    }
}

fn handshake(mode: &Mode, session_token: String) -> Envelope {
    let protocol_version = match mode {
        Mode::BadVersion(version) => *version,
        _ => PROTOCOL_VERSION,
    };
    let session_token = if matches!(mode, Mode::BadToken) {
        "deliberately-wrong-session-token".to_owned()
    } else {
        session_token
    };
    envelope(
        0,
        0,
        0,
        Payload::Handshake(Handshake {
            protocol_version,
            plugin_id: PLUGIN_ID.to_owned(),
            plugin_version: PLUGIN_VERSION.to_owned(),
            capabilities: vec![
                "streaming_catalog".to_owned(),
                "streaming_suggestions".to_owned(),
                "cancellation".to_owned(),
            ],
            session_token,
            plugin_name: PLUGIN_NAME.to_owned(),
            sdk_version: "raw-fixture".to_owned(),
            unknown: UnknownFields::default(),
        }),
    )
}

fn connect(endpoint: &Endpoint) -> Result<Box<dyn Transport>, ProtocolError> {
    if matches!(endpoint, Endpoint::Stdio) {
        Ok(transport::stdio())
    } else {
        transport::connect(endpoint, Some(Duration::from_secs(10)))
    }
}

fn send_lifecycle_ack(
    transport: &mut dyn Transport,
    connection_id: u64,
    request_id: u64,
    generation: u64,
    kind: LifecycleKind,
) -> Result<(), ProtocolError> {
    transport.send(&envelope(
        connection_id,
        request_id,
        generation,
        Payload::LifecycleAck(LifecycleAck {
            kind,
            ok: true,
            error: None,
            unknown: UnknownFields::default(),
        }),
    ))
}

fn send_empty_catalog(
    transport: &mut dyn Transport,
    connection_id: u64,
    request_id: u64,
    generation: u64,
) -> Result<(), ProtocolError> {
    transport.send(&envelope(
        connection_id,
        request_id,
        generation,
        Payload::CatalogBatch(CatalogBatch {
            items: Vec::new(),
            done: true,
            sequence: 0,
            error: None,
            unknown: UnknownFields::default(),
        }),
    ))
}

fn flood(
    transport: &mut dyn Transport,
    connection_id: u64,
    request_id: u64,
    generation: u64,
) -> Result<(), ProtocolError> {
    let mut sequence = 0u64;
    loop {
        transport.send(&envelope(
            connection_id,
            request_id,
            generation,
            Payload::Results(ResultBatch {
                state: BatchState::Partial,
                items: Vec::new(),
                sequence,
                error: None,
                unknown: UnknownFields::default(),
            }),
        ))?;
        sequence = sequence.saturating_add(1);
    }
}

fn log_flood(
    transport: &mut dyn Transport,
    connection_id: u64,
    request_id: u64,
    generation: u64,
) -> Result<(), ProtocolError> {
    let message = "x".repeat(LOG_MESSAGE_BYTES);
    loop {
        transport.send(&envelope(
            connection_id,
            request_id,
            generation,
            Payload::Log(LogRecord {
                level: LogLevel::Info,
                message: message.clone(),
                timestamp_ms: 0,
                unknown: UnknownFields::default(),
            }),
        ))?;
    }
}
fn stderr_flood(
    transport: &mut dyn Transport,
    connection_id: u64,
    request_id: u64,
    generation: u64,
) -> Result<(), ProtocolError> {
    let message = vec![b'x'; LOG_MESSAGE_BYTES];
    let mut stderr = io::stderr().lock();
    for _ in 0..64 {
        stderr
            .write_all(&message)
            .map_err(|error| ProtocolError::Io(error.to_string()))?;
    }
    stderr
        .flush()
        .map_err(|error| ProtocolError::Io(error.to_string()))?;
    transport.send(&envelope(
        connection_id,
        request_id,
        generation,
        Payload::Results(ResultBatch {
            state: BatchState::Final,
            items: Vec::new(),
            sequence: 0,
            error: None,
            unknown: UnknownFields::default(),
        }),
    ))
}
fn partial_no_terminal(
    transport: &mut dyn Transport,
    connection_id: u64,
    request_id: u64,
    generation: u64,
) -> Result<(), ProtocolError> {
    transport.send(&envelope(
        connection_id,
        request_id,
        generation,
        Payload::Results(ResultBatch {
            state: BatchState::Partial,
            items: Vec::new(),
            sequence: 0,
            error: None,
            unknown: UnknownFields::default(),
        }),
    ))?;
    loop {
        thread::park();
    }
}

fn duplicate_sequence(
    transport: &mut dyn Transport,
    connection_id: u64,
    request_id: u64,
    generation: u64,
) -> Result<(), ProtocolError> {
    for state in [BatchState::Partial, BatchState::Final] {
        transport.send(&envelope(
            connection_id,
            request_id,
            generation,
            Payload::Results(ResultBatch {
                state,
                items: Vec::new(),
                sequence: 0,
                error: None,
                unknown: UnknownFields::default(),
            }),
        ))?;
    }
    loop {
        thread::park();
    }
}

fn terminal_items(
    transport: &mut dyn Transport,
    connection_id: u64,
    request_id: u64,
    generation: u64,
) -> Result<(), ProtocolError> {
    let items = (0..8)
        .map(|index| control_item(index, "terminal"))
        .collect();
    transport.send(&envelope(
        connection_id,
        request_id,
        generation,
        Payload::Results(ResultBatch {
            state: BatchState::Final,
            items,
            sequence: 0,
            error: None,
            unknown: UnknownFields::default(),
        }),
    ))
}

fn control_item(index: usize, kind: &str) -> Item {
    Item {
        stable_id: format!("control-{index}"),
        label: kind.to_owned(),
        description: String::new(),
        target: format!("control://{kind}"),
        category: String::new(),
        search_terms: Vec::new(),
        icon_reference: String::new(),
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
        argument_policy: String::new(),
        hit_policy: String::new(),
        unknown: UnknownFields::default(),
    }
}

fn trigger_item(index: usize) -> Item {
    Item {
        stable_id: format!("control-trigger-{index}"),
        label: "control witness trigger".to_owned(),
        description: String::new(),
        target: "control://trigger".to_owned(),
        category: String::new(),
        search_terms: Vec::new(),
        icon_reference: String::new(),
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
        argument_policy: String::new(),
        hit_policy: String::new(),
        unknown: UnknownFields::default(),
    }
}

fn control_witness(
    transport: &mut dyn Transport,
    connection_id: u64,
    request_id: u64,
    generation: u64,
) -> Result<(), ProtocolError> {
    transport.send(&envelope(
        connection_id,
        request_id,
        generation,
        Payload::Results(ResultBatch {
            state: BatchState::Partial,
            items: vec![trigger_item(0), trigger_item(1)],
            sequence: 0,
            error: None,
            unknown: UnknownFields::default(),
        }),
    ))?;

    let mut controls = Vec::new();
    loop {
        let incoming = transport.recv()?;
        match incoming.payload {
            Some(Payload::Cancel(_)) => controls.push("cancel"),
            Some(Payload::Flow(_)) => controls.push("flow"),
            Some(Payload::Shutdown(_)) => {
                controls.push("shutdown");
                let items = controls
                    .iter()
                    .enumerate()
                    .map(|(index, kind)| control_item(index, kind))
                    .collect();
                transport.send(&envelope(
                    connection_id,
                    request_id,
                    generation,
                    Payload::Results(ResultBatch {
                        state: BatchState::Final,
                        items,
                        sequence: 1,
                        error: None,
                        unknown: UnknownFields::default(),
                    }),
                ))?;
                return Ok(());
            }
            _ => {}
        }
        let saw_cancel = controls.contains(&"cancel");
        let saw_flow = controls.contains(&"flow");
        if saw_cancel && saw_flow {
            let items = controls
                .iter()
                .enumerate()
                .map(|(index, kind)| control_item(index, kind))
                .collect();
            transport.send(&envelope(
                connection_id,
                request_id,
                generation,
                Payload::Results(ResultBatch {
                    state: BatchState::Final,
                    items,
                    sequence: 1,
                    error: None,
                    unknown: Default::default(),
                }),
            ))?;
            return Ok(());
        }
    }
}

fn write_oversized(endpoint: &Endpoint) -> Result<(), ProtocolError> {
    if !matches!(endpoint, Endpoint::Stdio) {
        return Err(ProtocolError::Malformed(
            "oversized fixture requires stdio transport".to_owned(),
        ));
    }

    // Exercise the shared framing API for one valid frame on a sink, then
    // deliberately hand-roll only the invalid length prefix sent to the host.
    // The payload must be non-empty: the shared writer rejects a zero-length
    // frame, and this call exists only to prove the fixture drives the real
    // framing API rather than reimplementing it.
    frame::write_frame(&mut io::sink(), &[0u8])?;
    let declared = u32::try_from(MAX_FRAME_BYTES.saturating_add(1))
        .map_err(|_| ProtocolError::Malformed("frame limit does not fit u32".to_owned()))?;
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(&declared.to_be_bytes())
        .map_err(|error| ProtocolError::Io(error.to_string()))?;
    stdout
        .flush()
        .map_err(|error| ProtocolError::Io(error.to_string()))
}

fn run() -> Result<(), ProtocolError> {
    let mode = parse_mode(&selected_mode());
    let endpoint = Endpoint::parse(&required(ENV_ENDPOINT)?)?;
    let session_token = required(ENV_SESSION_TOKEN)?;
    let mut transport = connect(&endpoint)?;
    transport.send(&handshake(&mode, session_token))?;

    let ack = transport.recv()?;
    let connection_id = ack.connection_id;
    match ack.payload {
        Some(Payload::HandshakeAck(ack)) if !ack.accepted => return Ok(()),
        Some(Payload::HandshakeAck(_)) => {}
        _ => {
            return Err(ProtocolError::Malformed(
                "host did not answer with handshake_ack".to_owned(),
            ));
        }
    }

    match mode {
        Mode::Oversized => {
            write_oversized(&endpoint)?;
            thread::park();
            Ok(())
        }
        Mode::LogFlood => log_flood(&mut *transport, connection_id, 0, 0),
        Mode::StderrFlood
        | Mode::PartialNoTerminal
        | Mode::Flood
        | Mode::ControlWitness
        | Mode::DuplicateSequence
        | Mode::TerminalItems
        | Mode::Hang => loop_messages(&mut *transport, connection_id, mode),
        Mode::BadVersion(_) | Mode::BadToken => Ok(()),
    }
}

fn loop_messages(transport: &mut dyn Transport, connection_id: u64, mode: Mode) -> Result<(), ProtocolError> {
    loop {
        let incoming = transport.recv()?;
        let request_id = incoming.request_id;
        let generation = incoming.generation;
        match incoming.payload {
            Some(Payload::Lifecycle(lifecycle)) => {
                send_lifecycle_ack(transport, connection_id, request_id, generation, lifecycle.kind)?;
            }
            Some(Payload::CatalogRequest(_)) => {
                send_empty_catalog(transport, connection_id, request_id, generation)?;
            }
            Some(Payload::Suggest(_)) => match &mode {
                Mode::Flood => return flood(transport, connection_id, request_id, generation),
                Mode::StderrFlood => {
                    return stderr_flood(transport, connection_id, request_id, generation);
                }
                Mode::PartialNoTerminal => {
                    return partial_no_terminal(transport, connection_id, request_id, generation);
                }
                Mode::DuplicateSequence => {
                    return duplicate_sequence(transport, connection_id, request_id, generation);
                }
                Mode::TerminalItems => {
                    return terminal_items(transport, connection_id, request_id, generation);
                }
                Mode::ControlWitness => {
                    control_witness(transport, connection_id, request_id, generation)?;
                }
                _ => {}
            },
            Some(Payload::Shutdown(_)) => return Ok(()),
            _ => {}
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("crikey-misbehaving-plugin: {error:?}");
        std::process::exit(1);
    }
}
