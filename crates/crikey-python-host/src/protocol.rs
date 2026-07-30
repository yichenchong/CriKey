//! The newline-delimited JSON protocol spoken between the modern host and the
//! `_crikey_modern_worker.py` child (spec 15.6, 15.7; contract §1, §2).
//!
//! One JSON object per line, both directions. The child's stdout is a strict
//! protocol channel; anything a plugin prints is captured by the shim and
//! returned inside a reply's `log`, never as a bare line. This module owns the
//! wire vocabulary: the frame builders the host writes, the item/action codec
//! that maps the §2 JSON to the real [`crikey_core::Item`] (contract §11), and
//! the bounded-log discipline every reply's `log` is held to.
//!
//! The bounds mirror the legacy worker exactly (`crikey-legacy-compat`): a
//! plugin cannot make the host allocate without limit, and an over-long line is
//! a named protocol failure rather than unbounded growth.

use serde_json::{json, Map, Value};

use crikey_core::{Action, ActionId, ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId};

use crate::worker::SuggestRequest;

/// The frame schema this host speaks. Echoed by the child's handshake.
///
/// One number for every CriKey transport, so a native and a modern worker can
/// never silently diverge: this is exactly the native protocol's version.
pub const PROTOCOL_VERSION: u32 = crikey_native_protocol::PROTOCOL_VERSION;

/// Ceiling on one protocol line, in bytes.
///
/// Generous, because a legitimate catalog frame from a large plugin is large.
/// Overflow behaviour: the line is abandoned and reported as a protocol failure
/// carrying a bounded excerpt. The channel is not resynchronised afterwards — a
/// peer that emitted an eight-megabyte line has already lost the framing — so
/// the child is stopped.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Ceiling on the number of log lines retained from one reply.
///
/// Overflow behaviour: the first [`MAX_LOG_LINES`] lines are kept and one
/// synthetic line records how many were dropped, so a truncated log says that
/// it is truncated instead of quietly lying.
pub const MAX_LOG_LINES: usize = 512;

/// Ceiling on one retained log line, in bytes. Longer lines are truncated at a
/// character boundary with an explicit marker.
pub const MAX_LOG_LINE_BYTES: usize = 4096;

/// Ceiling on the retained stderr tail, in bytes.
///
/// Overflow behaviour: the *oldest* lines are dropped. This buffer exists to
/// explain a crash, and the interesting output of a crashing process is the
/// output nearest the crash.
pub const MAX_STDERR_TAIL_BYTES: usize = 8 * 1024;

/// How much of an over-long protocol line is quoted back in the error.
pub(crate) const PROTOCOL_EXCERPT_BYTES: usize = 4096;

// ---------------------------------------------------------------------------
// Frame kinds (FROZEN — contract §2)
// ---------------------------------------------------------------------------

/// Host → worker: the startup handshake carrying the protocol version.
pub(crate) const KIND_HANDSHAKE: &str = "handshake";
/// Host → worker: ask the plugin to publish its static catalog.
pub(crate) const KIND_BUILD_CATALOG: &str = "build_catalog";
/// Host → worker: ask the plugin to suggest for one query generation.
pub(crate) const KIND_SUGGEST: &str = "suggest";
/// Host → worker: ask the plugin to execute an item (optionally an action).
pub(crate) const KIND_EXECUTE: &str = "execute";
/// Host → worker: raise or lower the cooperative cancellation flag. No `id`.
pub(crate) const KIND_SET_CANCEL: &str = "set_cancel";
/// Host → worker: ask the child to exit.
pub(crate) const KIND_SHUTDOWN: &str = "shutdown";

/// Worker → host: acknowledges the handshake.
pub(crate) const KIND_HANDSHAKE_ACK: &str = "handshake_ack";
/// Worker → host: a suggestion batch, partial or terminal.
pub(crate) const KIND_RESULT_BATCH: &str = "result_batch";
/// Worker → host: a catalog batch, more-to-come or done.
pub(crate) const KIND_CATALOG_BATCH: &str = "catalog_batch";
/// Worker → host: the outcome of one execute.
pub(crate) const KIND_EXECUTE_RESULT: &str = "execute_result";

// ---------------------------------------------------------------------------
// Host → worker frame builders
// ---------------------------------------------------------------------------

/// `{"id","kind":"handshake","protocol_version":1}`
pub(crate) fn encode_handshake(id: u64) -> Value {
    json!({ "id": id, "kind": KIND_HANDSHAKE, "protocol_version": PROTOCOL_VERSION })
}

