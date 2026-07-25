//! The catalog item model (spec 10.1 - 10.3).

use std::{collections::BTreeMap, fmt::Write};

use crate::action::Action;

/// Identifier of the plugin that owns an item.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginId(pub String);

/// Stable item identity. Never derived from the display label alone (spec 10.2).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId(pub String);

impl ItemId {
    /// Host-side fallback when a plugin does not supply an identifier.
    ///
    /// Every tuple component is byte-length-prefixed, and the category variant
    /// has its own tag. The encoding is therefore injective even when values
    /// contain the separators used by older display-oriented encodings.
    pub fn derived(plugin: &PluginId, category: &Category, target: &str) -> Self {
        let (category_tag, category_value) = match category {
            Category::Application => ("application", ""),
            Category::File => ("file", ""),
            Category::Directory => ("directory", ""),
            Category::Url => ("url", ""),
            Category::Command => ("command", ""),
            Category::Expression => ("expression", ""),
            Category::Keyword => ("keyword", ""),
            Category::Contact => ("contact", ""),
            Category::ClipboardItem => ("clipboard-item", ""),
            Category::PluginDefined(name) => ("plugin-defined", name.as_str()),
        };

        let mut encoded = String::new();
        for component in [
            "crikey-derived-item-v1",
            plugin.0.as_str(),
            category_tag,
            category_value,
            target,
        ] {
            write!(&mut encoded, "{}:", component.len()).expect("writing to a String cannot fail");
            encoded.push_str(component);
        }
        ItemId(encoded)
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

    #[test]
    fn derived_identity_distinguishes_builtin_and_plugin_defined_categories() {
        let plugin = PluginId("dev.example.apps".into());
        let builtin = ItemId::derived(&plugin, &Category::Application, "/usr/bin/foo");
        let plugin_defined = ItemId::derived(
            &plugin,
            &Category::PluginDefined("application".into()),
            "/usr/bin/foo",
        );

        assert_ne!(builtin, plugin_defined);
    }

    #[test]
    fn derived_identity_is_unambiguous_when_components_contain_separators() {
        let plugin = PluginId("dev.example.apps".into());
        let category_separator = ItemId::derived(
            &plugin,
            &Category::PluginDefined("documents::recent".into()),
            "entry",
        );
        let target_separator = ItemId::derived(
            &plugin,
            &Category::PluginDefined("documents".into()),
            "recent::entry",
        );

        assert_ne!(category_separator, target_separator);
    }
}
