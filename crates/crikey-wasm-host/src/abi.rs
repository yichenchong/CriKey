//! The CriKey WebAssembly guest ABI, version 1 (spec §2.2 later scope).
//!
//! This module is the single definition of how a `.wasm` plugin and the
//! [`crikey-wasm-host`](crate) executable exchange requests and items. Both
//! sides link this same code: the host encodes requests and decodes item
//! batches, the guest does the mirror. There is deliberately no second copy of
//! the format in the example plugin, because two hand-written codecs are how a
//! wire format drifts.
//!
//! # Shape
//!
//! Every blob is little-endian, self-delimiting and length-prefixed:
//!
//! ```text
//! blob   := u32 MAGIC, u32 kind, payload
//! str    := u32 len, len bytes of UTF-8
//! strvec := u32 count, count * str
//! ```
//!
//! # Hostile input
//!
//! A guest is third-party code, so a decoded blob is untrusted in exactly the
//! way an archive entry or a wire frame is (README invariant 8). [`Reader`]
//! never allocates from a length it has not first checked against the
//! remaining input and against a [`Limits`] ceiling, and a malformed blob is
//! refused whole: [`decode_item_batch`] returns an error rather than the items
//! it managed to parse before the damage. A partially applied batch would put
//! rows on screen that the plugin never agreed to publish.
//!
//! # Relationship to the wire
//!
//! The item shape here is a thin mapping onto [`crikey_core::Item`], which is
//! what the supervised native protocol already carries. A WASM plugin is
//! therefore indistinguishable downstream from a native one: the host decodes
//! into `Item` and hands it to the ordinary SDK sink.

use std::collections::BTreeMap;

use crikey_core::{
    Action, ActionId, ArgumentPolicy, Category, ExecutionPolicy, HitPolicy, Item, ItemId, PluginId,
};

/// `"CKW1"` read as a little-endian `u32`.
pub const MAGIC: u32 = 0x3157_4B43;

/// The ABI revision this host implements. A guest reports its own through the
/// `crikey_abi_version` export and is refused when the two disagree; the ABI
/// is versioned rather than sniffed so a mismatch is a named refusal at load
/// instead of a mis-parse at query time.
pub const ABI_VERSION: i32 = 1;

/// Blob discriminants. Additive only: a new payload takes the next number and
/// never repurposes an existing one.
pub mod kind {
    /// Host to guest: one suggestion request.
    pub const SUGGEST_REQUEST: u32 = 1;
    /// Guest to host: a batch of items, answering either a suggestion request
    /// or a catalog build.
    pub const ITEM_BATCH: u32 = 2;
    /// Host to guest: one action execution.
    pub const EXECUTE_REQUEST: u32 = 3;
}

/// Bounds applied while decoding a guest blob.
///
/// Every field is a hard refusal threshold, not a truncation point. The
/// defaults are the manifest model's own ceilings where one exists and
/// otherwise a value far above what a launcher row can display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum decoded blob length in bytes.
    pub max_blob_bytes: usize,
    /// Maximum length of any single string field.
    pub max_string_bytes: usize,
    /// Maximum number of items in one batch.
    pub max_items: usize,
    /// Maximum number of actions attached to one item.
    pub max_actions_per_item: usize,
    /// Maximum number of metadata entries on one item.
    pub max_metadata_entries: usize,
    /// Maximum number of search terms on one item.
    pub max_search_terms: usize,
    /// Maximum number of applicable categories on one action.
    pub max_action_categories: usize,
}

impl Limits {
    /// Ceiling on [`Limits::max_blob_bytes`] regardless of configuration. A
    /// guest response is copied out of linear memory, so this also bounds the
    /// host's own allocation for one call.
    pub const MAX_BLOB_BYTES: usize = 32 * 1024 * 1024;
    /// Ceiling on [`Limits::max_items`] regardless of configuration.
    pub const MAX_ITEMS: usize = 10_000;
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_blob_bytes: 8 * 1024 * 1024,
            max_string_bytes: 64 * 1024,
            max_items: 250,
            max_actions_per_item: 64,
            max_metadata_entries: 64,
            max_search_terms: 64,
            max_action_categories: 32,
        }
    }
}

