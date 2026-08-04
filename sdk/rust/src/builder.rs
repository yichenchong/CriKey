//! Fluent item and action builders (spec 10.1, 10.4).

use std::collections::BTreeMap;

use crikey_core::{
    Action, ActionId, ArgumentPolicy, Category, ExecutionPolicy, HitPolicy, Item, ItemId, PluginId,
};

/// Builds a core [`Item`] with the plugin-author defaults (spec 10.1–10.3).
#[must_use = "call ItemBuilder::build to create the item"]
#[derive(Debug, Clone)]
pub struct ItemBuilder {
    stable_id: String,
    label: String,
    description: String,
    target: String,
    category: Category,
    score_hint: i32,
    search_terms: Vec<String>,
    metadata: BTreeMap<String, String>,
    actions: Vec<Action>,
    icon_reference: Option<String>,
}

impl ItemBuilder {
    /// Starts an item with a stable identifier and display label.
    pub fn new(stable_id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            stable_id: stable_id.into(),
            label: label.into(),
            description: String::new(),
            target: String::new(),
            category: Category::PluginDefined("plugin-defined".to_owned()),
            score_hint: 0,
            search_terms: Vec::new(),
            metadata: BTreeMap::new(),
            actions: Vec::new(),
            icon_reference: None,
        }
    }

    /// Sets the launch target.
    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = target.into();
        self
    }

    /// Sets the display description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Sets the item category.
    pub fn category(mut self, category: Category) -> Self {
        self.category = category;
        self
    }

    /// Sets the ranker's score hint.
    pub fn score_hint(mut self, score_hint: i32) -> Self {
        self.score_hint = score_hint;
        self
    }

    /// Adds one searchable term.
    pub fn search_term(mut self, search_term: impl Into<String>) -> Self {
        self.search_terms.push(search_term.into());
        self
    }

    /// Inserts metadata; a repeated key replaces the previous value.
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Adds one action.
    pub fn action(mut self, action: Action) -> Self {
        self.actions.push(action);
        self
    }

    /// Sets the icon reference.
    pub fn icon(mut self, icon_reference: impl Into<String>) -> Self {
        self.icon_reference = Some(icon_reference.into());
        self
    }

    /// Consumes the builder and produces the core item.
    pub fn build(self) -> Item {
        Item {
            stable_id: ItemId(self.stable_id),
            plugin_id: PluginId(String::new()),
            category: self.category,
            label: self.label,
            description: self.description,
            target: self.target,
            search_terms: self.search_terms,
            icon_reference: self.icon_reference,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: HitPolicy::Recorded,
            score_hint: self.score_hint,
            metadata: self.metadata,
            actions: self.actions,
        }
    }
}

/// Builds an action for an item (spec 10.4).
#[must_use = "call ActionBuilder::build to create the action"]
#[derive(Debug, Clone)]
pub struct ActionBuilder {
    action_id: String,
    label: String,
    description: String,
    icon_reference: Option<String>,
    applicable_categories: Vec<Category>,
    execution_policy: ExecutionPolicy,
}

impl ActionBuilder {
    /// Starts an action with a stable identifier and display label.
    pub fn new(action_id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            action_id: action_id.into(),
            label: label.into(),
            description: String::new(),
            icon_reference: None,
            applicable_categories: Vec::new(),
            execution_policy: ExecutionPolicy::Plugin,
        }
    }

    /// Sets the action description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Sets the action icon reference.
    pub fn icon(mut self, icon_reference: impl Into<String>) -> Self {
        self.icon_reference = Some(icon_reference.into());
        self
    }

    /// Restricts this action to items of `category` (spec 10.4). Repeatable;
    /// leaving it unset means the action applies to any item the plugin
    /// returns.
    pub fn applicable_category(mut self, category: Category) -> Self {
        self.applicable_categories.push(category);
        self
    }

    /// Selects host-mediated execution instead of plugin execution.
    pub fn host_mediated(mut self) -> Self {
        self.execution_policy = ExecutionPolicy::HostMediated;
        self
    }

    /// Consumes the builder and produces the core action.
    pub fn build(self) -> Action {
        Action {
            action_id: ActionId(self.action_id),
            label: self.label,
            description: self.description,
            applicable_categories: self.applicable_categories,
            icon_reference: self.icon_reference,
            execution_policy: self.execution_policy,
        }
    }
}
