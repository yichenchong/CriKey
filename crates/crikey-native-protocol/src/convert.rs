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

/// Prefix that discriminates a plugin-defined category from a built-in one on
/// the wire (spec 10.3).
///
/// Without it the encoding is not injective: `PluginDefined("application")`
/// and `Category::Application` both render as `"application"`, so a decoder
/// silently rewrites the former into the latter. `crikey-core` treats them as
/// genuinely different categories — `ItemId::derived` gives them distinct
/// identities — so collapsing them would rewrite a plugin's item identity.
/// Core discriminates with a `plugin-defined` tag component; the wire mirrors
/// that spelling.
pub const PLUGIN_DEFINED_PREFIX: &str = "plugin-defined:";

/// Stable category tag used by the proto schema (spec 10.3).
///
/// Built-in categories keep their canonical bare name; a plugin-defined one is
/// prefixed, so every distinct `Category` maps to a distinct tag.
pub fn category_tag(category: &Category) -> String {
    match category {
        Category::PluginDefined(name) => format!("{PLUGIN_DEFINED_PREFIX}{name}"),
        builtin => builtin.as_str().to_owned(),
    }
}

/// Parses a category tag, retaining unknown plugin-defined categories.
///
/// An explicitly prefixed tag is always plugin-defined, even when the name
/// collides with a built-in. A bare unknown tag is still accepted as
/// plugin-defined, so an SDK in another language that has not adopted the
/// prefix keeps working; it simply cannot express a plugin-defined category
/// whose name shadows a built-in one.
pub fn category_from_tag(tag: &str) -> Category {
    if let Some(name) = tag.strip_prefix(PLUGIN_DEFINED_PREFIX) {
        return Category::PluginDefined(name.to_owned());
    }
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
