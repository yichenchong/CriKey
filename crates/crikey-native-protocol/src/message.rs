//! Hand-written bindings for `sdk/protocol/crikey/v1/plugin.proto`.
//!
//! Every decoder is total over arbitrary bytes and every message retains raw
//! unknown fields for additive protocol evolution (spec 16.3).

use std::collections::BTreeMap;

pub use crate::wire::UnknownFields;
use crate::wire::{
    decode_bytes, decode_field_varint, decode_string, expect_wire, push_decoded, put_bytes, put_message,
    put_string, put_varint, read_field, DecodeBudget, WireType,
};
use crate::{Message, ProtocolError};

trait DecodeWithBudget: Sized {
    fn decode_with_budget(bytes: &[u8], budget: &mut DecodeBudget) -> Result<Self, ProtocolError>;
}

macro_rules! proto_enum {
    ($name:ident, $default:ident, $( $variant:ident = $value:expr ),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name {
            $default,
            $( $variant ),+
        }

        impl $name {
            pub fn from_i32(value: i32) -> Self {
                match value {
                    $( $value => Self::$variant, )+
                    _ => Self::$default,
                }
            }

            pub fn as_i32(&self) -> i32 {
                match self {
                    Self::$default => 0,
                    $( Self::$variant => $value, )+
                }
            }
        }
    };
}

proto_enum!(
    LifecycleKind,
    KindUnspecified,
    Start = 1,
    Stop = 2,
    Activated = 3,
    Deactivated = 4
);
proto_enum!(
    BatchState,
    StateUnspecified,
    Partial = 1,
    Final = 2,
    Cancelled = 3,
    Failed = 4
);
proto_enum!(
    ExecuteOutcomeCode,
    OutcomeUnspecified,
    Ok = 1,
    Failed = 2,
    Unsupported = 3
);
proto_enum!(
    EventKind,
    KindUnspecified,
    Filesystem = 1,
    Network = 2,
    Configuration = 3,
    Applications = 4,
    Custom = 5
);
proto_enum!(
    ResourceKind,
    KindUnspecified,
    Icon = 1,
    File = 2,
    Configuration = 3
);
proto_enum!(
    LogLevel,
    LevelUnspecified,
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5
);
proto_enum!(
    ErrorCode,
    CodeUnspecified,
    Protocol = 1,
    Plugin = 2,
    Timeout = 3,
    Cancelled = 4,
    Resource = 5,
    Unsupported = 6
);

fn unknown(
    input: &[u8],
    start: usize,
    end: usize,
    fields: &mut UnknownFields,
    budget: &mut DecodeBudget,
) -> Result<(), ProtocolError> {
    fields.push_raw_bounded(&input[start..end], budget)
}

fn nested<M: DecodeWithBudget>(
    field: crate::wire::Field<'_>,
    budget: &mut DecodeBudget,
) -> Result<M, ProtocolError> {
    expect_wire(field, WireType::Length)?;
    M::decode_with_budget(field.value, budget)
}

fn map_entry(
    field: crate::wire::Field<'_>,
    budget: &mut DecodeBudget,
) -> Result<(String, String), ProtocolError> {
    expect_wire(field, WireType::Length)?;
    let bytes = field.value;
    let mut cursor = 0;
    let mut key = String::new();
    let mut value = String::new();
    while cursor < bytes.len() {
        let field = read_field(bytes, &mut cursor)?;
        match field.number {
            1 => key = decode_string(field, budget)?,
            2 => value = decode_string(field, budget)?,
            _ => {}
        }
    }
    Ok((key, value))
}

fn insert_map(
    map: &mut BTreeMap<String, String>,
    key: String,
    value: String,
    budget: &mut DecodeBudget,
) -> Result<(), ProtocolError> {
    budget.charge_map_entry()?;
    map.insert(key, value);
    Ok(())
}