/// `{"id","kind":"build_catalog"}`
pub(crate) fn encode_build_catalog(id: u64) -> Value {
    json!({ "id": id, "kind": KIND_BUILD_CATALOG })
}

/// `{"id","kind":"suggest","generation","text","normalized","selected_item_id"}`
pub(crate) fn encode_suggest(id: u64, request: &SuggestRequest) -> Value {
    json!({
        "id": id,
        "kind": KIND_SUGGEST,
        "generation": request.generation,
        "text": request.text,
        "normalized": request.normalized,
        "selected_item_id": request.selected_item_id,
    })
}

/// `{"id","kind":"execute","item":<Item>,"action_id","argument"}`
pub(crate) fn encode_execute(id: u64, item: &Item, action_id: Option<&str>, argument: Option<&str>) -> Value {
    json!({
        "id": id,
        "kind": KIND_EXECUTE,
        "item": encode_item(item),
        "action_id": action_id,
        "argument": argument,
    })
}

/// `{"kind":"set_cancel","cancelled":<bool>}` — a control frame with NO `id`,
/// written from a separate thread while a call is in flight.
pub(crate) fn encode_set_cancel(cancelled: bool) -> Value {
    json!({ "kind": KIND_SET_CANCEL, "cancelled": cancelled })
}

/// `{"id","kind":"shutdown"}`
pub(crate) fn encode_shutdown(id: u64) -> Value {
    json!({ "id": id, "kind": KIND_SHUTDOWN })
}

/// Renders a core item as the §2 wire `Item` a plugin receives in `execute`.
///
/// The plugin-facing shape carries no `plugin_id` (ownership is the host's, not
/// the plugin's to read back) and no argument/hit policy (those are host-side
/// concepts the modern SDK does not surface): exactly the §2 vocabulary.
pub(crate) fn encode_item(item: &Item) -> Value {
    json!({
        "stable_id": item.stable_id.0,
        "label": item.label,
        "description": item.description,
        "target": item.target,
        "category": item.category.as_str(),
        "search_terms": item.search_terms,
        "icon_reference": item.icon_reference,
        "score_hint": item.score_hint,
        "metadata": item.metadata,
        "actions": item.actions.iter().map(encode_action).collect::<Vec<_>>(),
    })
}

/// `{"action_id","label","description","icon_reference"}`
pub(crate) fn encode_action(action: &Action) -> Value {
    json!({
        "action_id": action.action_id.0,
        "label": action.label,
        "description": action.description,
        "icon_reference": action.icon_reference,
    })
}

// ---------------------------------------------------------------------------
// Worker → host item/action decode (contract §11)
// ---------------------------------------------------------------------------

/// Decodes the `items` array of a reply frame into core items owned by `plugin`.
///
/// All-or-nothing: a single malformed item makes the whole frame a protocol
/// failure, because a decoder that skipped one would silently drop a result the
/// plugin meant to publish.
pub(crate) fn decode_items(plugin: &PluginId, frame: &Map<String, Value>) -> Option<Vec<Item>> {
    let entries = frame.get("items")?.as_array()?;
    let mut items = Vec::with_capacity(entries.len());
    for entry in entries {
        items.push(decode_item(plugin, entry)?);
    }
    Some(items)
}

/// Builds a core item from a §2 wire item.
///
/// Ownership and identity: the HOST assigns `plugin_id` (spec 10.2) — a plugin
/// that could name another plugin's id could inject items into its catalog. A
/// modern plugin DOES supply its own `stable_id`, and the host keeps it exactly
/// (contract §11); it is never overwritten with a derived id. An unknown
/// `category` is a plugin-defined category, never an error (spec 10.3).
pub(crate) fn decode_item(plugin: &PluginId, value: &Value) -> Option<Item> {
    let object = value.as_object()?;

    let stable_id = ItemId(object.get("stable_id")?.as_str()?.to_owned());
    let label = object.get("label")?.as_str()?.to_owned();
    let target = object.get("target")?.as_str()?.to_owned();

    let category = object
        .get("category")
        .and_then(Value::as_str)
        .map(decode_category)
        .unwrap_or_else(|| Category::PluginDefined(String::from("plugin-defined")));

    Some(Item {
        stable_id,
        plugin_id: plugin.clone(),
        category,
        label,
        description: object
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        target,
        search_terms: decode_strings(object.get("search_terms")),
        icon_reference: object
            .get("icon_reference")
            .and_then(Value::as_str)
            .map(str::to_owned),
        // The modern SDK does not surface argument/hit policy to a plugin, so
        // these take their defaults: inventing a policy the plugin never
        // declared could let an item accept arguments it was meant to forbid.
        argument_policy: ArgumentPolicy::default(),
        hit_policy: HitPolicy::default(),
        score_hint: object
            .get("score_hint")
            .and_then(Value::as_i64)
            .and_then(|hint| i32::try_from(hint).ok())
            .unwrap_or(0),
        metadata: decode_metadata(object.get("metadata")),
        actions: decode_actions(object.get("actions")),
    })
}

