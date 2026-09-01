//! Proto3 primitive encoding and unknown-field retention (spec 16.3).

use crate::ProtocolError;
/// Maximum heap budget charged while decoding one message tree.
///
/// This is the *decodable* ceiling, and it is the tighter of the protocol's
/// two limits. [`crate::MAX_FRAME_BYTES`] bounds the bytes that may be put on
/// the wire; this bounds the heap those bytes may turn into. The two are not
/// interchangeable: a repeated field costs far more heap than wire, so a
/// producer that sizes a batch against the frame cap alone can emit a frame
/// that is accepted on the wire and then refused while decoding.
///
/// The budget deliberately does *not* scale with the input. Eight MiB of
/// two-byte empty `Item` submessages would materialise four million `Item`
/// values — hundreds of megabytes — so an absolute ceiling is the only thing
/// standing between a hostile frame and an allocation the host cannot refuse.
/// Raising it to make every frame-sized batch decodable would trade that
/// defence away, so the producer side derives from this instead: see
/// [`RepeatedFieldCharge`] and [`crate::message::max_decodable_items`],
/// which let a producer size a batch in the decoder's own currency rather
/// than guess in bytes.
pub const DECODE_ALLOCATION_BUDGET: usize = 8 * 1024 * 1024;
const MAX_DECODE_DEPTH: usize = 64;

/// Conservative accounting for one repeated value or map node. It prevents
/// a legal frame containing millions of empty repeated fields from forcing a
/// proportional allocation before host-level limits can run.
pub const DECODE_REPETITION_OVERHEAD: usize = 64;

/// Capacity a repeated field grows to once it is full at `current`.
///
/// The decoder ([`push_decoded`]) and the producer-side estimate
/// ([`RepeatedFieldCharge`]) both go through this one rule, so the cost a
/// producer computes cannot drift from the cost the decoder charges.
/// `None` means the growth arithmetic overflowed.
pub(crate) fn grown_capacity(current: usize) -> Option<usize> {
    if current == 0 {
        return Some(4);
    }
    let doubled = current.checked_mul(2)?;
    let limited = current.checked_add(1024)?;
    Some(doubled.min(limited))
}

/// Producer-side mirror of the heap [`push_decoded`] charges for one repeated
/// field, maintained incrementally so sizing a batch stays O(1) per element.
///
/// A producer cannot discover the real ceiling by counting wire bytes, and
/// it must not have to guess: this is the same arithmetic the decoder runs,
/// driven from the sending side.
#[derive(Debug, Clone)]
pub struct RepeatedFieldCharge {
    element: usize,
    capacity: usize,
    length: usize,
    charged: usize,
}

impl RepeatedFieldCharge {
    /// Accounting for a `Vec<T>` that starts empty.
    pub fn new<T>() -> Self {
        Self {
            element: std::mem::size_of::<T>(),
            capacity: 0,
            length: 0,
            charged: 0,
        }
    }

    /// Accounts for appending one more value and returns the running total.
    pub fn push(&mut self) -> usize {
        if self.length == self.capacity {
            let next = grown_capacity(self.capacity).unwrap_or(usize::MAX);
            self.charged = self
                .charged
                .saturating_add(next.saturating_sub(self.capacity).saturating_mul(self.element));
            self.capacity = next;
        }
        self.length = self.length.saturating_add(1);
        self.charged = self
            .charged
            .saturating_add(self.element.saturating_add(DECODE_REPETITION_OVERHEAD));
        self.charged
    }

    /// Heap charged for every value appended so far.
    pub fn charged(&self) -> usize {
        self.charged
    }
}

/// Heap the decoder charges for one decoded map entry, excluding the key and
/// value bytes themselves.
pub fn map_entry_charge() -> usize {
    std::mem::size_of::<(String, String)>().saturating_add(DECODE_REPETITION_OVERHEAD)
}

#[derive(Debug)]
pub(crate) struct DecodeBudget {
    remaining: usize,
    depth: usize,
}

impl DecodeBudget {
    pub(crate) fn new() -> Self {
        Self {
            remaining: DECODE_ALLOCATION_BUDGET,
            depth: 0,
        }
    }

    pub(crate) fn enter_nested(&mut self) -> Result<(), ProtocolError> {
        if self.depth >= MAX_DECODE_DEPTH {
            return Err(ProtocolError::Malformed("message nesting is too deep".to_owned()));
        }
        self.depth += 1;
        Ok(())
    }

