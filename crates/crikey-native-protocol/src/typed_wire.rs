// Included in message.rs so all nested decoders share the same allocation budget.
use crikey_core::structured_arguments::{
    ArgumentValues, FieldId, NonSecretValue, OptionId, Password, StructuredValue,
};

fn typed_error() -> ProtocolError {
    ProtocolError::Malformed("invalid typed payload".into())
}
fn merge_optional<M: DecodeWithBudget>(
    slot: &mut Option<M>,
    field: crate::wire::Field<'_>,
    budget: &mut DecodeBudget,
) -> Result<(), ProtocolError> {
    match slot {
        Some(value) => merge_nested(value, field, budget),
        None => {
            *slot = Some(nested(field, budget)?);
            Ok(())
        }
    }
}
fn bounded_push<T>(
    values: &mut Vec<T>,
    field: crate::wire::Field<'_>,
    budget: &mut DecodeBudget,
    max: usize,
) -> Result<(), ProtocolError>
where
    T: DecodeWithBudget,
{
    if values.len() >= max {
        return Err(typed_error());
    }
    let decoded = nested(field, budget)?;
    push_decoded(values, decoded, budget)
}
fn optional_string(value: &Option<String>, number: u32, out: &mut Vec<u8>) {
    if let Some(value) = value {
        put_string(number, value, out);
    }
}

