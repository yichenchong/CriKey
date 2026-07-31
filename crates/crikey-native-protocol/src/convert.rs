//! Conversion between core catalog values and the native wire schema.

use crikey_core::{
    Action, ActionId, ArgumentPolicy, Category, ExecutionPolicy, HitPolicy, Item, ItemId, PluginId,
};

use crate::message;
use crate::wire::UnknownFields;

/// Converts a core item to its lossless proto representation (spec 10.1-10.4).
pub fn to_proto_item(item: &Item) -> message::Item {
    message::Item {
        stable_id: item.stable_id.0.clone(),
        label: item.label.clone(),
        description: item.description.clone(),
        target: item.target.clone(),
        category: category_tag(&item.category),
        search_terms: item.search_terms.clone(),
        icon_reference: item.icon_reference.clone().unwrap_or_default(),
        score_hint: item.score_hint,
        metadata: item.metadata.clone(),
        actions: item.actions.iter().map(to_proto_action).collect(),
        argument_policy: argument_policy_tag(item.argument_policy).to_owned(),
        hit_policy: hit_policy_tag(item.hit_policy).to_owned(),
        unknown: UnknownFields::default(),
    }
}

/// Converts a plugin-owned proto item. The host supplies ownership and derives
/// an id when the plugin leaves `stable_id` empty (spec 10.2).
pub fn from_proto_item(plugin: &PluginId, item: &message::Item) -> Item {
    let category = category_from_tag(&item.category);
    let stable_id = if item.stable_id.is_empty() {
        ItemId::derived(plugin, &category, &item.target)
    } else {
        ItemId(item.stable_id.clone())
    };
    Item {
        stable_id,
        plugin_id: plugin.clone(),
        category,
        label: item.label.clone(),
        description: item.description.clone(),
        target: item.target.clone(),
        search_terms: item.search_terms.clone(),
        icon_reference: (!item.icon_reference.is_empty()).then(|| item.icon_reference.clone()),
        argument_policy: argument_policy_from_tag(&item.argument_policy),
        hit_policy: hit_policy_from_tag(&item.hit_policy),
        score_hint: item.score_hint,
        metadata: item.metadata.clone(),
        actions: item.actions.iter().map(from_proto_action).collect(),
    }
}

/// Wire spelling of an argument policy (spec 10.1).
pub fn argument_policy_tag(policy: ArgumentPolicy) -> &'static str {
    match policy {
        ArgumentPolicy::Forbidden => "forbidden",
        ArgumentPolicy::Optional => "optional",
        ArgumentPolicy::Required => "required",
    }
}

/// Parses an argument policy; an unknown value is the conservative default.
pub fn argument_policy_from_tag(tag: &str) -> ArgumentPolicy {
    match tag {
        "optional" => ArgumentPolicy::Optional,
        "required" => ArgumentPolicy::Required,
        _ => ArgumentPolicy::Forbidden,
    }
}

/// Wire spelling of a hit policy (spec 10.1).
pub fn hit_policy_tag(policy: HitPolicy) -> &'static str {
    match policy {
        HitPolicy::Recorded => "recorded",
        HitPolicy::Ignored => "ignored",
    }
}

/// Parses a hit policy; an unknown value is the conservative default.
pub fn hit_policy_from_tag(tag: &str) -> HitPolicy {
    match tag {
        "ignored" => HitPolicy::Ignored,
        _ => HitPolicy::Recorded,
    }
}

/// Converts a core action to its wire representation.
pub fn to_proto_action(action: &Action) -> message::Action {
    message::Action {
        action_id: action.action_id.0.clone(),
        label: action.label.clone(),
        description: action.description.clone(),
        icon_reference: action.icon_reference.clone().unwrap_or_default(),
        execution_policy: match action.execution_policy {
            ExecutionPolicy::HostMediated => "host-mediated".to_owned(),
            ExecutionPolicy::Plugin => "plugin".to_owned(),
        },
        applicable_categories: action.applicable_categories.iter().map(category_tag).collect(),
        unknown: UnknownFields::default(),
    }
}

/// Converts a plugin action; unknown execution policies are plugin mediated.
pub fn from_proto_action(action: &message::Action) -> Action {
    Action {
        action_id: ActionId(action.action_id.clone()),
        label: action.label.clone(),
        description: action.description.clone(),
        applicable_categories: action
            .applicable_categories
            .iter()
            .map(|tag| category_from_tag(tag))
            .collect(),
        icon_reference: (!action.icon_reference.is_empty()).then(|| action.icon_reference.clone()),
        execution_policy: if action.execution_policy == "host-mediated" {
            ExecutionPolicy::HostMediated
        } else {
            ExecutionPolicy::Plugin
        },
    }
}

/// Re-exported so a transport-level caller does not have to reach into
/// `crikey-core` for the discriminator (spec 10.3).
pub use crikey_core::PLUGIN_DEFINED_PREFIX;

/// Stable, injective category tag used by the proto schema (spec 10.3).
///
/// Delegates to [`Category::wire_tag`]: the encoding lives beside the type it
/// encodes, so this transport and the Python worker protocol cannot drift into
/// two different spellings.
pub fn category_tag(category: &Category) -> String {
    category.wire_tag()
}

/// Parses a category tag, retaining unknown plugin-defined categories.
pub fn category_from_tag(tag: &str) -> Category {
    Category::from_wire_tag(tag)
}