fn put_map(field: u32, key: &str, value: &str, out: &mut Vec<u8>) {
    let mut entry = Vec::new();
    if !key.is_empty() {
        put_string(1, key, &mut entry);
    }
    if !value.is_empty() {
        put_string(2, value, &mut entry);
    }
    put_bytes(field, &entry, out);
}

fn finish(mut out: Vec<u8>, unknown: &UnknownFields) -> Vec<u8> {
    out.extend_from_slice(unknown.as_bytes());
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub connection_id: u64,
    pub request_id: u64,
    pub generation: u64,
    pub deadline_ms: u64,
    pub payload: Option<Payload>,
    pub unknown: UnknownFields,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    Handshake(Handshake),
    HandshakeAck(HandshakeAck),
    Suggest(SuggestRequest),
    Results(ResultBatch),
    Cancel(Cancel),
    Shutdown(Shutdown),
    CatalogRequest(CatalogRequest),
    CatalogBatch(CatalogBatch),
    Execute(ExecuteRequest),
    ExecuteResult(ExecuteResult),
    Configuration(ConfigurationChange),
    Event(Event),
    Log(LogRecord),
    HealthCheck(HealthCheck),
    HealthReport(HealthReport),
    Error(StructuredError),
    Flow(FlowControl),
    ResourceRequest(ResourceRequest),
    ResourceResponse(ResourceResponse),
    Lifecycle(Lifecycle),
    LifecycleAck(LifecycleAck),
}

impl Payload {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Handshake(_) => "handshake",
            Self::HandshakeAck(_) => "handshake_ack",
            Self::Suggest(_) => "suggest",
            Self::Results(_) => "results",
            Self::Cancel(_) => "cancel",
            Self::Shutdown(_) => "shutdown",
            Self::CatalogRequest(_) => "catalog_request",
            Self::CatalogBatch(_) => "catalog_batch",
            Self::Execute(_) => "execute",
            Self::ExecuteResult(_) => "execute_result",
            Self::Configuration(_) => "configuration",
            Self::Event(_) => "event",
            Self::Log(_) => "log",
            Self::HealthCheck(_) => "health_check",
            Self::HealthReport(_) => "health_report",
            Self::Error(_) => "error",
            Self::Flow(_) => "flow",
            Self::ResourceRequest(_) => "resource_request",
            Self::ResourceResponse(_) => "resource_response",
            Self::Lifecycle(_) => "lifecycle",
            Self::LifecycleAck(_) => "lifecycle_ack",
        }
    }
}

impl Message for Envelope {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.connection_id != 0 {
            put_varint(1, self.connection_id, &mut out);
        }
        if self.request_id != 0 {
            put_varint(2, self.request_id, &mut out);
        }
        if self.generation != 0 {
            put_varint(3, self.generation, &mut out);
        }
        if self.deadline_ms != 0 {
            put_varint(4, self.deadline_ms, &mut out);
        }
        if let Some(payload) = &self.payload {
            match payload {
                Payload::Handshake(value) => put_message(10, value, &mut out),
                Payload::HandshakeAck(value) => put_message(11, value, &mut out),
                Payload::Suggest(value) => put_message(12, value, &mut out),
                Payload::Results(value) => put_message(13, value, &mut out),
                Payload::Cancel(value) => put_message(14, value, &mut out),
                Payload::Shutdown(value) => put_message(15, value, &mut out),
                Payload::CatalogRequest(value) => put_message(16, value, &mut out),
                Payload::CatalogBatch(value) => put_message(17, value, &mut out),
                Payload::Execute(value) => put_message(18, value, &mut out),
                Payload::ExecuteResult(value) => put_message(19, value, &mut out),
                Payload::Configuration(value) => put_message(20, value, &mut out),
                Payload::Event(value) => put_message(21, value, &mut out),
                Payload::Log(value) => put_message(22, value, &mut out),
                Payload::HealthCheck(value) => put_message(23, value, &mut out),
                Payload::HealthReport(value) => put_message(24, value, &mut out),
                Payload::Error(value) => put_message(25, value, &mut out),
                Payload::Flow(value) => put_message(26, value, &mut out),
                Payload::ResourceRequest(value) => put_message(27, value, &mut out),
                Payload::ResourceResponse(value) => put_message(28, value, &mut out),
                Payload::Lifecycle(value) => put_message(29, value, &mut out),
                Payload::LifecycleAck(value) => put_message(30, value, &mut out),
            }
        }
        finish(out, &self.unknown)
    }

    fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let mut budget = DecodeBudget::new();
        Self::decode_with_budget(bytes, &mut budget)
    }
}