/// Why a guest blob was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbiError {
    /// The blob did not start with [`MAGIC`].
    BadMagic(u32),
    /// The blob announced a payload kind this host does not decode.
    UnexpectedKind { expected: u32, found: u32 },
    /// The blob ended in the middle of a field.
    Truncated {
        field: &'static str,
        need: usize,
        have: usize,
    },
    /// A length prefix exceeded its [`Limits`] ceiling.
    TooLarge {
        field: &'static str,
        len: usize,
        limit: usize,
    },
    /// A string field was not valid UTF-8.
    NotUtf8 { field: &'static str },
    /// A tag byte did not name a variant this ABI defines.
    UnknownTag { field: &'static str, tag: u32 },
    /// Bytes remained after the payload was fully decoded. Trailing data means
    /// the guest and host disagree about the format, so the blob is refused
    /// rather than silently accepted up to the disagreement.
    TrailingBytes(usize),
}

impl std::fmt::Display for AbiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic(found) => {
                write!(formatter, "guest blob magic {found:#010x} is not {MAGIC:#010x}")
            }
            Self::UnexpectedKind { expected, found } => {
                write!(formatter, "guest blob kind {found} where {expected} was expected")
            }
            Self::Truncated { field, need, have } => write!(
                formatter,
                "guest blob truncated reading {field}: {need} bytes needed, {have} available"
            ),
            Self::TooLarge { field, len, limit } => write!(
                formatter,
                "guest blob field {field} declares {len} bytes, above the {limit} byte ceiling"
            ),
            Self::NotUtf8 { field } => write!(formatter, "guest blob field {field} is not UTF-8"),
            Self::UnknownTag { field, tag } => {
                write!(formatter, "guest blob field {field} has unknown tag {tag}")
            }
            Self::TrailingBytes(count) => {
                write!(formatter, "guest blob has {count} unread trailing bytes")
            }
        }
    }
}

impl std::error::Error for AbiError {}

/// Bounded cursor over a guest-supplied blob.
///
/// Reads are checked against the remaining input before anything is allocated,
/// and every length-prefixed field is additionally checked against its
/// [`Limits`] ceiling. The reader therefore cannot be made to reserve memory
/// proportional to a number the guest invented.
#[derive(Debug)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
    limits: Limits,
}

impl<'a> Reader<'a> {
    /// Wraps `bytes` with the decoding ceilings in `limits`.
    pub fn new(bytes: &'a [u8], limits: Limits) -> Self {
        Self {
            bytes,
            cursor: 0,
            limits,
        }
    }

    /// Number of bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.cursor
    }

    fn take(&mut self, field: &'static str, count: usize) -> Result<&'a [u8], AbiError> {
        let have = self.remaining();
        if count > have {
            return Err(AbiError::Truncated {
                field,
                need: count,
                have,
            });
        }
        let slice = &self.bytes[self.cursor..self.cursor + count];
        self.cursor += count;
        Ok(slice)
    }

    /// Reads a `u32`.
    pub fn u32(&mut self, field: &'static str) -> Result<u32, AbiError> {
        let bytes = self.take(field, 4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Reads an `i32`.
    pub fn i32(&mut self, field: &'static str) -> Result<i32, AbiError> {
        Ok(self.u32(field)? as i32)
    }

    /// Reads a `u64`.
    pub fn u64(&mut self, field: &'static str) -> Result<u64, AbiError> {
        let bytes = self.take(field, 8)?;
        let mut buffer = [0u8; 8];
        buffer.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(buffer))
    }

    /// Reads a `u32` that must be a boolean flag. Any value other than 0 or 1
    /// is a disagreement about the format, not a truthy value.
    pub fn flag(&mut self, field: &'static str) -> Result<bool, AbiError> {
        match self.u32(field)? {
            0 => Ok(false),
            1 => Ok(true),
            tag => Err(AbiError::UnknownTag { field, tag }),
        }
    }

    /// Reads a length-prefixed UTF-8 string.
    pub fn string(&mut self, field: &'static str) -> Result<String, AbiError> {
        let len = self.u32(field)? as usize;
        if len > self.limits.max_string_bytes {
            return Err(AbiError::TooLarge {
                field,
                len,
                limit: self.limits.max_string_bytes,
            });
        }
        let bytes = self.take(field, len)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| AbiError::NotUtf8 { field })
    }

    /// Reads a count prefix, refusing one above `limit`.
    ///
    /// The count is checked before the caller reserves anything, which is the
    /// whole point: a four-byte guest field must never authorise a four-
    /// billion-element allocation.
    pub fn count(&mut self, field: &'static str, limit: usize) -> Result<usize, AbiError> {
        let count = self.u32(field)? as usize;
        if count > limit {
            return Err(AbiError::TooLarge {
                field,
                len: count,
                limit,
            });
        }
        Ok(count)
    }

    /// Reads a count-prefixed vector of strings.
    pub fn strings(&mut self, field: &'static str, limit: usize) -> Result<Vec<String>, AbiError> {
        let count = self.count(field, limit)?;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(self.string(field)?);
        }
        Ok(values)
    }

    /// Reads and checks the blob header.
    pub fn header(&mut self, expected: u32) -> Result<(), AbiError> {
        let magic = self.u32("magic")?;
        if magic != MAGIC {
            return Err(AbiError::BadMagic(magic));
        }
        let found = self.u32("kind")?;
        if found != expected {
            return Err(AbiError::UnexpectedKind { expected, found });
        }
        Ok(())
    }

    /// Asserts the payload consumed the whole blob.
    pub fn finish(self) -> Result<(), AbiError> {
        match self.remaining() {
            0 => Ok(()),
            count => Err(AbiError::TrailingBytes(count)),
        }
    }
}