    pub(crate) fn leave_nested(&mut self) {
        self.depth -= 1;
    }

    /// Charging past the budget is reported as [`ProtocolError::DecodeBudgetExceeded`]
    /// rather than as generic malformedness: the bytes were well formed and
    /// within the frame cap, and the receiver refused only the heap they
    /// would become. Saying so is what lets a diagnostic name the real cause
    /// instead of blaming the producer for bad bytes.
    fn charge(&mut self, amount: usize) -> Result<(), ProtocolError> {
        if amount > self.remaining {
            return Err(ProtocolError::DecodeBudgetExceeded {
                requested: amount,
                remaining: self.remaining,
            });
        }
        self.remaining -= amount;
        Ok(())
    }

    pub(crate) fn charge_map_entry(&mut self) -> Result<(), ProtocolError> {
        self.charge(map_entry_charge())
    }
}

/// Appends one decoded repeated value without allowing a hostile count to
/// grow a vector without a budget check.
pub(crate) fn push_decoded<T>(
    values: &mut Vec<T>,
    value: T,
    budget: &mut DecodeBudget,
) -> Result<(), ProtocolError> {
    let growth = if values.len() == values.capacity() {
        let target = grown_capacity(values.capacity())
            .ok_or_else(|| ProtocolError::Malformed("message decode allocation overflow".to_owned()))?;
        target
            .checked_sub(values.capacity())
            .ok_or_else(|| ProtocolError::Malformed("message decode allocation overflow".to_owned()))?
    } else {
        0
    };
    let allocation = growth
        .checked_mul(std::mem::size_of::<T>())
        .and_then(|value| value.checked_add(std::mem::size_of::<T>()))
        .and_then(|value| value.checked_add(DECODE_REPETITION_OVERHEAD))
        .ok_or_else(|| ProtocolError::Malformed("message decode allocation overflow".to_owned()))?;
    budget.charge(allocation)?;
    if growth != 0 {
        values
            .try_reserve_exact(growth)
            .map_err(|_| ProtocolError::Malformed("message decode allocation failed".to_owned()))?;
    }
    values.push(value);
    Ok(())
}

/// The four protobuf wire types supported by version 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireType {
    Varint,
    Fixed64,
    Length,
    Fixed32,
}

impl WireType {
    pub(crate) fn bits(self) -> u8 {
        match self {
            Self::Varint => 0,
            Self::Fixed64 => 1,
            Self::Length => 2,
            Self::Fixed32 => 5,
        }
    }

    fn from_bits(bits: u8) -> Result<Self, ProtocolError> {
        match bits {
            0 => Ok(Self::Varint),
            1 => Ok(Self::Fixed64),
            2 => Ok(Self::Length),
            5 => Ok(Self::Fixed32),
            _ => Err(ProtocolError::Malformed(format!(
                "unsupported protobuf wire type {bits}"
            ))),
        }
    }
}