impl DecodeWithBudget for Envelope {
    fn decode_with_budget(bytes: &[u8], budget: &mut DecodeBudget) -> Result<Self, ProtocolError> {
        let mut value = Self {
            connection_id: 0,
            request_id: 0,
            generation: 0,
            deadline_ms: 0,
            payload: None,
            unknown: UnknownFields::default(),
        };
        let mut cursor = 0;
        while cursor < bytes.len() {
            let start = cursor;
            let field = read_field(bytes, &mut cursor)?;
            match field.number {
                1 => value.connection_id = decode_field_varint(field)?,
                2 => value.request_id = decode_field_varint(field)?,
                3 => value.generation = decode_field_varint(field)?,
                4 => value.deadline_ms = decode_field_varint(field)?,
                10 => value.payload = Some(Payload::Handshake(nested(field, budget)?)),
                11 => value.payload = Some(Payload::HandshakeAck(nested(field, budget)?)),
                12 => value.payload = Some(Payload::Suggest(nested(field, budget)?)),
                13 => value.payload = Some(Payload::Results(nested(field, budget)?)),
                14 => value.payload = Some(Payload::Cancel(nested(field, budget)?)),
                15 => value.payload = Some(Payload::Shutdown(nested(field, budget)?)),
                16 => value.payload = Some(Payload::CatalogRequest(nested(field, budget)?)),
                17 => value.payload = Some(Payload::CatalogBatch(nested(field, budget)?)),
                18 => value.payload = Some(Payload::Execute(nested(field, budget)?)),
                19 => value.payload = Some(Payload::ExecuteResult(nested(field, budget)?)),
                20 => value.payload = Some(Payload::Configuration(nested(field, budget)?)),
                21 => value.payload = Some(Payload::Event(nested(field, budget)?)),
                22 => value.payload = Some(Payload::Log(nested(field, budget)?)),
                23 => value.payload = Some(Payload::HealthCheck(nested(field, budget)?)),
                24 => value.payload = Some(Payload::HealthReport(nested(field, budget)?)),
                25 => value.payload = Some(Payload::Error(nested(field, budget)?)),
                26 => value.payload = Some(Payload::Flow(nested(field, budget)?)),
                27 => value.payload = Some(Payload::ResourceRequest(nested(field, budget)?)),
                28 => value.payload = Some(Payload::ResourceResponse(nested(field, budget)?)),
                29 => value.payload = Some(Payload::Lifecycle(nested(field, budget)?)),
                30 => value.payload = Some(Payload::LifecycleAck(nested(field, budget)?)),
                _ => unknown(bytes, start, field.end, &mut value.unknown, budget)?,
            }
        }
        Ok(value)
    }
}