/// Append-only encoder producing the format [`Reader`] consumes.
#[derive(Debug, Default)]
pub struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    /// Starts a blob of `kind` with its header already written.
    pub fn new(kind: u32) -> Self {
        let mut writer = Self { bytes: Vec::new() };
        writer.u32(MAGIC);
        writer.u32(kind);
        writer
    }

    /// Appends a `u32`.
    pub fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    /// Appends an `i32`.
    pub fn i32(&mut self, value: i32) {
        self.u32(value as u32);
    }

    /// Appends a `u64`.
    pub fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    /// Appends a boolean flag.
    pub fn flag(&mut self, value: bool) {
        self.u32(u32::from(value));
    }

    /// Appends a length-prefixed string.
    pub fn string(&mut self, value: &str) {
        self.u32(value.len() as u32);
        self.bytes.extend_from_slice(value.as_bytes());
    }

    /// Appends a count-prefixed vector of strings.
    pub fn strings<S: AsRef<str>>(&mut self, values: &[S]) {
        self.u32(values.len() as u32);
        for value in values {
            self.string(value.as_ref());
        }
    }

    /// Consumes the encoder and yields the blob.
    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// One suggestion request as the guest sees it.
///
/// A thin projection of [`crikey_plugin_sdk::Query`]: identical fields minus
/// the host request id, which is a transport concern the guest cannot act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestRequest {
    /// Raw query text.
    pub text: String,
    /// Normalized query text.
    pub normalized: String,
    /// Monotonic query generation.
    pub generation: u64,
    /// Remaining budget in milliseconds, when the host supplied one.
    pub deadline_ms: Option<u64>,
    /// Item selected by the user, for argument suggestions.
    pub selected_item_id: Option<String>,
}

impl SuggestRequest {
    /// Encodes the request for the guest.
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new(kind::SUGGEST_REQUEST);
        writer.string(&self.text);
        writer.string(&self.normalized);
        writer.u64(self.generation);
        writer.flag(self.deadline_ms.is_some());
        writer.u64(self.deadline_ms.unwrap_or_default());
        writer.flag(self.selected_item_id.is_some());
        writer.string(self.selected_item_id.as_deref().unwrap_or_default());
        writer.finish()
    }

    /// Decodes a request the host encoded. Used by guests through this same
    /// crate, and by the host's own round-trip tests.
    pub fn decode(bytes: &[u8], limits: Limits) -> Result<Self, AbiError> {
        let mut reader = Reader::new(bytes, limits);
        reader.header(kind::SUGGEST_REQUEST)?;
        let text = reader.string("text")?;
        let normalized = reader.string("normalized")?;
        let generation = reader.u64("generation")?;
        let has_deadline = reader.flag("deadline-present")?;
        let deadline = reader.u64("deadline-ms")?;
        let has_selected = reader.flag("selected-present")?;
        let selected = reader.string("selected-item-id")?;
        reader.finish()?;
        Ok(Self {
            text,
            normalized,
            generation,
            deadline_ms: has_deadline.then_some(deadline),
            selected_item_id: has_selected.then_some(selected),
        })
    }
}