/// Appends a canonical unsigned protobuf varint.
pub fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Decodes one unsigned protobuf varint and advances `cursor`.
///
/// The tenth byte of a u64 varint may contain only bit zero. Rejecting an
/// overflowing or unterminated sequence is important because message decoding
/// is total for arbitrary plugin bytes (spec 16.3).
pub fn decode_varint(input: &[u8], cursor: &mut usize) -> Result<u64, ProtocolError> {
    let mut value = 0_u64;
    for index in 0..10 {
        let position = *cursor;
        let byte = *input
            .get(position)
            .ok_or_else(|| ProtocolError::Malformed("truncated protobuf varint".to_owned()))?;
        *cursor = position
            .checked_add(1)
            .ok_or_else(|| ProtocolError::Malformed("protobuf cursor overflow".to_owned()))?;
        if index == 9 && (byte & 0xfe) != 0 {
            return Err(ProtocolError::Malformed(
                "protobuf varint overflows u64".to_owned(),
            ));
        }
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(ProtocolError::Malformed(
        "unterminated protobuf varint".to_owned(),
    ))
}

/// Appends a field key made of a field number and wire type.
pub fn encode_key(field: u32, wire: WireType, out: &mut Vec<u8>) {
    if field == 0 {
        return;
    }
    encode_varint((u64::from(field) << 3) | u64::from(wire.bits()), out);
}

/// Zig-zag encoding for signed `sint` values.
pub fn zigzag_encode(value: i64) -> u64 {
    ((value as u64) << 1) ^ ((value >> 63) as u64)
}

/// Zig-zag decoding for signed `sint` values.
pub fn zigzag_decode(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// Raw bytes of fields that a decoder did not recognise.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnknownFields(Vec<u8>);

impl UnknownFields {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Appends one already encoded key/value field pair.
    pub fn push_raw(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    pub(crate) fn push_raw_bounded(
        &mut self,
        bytes: &[u8],
        budget: &mut DecodeBudget,
    ) -> Result<(), ProtocolError> {
        let required = self
            .0
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| ProtocolError::Malformed("unknown fields are too large".to_owned()))?;
        if required > self.0.capacity() {
            let doubled = self
                .0
                .capacity()
                .checked_mul(2)
                .ok_or_else(|| ProtocolError::Malformed("unknown fields are too large".to_owned()))?;
            let target = doubled.max(required);
            let growth = target
                .checked_sub(self.0.capacity())
                .ok_or_else(|| ProtocolError::Malformed("unknown fields are too large".to_owned()))?;
            budget.charge(growth)?;
            self.0
                .try_reserve_exact(growth)
                .map_err(|_| ProtocolError::Malformed("message decode allocation failed".to_owned()))?;
        }
        self.0.extend_from_slice(bytes);
        Ok(())
    }
}

/// One fully consumed protobuf field. The value excludes its key and, for a
/// length-delimited field, excludes its length prefix.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Field<'a> {
    pub number: u32,
    pub wire_type: WireType,
    pub value: &'a [u8],
    pub raw: &'a [u8],
    pub end: usize,
}
/// Reads one field without allocating. `cursor` is advanced only over a
/// successfully validated field (or over a malformed field's prefix).
pub(crate) fn read_field<'a>(input: &'a [u8], cursor: &mut usize) -> Result<Field<'a>, ProtocolError> {
    let field_start = *cursor;
    let key = decode_varint(input, cursor)?;
    let number = key >> 3;
    // Protobuf reserves three bits of every key for the wire type, leaving a
    // 29-bit field-number space.  Accepting a larger number would make this
    // decoder accept bytes no conforming proto3 peer can emit.
    if number == 0 || number > 0x1fff_ffff {
        return Err(ProtocolError::Malformed(
            "invalid protobuf field number".to_owned(),
        ));
    }
    let field_number = u32::try_from(number)
        .map_err(|_| ProtocolError::Malformed("invalid protobuf field number".to_owned()))?;
    let wire_type = WireType::from_bits(
        u8::try_from(key & 7)
            .map_err(|_| ProtocolError::Malformed("invalid protobuf wire type".to_owned()))?,
    )?;
    let value_start = *cursor;
    let value_end = match wire_type {
        WireType::Varint => {
            decode_varint(input, cursor)?;
            *cursor
        }
        WireType::Fixed64 => {
            let end = value_start
                .checked_add(8)
                .ok_or_else(|| ProtocolError::Malformed("fixed64 length overflow".to_owned()))?;
            if end > input.len() {
                return Err(ProtocolError::Malformed("truncated fixed64 field".to_owned()));
            }
            *cursor = end;
            end
        }
        WireType::Length => {
            let length = decode_varint(input, cursor)?;
            let length = usize::try_from(length)
                .map_err(|_| ProtocolError::Malformed("length-delimited field is too large".to_owned()))?;
            let end = (*cursor).checked_add(length).ok_or_else(|| {
                ProtocolError::Malformed("length-delimited field overflows input".to_owned())
            })?;
            if end > input.len() {
                return Err(ProtocolError::Malformed(
                    "length-delimited field runs past input".to_owned(),
                ));
            }
            let start = *cursor;
            *cursor = end;
            return Ok(Field {
                number: field_number,
                wire_type,
                value: &input[start..end],
                raw: &input[field_start..end],
                end,
            });
        }
        WireType::Fixed32 => {
            let end = value_start
                .checked_add(4)
                .ok_or_else(|| ProtocolError::Malformed("fixed32 length overflow".to_owned()))?;
            if end > input.len() {
                return Err(ProtocolError::Malformed("truncated fixed32 field".to_owned()));
            }
            *cursor = end;
            end
        }
    };
    Ok(Field {
        number: field_number,
        wire_type,
        value: &input[value_start..value_end],
        raw: &input[field_start..value_end],
        end: value_end,
    })
}