macro_rules! impl_simple {
    ($type:ident { $( $field_name:ident : $fty:ty = $default:expr ),* $(,)? } encode($this:ident, $out:ident) { $( $enc:tt )* } decode($value:ident, $field:ident, $budget:ident) { $( $number:literal => $body:expr, )* }) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $type { $( pub $field_name: $fty, )* pub unknown: UnknownFields }
        impl Message for $type {
            fn encode(&self) -> Vec<u8> {
                let $this = self;
                let mut $out = Vec::new();
                $( $enc )*
                finish($out, &$this.unknown)
            }
            fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
                let mut budget = DecodeBudget::new();
                Self::decode_with_budget(bytes, &mut budget)
            }
        }
        impl DecodeWithBudget for $type {
            fn decode_with_budget(
                bytes: &[u8],
                $budget: &mut DecodeBudget,
            ) -> Result<Self, ProtocolError> {
                let mut $value = Self { $( $field_name: $default, )* unknown: UnknownFields::default() };
                let mut cursor = 0;
                while cursor < bytes.len() {
                    let start = cursor;
                    let $field = read_field(bytes, &mut cursor)?;
                    match $field.number {
                        $( $number => $body, )*
                        _ => unknown(bytes, start, $field.end, &mut $value.unknown, $budget)?,
                    }
                }
                Ok($value)
            }
        }
    };
}

impl_simple!(Handshake {
    protocol_version: u32 = 0,
    plugin_id: String = String::new(),
    plugin_version: String = String::new(),
    capabilities: Vec<String> = Vec::new(),
    session_token: String = String::new(),
    plugin_name: String = String::new(),
    sdk_version: String = String::new()
} encode(this, out) {
    if this.protocol_version != 0 { put_varint(1, u64::from(this.protocol_version), &mut out); }
    if !this.plugin_id.is_empty() { put_string(2, &this.plugin_id, &mut out); }
    if !this.plugin_version.is_empty() { put_string(3, &this.plugin_version, &mut out); }
    for value in &this.capabilities { put_string(4, value, &mut out); }
    if !this.session_token.is_empty() { put_string(5, &this.session_token, &mut out); }
    if !this.plugin_name.is_empty() { put_string(6, &this.plugin_name, &mut out); }
    if !this.sdk_version.is_empty() { put_string(7, &this.sdk_version, &mut out); }
} decode(value, field, budget) {
    1 => value.protocol_version = decode_field_varint(field)? as u32,
    2 => value.plugin_id = decode_string(field, budget)?,
    3 => value.plugin_version = decode_string(field, budget)?,
    4 => push_decoded(&mut value.capabilities, decode_string(field, budget)?, budget)?,
    5 => value.session_token = decode_string(field, budget)?,
    6 => value.plugin_name = decode_string(field, budget)?,
    7 => value.sdk_version = decode_string(field, budget)?,
});

impl_simple!(HandshakeAck {
    protocol_version: u32 = 0,
    host_capabilities: Vec<String> = Vec::new(),
    host_version: String = String::new(),
    accepted: bool = false,
    reject_reason: String = String::new(),
    max_frame_bytes: u64 = 0,
    initial_credits: u32 = 0
} encode(this, out) {
    if this.protocol_version != 0 { put_varint(1, u64::from(this.protocol_version), &mut out); }
    for value in &this.host_capabilities { put_string(2, value, &mut out); }
    if !this.host_version.is_empty() { put_string(3, &this.host_version, &mut out); }
    if this.accepted { put_varint(4, 1, &mut out); }
    if !this.reject_reason.is_empty() { put_string(5, &this.reject_reason, &mut out); }
    if this.max_frame_bytes != 0 { put_varint(6, this.max_frame_bytes, &mut out); }
    if this.initial_credits != 0 { put_varint(7, u64::from(this.initial_credits), &mut out); }
} decode(value, field, budget) {
    1 => value.protocol_version = decode_field_varint(field)? as u32,
    2 => push_decoded(&mut value.host_capabilities, decode_string(field, budget)?, budget)?,
    3 => value.host_version = decode_string(field, budget)?,
    4 => value.accepted = decode_field_varint(field)? != 0,
    5 => value.reject_reason = decode_string(field, budget)?,
    6 => value.max_frame_bytes = decode_field_varint(field)?,
    7 => value.initial_credits = decode_field_varint(field)? as u32,
});

