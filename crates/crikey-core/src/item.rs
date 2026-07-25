//! The catalog item model (spec 10.1 - 10.3).

use std::collections::BTreeMap;

use crate::action::Action;

/// Identifier of the plugin that owns an item.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginId(pub String);

/// Stable item identity. Never derived from the display label alone (spec 10.2).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId(pub String);

impl ItemId {
    /// Host-side fallback when a plugin does not supply an identifier.
    pub fn derived(plugin: &PluginId, category: &Category, target: &str) -> Self {
        ItemId(format!("{}::{}::{}", plugin.0, category.as_str(), target))
    }
}

/// Extensible item category (spec 10.3).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    Application,
    File,
    Directory,
    Url,
    Command,
    Expression,
    Keyword,
    Contact,
    ClipboardItem,
    /// Plugin defined category, identified by its declared name.
    PluginDefined(String),
}

impl Category {
    pub fn as_str(&self) -> &str {
        match self {
            Category::Application => "application",
            Category::File => "file",
            Category::Directory => "directory",
            Category::Url => "url",
            Category::Command => "command",
            Category::Expression => "expression",
            Category::Keyword => "keyword",
            Category::Contact => "contact",
            Category::ClipboardItem => "clipboard-item",
            Category::PluginDefined(name) => name,
        }
    }
}

/// How an item consumes user supplied arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ArgumentPolicy {
    #[default]
    Forbidden,
    Optional,
    Required,
}

/// How selecting an item affects history and reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HitPolicy {
    #[default]
    Recorded,
    Ignored,
}

/// A catalog item as held by the core (spec 10.1).
#[derive(Debug, Clone)]
pub struct Item {
    pub stable_id: ItemId,
    pub plugin_id: PluginId,
    pub category: Category,
    pub label: String,
    pub description: String,
    pub target: String,
    pub search_terms: Vec<String>,
    pub icon_reference: Option<String>,
    pub argument_policy: ArgumentPolicy,
    pub hit_policy: HitPolicy,
    pub score_hint: i32,
    pub metadata: BTreeMap<String, String>,
    pub actions: Vec<Action>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_identity_ignores_label() {
        let plugin = PluginId("dev.example.apps".into());
        let a = ItemId::derived(&plugin, &Category::Application, "/usr/bin/foo");
        let b = ItemId::derived(&plugin, &Category::Application, "/usr/bin/foo");
        assert_eq!(a, b);
    }
}