// Persist the known-member mask across message fragments. Equality deliberately
// ignores decoder bookkeeping, and unknown bytes retain the usual bounded policy.
macro_rules! typed_message {
    ($type:ident, $max:expr, [$($repeat:literal),*], { $($name:ident : $ty:ty = $default:expr),* $(,)? }, encode($this:ident,$out:ident) {$($enc:tt)*}, decode($v:ident,$f:ident,$b:ident) {$($n:literal => $body:expr,)*}) => {
        #[derive(Debug, Clone)]
        pub struct $type { $(pub $name: $ty,)* pub unknown: UnknownFields, #[doc(hidden)] pub known_fields: u64, #[doc(hidden)] pub decoded_bytes: usize, #[doc(hidden)] pub decoded_allocation: usize }
        impl Default for $type { fn default() -> Self { Self { $($name:$default,)* unknown:Default::default(), known_fields:0, decoded_bytes:0, decoded_allocation:0 } } }
        impl PartialEq for $type { fn eq(&self, other:&Self)->bool { true $(&& self.$name == other.$name)* && self.unknown == other.unknown } }
        impl Eq for $type {}
        impl Message for $type {
            fn encode(&self)->Vec<u8> { let $this=self; let mut $out=Vec::new(); $($enc)* finish($out,&$this.unknown) }
            fn decode(bytes:&[u8])->Result<Self,ProtocolError> { ensure_message_size(bytes)?; Self::decode_with_budget(bytes,&mut DecodeBudget::new()) }
        }
        impl DecodeWithBudget for $type {
            fn decode_with_budget(bytes:&[u8],budget:&mut DecodeBudget)->Result<Self,ProtocolError> { let mut value=Self::default(); value.merge_with_budget(bytes,budget)?; Ok(value) }
            fn merge_with_budget(&mut self,bytes:&[u8],$b:&mut DecodeBudget)->Result<(),ProtocolError> {
                preflight_typed_shape(stringify!($type),bytes)?;
                let $v=self;
                $v.decoded_bytes=$v.decoded_bytes.checked_add(bytes.len()).ok_or_else(typed_error)?;
                if $v.decoded_bytes > $max { return Err(typed_error()); }
                let parent_remaining=$b.remaining();
                let result=$b.with_limit(($max as usize).saturating_sub($v.decoded_allocation), |$b| {
                let repeated: &[u32] = &[$($repeat),*];
                let mut cursor=0;
                while cursor<bytes.len() { let start=cursor; let $f=read_field(bytes,&mut cursor)?;
                    match $f.number { $($n => {
                        if !repeated.contains(&$f.number) {
                            let bit=1u64 << $n;
                            if $v.known_fields & bit != 0 { return Err(typed_error()); }
                            $v.known_fields |= bit;
                        }
                        $body
                    },)* _ => unknown(bytes,start,$f.end,&mut $v.unknown,$b)?, }
                }
                    Ok(())
                });
                $v.decoded_allocation=$v.decoded_allocation.saturating_add(parent_remaining-$b.remaining());
                result
            }
        }
    }
}

fn preflight_typed_shape(name: &str, bytes: &[u8]) -> Result<(), ProtocolError> {
    if name != "ArgumentField" {
        return Ok(());
    }
    let mut cursor = 0;
    let mut seen = 0u16;
    let mut kind = 0;
    let mut min = 0;
    let mut max = 8192;
    let mut has_default = false;
    let mut multiline = false;
    let mut options = 0;
    while cursor < bytes.len() {
        let field = read_field(bytes, &mut cursor)?;
        if (1..=11).contains(&field.number) && field.number != 11 {
            let bit = 1u16 << field.number;
            if seen & bit != 0 {
                return Err(typed_error());
            }
            seen |= bit;
        }
        match field.number {
            6 => kind = decode_u32(field)?,
            7 => min = decode_u32(field)?,
            8 => max = decode_u32(field)?,
            9 => multiline = decode_field_varint(field)? != 0,
            10 => has_default = true,
            11 => {
                options += 1;
                if options > 256 {
                    return Err(typed_error());
                }
            }
            _ => {}
        }
    }
    if min > max || max > 8192 {
        return Err(typed_error());
    }
    match kind {
        1 if options == 0 => Ok(()),
        2 if !has_default && !multiline && options == 0 => Ok(()),
        3 if options > 0 && !multiline && min == 0 && max == 8192 => Ok(()),
        _ => Err(typed_error()),
    }
}

typed_message!(PresentationDeclaration, 16384, [], {
    id:String=String::new(), label:String=String::new()
}, encode(t,o) { put_string(1,&t.id,&mut o); put_string(2,&t.label,&mut o); }, decode(v,f,b) {
    1 => v.id=preview_string(f,b,256)?,
    2 => v.label=preview_string(f,b,8192)?,
});
typed_message!(DropdownOption, 16384, [], {
    id:String=String::new(), label:String=String::new()
}, encode(t,o) { put_string(1,&t.id,&mut o); put_string(2,&t.label,&mut o); }, decode(v,f,b) {
    1 => v.id=preview_string(f,b,256)?,
    2 => v.label=preview_string(f,b,8192)?,
});
typed_message!(ArgumentField, 262144, [11], {
    id:String=String::new(), label:String=String::new(), required:bool=false,
    placeholder:Option<String>=None, validation_message:Option<String>=None,
    kind:u32=0, min_bytes:u32=0, max_bytes:Option<u32>=None, multiline:bool=false,
    default_value:Option<String>=None, options:Vec<DropdownOption>=Vec::new()
}, encode(t,o) {
    put_string(1,&t.id,&mut o); put_string(2,&t.label,&mut o);
    if t.required { put_varint(3,1,&mut o); }
    optional_string(&t.placeholder,4,&mut o); optional_string(&t.validation_message,5,&mut o);
    put_varint(6,u64::from(t.kind),&mut o);
    if t.min_bytes!=0 { put_varint(7,u64::from(t.min_bytes),&mut o); }
    if let Some(max)=t.max_bytes { put_varint(8,u64::from(max),&mut o); }
    if t.multiline { put_varint(9,1,&mut o); }
    optional_string(&t.default_value,10,&mut o);
    for option in &t.options { put_message(11,option,&mut o); }
}, decode(v,f,b) {
    1 => v.id=preview_string(f,b,256)?,
    2 => v.label=preview_string(f,b,8192)?,
    3 => v.required=decode_field_varint(f)?!=0,
    4 => v.placeholder=Some(preview_string(f,b,8192)?),
    5 => v.validation_message=Some(preview_string(f,b,8192)?),
    6 => v.kind=decode_u32(f)?,
    7 => v.min_bytes=decode_u32(f)?,
    8 => v.max_bytes=Some(decode_u32(f)?),
    9 => v.multiline=decode_field_varint(f)?!=0,
    10 => v.default_value=Some(preview_string(f,b,8192)?),
    11 => bounded_push(&mut v.options,f,b,256)?,
});
typed_message!(ArgumentSchema, 262144, [1], {
    fields:Vec<ArgumentField>=Vec::new()
}, encode(t,o) { for field in &t.fields { put_message(1,field,&mut o); } }, decode(v,f,b) {
    1 => bounded_push(&mut v.fields,f,b,64)?,
});
typed_message!(ExecutableSchemaBinding, 263168, [3], {
    item_id:String=String::new(), action_id:Option<String>=None, schema:Option<ArgumentSchema>=None
}, encode(t,o) { put_string(1,&t.item_id,&mut o); optional_string(&t.action_id,2,&mut o); if let Some(schema)=&t.schema { put_message(3,schema,&mut o); } }, decode(v,f,b) {
    1 => v.item_id=preview_string(f,b,256)?,
    2 => v.action_id=Some(preview_string(f,b,256)?),
    3 => merge_optional(&mut v.schema,f,b)?,
});
typed_message!(ItemPresentation, 65536, [3], {
    item_id:String=String::new(), section_id:Option<String>=None, scope_ids:Vec<String>=Vec::new()
}, encode(t,o) { put_string(1,&t.item_id,&mut o); optional_string(&t.section_id,2,&mut o); for scope in &t.scope_ids { put_string(3,scope,&mut o); } }, decode(v,f,b) {
    1 => v.item_id=preview_string(f,b,256)?,
    2 => v.section_id=Some(preview_string(f,b,256)?),
    3 => { if v.scope_ids.len()>=128 { return Err(typed_error()); } let scope=preview_string(f,b,256)?; push_decoded(&mut v.scope_ids,scope,b)?; },
});
typed_message!(ResultPresentation, 4194304, [1,2,3,4], {
    sections:Vec<PresentationDeclaration>=Vec::new(), scopes:Vec<PresentationDeclaration>=Vec::new(),
    items:Vec<ItemPresentation>=Vec::new(), schemas:Vec<ExecutableSchemaBinding>=Vec::new()
}, encode(t,o) {
    for entry in &t.sections { put_message(1,entry,&mut o); }
    for entry in &t.scopes { put_message(2,entry,&mut o); }
    for entry in &t.items { put_message(3,entry,&mut o); }
    for entry in &t.schemas { put_message(4,entry,&mut o); }
}, decode(v,f,b) {
    1 => bounded_push(&mut v.sections,f,b,128)?,
    2 => bounded_push(&mut v.scopes,f,b,128)?,
    3 => bounded_push(&mut v.items,f,b,4096)?,
    4 => bounded_push(&mut v.schemas,f,b,4096)?,
});
typed_message!(TypedFieldFailure, 1024, [], {
    field_id:String=String::new(), code:u32=0
}, encode(t,o) { put_string(1,&t.field_id,&mut o); put_varint(2,u64::from(t.code),&mut o); }, decode(v,f,b) {
    1 => v.field_id=preview_string(f,b,256)?,
    2 => v.code=decode_u32(f)?,
});
typed_message!(TypedExecuteResponse, 65536, [2], {
    status:u32=0, failures:Vec<TypedFieldFailure>=Vec::new(), failure_code:u32=0
}, encode(t,o) { put_varint(1,u64::from(t.status),&mut o); for failure in &t.failures { put_message(2,failure,&mut o); } if t.failure_code!=0 { put_varint(3,u64::from(t.failure_code),&mut o); } }, decode(v,f,b) {
    1 => v.status=decode_u32(f)?,
    2 => bounded_push(&mut v.failures,f,b,64)?,
    3 => v.failure_code=decode_u32(f)?,
});

/// Move-only input. Debug never renders any submitted value (including text).
#[derive(Default)]
pub struct TypedExecuteRequest {
    pub item_id: String,
    pub action_id: Option<String>,
    pub expected_fingerprint: Vec<u8>,
    pub values: ArgumentValues,
    pub unknown: UnknownFields,
    #[doc(hidden)]
    pub value_unknown: BTreeMap<FieldId, UnknownFields>,
    #[doc(hidden)]
    pub known_fields: u64,
    #[doc(hidden)]
    pub decoded_bytes: usize,
}
impl std::fmt::Debug for TypedExecuteRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TypedExecuteRequest { values: [REDACTED] }")
    }
}
impl PartialEq for TypedExecuteRequest {
    fn eq(&self, other: &Self) -> bool {
        self.item_id == other.item_id
            && self.action_id == other.action_id
            && self.expected_fingerprint == other.expected_fingerprint
            && self.unknown == other.unknown
            && self.value_unknown == other.value_unknown
            && self.values.0.len() == other.values.0.len()
            && self.values.0.iter().zip(&other.values.0).all(|((a, x), (b, y))| {
                a == b
                    && match (x, y) {
                        (StructuredValue::NonSecret(a), StructuredValue::NonSecret(b)) => a == b,
                        (StructuredValue::Password(a), StructuredValue::Password(b)) => {
                            a.expose_plaintext() == b.expose_plaintext()
                        }
                        _ => false,
                    }
            })
    }
}
impl TypedExecuteRequest {
    fn encoded_len(&self) -> usize {
        let string_size = |length| 1 + varint_size(length as u64) + length;
        let mut size = string_size(self.item_id.len())
            + string_size(self.expected_fingerprint.len())
            + self.unknown.as_bytes().len();
        if let Some(action) = &self.action_id {
            size += string_size(action.len());
        }
        for (id, value) in &self.values.0 {
            let length = match value {
                StructuredValue::NonSecret(NonSecretValue::Text(s)) => s.len(),
                StructuredValue::Password(s) => s.expose_plaintext().len(),
                StructuredValue::NonSecret(NonSecretValue::Dropdown(id)) => id.0.len(),
            };
            let unknown = self.value_unknown.get(id).map_or(0, |u| u.as_bytes().len());
            size += string_size(string_size(id.0.len()) + 2 + string_size(length) + unknown);
        }
        size
    }
    fn encode_into(&self, out: &mut Vec<u8>) {
        put_string(1, &self.item_id, out);
        optional_string(&self.action_id, 2, out);
        put_bytes(3, &self.expected_fingerprint, out);
        for (id, value) in &self.values.0 {
            let (kind, text) = match value {
                StructuredValue::NonSecret(NonSecretValue::Text(s)) => (1, s.as_str()),
                StructuredValue::Password(s) => (2, s.expose_plaintext()),
                StructuredValue::NonSecret(NonSecretValue::Dropdown(id)) => (3, id.0.as_str()),
            };
            // Encode directly into the destination: no temporary secret-bearing
            // nested Vec allocation or clone of the submitted value.
            let unknown = self
                .value_unknown
                .get(id)
                .map(|u| u.as_bytes())
                .unwrap_or_default();
            let length = 2
                + varint_size(id.0.len() as u64)
                + id.0.len()
                + 2
                + varint_size(text.len() as u64)
                + text.len()
                + unknown.len();
            crate::wire::encode_key(4, WireType::Length, out);
            crate::wire::encode_varint(length as u64, out);
            put_string(1, &id.0, out);
            put_varint(2, kind, out);
            put_string(3, text, out);
            out.extend_from_slice(unknown);
        }
        out.extend_from_slice(self.unknown.as_bytes());
    }
}
impl Message for TypedExecuteRequest {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        self.encode_into(&mut out);
        out
    }
    fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        ensure_message_size(bytes)?;
        Self::decode_with_budget(bytes, &mut DecodeBudget::new())
    }
}
fn varint_size(mut n: u64) -> usize {
    let mut size = 1;
    while n >= 128 {
        n >>= 7;
        size += 1;
    }
    size
}
impl DecodeWithBudget for TypedExecuteRequest {
    fn decode_with_budget(bytes: &[u8], budget: &mut DecodeBudget) -> Result<Self, ProtocolError> {
        let mut value = Self::default();
        value.merge_with_budget(bytes, budget)?;
        Ok(value)
    }
    fn merge_with_budget(&mut self, bytes: &[u8], budget: &mut DecodeBudget) -> Result<(), ProtocolError> {
        self.decoded_bytes = self
            .decoded_bytes
            .checked_add(bytes.len())
            .ok_or_else(typed_error)?;
        if self.decoded_bytes > 1024 * 1024 {
            return Err(typed_error());
        }
        let mut cursor = 0;
        while cursor < bytes.len() {
            let start = cursor;
            let field = read_field(bytes, &mut cursor)?;
            if (1..=3).contains(&field.number) {
                let bit = 1 << field.number;
                if self.known_fields & bit != 0 {
                    return Err(typed_error());
                }
                self.known_fields |= bit;
            }
            match field.number {
                1 => self.item_id = preview_string(field, budget, 256)?,
                2 => self.action_id = Some(preview_string(field, budget, 256)?),
                3 => {
                    if field.value.len() != 32 {
                        return Err(typed_error());
                    }
                    self.expected_fingerprint = decode_bytes(field, budget)?;
                }
                4 => {
                    if self.values.0.len() >= 64 {
                        return Err(typed_error());
                    }
                    expect_wire(field, WireType::Length)?;
                    let (id, value, unknown) = decode_typed_value(field.value, budget)?;
                    if self.values.0.contains_key(&id) {
                        return Err(typed_error());
                    }
                    if !unknown.as_bytes().is_empty() {
                        budget.charge_map_entry()?;
                        budget.charge_discarded(id.0.len())?;
                        self.value_unknown.insert(id.clone(), unknown);
                    }
                    budget.charge_map_entry()?;
                    self.values.0.insert(id, value);
                }
                // There is deliberately no legacy argument member. Unknown
                // additions remain bounded, but never become callback values.
                _ => unknown(bytes, start, field.end, &mut self.unknown, budget)?,
            }
        }
        Ok(())
    }
}
fn decode_typed_value(
    bytes: &[u8],
    budget: &mut DecodeBudget,
) -> Result<(FieldId, StructuredValue, UnknownFields), ProtocolError> {
    if bytes.len() > 9216 {
        return Err(typed_error());
    }
    // Preflight the complete element before allocating any submitted plaintext.
    let mut cursor = 0;
    let mut seen = 0u8;
    let mut id = None;
    let mut text = None;
    let mut kind = 0;
    while cursor < bytes.len() {
        let field = read_field(bytes, &mut cursor)?;
        if (1..=3).contains(&field.number) {
            let bit = 1u8 << field.number;
            if seen & bit != 0 {
                return Err(typed_error());
            }
            seen |= bit;
        }
        match field.number {
            1 => {
                expect_wire(field, WireType::Length)?;
                if field.value.len() > 256 {
                    return Err(typed_error());
                }
                id = Some(field);
            }
            2 => kind = decode_u32(field)?,
            3 => {
                expect_wire(field, WireType::Length)?;
                if field.value.len() > 8192 {
                    return Err(typed_error());
                }
                text = Some(field);
            }
            _ => { /* Unknown values are scanned but not materialized or exposed. */ }
        }
    }
    if !(1..=3).contains(&kind) {
        return Err(typed_error());
    }
    if kind == 3 && text.as_ref().is_some_and(|field| field.value.len() > 256) {
        return Err(typed_error());
    }
    let mut unknown = UnknownFields::default();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let field = read_field(bytes, &mut cursor)?;
        if !(1..=3).contains(&field.number) {
            unknown.push_raw_bounded(field.raw, budget)?;
        }
    }
    let id = FieldId(decode_string(id.ok_or_else(typed_error)?, budget)?);
    if id.0.is_empty() || id.0.chars().any(char::is_control) {
        return Err(typed_error());
    }
    let value = match text {
        Some(field) => decode_string(field, budget)?,
        None => String::new(),
    };
    let value = match kind {
        1 => StructuredValue::NonSecret(NonSecretValue::Text(value)),
        2 => StructuredValue::Password(Password::new(value).map_err(|_| typed_error())?),
        3 => StructuredValue::NonSecret(NonSecretValue::Dropdown(OptionId(value))),
        _ => return Err(typed_error()),
    };
    Ok((id, value, unknown))
}