impl_simple!(Lifecycle {
    kind: LifecycleKind = LifecycleKind::KindUnspecified
} encode(this, out) {
    if this.kind.as_i32() != 0 { put_varint(1, this.kind.as_i32() as u64, &mut out); }
} decode(value, field, budget) {
    1 => value.kind = LifecycleKind::from_i32(decode_field_varint(field)? as i32),
});

impl_simple!(LifecycleAck {
    kind: LifecycleKind = LifecycleKind::KindUnspecified,
    ok: bool = false,
    error: Option<StructuredError> = None
} encode(this, out) {
    if this.kind.as_i32() != 0 { put_varint(1, this.kind.as_i32() as u64, &mut out); }
    if this.ok { put_varint(2, 1, &mut out); }
    if let Some(error) = &this.error { put_message(3, error, &mut out); }
} decode(value, field, budget) {
    1 => value.kind = LifecycleKind::from_i32(decode_field_varint(field)? as i32),
    2 => value.ok = decode_field_varint(field)? != 0,
    3 => value.error = Some(nested(field, budget)?),
});

impl_simple!(CatalogRequest {
    max_items: u64 = 0
} encode(this, out) {
    if this.max_items != 0 { put_varint(1, this.max_items, &mut out); }
} decode(value, field, budget) {
    1 => value.max_items = decode_field_varint(field)?,
});

impl_simple!(CatalogBatch {
    items: Vec<Item> = Vec::new(),
    done: bool = false,
    sequence: u64 = 0,
    error: Option<StructuredError> = None
} encode(this, out) {
    for item in &this.items { put_message(1, item, &mut out); }
    if this.done { put_varint(2, 1, &mut out); }
    if this.sequence != 0 { put_varint(3, this.sequence, &mut out); }
    if let Some(error) = &this.error { put_message(4, error, &mut out); }
} decode(value, field, budget) {
    1 => push_decoded(&mut value.items, nested(field, budget)?, budget)?,
    2 => value.done = decode_field_varint(field)? != 0,
    3 => value.sequence = decode_field_varint(field)?,
    4 => value.error = Some(nested(field, budget)?),
});

impl_simple!(SuggestRequest {
    text: String = String::new(),
    normalized_text: String = String::new(),
    selected_item_id: String = String::new(),
    max_items: u64 = 0,
    max_batches: u64 = 0
} encode(this, out) {
    if !this.text.is_empty() { put_string(1, &this.text, &mut out); }
    if !this.normalized_text.is_empty() { put_string(2, &this.normalized_text, &mut out); }
    if !this.selected_item_id.is_empty() { put_string(3, &this.selected_item_id, &mut out); }
    if this.max_items != 0 { put_varint(4, this.max_items, &mut out); }
    if this.max_batches != 0 { put_varint(5, this.max_batches, &mut out); }
} decode(value, field, budget) {
    1 => value.text = decode_string(field, budget)?,
    2 => value.normalized_text = decode_string(field, budget)?,
    3 => value.selected_item_id = decode_string(field, budget)?,
    4 => value.max_items = decode_field_varint(field)?,
    5 => value.max_batches = decode_field_varint(field)?,
});

impl_simple!(Action {
    action_id: String = String::new(),
    label: String = String::new(),
    description: String = String::new(),
    icon_reference: String = String::new(),
    execution_policy: String = String::new()
} encode(this, out) {
    if !this.action_id.is_empty() { put_string(1, &this.action_id, &mut out); }
    if !this.label.is_empty() { put_string(2, &this.label, &mut out); }
    if !this.description.is_empty() { put_string(3, &this.description, &mut out); }
    if !this.icon_reference.is_empty() { put_string(4, &this.icon_reference, &mut out); }
    if !this.execution_policy.is_empty() { put_string(5, &this.execution_policy, &mut out); }
} decode(value, field, budget) {
    1 => value.action_id = decode_string(field, budget)?,
    2 => value.label = decode_string(field, budget)?,
    3 => value.description = decode_string(field, budget)?,
    4 => value.icon_reference = decode_string(field, budget)?,
    5 => value.execution_policy = decode_string(field, budget)?,
});