pub(crate) fn decode_field_varint(field: Field<'_>) -> Result<u64, ProtocolError> {
    if field.wire_type != WireType::Varint {
        return Err(ProtocolError::Malformed(
            "unexpected protobuf wire type".to_owned(),
        ));
    }
    let mut cursor = 0;
    let value = decode_varint(field.value, &mut cursor)?;
    if cursor != field.value.len() {
        return Err(ProtocolError::Malformed(
            "invalid protobuf varint field".to_owned(),
        ));
    }
    Ok(value)
}

pub(crate) fn decode_string(field: Field<'_>, budget: &mut DecodeBudget) -> Result<String, ProtocolError> {
    if field.wire_type != WireType::Length {
        return Err(ProtocolError::Malformed(
            "expected length-delimited string".to_owned(),
        ));
    }
    budget.charge(field.value.len())?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(field.value.len())
        .map_err(|_| ProtocolError::Malformed("message decode allocation failed".to_owned()))?;
    bytes.extend_from_slice(field.value);
    String::from_utf8(bytes).map_err(|_| ProtocolError::Malformed("protobuf string is not UTF-8".to_owned()))
}

pub(crate) fn decode_bytes(field: Field<'_>, budget: &mut DecodeBudget) -> Result<Vec<u8>, ProtocolError> {
    if field.wire_type != WireType::Length {
        return Err(ProtocolError::Malformed(
            "expected length-delimited bytes".to_owned(),
        ));
    }
    budget.charge(field.value.len())?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(field.value.len())
        .map_err(|_| ProtocolError::Malformed("message decode allocation failed".to_owned()))?;
    bytes.extend_from_slice(field.value);
    Ok(bytes)
}

pub(crate) fn put_varint(field: u32, value: u64, out: &mut Vec<u8>) {
    encode_key(field, WireType::Varint, out);
    encode_varint(value, out);
}

/// Reads a proto3 `float`: four little-endian IEEE-754 bytes.
///
/// Deliberately faithful, including to NaN and the infinities. Refusing them
/// here would put a semantic judgement in the codec and leave the same bytes
/// legal for some other field; the layer that knows what a number means is
/// the layer that rejects it, which for page geometry is
/// `crikey_core::PageFrame::validate`.
pub(crate) fn decode_f32(field: Field<'_>) -> Result<f32, ProtocolError> {
    if field.wire_type != WireType::Fixed32 {
        return Err(ProtocolError::Malformed(
            "expected a fixed32 float field".to_owned(),
        ));
    }
    let bytes: [u8; 4] = field
        .value
        .try_into()
        .map_err(|_| ProtocolError::Malformed("invalid protobuf float field".to_owned()))?;
    Ok(f32::from_le_bytes(bytes))
}

/// Writes a proto3 `float`. Callers elide the default the way every other
/// writer here does; note that `-0.0 == 0.0` in Rust, so a negative zero is
/// elided and read back as a positive one. No geometry distinguishes them.
pub(crate) fn put_f32(field: u32, value: f32, out: &mut Vec<u8>) {
    encode_key(field, WireType::Fixed32, out);
    out.extend_from_slice(&value.to_le_bytes());
}

pub(crate) fn put_string(field: u32, value: &str, out: &mut Vec<u8>) {
    encode_key(field, WireType::Length, out);
    encode_varint(
        u64::try_from(value.len()).expect("a Rust string length always fits in u64"),
        out,
    );
    out.extend_from_slice(value.as_bytes());
}

pub(crate) fn put_bytes(field: u32, value: &[u8], out: &mut Vec<u8>) {
    encode_key(field, WireType::Length, out);
    encode_varint(
        u64::try_from(value.len()).expect("a Rust slice length always fits in u64"),
        out,
    );
    out.extend_from_slice(value);
}

pub(crate) fn put_message<M: crate::Message>(field: u32, value: &M, out: &mut Vec<u8>) {
    let bytes = value.encode();
    put_bytes(field, &bytes, out);
}

pub(crate) fn expect_wire(field: Field<'_>, expected: WireType) -> Result<(), ProtocolError> {
    if field.wire_type == expected {
        Ok(())
    } else {
        Err(ProtocolError::Malformed(format!(
            "field {} has wrong protobuf wire type",
            field.number
        )))
    }
}
