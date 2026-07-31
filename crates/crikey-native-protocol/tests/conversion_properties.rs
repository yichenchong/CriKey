//! Deterministic property-style coverage for core/native conversion boundaries.
//!
//! These tests deliberately use values that are easy for a hand-picked fixture
//! to miss: empty and Unicode strings, long strings, colliding map-key
//! prefixes, shadowing category names, and non-default policy/collection
//! values.  The generator is deterministic so a failure can always be
//! reproduced from `GENERATOR_SEED`.

use std::collections::BTreeMap;

use crikey_core::{
    Action, ActionId, ArgumentPolicy, Category, ExecutionPolicy, HitPolicy, Item, ItemId, PluginId,
    PLUGIN_DEFINED_PREFIX,
};
use crikey_native_protocol::{
    convert::{
        category_from_tag, category_tag, from_proto_action, from_proto_item, to_proto_action, to_proto_item,
    },
    message, wire, Message,
};

const GENERATOR_SEED: u64 = 0x4d53_2d70_726f_7073;
const TEST_PLUGIN: &str = "dev.example.property-plugin";
const TEST_TARGET: &str = "target-for-category-injectivity";

const BUILTIN_CATEGORY_NAMES: [&str; 9] = [
    "application",
    "file",
    "directory",
    "url",
    "command",
    "expression",
    "keyword",
    "contact",
    "clipboard-item",
];

/// A tiny deterministic xorshift generator.  It is intentionally local to
/// this test: the values, seed, and construction rules are part of the test
/// contract rather than a general-purpose random API.
#[derive(Debug, Clone)]
struct Generator {
    state: u64,
}

