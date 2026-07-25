//! Item actions (spec 10.4).

use crate::item::Category;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionId(pub String);

/// Where and how an action runs. Action execution has a lifecycle separate
/// from suggestion requests and is not cancelled by a query change (spec 9.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionPolicy {
    /// Executed by the host on behalf of the plugin.
    #[default]
    HostMediated,
    /// Executed inside the owning plugin worker.
    Plugin,
}

#[derive(Debug, Clone)]
pub struct Action {
    pub action_id: ActionId,
    pub label: String,
    pub description: String,
    pub applicable_categories: Vec<Category>,
    pub icon_reference: Option<String>,
    pub execution_policy: ExecutionPolicy,
}