impl_simple!(Item {
    stable_id: String = String::new(),
    label: String = String::new(),
    description: String = String::new(),
    target: String = String::new(),
    category: String = String::new(),
    search_terms: Vec<String> = Vec::new(),
    icon_reference: String = String::new(),
    score_hint: i32 = 0,
    metadata: BTreeMap<String, String> = BTreeMap::new(),
    actions: Vec<Action> = Vec::new()
} encode(this, out) {
    if !this.stable_id.is_empty() { put_string(1, &this.stable_id, &mut out); }
    if !this.label.is_empty() { put_string(2, &this.label, &mut out); }
    if !this.description.is_empty() { put_string(3, &this.description, &mut out); }
    if !this.target.is_empty() { put_string(4, &this.target, &mut out); }
    if !this.category.is_empty() { put_string(5, &this.category, &mut out); }
    for value in &this.search_terms { put_string(6, value, &mut out); }
    if !this.icon_reference.is_empty() { put_string(7, &this.icon_reference, &mut out); }
    if this.score_hint != 0 { put_varint(8, this.score_hint as i64 as u64, &mut out); }
    for (key, value) in &this.metadata { put_map(9, key, value, &mut out); }
    for action in &this.actions { put_message(10, action, &mut out); }
} decode(value, field, budget) {
    1 => value.stable_id = decode_string(field, budget)?,
    2 => value.label = decode_string(field, budget)?,
    3 => value.description = decode_string(field, budget)?,
    4 => value.target = decode_string(field, budget)?,
    5 => value.category = decode_string(field, budget)?,
    6 => push_decoded(&mut value.search_terms, decode_string(field, budget)?, budget)?,
    7 => value.icon_reference = decode_string(field, budget)?,
    8 => value.score_hint = decode_field_varint(field)? as i32,
    9 => { let (key, map_value) = map_entry(field, budget)?; insert_map(&mut value.metadata, key, map_value, budget)?; },
    10 => push_decoded(&mut value.actions, nested(field, budget)?, budget)?,
});

impl_simple!(ResultBatch {
    state: BatchState = BatchState::StateUnspecified,
    items: Vec<Item> = Vec::new(),
    sequence: u64 = 0,
    error: Option<StructuredError> = None
} encode(this, out) {
    if this.state.as_i32() != 0 { put_varint(1, this.state.as_i32() as u64, &mut out); }
    for item in &this.items { put_message(2, item, &mut out); }
    if this.sequence != 0 { put_varint(3, this.sequence, &mut out); }
    if let Some(error) = &this.error { put_message(4, error, &mut out); }
} decode(value, field, budget) {
    1 => value.state = BatchState::from_i32(decode_field_varint(field)? as i32),
    2 => push_decoded(&mut value.items, nested(field, budget)?, budget)?,
    3 => value.sequence = decode_field_varint(field)?,
    4 => value.error = Some(nested(field, budget)?),
});

impl_simple!(Cancel {
    reason: String = String::new()
} encode(this, out) {
    if !this.reason.is_empty() { put_string(1, &this.reason, &mut out); }
} decode(value, field, budget) {
    1 => value.reason = decode_string(field, budget)?,
});

impl_simple!(ExecuteRequest {
    item_id: String = String::new(),
    action_id: String = String::new(),
    argument: String = String::new()
} encode(this, out) {
    if !this.item_id.is_empty() { put_string(1, &this.item_id, &mut out); }
    if !this.action_id.is_empty() { put_string(2, &this.action_id, &mut out); }
    if !this.argument.is_empty() { put_string(3, &this.argument, &mut out); }
} decode(value, field, budget) {
    1 => value.item_id = decode_string(field, budget)?,
    2 => value.action_id = decode_string(field, budget)?,
    3 => value.argument = decode_string(field, budget)?,
});

