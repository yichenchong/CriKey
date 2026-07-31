//! Conversion between core catalog values and the native wire schema.

use crikey_core::{Action, ActionId, Category, ExecutionPolicy, Item, ItemId, PluginId};

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
        argument_policy: crikey_core::ArgumentPolicy::default(),
        hit_policy: crikey_core::HitPolicy::default(),
        score_hint: item.score_hint,
        metadata: item.metadata.clone(),
        actions: item.actions.iter().map(from_proto_action).collect(),
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
        unknown: UnknownFields::default(),
    }
}

/// Converts a plugin action; unknown execution policies are plugin mediated.
pub fn from_proto_action(action: &message::Action) -> Action {
    Action {
        action_id: ActionId(action.action_id.clone()),
        label: action.label.clone(),
        description: action.description.clone(),
        applicable_categories: Vec::new(),
        icon_reference: (!action.icon_reference.is_empty()).then(|| action.icon_reference.clone()),
        execution_policy: if action.execution_policy == "host-mediated" {
            ExecutionPolicy::HostMediated
        } else {
            ExecutionPolicy::Plugin
        },
    }
}

/// Stable category tag used by the proto schema (spec 10.3).
pub fn category_tag(category: &Category) -> String {
    category.as_str().to_owned()
}

/// Parses a category tag, retaining unknown plugin-defined categories.
pub fn category_from_tag(tag: &str) -> Category {
    match tag {
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