/// One action execution as the guest sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteRequest {
    /// Item selected for execution.
    pub item_id: String,
    /// Optional item action.
    pub action_id: Option<String>,
    /// Optional user argument.
    pub argument: Option<String>,
}

impl ExecuteRequest {
    /// Encodes the request for the guest.
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new(kind::EXECUTE_REQUEST);
        writer.string(&self.item_id);
        writer.flag(self.action_id.is_some());
        writer.string(self.action_id.as_deref().unwrap_or_default());
        writer.flag(self.argument.is_some());
        writer.string(self.argument.as_deref().unwrap_or_default());
        writer.finish()
    }

    /// Decodes a request the host encoded.
    pub fn decode(bytes: &[u8], limits: Limits) -> Result<Self, AbiError> {
        let mut reader = Reader::new(bytes, limits);
        reader.header(kind::EXECUTE_REQUEST)?;
        let item_id = reader.string("item-id")?;
        let has_action = reader.flag("action-present")?;
        let action_id = reader.string("action-id")?;
        let has_argument = reader.flag("argument-present")?;
        let argument = reader.string("argument")?;
        reader.finish()?;
        Ok(Self {
            item_id,
            action_id: has_action.then_some(action_id),
            argument: has_argument.then_some(argument),
        })
    }
}

fn argument_policy_tag(policy: ArgumentPolicy) -> u32 {
    match policy {
        ArgumentPolicy::Forbidden => 0,
        ArgumentPolicy::Optional => 1,
        ArgumentPolicy::Required => 2,
    }
}

fn argument_policy_from_tag(tag: u32) -> Result<ArgumentPolicy, AbiError> {
    match tag {
        0 => Ok(ArgumentPolicy::Forbidden),
        1 => Ok(ArgumentPolicy::Optional),
        2 => Ok(ArgumentPolicy::Required),
        tag => Err(AbiError::UnknownTag {
            field: "argument-policy",
            tag,
        }),
    }
}

fn hit_policy_tag(policy: HitPolicy) -> u32 {
    match policy {
        HitPolicy::Recorded => 0,
        HitPolicy::Ignored => 1,
    }
}

fn hit_policy_from_tag(tag: u32) -> Result<HitPolicy, AbiError> {
    match tag {
        0 => Ok(HitPolicy::Recorded),
        1 => Ok(HitPolicy::Ignored),
        tag => Err(AbiError::UnknownTag {
            field: "hit-policy",
            tag,
        }),
    }
}

fn execution_policy_tag(policy: ExecutionPolicy) -> u32 {
    match policy {
        ExecutionPolicy::HostMediated => 0,
        ExecutionPolicy::Plugin => 1,
    }
}

fn execution_policy_from_tag(tag: u32) -> Result<ExecutionPolicy, AbiError> {
    match tag {
        0 => Ok(ExecutionPolicy::HostMediated),
        1 => Ok(ExecutionPolicy::Plugin),
        tag => Err(AbiError::UnknownTag {
            field: "execution-policy",
            tag,
        }),
    }
}

fn encode_action(writer: &mut Writer, action: &Action) {
    writer.string(&action.action_id.0);
    writer.string(&action.label);
    writer.string(&action.description);
    writer.u32(execution_policy_tag(action.execution_policy));
    writer.flag(action.icon_reference.is_some());
    writer.string(action.icon_reference.as_deref().unwrap_or_default());
    let categories: Vec<String> = action
        .applicable_categories
        .iter()
        .map(Category::wire_tag)
        .collect();
    writer.strings(&categories);
}

fn decode_action(reader: &mut Reader<'_>, limits: &Limits) -> Result<Action, AbiError> {
    let action_id = reader.string("action-id")?;
    let label = reader.string("action-label")?;
    let description = reader.string("action-description")?;
    let execution_policy = execution_policy_from_tag(reader.u32("execution-policy")?)?;
    let has_icon = reader.flag("action-icon-present")?;
    let icon = reader.string("action-icon")?;
    let categories = reader.strings("action-categories", limits.max_action_categories)?;
    Ok(Action {
        action_id: ActionId(action_id),
        label,
        description,
        applicable_categories: categories
            .iter()
            .map(|tag| Category::from_wire_tag(tag))
            .collect(),
        icon_reference: has_icon.then_some(icon),
        execution_policy,
    })
}