impl_simple!(ExecuteResult {
    outcome: ExecuteOutcomeCode = ExecuteOutcomeCode::OutcomeUnspecified,
    error: Option<StructuredError> = None
} encode(this, out) {
    if this.outcome.as_i32() != 0 { put_varint(1, this.outcome.as_i32() as u64, &mut out); }
    if let Some(error) = &this.error { put_message(2, error, &mut out); }
} decode(value, field, budget) {
    1 => value.outcome = ExecuteOutcomeCode::from_i32(decode_field_varint(field)? as i32),
    2 => value.error = Some(nested(field, budget)?),
});

impl_simple!(ConfigurationChange {
    values: BTreeMap<String, String> = BTreeMap::new(),
    complete: bool = false
} encode(this, out) {
    for (key, value) in &this.values { put_map(1, key, value, &mut out); }
    if this.complete { put_varint(2, 1, &mut out); }
} decode(value, field, budget) {
    1 => { let (key, map_value) = map_entry(field, budget)?; insert_map(&mut value.values, key, map_value, budget)?; },
    2 => value.complete = decode_field_varint(field)? != 0,
});

impl_simple!(Event {
    kind: EventKind = EventKind::KindUnspecified,
    attributes: BTreeMap<String, String> = BTreeMap::new(),
    flags: u64 = 0
} encode(this, out) {
    if this.kind.as_i32() != 0 { put_varint(1, this.kind.as_i32() as u64, &mut out); }
    for (key, value) in &this.attributes { put_map(2, key, value, &mut out); }
    if this.flags != 0 { put_varint(3, this.flags, &mut out); }
} decode(value, field, budget) {
    1 => value.kind = EventKind::from_i32(decode_field_varint(field)? as i32),
    2 => { let (key, map_value) = map_entry(field, budget)?; insert_map(&mut value.attributes, key, map_value, budget)?; },
    3 => value.flags = decode_field_varint(field)?,
});

impl_simple!(ResourceRequest {
    kind: ResourceKind = ResourceKind::KindUnspecified,
    reference: String = String::new()
} encode(this, out) {
    if this.kind.as_i32() != 0 { put_varint(1, this.kind.as_i32() as u64, &mut out); }
    if !this.reference.is_empty() { put_string(2, &this.reference, &mut out); }
} decode(value, field, budget) {
    1 => value.kind = ResourceKind::from_i32(decode_field_varint(field)? as i32),
    2 => value.reference = decode_string(field, budget)?,
});

impl_simple!(ResourceResponse {
    reference: String = String::new(),
    found: bool = false,
    content: Vec<u8> = Vec::new(),
    media_type: String = String::new(),
    error: Option<StructuredError> = None
} encode(this, out) {
    if !this.reference.is_empty() { put_string(1, &this.reference, &mut out); }
    if this.found { put_varint(2, 1, &mut out); }
    if !this.content.is_empty() { put_bytes(3, &this.content, &mut out); }
    if !this.media_type.is_empty() { put_string(4, &this.media_type, &mut out); }
    if let Some(error) = &this.error { put_message(5, error, &mut out); }
} decode(value, field, budget) {
    1 => value.reference = decode_string(field, budget)?,
    2 => value.found = decode_field_varint(field)? != 0,
    3 => value.content = decode_bytes(field, budget)?,
    4 => value.media_type = decode_string(field, budget)?,
    5 => value.error = Some(nested(field, budget)?),
});

impl_simple!(LogRecord {
    level: LogLevel = LogLevel::LevelUnspecified,
    message: String = String::new(),
    timestamp_ms: u64 = 0
} encode(this, out) {
    if this.level.as_i32() != 0 { put_varint(1, this.level.as_i32() as u64, &mut out); }
    if !this.message.is_empty() { put_string(2, &this.message, &mut out); }
    if this.timestamp_ms != 0 { put_varint(3, this.timestamp_ms, &mut out); }
} decode(value, field, budget) {
    1 => value.level = LogLevel::from_i32(decode_field_varint(field)? as i32),
    2 => value.message = decode_string(field, budget)?,
    3 => value.timestamp_ms = decode_field_varint(field)?,
});