/// An unknown category is a plugin-defined one, never an error (spec 10.3).
pub(crate) fn decode_category(name: &str) -> Category {
    match name {
        "application" => Category::Application,
        "file" => Category::File,
        "directory" => Category::Directory,
        "url" => Category::Url,
        "command" => Category::Command,
        "expression" => Category::Expression,
        "keyword" => Category::Keyword,
        "contact" => Category::Contact,
        "clipboard-item" => Category::ClipboardItem,
        other => Category::PluginDefined(other.to_owned()),
    }
}

/// Decodes an item's actions. A missing or malformed `actions` field yields no
/// actions rather than failing the whole item: an item without actions is still
/// a valid item.
fn decode_actions(value: Option<&Value>) -> Vec<Action> {
    let Some(entries) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    entries.iter().filter_map(decode_action).collect()
}

/// `{"action_id","label","description","icon_reference"}` → [`Action`].
fn decode_action(value: &Value) -> Option<Action> {
    let object = value.as_object()?;
    Some(Action {
        action_id: ActionId(object.get("action_id")?.as_str()?.to_owned()),
        label: object.get("label")?.as_str()?.to_owned(),
        description: object
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        // The wire carries no applicable-category set or execution policy for a
        // modern action; both take their host-side defaults.
        applicable_categories: Vec::new(),
        icon_reference: object
            .get("icon_reference")
            .and_then(Value::as_str)
            .map(str::to_owned),
        execution_policy: crikey_core::ExecutionPolicy::default(),
    })
}

/// A JSON array of strings, dropping any non-string entries.
fn decode_strings(value: Option<&Value>) -> Vec<String> {
    let Some(entries) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| entry.as_str().map(str::to_owned))
        .collect()
}

/// A JSON object of string → string, dropping any non-string values.
fn decode_metadata(value: Option<&Value>) -> std::collections::BTreeMap<String, String> {
    let Some(object) = value.and_then(Value::as_object) else {
        return std::collections::BTreeMap::new();
    };
    object
        .iter()
        .filter_map(|(key, val)| val.as_str().map(|text| (key.clone(), text.to_owned())))
        .collect()
}

// ---------------------------------------------------------------------------
// Bounded log decode
// ---------------------------------------------------------------------------

/// Retains a bounded, self-describing record of what the plugin printed.
///
/// Mirrors the legacy worker: at most [`MAX_LOG_LINES`] lines, each at most
/// [`MAX_LOG_LINE_BYTES`], and a synthetic trailer when either bound clamps.
pub(crate) fn decode_log(frame: &Map<String, Value>) -> Vec<String> {
    let Some(entries) = frame.get("log").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut log: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    for entry in entries {
        let Some(text) = entry.as_str() else {
            continue;
        };
        if log.len() >= MAX_LOG_LINES {
            dropped += 1;
            continue;
        }
        log.push(clamp_log_line(text));
    }

    if dropped > 0 {
        // A truncated log says so. A log that silently ended would be read as
        // a plugin that silently stopped.
        log.push(format!(
            "[crikey: {dropped} further log line(s) dropped; a reply retains at most \
             {MAX_LOG_LINES}]"
        ));
    }
    log
}

fn clamp_log_line(text: &str) -> String {
    if text.len() <= MAX_LOG_LINE_BYTES {
        return text.to_owned();
    }

    let end = floor_char_boundary(text, MAX_LOG_LINE_BYTES);
    format!(
        "{}[crikey: log line truncated at {MAX_LOG_LINE_BYTES} bytes]",
        &text[..end]
    )
}

/// The largest index at or below `limit` that splits `text` between characters.
///
/// Hand-written because `str::floor_char_boundary` is still unstable, and
/// slicing a multi-byte character in half would panic on a plugin's output.
pub(crate) fn floor_char_boundary(text: &str, limit: usize) -> usize {
    let mut end = limit.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}