/// Encodes one item batch. Used by a guest to answer a request.
///
/// The owning plugin identity is not transmitted: it is the host's, not the
/// guest's, so a guest cannot publish an item attributed to a sibling plugin.
pub fn encode_item_batch(items: &[Item]) -> Vec<u8> {
    let mut writer = Writer::new(kind::ITEM_BATCH);
    writer.u32(items.len() as u32);
    for item in items {
        writer.string(&item.stable_id.0);
        writer.string(&item.label);
        writer.string(&item.description);
        writer.string(&item.target);
        writer.string(&item.category.wire_tag());
        writer.i32(item.score_hint);
        writer.u32(argument_policy_tag(item.argument_policy));
        writer.u32(hit_policy_tag(item.hit_policy));
        writer.flag(item.icon_reference.is_some());
        writer.string(item.icon_reference.as_deref().unwrap_or_default());
        writer.strings(&item.search_terms);
        writer.u32(item.metadata.len() as u32);
        for (key, value) in &item.metadata {
            writer.string(key);
            writer.string(value);
        }
        writer.u32(item.actions.len() as u32);
        for action in &item.actions {
            encode_action(&mut writer, action);
        }
    }
    writer.finish()
}

/// Decodes an item batch produced by a guest, attributing every item to
/// `plugin`.
///
/// Refuses the whole batch on the first malformed field. Partial acceptance is
/// not offered on purpose: rows the plugin never finished describing must not
/// reach the ranker.
pub fn decode_item_batch(bytes: &[u8], plugin: &PluginId, limits: Limits) -> Result<Vec<Item>, AbiError> {
    if bytes.len() > limits.max_blob_bytes {
        return Err(AbiError::TooLarge {
            field: "item-batch",
            len: bytes.len(),
            limit: limits.max_blob_bytes,
        });
    }
    let mut reader = Reader::new(bytes, limits);
    reader.header(kind::ITEM_BATCH)?;
    let count = reader.count("item-count", limits.max_items)?;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        let stable_id = reader.string("stable-id")?;
        let label = reader.string("label")?;
        let description = reader.string("description")?;
        let target = reader.string("target")?;
        let category = Category::from_wire_tag(&reader.string("category")?);
        let score_hint = reader.i32("score-hint")?;
        let argument_policy = argument_policy_from_tag(reader.u32("argument-policy")?)?;
        let hit_policy = hit_policy_from_tag(reader.u32("hit-policy")?)?;
        let has_icon = reader.flag("icon-present")?;
        let icon = reader.string("icon")?;
        let search_terms = reader.strings("search-terms", limits.max_search_terms)?;
        let metadata_count = reader.count("metadata-count", limits.max_metadata_entries)?;
        let mut metadata = BTreeMap::new();
        for _ in 0..metadata_count {
            let key = reader.string("metadata-key")?;
            let value = reader.string("metadata-value")?;
            metadata.insert(key, value);
        }
        let action_count = reader.count("action-count", limits.max_actions_per_item)?;
        let mut actions = Vec::with_capacity(action_count);
        for _ in 0..action_count {
            actions.push(decode_action(&mut reader, &limits)?);
        }
        let stable_id = if stable_id.is_empty() {
            ItemId::derived(plugin, &category, &target)
        } else {
            ItemId(stable_id)
        };
        items.push(Item {
            stable_id,
            plugin_id: plugin.clone(),
            category,
            label,
            description,
            target,
            search_terms,
            icon_reference: has_icon.then_some(icon),
            argument_policy,
            hit_policy,
            score_hint,
            metadata,
            actions,
        });
    }
    reader.finish()?;
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin() -> PluginId {
        PluginId("wasm.dev.example.demo".into())
    }

    fn sample_item() -> Item {
        Item {
            stable_id: ItemId("item-1".into()),
            plugin_id: plugin(),
            category: Category::PluginDefined("application".into()),
            label: "Label".into(),
            description: "Description".into(),
            target: "/usr/bin/demo".into(),
            search_terms: vec!["demo".into(), "example".into()],
            icon_reference: Some("icon:demo".into()),
            argument_policy: ArgumentPolicy::Optional,
            hit_policy: HitPolicy::Ignored,
            score_hint: -7,
            metadata: BTreeMap::from([("k".to_owned(), "v".to_owned())]),
            actions: vec![Action {
                action_id: ActionId("open".into()),
                label: "Open".into(),
                description: "Open it".into(),
                applicable_categories: vec![Category::File, Category::PluginDefined("x".into())],
                icon_reference: None,
                execution_policy: ExecutionPolicy::Plugin,
            }],
        }
    }

    #[test]
    fn an_item_survives_the_boundary_with_every_field_intact() {
        let original = sample_item();
        let decoded = decode_item_batch(
            &encode_item_batch(std::slice::from_ref(&original)),
            &plugin(),
            Limits::default(),
        )
        .expect("a batch this host encoded must decode");

        assert_eq!(decoded.len(), 1);
        let item = &decoded[0];
        assert_eq!(item.stable_id, original.stable_id);
        assert_eq!(item.category, original.category);
        assert_eq!(item.label, original.label);
        assert_eq!(item.description, original.description);
        assert_eq!(item.target, original.target);
        assert_eq!(item.search_terms, original.search_terms);
        assert_eq!(item.icon_reference, original.icon_reference);
        assert_eq!(item.argument_policy, original.argument_policy);
        assert_eq!(item.hit_policy, original.hit_policy);
        assert_eq!(item.score_hint, original.score_hint);
        assert_eq!(item.metadata, original.metadata);
        assert_eq!(item.actions.len(), 1);
        assert_eq!(item.actions[0].action_id, original.actions[0].action_id);
        assert_eq!(
            item.actions[0].applicable_categories,
            original.actions[0].applicable_categories
        );
        assert_eq!(
            item.actions[0].execution_policy,
            original.actions[0].execution_policy
        );
    }

    /// A plugin-defined category named after a built-in must not decode into
    /// the built-in: `Category::wire_tag` is the injective spelling and the
    /// codec has to use it on both sides.
    #[test]
    fn a_shadowing_plugin_category_does_not_decode_into_the_builtin() {
        let mut item = sample_item();
        item.category = Category::PluginDefined("application".into());
        let decoded =
            decode_item_batch(&encode_item_batch(&[item]), &plugin(), Limits::default()).expect("decode");
        assert_eq!(decoded[0].category, Category::PluginDefined("application".into()));
        assert_ne!(decoded[0].category, Category::Application);
    }

    #[test]
    fn an_item_without_a_stable_id_is_given_the_host_derived_identity() {
        let mut item = sample_item();
        item.stable_id = ItemId(String::new());
        let decoded =
            decode_item_batch(&encode_item_batch(&[item]), &plugin(), Limits::default()).expect("decode");
        assert_eq!(
            decoded[0].stable_id,
            ItemId::derived(
                &plugin(),
                &Category::PluginDefined("application".into()),
                "/usr/bin/demo"
            )
        );
    }

    /// The guest cannot attribute its rows to another plugin: ownership is
    /// supplied by the host and is not on the wire at all.
    #[test]
    fn decoded_items_are_attributed_to_the_host_supplied_plugin() {
        let mut item = sample_item();
        item.plugin_id = PluginId("wasm.someone.else".into());
        let owner = PluginId("wasm.dev.example.demo".into());
        let decoded =
            decode_item_batch(&encode_item_batch(&[item]), &owner, Limits::default()).expect("decode");
        assert_eq!(decoded[0].plugin_id, owner);
    }

    #[test]
    fn a_blob_with_the_wrong_magic_is_refused() {
        let mut bytes = encode_item_batch(&[]);
        bytes[0] ^= 0xFF;
        assert!(matches!(
            decode_item_batch(&bytes, &plugin(), Limits::default()),
            Err(AbiError::BadMagic(_))
        ));
    }

    #[test]
    fn a_request_blob_is_not_accepted_where_an_item_batch_is_expected() {
        let bytes = SuggestRequest {
            text: "a".into(),
            normalized: "a".into(),
            generation: 1,
            deadline_ms: None,
            selected_item_id: None,
        }
        .encode();
        assert!(matches!(
            decode_item_batch(&bytes, &plugin(), Limits::default()),
            Err(AbiError::UnexpectedKind {
                expected: kind::ITEM_BATCH,
                found: kind::SUGGEST_REQUEST
            })
        ));
    }

    #[test]
    fn a_truncated_blob_is_refused_rather_than_partially_applied() {
        let full = encode_item_batch(&[sample_item()]);
        for cut in [8usize, 12, 20, full.len() - 1] {
            assert!(
                decode_item_batch(&full[..cut], &plugin(), Limits::default()).is_err(),
                "a blob cut at {cut} must be refused"
            );
        }
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut bytes = encode_item_batch(&[sample_item()]);
        bytes.push(0);
        assert!(matches!(
            decode_item_batch(&bytes, &plugin(), Limits::default()),
            Err(AbiError::TrailingBytes(1))
        ));
    }

    /// An enormous declared count must be refused before any allocation is
    /// made proportional to it. The blob here is twelve bytes long and claims
    /// four billion items.
    #[test]
    fn an_enormous_item_count_is_refused_before_allocating() {
        let mut writer = Writer::new(kind::ITEM_BATCH);
        writer.u32(u32::MAX);
        let bytes = writer.finish();
        assert!(matches!(
            decode_item_batch(&bytes, &plugin(), Limits::default()),
            Err(AbiError::TooLarge {
                field: "item-count",
                ..
            })
        ));
    }

    #[test]
    fn an_enormous_string_length_is_refused_before_allocating() {
        let mut writer = Writer::new(kind::ITEM_BATCH);
        writer.u32(1);
        writer.u32(u32::MAX); // stable-id length
        let bytes = writer.finish();
        assert!(matches!(
            decode_item_batch(&bytes, &plugin(), Limits::default()),
            Err(AbiError::TooLarge {
                field: "stable-id",
                ..
            })
        ));
    }

    #[test]
    fn a_blob_above_the_byte_ceiling_is_refused_without_being_parsed() {
        let limits = Limits {
            max_blob_bytes: 16,
            ..Limits::default()
        };
        let bytes = encode_item_batch(&[sample_item()]);
        assert!(bytes.len() > 16);
        assert!(matches!(
            decode_item_batch(&bytes, &plugin(), limits),
            Err(AbiError::TooLarge {
                field: "item-batch",
                ..
            })
        ));
    }

    #[test]
    fn a_non_utf8_string_is_refused() {
        let mut writer = Writer::new(kind::ITEM_BATCH);
        writer.u32(1);
        writer.u32(2);
        let mut bytes = writer.finish();
        bytes.extend_from_slice(&[0xFF, 0xFE]);
        assert!(matches!(
            decode_item_batch(&bytes, &plugin(), Limits::default()),
            Err(AbiError::NotUtf8 { field: "stable-id" })
        ));
    }

    #[test]
    fn an_unknown_policy_tag_is_refused_rather_than_defaulted() {
        let item = sample_item();
        let good = encode_item_batch(std::slice::from_ref(&item));
        // Locate the argument-policy field by re-encoding the prefix that
        // precedes it, so the offset stays correct if a field is added above.
        let mut prefix = Writer::new(kind::ITEM_BATCH);
        prefix.u32(1);
        prefix.string(&item.stable_id.0);
        prefix.string(&item.label);
        prefix.string(&item.description);
        prefix.string(&item.target);
        prefix.string(&item.category.wire_tag());
        prefix.i32(item.score_hint);
        let offset = prefix.finish().len();

        let mut bytes = good.clone();
        bytes[offset..offset + 4].copy_from_slice(&99u32.to_le_bytes());
        assert!(matches!(
            decode_item_batch(&bytes, &plugin(), Limits::default()),
            Err(AbiError::UnknownTag {
                field: "argument-policy",
                tag: 99
            })
        ));
    }

    #[test]
    fn a_suggest_request_round_trips() {
        let request = SuggestRequest {
            text: "Ho La".into(),
            normalized: "ho la".into(),
            generation: 42,
            deadline_ms: Some(37),
            selected_item_id: Some("sel".into()),
        };
        assert_eq!(
            SuggestRequest::decode(&request.encode(), Limits::default()).expect("decode"),
            request
        );

        let bare = SuggestRequest {
            text: String::new(),
            normalized: String::new(),
            generation: 0,
            deadline_ms: None,
            selected_item_id: None,
        };
        assert_eq!(
            SuggestRequest::decode(&bare.encode(), Limits::default()).expect("decode"),
            bare
        );
    }

    #[test]
    fn an_execute_request_round_trips() {
        let request = ExecuteRequest {
            item_id: "item-1".into(),
            action_id: Some("open".into()),
            argument: None,
        };
        assert_eq!(
            ExecuteRequest::decode(&request.encode(), Limits::default()).expect("decode"),
            request
        );
    }
}