/// Reject typed producer data before serialization allocates a buffer.
pub fn validate_outbound(envelope: &Envelope) -> Result<(), ProtocolError> {
    let mut charge = unknown_charge(&envelope.unknown).saturating_add(BATCH_OVERHEAD_RESERVE);
    match &envelope.payload {
        Some(Payload::Execute(request)) => {
            // This legacy payload has no repeated allocation overhead. Preserve
            // its existing frame-sized allowance, but reject it locally before
            // queueing: a writer-side frame refusal would close a healthy child.
            let mut payload_bytes = request.unknown.as_bytes().len();
            charge = unknown_charge(&envelope.unknown).saturating_add(unknown_charge(&request.unknown));
            for value in [&request.item_id, &request.action_id, &request.argument] {
                charge = charge.saturating_add(value.len());
                if !value.is_empty() {
                    payload_bytes = payload_bytes
                        .saturating_add(1)
                        .saturating_add(varint_size(value.len() as u64))
                        .saturating_add(value.len());
                }
            }
            let mut frame_bytes = envelope
                .unknown
                .as_bytes()
                .len()
                .saturating_add(2) // Envelope.execute, field 18.
                .saturating_add(varint_size(payload_bytes as u64))
                .saturating_add(payload_bytes);
            for value in [
                envelope.connection_id,
                envelope.request_id,
                envelope.generation,
                envelope.deadline_ms,
            ] {
                if value != 0 {
                    frame_bytes = frame_bytes.saturating_add(1).saturating_add(varint_size(value));
                }
            }
            if frame_bytes > MAX_FRAME_BYTES {
                return Err(ProtocolError::FrameTooLarge(frame_bytes));
            }
        }
        Some(Payload::TypedExecuteRequest(request)) => {
            crate::typed::check_request(request)?;
            charge = charge.saturating_add(crate::typed::request_charge(request));
        }
        Some(Payload::TypedExecuteResponse(response)) => {
            if response.failures.len() > 64 {
                return Err(typed_error());
            }
            charge = charge.saturating_add(unknown_charge(&response.unknown));
            let mut repeated = RepeatedFieldCharge::new::<TypedFieldFailure>();
            for failure in &response.failures {
                if failure.field_id.len() > 256 {
                    return Err(typed_error());
                }
                repeated.push();
                charge = charge
                    .saturating_add(failure.field_id.len())
                    .saturating_add(unknown_charge(&failure.unknown));
            }
            charge = charge.saturating_add(repeated.charged());
        }
        Some(Payload::CatalogBatch(batch)) => {
            charge = charge.saturating_add(unknown_charge(&batch.unknown));
            let mut repeated = RepeatedFieldCharge::new::<Item>();
            for item in &batch.items {
                repeated.push();
                charge = charge.saturating_add(item.decode_charge());
            }
            charge = charge.saturating_add(repeated.charged());
            if let Some(presentation) = &batch.presentation {
                crate::typed::check_presentation(presentation)?;
                charge = charge.saturating_add(crate::typed::presentation_charge(presentation));
            }
        }
        Some(Payload::Results(batch)) => {
            charge = charge.saturating_add(unknown_charge(&batch.unknown));
            let mut repeated = RepeatedFieldCharge::new::<Item>();
            for item in &batch.items {
                repeated.push();
                charge = charge.saturating_add(item.decode_charge());
            }
            charge = charge.saturating_add(repeated.charged());
            if let Some(presentation) = &batch.presentation {
                crate::typed::check_presentation(presentation)?;
                charge = charge.saturating_add(crate::typed::presentation_charge(presentation));
            }
        }
        Some(Payload::Suggest(query)) => {
            if query
                .selected_scope_id
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
            {
                return Err(typed_error());
            }
        }
        _ => {}
    }
    if charge > DECODE_ALLOCATION_BUDGET {
        return Err(ProtocolError::DecodeBudgetExceeded {
            requested: charge,
            remaining: DECODE_ALLOCATION_BUDGET,
        });
    }
    Ok(())
}

pub fn checked_encode(envelope: &Envelope) -> Result<Vec<u8>, ProtocolError> {
    validate_outbound(envelope)?;
    let bytes = envelope.encode();
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(bytes.len()));
    }
    Ok(bytes)
}