impl_simple!(HealthCheck {
    nonce: u64 = 0
} encode(this, out) {
    if this.nonce != 0 { put_varint(1, this.nonce, &mut out); }
} decode(value, field, budget) {
    1 => value.nonce = decode_field_varint(field)?,
});

impl_simple!(HealthReport {
    nonce: u64 = 0,
    healthy: bool = false,
    memory_bytes: u64 = 0,
    queue_depth: u32 = 0,
    in_flight: u32 = 0,
    detail: String = String::new()
} encode(this, out) {
    if this.nonce != 0 { put_varint(1, this.nonce, &mut out); }
    if this.healthy { put_varint(2, 1, &mut out); }
    if this.memory_bytes != 0 { put_varint(3, this.memory_bytes, &mut out); }
    if this.queue_depth != 0 { put_varint(4, u64::from(this.queue_depth), &mut out); }
    if this.in_flight != 0 { put_varint(5, u64::from(this.in_flight), &mut out); }
    if !this.detail.is_empty() { put_string(6, &this.detail, &mut out); }
} decode(value, field, budget) {
    1 => value.nonce = decode_field_varint(field)?,
    2 => value.healthy = decode_field_varint(field)? != 0,
    3 => value.memory_bytes = decode_field_varint(field)?,
    4 => value.queue_depth = decode_field_varint(field)? as u32,
    5 => value.in_flight = decode_field_varint(field)? as u32,
    6 => value.detail = decode_string(field, budget)?,
});

impl_simple!(StructuredError {
    code: ErrorCode = ErrorCode::CodeUnspecified,
    message: String = String::new(),
    detail: String = String::new(),
    request_id: u64 = 0
} encode(this, out) {
    if this.code.as_i32() != 0 { put_varint(1, this.code.as_i32() as u64, &mut out); }
    if !this.message.is_empty() { put_string(2, &this.message, &mut out); }
    if !this.detail.is_empty() { put_string(3, &this.detail, &mut out); }
    if this.request_id != 0 { put_varint(4, this.request_id, &mut out); }
} decode(value, field, budget) {
    1 => value.code = ErrorCode::from_i32(decode_field_varint(field)? as i32),
    2 => value.message = decode_string(field, budget)?,
    3 => value.detail = decode_string(field, budget)?,
    4 => value.request_id = decode_field_varint(field)?,
});

impl_simple!(FlowControl {
    credits: u32 = 0,
    paused: bool = false
} encode(this, out) {
    if this.credits != 0 { put_varint(1, u64::from(this.credits), &mut out); }
    if this.paused { put_varint(2, 1, &mut out); }
} decode(value, field, budget) {
    1 => value.credits = decode_field_varint(field)? as u32,
    2 => value.paused = decode_field_varint(field)? != 0,
});

impl_simple!(Shutdown {
    immediate: bool = false
} encode(this, out) {
    if this.immediate { put_varint(1, 1, &mut out); }
} decode(value, field, budget) {
    1 => value.immediate = decode_field_varint(field)? != 0,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_repeated_items_hit_decode_budget() {
        // Each pair is an empty `ResultBatch.items` message. The input remains
        // well below MAX_FRAME_BYTES, but the decoded vector would otherwise
        // grow with the hostile repetition count.
        let mut encoded = Vec::with_capacity(2 * 100_000);
        for _ in 0..100_000 {
            encoded.extend_from_slice(&[0x12, 0x00]);
        }

        let error = ResultBatch::decode(&encoded).expect_err("repetition must be bounded");
        assert!(matches!(
            error,
            ProtocolError::Malformed(detail)
                if detail.contains("allocation budget exhausted")
        ));
    }
}