impl Generator {
    fn new(seed: u64) -> Self {
        assert_ne!(seed, 0, "the xorshift generator cannot start at zero");
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    fn choose(&mut self, length: usize) -> usize {
        (self.next_u64() as usize) % length
    }

    fn text(&mut self) -> String {
        let values = adversarial_strings();
        values[self.choose(values.len())].clone()
    }

    fn category(&mut self) -> Category {
        let categories = exhaustive_categories();
        categories[self.choose(categories.len())].clone()
    }

    fn metadata(&mut self, index: usize) -> BTreeMap<String, String> {
        match index % 4 {
            0 => BTreeMap::new(),
            1 => BTreeMap::from([("single".to_owned(), self.text())]),
            _ => BTreeMap::from([
                // These keys are distinct but share prefixes and separators.
                // A codec that accidentally sorts/joins map entries can
                // collapse or reorder this set.
                ("key".to_owned(), self.text()),
                ("key:".to_owned(), self.text()),
                ("key::".to_owned(), self.text()),
                ("key\0".to_owned(), self.text()),
                ("key\u{10ffff}".to_owned(), self.text()),
            ]),
        }
    }

    fn action(&mut self, index: usize) -> Action {
        let applicable_categories = match index % 4 {
            0 => Vec::new(),
            1 => vec![self.category()],
            _ => vec![
                Category::Application,
                Category::PluginDefined("application".to_owned()),
                self.category(),
            ],
        };
        let icon_reference = if index % 3 == 0 {
            None
        } else {
            Some(non_empty_text(self))
        };
        Action {
            action_id: ActionId(self.text()),
            label: self.text(),
            description: self.text(),
            applicable_categories,
            icon_reference,
            execution_policy: if index % 2 == 0 {
                ExecutionPolicy::HostMediated
            } else {
                ExecutionPolicy::Plugin
            },
        }
    }

    fn item(&mut self, index: usize) -> Item {
        let actions = match index % 4 {
            0 => Vec::new(),
            1 => vec![self.action(index)],
            _ => vec![self.action(index), self.action(index + 1), self.action(index + 2)],
        };
        let icon_reference = if index % 3 == 0 {
            None
        } else {
            Some(non_empty_text(self))
        };
        Item {
            // Empty IDs exercise the documented derived-ID path; odd cases
            // retain a supplied stable ID.
            stable_id: if index % 2 == 0 {
                ItemId(String::new())
            } else {
                ItemId(format!("stable-id-{index}-{}", non_empty_text(self)))
            },
            // The conversion API receives ownership separately, so generated
            // items intentionally use the same fixed plugin as the test call.
            plugin_id: PluginId(TEST_PLUGIN.to_owned()),
            category: self.category(),
            label: self.text(),
            description: self.text(),
            target: self.text(),
            search_terms: match index % 4 {
                0 => Vec::new(),
                1 => vec![self.text()],
                _ => vec![self.text(), self.text(), self.text()],
            },
            icon_reference,
            argument_policy: match index % 3 {
                0 => ArgumentPolicy::Forbidden,
                1 => ArgumentPolicy::Optional,
                _ => ArgumentPolicy::Required,
            },
            hit_policy: if index % 2 == 0 {
                HitPolicy::Recorded
            } else {
                HitPolicy::Ignored
            },
            score_hint: match index % 5 {
                0 => 0,
                1 => -1,
                2 => i32::MAX,
                3 => i32::MIN,
                _ => (self.next_u64() as i32).wrapping_mul(17),
            },
            metadata: self.metadata(index),
            actions,
        }
    }
}

fn adversarial_strings() -> Vec<String> {
    vec![
        String::new(),
        "ascii".to_owned(),
        "with spaces and punctuation: /\\?&=".to_owned(),
        "日本語・文字列・🚀".to_owned(),
        "\0embedded\nline\tbreak".to_owned(),
        "x".repeat(4096),
        "prefix:key::value".to_owned(),
    ]
}

fn non_empty_text(generator: &mut Generator) -> String {
    loop {
        let value = generator.text();
        if !value.is_empty() {
            return value;
        }
    }
}

fn builtin_category(index: usize) -> Category {
    match index {
        0 => Category::Application,
        1 => Category::File,
        2 => Category::Directory,
        3 => Category::Url,
        4 => Category::Command,
        5 => Category::Expression,
        6 => Category::Keyword,
        7 => Category::Contact,
        8 => Category::ClipboardItem,
        _ => unreachable!("builtin category index is bounded by the table"),
    }
}

fn exhaustive_categories() -> Vec<Category> {
    let mut categories = BUILTIN_CATEGORY_NAMES
        .iter()
        .enumerate()
        .map(|(index, _)| builtin_category(index))
        .collect::<Vec<_>>();
    categories.extend([
        // Every built-in name is included in the plugin-defined namespace.
        Category::PluginDefined("application".to_owned()),
        Category::PluginDefined("file".to_owned()),
        Category::PluginDefined("directory".to_owned()),
        Category::PluginDefined("url".to_owned()),
        Category::PluginDefined("command".to_owned()),
        Category::PluginDefined("expression".to_owned()),
        Category::PluginDefined("keyword".to_owned()),
        Category::PluginDefined("contact".to_owned()),
        Category::PluginDefined("clipboard-item".to_owned()),
        Category::PluginDefined(String::new()),
        Category::PluginDefined("nested-plugin-defined:category".to_owned()),
        Category::PluginDefined(PLUGIN_DEFINED_PREFIX.to_owned()),
        Category::PluginDefined("日本語/🚀".to_owned()),
        Category::PluginDefined("documents::recent/with:separators".to_owned()),
    ]);
    categories
}

fn field_eq<T: std::fmt::Debug + PartialEq>(actual: &T, expected: &T, case: &str, field: &str) {
    assert_eq!(
        actual, expected,
        "{case}: {field} mismatch (generator seed 0x{GENERATOR_SEED:016x})"
    );
}

fn assert_action_fields(actual: &Action, expected: &Action, case: &str) {
    let Action {
        action_id: actual_action_id,
        label: actual_label,
        description: actual_description,
        applicable_categories: actual_applicable_categories,
        icon_reference: actual_icon_reference,
        execution_policy: actual_execution_policy,
    } = actual;
    let Action {
        action_id: expected_action_id,
        label: expected_label,
        description: expected_description,
        applicable_categories: expected_applicable_categories,
        icon_reference: expected_icon_reference,
        execution_policy: expected_execution_policy,
    } = expected;

    field_eq(actual_action_id, expected_action_id, case, "Action.action_id");
    field_eq(actual_label, expected_label, case, "Action.label");
    field_eq(
        actual_description,
        expected_description,
        case,
        "Action.description",
    );
    field_eq(
        actual_applicable_categories,
        expected_applicable_categories,
        case,
        "Action.applicable_categories",
    );
    field_eq(
        actual_icon_reference,
        expected_icon_reference,
        case,
        "Action.icon_reference",
    );
    field_eq(
        actual_execution_policy,
        expected_execution_policy,
        case,
        "Action.execution_policy",
    );
}

fn assert_item_fields(actual: &Item, expected: &Item, plugin: &PluginId, case: &str) {
    // No `..` here is deliberate.  Adding a field to core::Item must force this
    // guard to be updated instead of silently escaping conversion coverage.
    let Item {
        stable_id: actual_stable_id,
        plugin_id: actual_plugin_id,
        category: actual_category,
        label: actual_label,
        description: actual_description,
        target: actual_target,
        search_terms: actual_search_terms,
        icon_reference: actual_icon_reference,
        argument_policy: actual_argument_policy,
        hit_policy: actual_hit_policy,
        score_hint: actual_score_hint,
        metadata: actual_metadata,
        actions: actual_actions,
    } = actual;
    let Item {
        stable_id: expected_stable_id,
        plugin_id: expected_plugin_id,
        category: expected_category,
        label: expected_label,
        description: expected_description,
        target: expected_target,
        search_terms: expected_search_terms,
        icon_reference: expected_icon_reference,
        argument_policy: expected_argument_policy,
        hit_policy: expected_hit_policy,
        score_hint: expected_score_hint,
        metadata: expected_metadata,
        actions: expected_actions,
    } = expected;

    let derived_stable_id = ItemId::derived(plugin, expected_category, expected_target);
    if expected_stable_id.0.is_empty() {
        field_eq(
            actual_stable_id,
            &derived_stable_id,
            case,
            "Item.stable_id (derived)",
        );
    } else {
        field_eq(actual_stable_id, expected_stable_id, case, "Item.stable_id");
    }
    field_eq(actual_plugin_id, expected_plugin_id, case, "Item.plugin_id");
    field_eq(actual_plugin_id, plugin, case, "Item.plugin_id (host ownership)");
    field_eq(actual_category, expected_category, case, "Item.category");
    field_eq(actual_label, expected_label, case, "Item.label");
    field_eq(actual_description, expected_description, case, "Item.description");
    field_eq(actual_target, expected_target, case, "Item.target");
    field_eq(
        actual_search_terms,
        expected_search_terms,
        case,
        "Item.search_terms",
    );
    field_eq(
        actual_icon_reference,
        expected_icon_reference,
        case,
        "Item.icon_reference",
    );
    field_eq(
        actual_argument_policy,
        expected_argument_policy,
        case,
        "Item.argument_policy",
    );
    field_eq(actual_hit_policy, expected_hit_policy, case, "Item.hit_policy");
    field_eq(actual_score_hint, expected_score_hint, case, "Item.score_hint");
    field_eq(actual_metadata, expected_metadata, case, "Item.metadata");

    if actual_actions.len() != expected_actions.len() {
        panic!(
            "{case}: Item.actions length mismatch: actual={} expected={} (generator seed 0x{GENERATOR_SEED:016x})",
            actual_actions.len(),
            expected_actions.len()
        );
    }
    for (index, (actual_action, expected_action)) in actual_actions.iter().zip(expected_actions).enumerate() {
        assert_action_fields(
            actual_action,
            expected_action,
            &format!("{case}.Item.actions[{index}]"),
        );
    }
}

fn assert_proto_action_fields(actual: &message::Action, expected: &message::Action, case: &str) {
    let message::Action {
        action_id: actual_action_id,
        label: actual_label,
        description: actual_description,
        icon_reference: actual_icon_reference,
        execution_policy: actual_execution_policy,
        applicable_categories: actual_applicable_categories,
        unknown: actual_unknown,
    } = actual;
    let message::Action {
        action_id: expected_action_id,
        label: expected_label,
        description: expected_description,
        icon_reference: expected_icon_reference,
        execution_policy: expected_execution_policy,
        applicable_categories: expected_applicable_categories,
        unknown: expected_unknown,
    } = expected;

    field_eq(
        actual_action_id,
        expected_action_id,
        case,
        "proto.Action.action_id",
    );
    field_eq(actual_label, expected_label, case, "proto.Action.label");
    field_eq(
        actual_description,
        expected_description,
        case,
        "proto.Action.description",
    );
    field_eq(
        actual_icon_reference,
        expected_icon_reference,
        case,
        "proto.Action.icon_reference",
    );
    field_eq(
        actual_execution_policy,
        expected_execution_policy,
        case,
        "proto.Action.execution_policy",
    );
    field_eq(
        actual_applicable_categories,
        expected_applicable_categories,
        case,
        "proto.Action.applicable_categories",
    );
    field_eq(actual_unknown, expected_unknown, case, "proto.Action.unknown");
}

fn assert_proto_item_fields(actual: &message::Item, expected: &message::Item, case: &str) {
    let message::Item {
        stable_id: actual_stable_id,
        label: actual_label,
        description: actual_description,
        target: actual_target,
        category: actual_category,
        search_terms: actual_search_terms,
        icon_reference: actual_icon_reference,
        argument_policy: actual_argument_policy,
        hit_policy: actual_hit_policy,
        score_hint: actual_score_hint,
        metadata: actual_metadata,
        actions: actual_actions,
        unknown: actual_unknown,
    } = actual;
    let message::Item {
        stable_id: expected_stable_id,
        label: expected_label,
        description: expected_description,
        target: expected_target,
        category: expected_category,
        search_terms: expected_search_terms,
        icon_reference: expected_icon_reference,
        argument_policy: expected_argument_policy,
        hit_policy: expected_hit_policy,
        score_hint: expected_score_hint,
        metadata: expected_metadata,
        actions: expected_actions,
        unknown: expected_unknown,
    } = expected;

    field_eq(actual_stable_id, expected_stable_id, case, "proto.Item.stable_id");
    field_eq(actual_label, expected_label, case, "proto.Item.label");
    field_eq(
        actual_description,
        expected_description,
        case,
        "proto.Item.description",
    );
    field_eq(actual_target, expected_target, case, "proto.Item.target");
    field_eq(actual_category, expected_category, case, "proto.Item.category");
    field_eq(
        actual_search_terms,
        expected_search_terms,
        case,
        "proto.Item.search_terms",
    );
    field_eq(
        actual_icon_reference,
        expected_icon_reference,
        case,
        "proto.Item.icon_reference",
    );
    field_eq(
        actual_argument_policy,
        expected_argument_policy,
        case,
        "proto.Item.argument_policy",
    );
    field_eq(
        actual_hit_policy,
        expected_hit_policy,
        case,
        "proto.Item.hit_policy",
    );
    field_eq(
        actual_score_hint,
        expected_score_hint,
        case,
        "proto.Item.score_hint",
    );
    field_eq(actual_metadata, expected_metadata, case, "proto.Item.metadata");
    if actual_actions.len() != expected_actions.len() {
        panic!(
            "{case}: proto.Item.actions length mismatch: actual={} expected={} (generator seed 0x{GENERATOR_SEED:016x})",
            actual_actions.len(),
            expected_actions.len()
        );
    }
    for (index, (actual_action, expected_action)) in actual_actions.iter().zip(expected_actions).enumerate() {
        assert_proto_action_fields(
            actual_action,
            expected_action,
            &format!("{case}.proto.Item.actions[{index}]"),
        );
    }
    field_eq(actual_unknown, expected_unknown, case, "proto.Item.unknown");
}

/// Decode just enough protobuf framing to identify field numbers without
/// trusting the message decoder under test.  This prevents a field-value byte
/// that happens to equal a key from making the completeness guard pass.
fn encoded_field_numbers(bytes: &[u8]) -> Vec<u32> {
    let mut cursor = 0;
    let mut fields = Vec::new();
    while cursor < bytes.len() {
        let key = wire::decode_varint(bytes, &mut cursor).expect("encoded key must decode");
        let field = u32::try_from(key >> 3).expect("test fields fit in u32");
        assert_ne!(field, 0, "encoded field number must be non-zero");
        fields.push(field);
        match key & 0x07 {
            0 => {
                wire::decode_varint(bytes, &mut cursor).expect("encoded varint must decode");
            }
            1 => {
                cursor = cursor.checked_add(8).expect("fixed64 length overflow");
            }
            2 => {
                let length = wire::decode_varint(bytes, &mut cursor).expect("encoded length must decode");
                let length = usize::try_from(length).expect("encoded length fits usize");
                cursor = cursor
                    .checked_add(length)
                    .expect("length-delimited field overflow");
            }
            5 => {
                cursor = cursor.checked_add(4).expect("fixed32 length overflow");
            }
            wire_type => panic!("unexpected wire type {wire_type}"),
        }
        assert!(
            cursor <= bytes.len(),
            "encoded field extends beyond the message (cursor={cursor}, len={})",
            bytes.len()
        );
    }
    fields
}

fn assert_nonempty_with_fields(bytes: &[u8], fields: &[u32], case: &str) {
    assert!(!bytes.is_empty(), "{case}: encoded message is empty");
    let encoded_fields = encoded_field_numbers(bytes);
    for field in fields {
        assert!(
            encoded_fields.contains(field),
            "{case}: encoded message is missing field key {field}; got {encoded_fields:?}"
        );
    }
}

fn fully_populated_action() -> Action {
    Action {
        action_id: ActionId("action-id/🚀".to_owned()),
        label: "Action label".to_owned(),
        description: "Action description with:separators".to_owned(),
        applicable_categories: vec![
            Category::Application,
            Category::PluginDefined("application".to_owned()),
            Category::PluginDefined(PLUGIN_DEFINED_PREFIX.to_owned()),
        ],
        icon_reference: Some("icon/action".to_owned()),
        execution_policy: ExecutionPolicy::Plugin,
    }
}

fn fully_populated_item(plugin: &PluginId) -> Item {
    Item {
        stable_id: ItemId("stable-id/non-default".to_owned()),
        plugin_id: plugin.clone(),
        category: Category::PluginDefined("documents::recent/🚀".to_owned()),
        label: "Item label".to_owned(),
        description: "Item description with Unicode 日本語".to_owned(),
        target: "target:/with/separators".to_owned(),
        search_terms: vec!["search term".to_owned(), "日本語".to_owned()],
        icon_reference: Some("icon/item".to_owned()),
        argument_policy: ArgumentPolicy::Required,
        hit_policy: HitPolicy::Ignored,
        score_hint: -31415,
        metadata: BTreeMap::from([
            ("key".to_owned(), "value-1".to_owned()),
            ("key:".to_owned(), "value-2".to_owned()),
            ("key::".to_owned(), "value-3".to_owned()),
        ]),
        actions: vec![fully_populated_action()],
    }
}

#[test]
fn category_wire_encoding_is_exhaustive_round_trip_and_injective() {
    let plugin = PluginId(TEST_PLUGIN.to_owned());
    let categories = exhaustive_categories();
    let mut tags = BTreeMap::<String, Category>::new();
    let mut identities = BTreeMap::<String, Category>::new();

    for category in categories {
        let tag = category.wire_tag();
        assert_eq!(
            Category::from_wire_tag(&tag),
            category,
            "core category wire round-trip failed for {category:?}"
        );
        assert_eq!(
            category_from_tag(&category_tag(&category)),
            category,
            "native transport category wire round-trip failed for {category:?}"
        );

        if let Some(previous) = tags.insert(tag.clone(), category.clone()) {
            panic!("category wire-tag collision: {previous:?} and {category:?} both encode as {tag:?}");
        }
        let identity = ItemId::derived(&plugin, &category, TEST_TARGET).0;
        if let Some(previous) = identities.insert(identity.clone(), category.clone()) {
            panic!(
                "derived ItemId collision for fixed plugin/target: {previous:?} and {category:?} both encode as {identity:?}"
            );
        }
    }
}

#[test]
fn generated_core_conversions_are_lossless_and_wire_round_trippable() {
    let plugin = PluginId(TEST_PLUGIN.to_owned());
    let mut generator = Generator::new(GENERATOR_SEED);

    for index in 0..96 {
        let item = generator.item(index);
        let case = format!("Item[{index}]");
        let proto_item = to_proto_item(&item);
        let item_bytes = proto_item.encode();
        let decoded_proto_item =
            message::Item::decode(&item_bytes).expect("generated Item must decode after encoding");
        assert_proto_item_fields(&decoded_proto_item, &proto_item, &case);

        let decoded_item = from_proto_item(&plugin, &proto_item);
        assert_item_fields(&decoded_item, &item, &plugin, &case);

        // Decode the actual wire bytes before conversion too; this catches a
        // field that is present in the struct mapping but omitted by encode.
        let decoded_wire_item =
            message::Item::decode(&item_bytes).expect("generated Item wire bytes must decode");
        let decoded_wire_core = from_proto_item(&plugin, &decoded_wire_item);
        assert_item_fields(&decoded_wire_core, &item, &plugin, &format!("{case} wire"));

        for (action_index, action) in item.actions.iter().enumerate() {
            let proto_action = to_proto_action(action);
            let action_case = format!("{case}.Action[{action_index}]");
            let action_bytes = proto_action.encode();
            let decoded_proto_action =
                message::Action::decode(&action_bytes).expect("generated Action must decode after encoding");
            assert_proto_action_fields(&decoded_proto_action, &proto_action, &action_case);
            let decoded_action = from_proto_action(&proto_action);
            assert_action_fields(&decoded_action, action, &action_case);
            let decoded_wire_action =
                message::Action::decode(&action_bytes).expect("generated Action wire bytes must decode");
            let decoded_wire_core = from_proto_action(&decoded_wire_action);
            assert_action_fields(&decoded_wire_core, action, &format!("{action_case} wire"));
        }
    }
}

#[test]
fn fully_populated_proto_messages_emit_every_field_key_and_round_trip_every_core_field() {
    let plugin = PluginId(TEST_PLUGIN.to_owned());
    let item = fully_populated_item(&plugin);
    let action = fully_populated_action();

    let proto_action = to_proto_action(&action);
    let action_bytes = proto_action.encode();
    assert_nonempty_with_fields(&action_bytes, &[1, 2, 3, 4, 5, 6], "fully populated Action");
    let decoded_proto_action =
        message::Action::decode(&action_bytes).expect("fully populated Action must decode");
    assert_proto_action_fields(&decoded_proto_action, &proto_action, "fully populated Action");
    assert_action_fields(
        &from_proto_action(&decoded_proto_action),
        &action,
        "fully populated Action",
    );

    let proto_item = to_proto_item(&item);
    let item_bytes = proto_item.encode();
    assert_nonempty_with_fields(
        &item_bytes,
        // Item fields 11 and 12 carry the formerly host-only policies.
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        "fully populated Item",
    );
    let decoded_proto_item = message::Item::decode(&item_bytes).expect("fully populated Item must decode");
    assert_proto_item_fields(&decoded_proto_item, &proto_item, "fully populated Item");
    assert_item_fields(
        &from_proto_item(&plugin, &decoded_proto_item),
        &item,
        &plugin,
        "fully populated Item",
    );
}
