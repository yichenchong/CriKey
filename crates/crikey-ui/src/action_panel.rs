//! Bounded host-owned action presentation and identity-based navigation.
//!
//! Trees are prepared on result changes, not during painting. A renderer borrows
//! `visible_entries`, reports identities, and dispatches only `activation` results.
//! Presentation metadata grants no permission and destructive styling is not confirmation.

use std::collections::HashSet;
use std::fmt;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use crikey_core::{ActionId, ItemId, PluginId};

use crate::ResultRow;

pub const MAX_ACTION_ENTRIES: usize = 256;
/// Root entries have depth one; sections do not consume a level.
pub const MAX_ACTION_DEPTH: usize = 4;
pub const MAX_ACTION_TEXT_BYTES: usize = 8 * 1024;
pub const MAX_ACTION_TOTAL_TEXT_BYTES: usize = 256 * 1024;
pub const MAX_ACTION_QUERY_BYTES: usize = 8 * 1024;

static NEXT_PANEL_FOCUS: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ActionTarget {
    Result { owner: PluginId, item: ItemId },
    Command(crikey_core::CommandRef),
}

impl ActionTarget {
    pub fn of(row: &ResultRow) -> Self {
        row.target.clone()
    }

    pub fn matches(&self, row: &ResultRow) -> bool {
        self == &row.target
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        match self {
            Self::Result { owner, item } => owner.0.capacity() + item.0.capacity(),
            Self::Command(reference) => reference.provider_id.0.capacity() + reference.command_id.capacity(),
        }
    }
}

/// Only implemented host operations belong here. These never enter plugin dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostAction {
    OpenSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ActionIdentity {
    InvokeCommand,
    Plugin(ActionId),
    Host(HostAction),
    Submenu(String),
}

/// Explicit host presentation declarations; no meaning is inferred from labels.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActionPresentation {
    pub label: String,
    pub description: String,
    pub icon_reference: Option<String>,
    pub shortcut: Option<String>,
    pub destructive: bool,
}

#[derive(Debug, Clone)]
pub enum ActionEntry {
    Command {
        presentation: ActionPresentation,
    },
    Plugin {
        action: ActionId,
        presentation: ActionPresentation,
    },
    Host {
        action: HostAction,
        presentation: ActionPresentation,
    },
    Submenu {
        id: String,
        presentation: ActionPresentation,
        entries: Vec<ActionEntry>,
    },
}

#[derive(Debug, Clone)]
pub struct ActionSection {
    pub id: String,
    pub label: String,
    pub entries: Vec<ActionEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionPanelRefusal {
    TooManyEntries,
    TooDeep,
    TextTooLong(&'static str),
    TotalTextTooLong,
    EmptyIdentity(&'static str),
    EmptyLabel,
    DuplicateSection,
    DuplicateIdentity,
    EmptySubmenu,
    UnknownPluginAction(ActionId),
    AmbiguousPluginAction(ActionId),
    QueryTooLong,
}

impl fmt::Display for ActionPanelRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyEntries => write!(f, "Action panel exceeds {MAX_ACTION_ENTRIES} entries"),
            Self::TooDeep => write!(f, "Action panel exceeds depth {MAX_ACTION_DEPTH}"),
            Self::TextTooLong(field) => write!(f, "Action panel {field} exceeds its text limit"),
            Self::TotalTextTooLong => f.write_str("Action panel exceeds its total text limit"),
            Self::EmptyIdentity(field) => write!(f, "Action panel {field} must not be empty"),
            Self::EmptyLabel => f.write_str("Action panel labels must not be empty"),
            Self::DuplicateSection => f.write_str("Action panel section identity is duplicated"),
            Self::DuplicateIdentity => f.write_str("Action panel entry identity is duplicated"),
            Self::EmptySubmenu => f.write_str("Action submenu must contain an executable action"),
            Self::UnknownPluginAction(id) => write!(f, "Action {:?} is not declared by this target", id.0),
            Self::AmbiguousPluginAction(id) => write!(f, "Action {:?} is declared more than once", id.0),
            Self::QueryTooLong => write!(f, "Action search exceeds {MAX_ACTION_QUERY_BYTES} bytes"),
        }
    }
}

impl std::error::Error for ActionPanelRefusal {}

#[derive(Debug)]
struct PreparedSection {
    id: String,
    label: String,
    children: Vec<usize>,
}

#[derive(Debug)]
struct PreparedEntry {
    identity: ActionIdentity,
    presentation: ActionPresentation,
    section: usize,
    parent: Option<usize>,
    children: Vec<usize>,
    breadcrumb: String,
    search: String,
}

/// Immutable, shareable presentation for one exact owner and result item.
#[derive(Debug)]
pub struct ActionTree {
    target: ActionTarget,
    sections: Vec<PreparedSection>,
    entries: Vec<PreparedEntry>,
    primary: Option<ActionIdentity>,
    management: Vec<ResultRow>,
}

impl ActionTree {
    /// Validates all declarations against the current flat plugin actions. Every
    /// executable plugin leaf must name a real action, including future nested presentations.
    pub fn new(row: &ResultRow, sections: Vec<ActionSection>) -> Result<Arc<Self>, ActionPanelRefusal> {
        let mut budget = TextBudget(0);
        match &row.target {
            ActionTarget::Result { owner, item } => {
                budget.identity(&owner.0, "owner")?;
                budget.identity(&item.0, "item")?;
            }
            ActionTarget::Command(reference) => {
                budget.identity(&reference.provider_id.0, "owner")?;
                budget.identity(&reference.command_id, "command")?;
            }
        }
        if row.alternate_actions.len() > MAX_ACTION_ENTRIES - usize::from(row.default_action.is_some()) {
            return Err(ActionPanelRefusal::TooManyEntries);
        }
        let mut available = HashSet::new();
        for action in row.default_action.iter().chain(row.alternate_actions.iter()) {
            budget.identity(&action.action_id.0, "action identity")?;
            if !available.insert(&action.action_id) {
                return Err(ActionPanelRefusal::AmbiguousPluginAction(
                    action.action_id.clone(),
                ));
            }
        }
        if sections.len() > MAX_ACTION_ENTRIES {
            return Err(ActionPanelRefusal::TooManyEntries);
        }
        let mut tree = Self {
            target: ActionTarget::of(row),
            sections: Vec::with_capacity(sections.len()),
            entries: Vec::new(),
            primary: None,
            management: Vec::new(),
        };
        let mut section_ids = HashSet::new();
        let mut identities = HashSet::new();
        for section in sections {
            budget.identity(&section.id, "section identity")?;
            budget.label(&section.label)?;
            if !section_ids.insert(section.id.clone()) {
                return Err(ActionPanelRefusal::DuplicateSection);
            }
            let index = tree.sections.len();
            tree.sections.push(PreparedSection {
                id: section.id,
                label: section.label,
                children: Vec::new(),
            });
            for entry in section.entries {
                let child = tree.prepare(entry, index, None, 1, &available, &mut identities, &mut budget)?;
                tree.sections[index].children.push(child);
            }
        }
        if matches!(row.target, ActionTarget::Command(_)) {
            if tree.index(&ActionIdentity::InvokeCommand).is_some() {
                tree.primary = Some(ActionIdentity::InvokeCommand);
            }
        } else if let Some(action) = &row.default_action {
            let identity = ActionIdentity::Plugin(action.action_id.clone());
            if tree.index(&identity).is_some() {
                tree.primary = Some(identity);
            }
        }
        Ok(Arc::new(tree))
    }

    /// Preserves flat plugin order and appends only explicitly supplied host entries.
    /// Core Action has no shortcut/destructive declaration; only the primary
    /// Enter label is supplied by the host here.
    pub fn from_flat(
        row: &ResultRow,
        host_entries: Vec<ActionEntry>,
    ) -> Result<Arc<Self>, ActionPanelRefusal> {
        // Refuse unbounded provider text before copying it into declarations.
        if row.alternate_actions.len() > MAX_ACTION_ENTRIES - usize::from(row.default_action.is_some()) {
            return Err(ActionPanelRefusal::TooManyEntries);
        }
        let mut budget = TextBudget(0);
        for action in row.default_action.iter().chain(row.alternate_actions.iter()) {
            budget.identity(&action.action_id.0, "action identity")?;
            budget.label(&action.label)?;
            budget.text(&action.description, "description", MAX_ACTION_TEXT_BYTES)?;
            if let Some(icon) = &action.icon_reference {
                budget.text(icon, "icon", MAX_ACTION_TEXT_BYTES)?;
            }
        }
        let actions = row.default_action.iter().chain(row.alternate_actions.iter());
        let mut plugin_entries: Vec<ActionEntry> = actions
            .map(|action| ActionEntry::Plugin {
                action: action.action_id.clone(),
                presentation: ActionPresentation {
                    label: action.label.clone(),
                    description: action.description.clone(),
                    icon_reference: action.icon_reference.clone(),
                    shortcut: row
                        .default_action
                        .as_ref()
                        .filter(|primary| {
                            matches!(row.target, ActionTarget::Result { .. })
                                && primary.action_id == action.action_id
                        })
                        .map(|_| "Enter".into()),
                    destructive: false,
                },
            })
            .collect();
        if matches!(row.target, ActionTarget::Command(_)) {
            plugin_entries.insert(
                0,
                ActionEntry::Command {
                    presentation: ActionPresentation {
                        label: "Invoke Command".into(),
                        shortcut: Some("Enter".into()),
                        ..Default::default()
                    },
                },
            );
        }
        Self::new(
            row,
            vec![
                ActionSection {
                    id: "plugin".into(),
                    label: "Actions".into(),
                    entries: plugin_entries,
                },
                ActionSection {
                    id: "host".into(),
                    label: "CriKey".into(),
                    entries: host_entries,
                },
            ],
        )
    }

    pub fn open_settings_entry() -> ActionEntry {
        ActionEntry::Host {
            action: HostAction::OpenSettings,
            presentation: ActionPresentation {
                label: "Open Settings".into(),
                icon_reference: Some("host:settings".into()),
                shortcut: Some("Ctrl/Cmd+,".into()),
                ..Default::default()
            },
        }
    }

    pub fn target(&self) -> &ActionTarget {
        &self.target
    }

    pub fn management_rows(&self) -> &[ResultRow] {
        &self.management
    }

    pub(crate) fn with_management(mut tree: Arc<Self>, row: &ResultRow, commands: &[ResultRow]) -> Arc<Self> {
        if matches!(row.target, ActionTarget::Result { .. }) {
            if let Some(source) = &row.source {
                let mut related: Vec<_> = commands
                    .iter()
                    .filter(|command| {
                        matches!(command.target, ActionTarget::Command(_))
                            && command.source.as_ref().is_some_and(|candidate| {
                                candidate.owner == source.owner && candidate.item == source.item
                            })
                    })
                    .cloned()
                    .collect();
                related.sort_by(|left, right| match (&left.target, &right.target) {
                    (ActionTarget::Command(left), ActionTarget::Command(right)) => left.cmp(right),
                    _ => std::cmp::Ordering::Equal,
                });
                related.dedup_by(|left, right| left.target == right.target);
                if let Some(tree) = Arc::get_mut(&mut tree) {
                    tree.management = related;
                }
            }
        }
        tree
    }
    pub fn primary(&self) -> Option<&ActionIdentity> {
        self.primary.as_ref()
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn presentation(&self, identity: &ActionIdentity) -> Option<&ActionPresentation> {
        self.index(identity)
            .map(|index| &self.entries[index].presentation)
    }

    /// The primary/footer path and panel path share this exact validation.
    /// None means a submenu, never an executable placeholder.
    pub fn execution(
        &self,
        identity: &ActionIdentity,
        current: &ResultRow,
    ) -> Result<Option<ActionExecution>, ActionPanelInvalidation> {
        if !self.target.matches(current) {
            return Err(ActionPanelInvalidation::TargetUnavailable);
        }
        if self.index(identity).is_none() {
            return Err(ActionPanelInvalidation::ActionUnavailable);
        }
        match identity {
            ActionIdentity::InvokeCommand => {
                let ActionTarget::Command(reference) = &current.target else {
                    return Err(ActionPanelInvalidation::ActionUnavailable);
                };
                if !current
                    .command_state
                    .as_ref()
                    .is_some_and(|state| state.available)
                {
                    return Err(ActionPanelInvalidation::ActionUnavailable);
                }
                Ok(Some(ActionExecution::Command {
                    reference: reference.clone(),
                }))
            }
            ActionIdentity::Submenu(_) => Ok(None),
            ActionIdentity::Host(action) => Ok(Some(ActionExecution::Host(*action))),
            ActionIdentity::Plugin(action) => {
                if current.alternate_actions.len()
                    > MAX_ACTION_ENTRIES - usize::from(current.default_action.is_some())
                {
                    return Err(ActionPanelInvalidation::ActionUnavailable);
                }
                let count = current
                    .default_action
                    .iter()
                    .chain(current.alternate_actions.iter())
                    .filter(|candidate| &candidate.action_id == action)
                    .count();
                if count != 1 {
                    return Err(ActionPanelInvalidation::ActionUnavailable);
                }
                let source = current
                    .source
                    .as_ref()
                    .ok_or(ActionPanelInvalidation::ActionUnavailable)?;
                Ok(Some(ActionExecution::Plugin {
                    owner: source.owner.clone(),
                    item: source.item.clone(),
                    action: action.clone(),
                }))
            }
        }
    }

    fn index(&self, identity: &ActionIdentity) -> Option<usize> {
        self.entries.iter().position(|entry| &entry.identity == identity)
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare(
        &mut self,
        entry: ActionEntry,
        section: usize,
        parent: Option<usize>,
        depth: usize,
        available: &HashSet<&ActionId>,
        identities: &mut HashSet<ActionIdentity>,
        budget: &mut TextBudget,
    ) -> Result<usize, ActionPanelRefusal> {
        if depth > MAX_ACTION_DEPTH {
            return Err(ActionPanelRefusal::TooDeep);
        }
        if self.entries.len() == MAX_ACTION_ENTRIES {
            return Err(ActionPanelRefusal::TooManyEntries);
        }
        let (identity, presentation, children) = match entry {
            ActionEntry::Command { presentation } => {
                (ActionIdentity::InvokeCommand, presentation, Vec::new())
            }
            ActionEntry::Plugin { action, presentation } => {
                if !available.contains(&action) {
                    return Err(ActionPanelRefusal::UnknownPluginAction(action));
                }
                (ActionIdentity::Plugin(action), presentation, Vec::new())
            }
            ActionEntry::Host { action, presentation } => {
                (ActionIdentity::Host(action), presentation, Vec::new())
            }
            ActionEntry::Submenu {
                id,
                presentation,
                entries,
            } => {
                budget.identity(&id, "submenu identity")?;
                if entries.is_empty() {
                    return Err(ActionPanelRefusal::EmptySubmenu);
                }
                (ActionIdentity::Submenu(id), presentation, entries)
            }
        };
        if !identities.insert(identity.clone()) {
            return Err(ActionPanelRefusal::DuplicateIdentity);
        }
        budget.label(&presentation.label)?;
        budget.text(&presentation.description, "description", MAX_ACTION_TEXT_BYTES)?;
        if let Some(icon) = &presentation.icon_reference {
            budget.text(icon, "icon", MAX_ACTION_TEXT_BYTES)?;
        }
        if let Some(shortcut) = &presentation.shortcut {
            budget.text(shortcut, "shortcut", 256)?;
        }
        let breadcrumb = match parent {
            Some(index) => format!(
                "{} / {}",
                self.entries[index].breadcrumb, self.entries[index].presentation.label
            ),
            None => self.sections[section].label.clone(),
        };
        // Derived strings are also charged, bounding retained search/breadcrumb storage.
        budget.derived(&breadcrumb)?;
        // Section headings describe layout, not every action's search terms.
        // Preserve ancestor menu terms without making a generic "Actions"
        // heading keep a renamed leaf in every matching search.
        let search = match parent {
            Some(index) => format!(
                "{} {} {}",
                self.entries[index].search, presentation.label, presentation.description
            ),
            None => format!("{} {}", presentation.label, presentation.description),
        }
        .to_lowercase();
        budget.derived(&search)?;
        let index = self.entries.len();
        self.entries.push(PreparedEntry {
            identity,
            presentation,
            section,
            parent,
            children: Vec::new(),
            breadcrumb,
            search,
        });
        for child in children {
            let child = self.prepare(
                child,
                section,
                Some(index),
                depth + 1,
                available,
                identities,
                budget,
            )?;
            self.entries[index].children.push(child);
        }
        Ok(index)
    }
}

struct TextBudget(usize);
impl TextBudget {
    fn text(&mut self, text: &str, field: &'static str, max: usize) -> Result<(), ActionPanelRefusal> {
        if text.len() > max {
            return Err(ActionPanelRefusal::TextTooLong(field));
        }
        self.derived(text)
    }
    fn derived(&mut self, text: &str) -> Result<(), ActionPanelRefusal> {
        self.0 = self
            .0
            .checked_add(text.len())
            .ok_or(ActionPanelRefusal::TotalTextTooLong)?;
        if self.0 > MAX_ACTION_TOTAL_TEXT_BYTES {
            return Err(ActionPanelRefusal::TotalTextTooLong);
        }
        Ok(())
    }
    fn identity(&mut self, text: &str, field: &'static str) -> Result<(), ActionPanelRefusal> {
        if text.is_empty() {
            return Err(ActionPanelRefusal::EmptyIdentity(field));
        }
        self.text(text, field, MAX_ACTION_TEXT_BYTES)
    }
    fn label(&mut self, text: &str) -> Result<(), ActionPanelRefusal> {
        if text.trim().is_empty() {
            return Err(ActionPanelRefusal::EmptyLabel);
        }
        self.text(text, "label", MAX_ACTION_TEXT_BYTES)
    }
}

/// Borrowed paint data. Sections and breadcrumbs are prepared once in the tree.
#[derive(Debug, Clone, Copy)]
pub struct VisibleAction<'a> {
    pub identity: &'a ActionIdentity,
    pub presentation: &'a ActionPresentation,
    pub section_id: &'a str,
    pub section_label: &'a str,
    pub breadcrumb: &'a str,
    pub submenu: bool,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionExecution {
    Command {
        reference: crikey_core::CommandRef,
    },
    Plugin {
        owner: PluginId,
        item: ItemId,
        action: ActionId,
    },
    Host(HostAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionPanelInvalidation {
    TargetUnavailable,
    ActionUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionPanelNavigation {
    Changed,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionActivation {
    EnteredSubmenu,
    Execute(ActionExecution),
}

/// Mutable panel state is small; clones share the tree and never clone results.
#[derive(Debug, Clone)]
pub struct ActionPanel {
    tree: Arc<ActionTree>,
    focus_token: u64,
    query: String,
    path: Vec<ActionIdentity>,
    selected: Option<ActionIdentity>,
    browse_selection: Option<ActionIdentity>,
    visible: Vec<usize>,
    invalidation: Option<ActionPanelInvalidation>,
}

impl ActionPanel {
    pub fn new(tree: Arc<ActionTree>) -> Self {
        let mut panel = Self {
            focus_token: NEXT_PANEL_FOCUS
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| next.checked_add(1))
                .expect("action panel identity exhausted"),
            tree,
            query: String::new(),
            path: Vec::with_capacity(MAX_ACTION_DEPTH),
            selected: None,
            browse_selection: None,
            visible: Vec::with_capacity(MAX_ACTION_ENTRIES),
            invalidation: None,
        };
        panel.rebuild();
        panel
    }

    pub(crate) fn focus_token(&self) -> u64 {
        self.focus_token
    }

    pub fn tree(&self) -> &Arc<ActionTree> {
        &self.tree
    }
    pub fn query(&self) -> &str {
        &self.query
    }
    pub fn path(&self) -> &[ActionIdentity] {
        &self.path
    }
    pub fn selected(&self) -> Option<&ActionIdentity> {
        self.selected.as_ref()
    }
    pub fn invalidation(&self) -> Option<ActionPanelInvalidation> {
        self.invalidation
    }
    pub fn is_searching(&self) -> bool {
        !self.query.trim().is_empty()
    }

    /// Atomic refusal: oversize input leaves search, path, focus and results untouched.
    pub fn set_query(&mut self, query: String) -> Result<bool, ActionPanelRefusal> {
        if query.len() > MAX_ACTION_QUERY_BYTES {
            return Err(ActionPanelRefusal::QueryTooLong);
        }
        if self.query == query || self.invalidation.is_some() {
            return Ok(false);
        }
        let was_searching = self.is_searching();
        let searching = !query.trim().is_empty();
        if !was_searching && searching {
            self.browse_selection = self.selected.clone();
        }
        self.query = query;
        if was_searching && !searching {
            self.selected = self.browse_selection.take();
        }
        self.rebuild();
        Ok(true)
    }

    /// No matching, allocation, or result cloning occurs while iterating paint data.
    pub fn visible_entries(&self) -> impl ExactSizeIterator<Item = VisibleAction<'_>> {
        self.visible.iter().map(|&index| {
            let entry = &self.tree.entries[index];
            let section = &self.tree.sections[entry.section];
            VisibleAction {
                identity: &entry.identity,
                presentation: &entry.presentation,
                section_id: &section.id,
                section_label: &section.label,
                breadcrumb: &entry.breadcrumb,
                submenu: !entry.children.is_empty(),
                selected: self.selected.as_ref() == Some(&entry.identity),
            }
        })
    }

    /// Pointer and keyboard both select exact identities, never filtered indexes.
    pub fn select(&mut self, identity: &ActionIdentity) -> bool {
        if self.selected.as_ref() == Some(identity) {
            return false;
        }
        if !self
            .visible
            .iter()
            .any(|&i| &self.tree.entries[i].identity == identity)
        {
            return false;
        }
        self.selected = Some(identity.clone());
        true
    }

    /// Clamped navigation; negative deltas move up. Empty search results have no focus.
    pub fn move_selection(&mut self, delta: isize) -> bool {
        if self.visible.is_empty() || delta == 0 {
            return false;
        }
        let current = self
            .visible
            .iter()
            .position(|&i| Some(&self.tree.entries[i].identity) == self.selected.as_ref());
        let next = match current {
            Some(current) => current.saturating_add_signed(delta).min(self.visible.len() - 1),
            None if delta < 0 => self.visible.len() - 1,
            None => 0,
        };
        let identity = self.tree.entries[self.visible[next]].identity.clone();
        self.select(&identity)
    }

    /// Escape clears action search first, then backs out of a submenu, then asks
    /// the owning view to close this panel. It never touches the launcher query.
    pub fn back(&mut self) -> ActionPanelNavigation {
        if self.invalidation.is_some() {
            return ActionPanelNavigation::Close;
        }
        if !self.query.is_empty() {
            self.set_query(String::new())
                .expect("empty action query is bounded");
            return ActionPanelNavigation::Changed;
        }
        let Some(parent) = self.path.pop() else {
            return ActionPanelNavigation::Close;
        };
        self.selected = Some(parent);
        self.rebuild();
        ActionPanelNavigation::Changed
    }

    /// Revalidates target and leaf against the current row immediately before dispatch.
    /// Host execution is a separate typed branch, never a plugin action ID.
    pub fn activation(
        &mut self,
        current: &ResultRow,
    ) -> Result<Option<ActionActivation>, ActionPanelInvalidation> {
        if let Some(reason) = self.invalidation {
            return Err(reason);
        }
        if !self.tree.target.matches(current) {
            self.invalidate(ActionPanelInvalidation::TargetUnavailable);
            return Err(ActionPanelInvalidation::TargetUnavailable);
        }
        let Some(identity) = self.selected.clone() else {
            return Ok(None);
        };
        let Some(index) = self.tree.index(&identity) else {
            self.invalidate(ActionPanelInvalidation::ActionUnavailable);
            return Err(ActionPanelInvalidation::ActionUnavailable);
        };
        if !self.visible.contains(&index) {
            return Ok(None);
        }
        match self.tree.execution(&identity, current) {
            Err(reason) => {
                self.invalidate(reason);
                Err(reason)
            }
            Ok(Some(execution)) => Ok(Some(ActionActivation::Execute(execution))),
            Ok(None) => {
                self.path.push(identity);
                self.selected = None;
                self.rebuild();
                Ok(Some(ActionActivation::EnteredSubmenu))
            }
        }
    }

    /// Call after an admitted provider update, with the rebuilt tree for the same
    /// exact target, or None if it vanished. Retired panels cannot be revived.
    pub fn reconcile(&mut self, replacement: Option<Arc<ActionTree>>) -> Result<(), ActionPanelInvalidation> {
        if let Some(reason) = self.invalidation {
            return Err(reason);
        }
        let Some(tree) = replacement.filter(|tree| tree.target == self.tree.target) else {
            self.invalidate(ActionPanelInvalidation::TargetUnavailable);
            return Err(ActionPanelInvalidation::TargetUnavailable);
        };
        if self.selected.as_ref().is_some_and(|id| tree.index(id).is_none()) {
            self.invalidate(ActionPanelInvalidation::ActionUnavailable);
            return Err(ActionPanelInvalidation::ActionUnavailable);
        }
        self.tree = tree;
        // Validate the full parent chain; moved menus cannot silently redirect a path.
        let mut parent = None;
        let mut valid = 0;
        for identity in &self.path {
            let Some(index) = self.tree.index(identity) else {
                break;
            };
            let entry = &self.tree.entries[index];
            if entry.parent != parent || entry.children.is_empty() {
                break;
            }
            parent = Some(index);
            valid += 1;
        }
        self.path.truncate(valid);
        let previous = self.selected.clone();
        // In browsing mode, follow a surviving focused action if its declared
        // parent changed. Search keeps its suspended browse path instead.
        if !self.is_searching() {
            if let Some(index) = previous.as_ref().and_then(|id| self.tree.index(id)) {
                self.path.clear();
                let mut parent = self.tree.entries[index].parent;
                while let Some(index) = parent {
                    self.path.push(self.tree.entries[index].identity.clone());
                    parent = self.tree.entries[index].parent;
                }
                self.path.reverse();
            }
        }
        self.rebuild();
        // Refresh may retain only the exact previously focused visible identity.
        // In particular, rebuild's browsing fallback must never revive focus
        // lost on an earlier refresh; only a user interaction may choose anew.
        self.selected = previous.filter(|identity| {
            self.visible
                .iter()
                .any(|&index| &self.tree.entries[index].identity == identity)
        });
        Ok(())
    }

    fn invalidate(&mut self, reason: ActionPanelInvalidation) {
        self.invalidation = Some(reason);
        self.selected = None;
        self.browse_selection = None;
        self.visible.clear();
    }

    fn rebuild(&mut self) {
        self.visible.clear();
        if self.invalidation.is_some() {
            return;
        }
        if self.is_searching() {
            let query = self.query.to_lowercase();
            for (index, entry) in self.tree.entries.iter().enumerate() {
                if entry.children.is_empty()
                    && query.split_whitespace().all(|word| entry.search.contains(word))
                {
                    self.visible.push(index);
                }
            }
        } else if let Some(parent) = self.path.last().and_then(|id| self.tree.index(id)) {
            self.visible
                .extend_from_slice(&self.tree.entries[parent].children);
        } else {
            for section in &self.tree.sections {
                self.visible.extend_from_slice(&section.children);
            }
        }
        if !self
            .visible
            .iter()
            .any(|&i| Some(&self.tree.entries[i].identity) == self.selected.as_ref())
        {
            self.selected = self
                .visible
                .first()
                .map(|&i| self.tree.entries[i].identity.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crikey_core::{Action, ExecutionPolicy};

    fn row() -> ResultRow {
        ResultRow {
            target: crate::ActionTarget::Result {
                owner: PluginId("owner-a".into()),
                item: ItemId("item".into()),
            },
            source: Some(crate::ResultSource {
                owner: PluginId("owner-a".into()),
                item: ItemId("item".into()),
                kind: crate::ResultSourceKind::Ordinary,
            }),
            command_state: None,
            label: "Result".into(),
            description: String::new(),
            icon_reference: None,
            icon: None,
            category: String::new(),
            plugin_name: String::new(),
            highlights: Vec::new(),
            argument_hint: None,
            status: None,
            default_action: Some(action("open", "Open")),
            alternate_actions: vec![action("copy", "Copy"), action("remove", "Delete permanently")],
        }
    }

    fn action(id: &str, label: &str) -> Action {
        Action {
            action_id: ActionId(id.into()),
            label: label.into(),
            description: String::new(),
            applicable_categories: Vec::new(),
            icon_reference: None,
            execution_policy: ExecutionPolicy::Plugin,
        }
    }

    fn id(id: &str) -> ActionIdentity {
        ActionIdentity::Plugin(ActionId(id.into()))
    }

    fn leaf(id: &str, label: &str) -> ActionEntry {
        ActionEntry::Plugin {
            action: ActionId(id.into()),
            presentation: ActionPresentation {
                label: label.into(),
                ..Default::default()
            },
        }
    }

    fn menu(id: &str, label: &str, entries: Vec<ActionEntry>) -> ActionEntry {
        ActionEntry::Submenu {
            id: id.into(),
            presentation: ActionPresentation {
                label: label.into(),
                ..Default::default()
            },
            entries,
        }
    }

    fn section(entries: Vec<ActionEntry>) -> Vec<ActionSection> {
        vec![ActionSection {
            id: "main".into(),
            label: "Item actions".into(),
            entries,
        }]
    }

    fn nested(row: &ResultRow) -> Arc<ActionTree> {
        ActionTree::new(
            row,
            section(vec![
                leaf("open", "Open"),
                menu(
                    "tools",
                    "Tools",
                    vec![
                        leaf("copy", "Copy"),
                        menu("advanced", "Advanced", vec![leaf("remove", "Remove")]),
                    ],
                ),
                ActionTree::open_settings_entry(),
            ]),
        )
        .unwrap()
    }

    #[test]
    fn filtered_nonfirst_execution_keeps_exact_owner_item_action() {
        let row = row();
        let mut panel = ActionPanel::new(ActionTree::from_flat(&row, vec![]).unwrap());
        panel.set_query("DELETE".into()).unwrap();
        assert_eq!(panel.visible_entries().len(), 1);
        assert_eq!(
            panel.activation(&row),
            Ok(Some(ActionActivation::Execute(ActionExecution::Plugin {
                owner: row.source.as_ref().unwrap().owner.clone(),
                item: row.source.as_ref().unwrap().item.clone(),
                action: ActionId("remove".into()),
            })))
        );
        // Labels never infer destructive metadata.
        assert!(!panel.visible_entries().next().unwrap().presentation.destructive);
    }

    #[test]
    fn host_settings_is_not_a_plugin_action_even_with_identical_label() {
        let mut row = row();
        row.default_action.as_mut().unwrap().label = "Open Settings".into();
        let mut panel =
            ActionPanel::new(ActionTree::from_flat(&row, vec![ActionTree::open_settings_entry()]).unwrap());
        assert!(panel.select(&ActionIdentity::Host(HostAction::OpenSettings)));
        assert_eq!(
            panel.activation(&row),
            Ok(Some(ActionActivation::Execute(ActionExecution::Host(
                HostAction::OpenSettings
            ))))
        );
        assert_eq!(
            panel.tree().execution(&id("open"), &row),
            Ok(Some(ActionExecution::Plugin {
                owner: row.source.as_ref().unwrap().owner.clone(),
                item: row.source.as_ref().unwrap().item.clone(),
                action: ActionId("open".into()),
            }))
        );
    }

    #[test]
    fn search_flattens_leaves_with_context_and_restores_path_and_focus() {
        let row = row();
        let mut panel = ActionPanel::new(nested(&row));
        let tools = ActionIdentity::Submenu("tools".into());
        panel.select(&tools);
        assert_eq!(panel.activation(&row), Ok(Some(ActionActivation::EnteredSubmenu)));
        assert_eq!(panel.selected(), Some(&id("copy")));
        let browse_path = panel.path().to_vec();
        panel.set_query("TOOLS advanced remove".into()).unwrap();
        let entry = panel.visible_entries().next().unwrap();
        assert_eq!(entry.identity, &id("remove"));
        assert_eq!(entry.breadcrumb, "Item actions / Tools / Advanced");
        assert!(!entry.submenu);
        assert_eq!(panel.back(), ActionPanelNavigation::Changed);
        assert_eq!(panel.path(), browse_path);
        assert_eq!(panel.selected(), Some(&id("copy")));
        assert_eq!(panel.back(), ActionPanelNavigation::Changed);
        assert_eq!(panel.selected(), Some(&tools));
        assert_eq!(panel.back(), ActionPanelNavigation::Close);
    }

    #[test]
    fn refresh_preserves_search_focus_across_order_and_label_changes() {
        let mut row = row();
        let mut panel = ActionPanel::new(ActionTree::from_flat(&row, vec![]).unwrap());
        panel.set_query("o".into()).unwrap();
        panel.select(&id("copy"));
        row.alternate_actions.reverse();
        row.alternate_actions[1].label = "Copy content".into();
        panel
            .reconcile(Some(ActionTree::from_flat(&row, vec![]).unwrap()))
            .unwrap();
        assert_eq!(panel.query(), "o");
        assert_eq!(panel.selected(), Some(&id("copy")));
        assert!(
            matches!(panel.activation(&row), Ok(Some(ActionActivation::Execute(ActionExecution::Plugin { action, .. }))) if action == ActionId("copy".into()))
        );
    }

    #[test]
    fn disappearing_action_invalidates_without_retargeting_or_revival() {
        let mut row = row();
        let mut panel = ActionPanel::new(ActionTree::from_flat(&row, vec![]).unwrap());
        panel.select(&id("copy"));
        row.alternate_actions.remove(0);
        let tree = ActionTree::from_flat(&row, vec![]).unwrap();
        assert_eq!(
            panel.reconcile(Some(tree.clone())),
            Err(ActionPanelInvalidation::ActionUnavailable)
        );
        assert_eq!(panel.visible_entries().len(), 0);
        assert_eq!(
            panel.reconcile(Some(tree)),
            Err(ActionPanelInvalidation::ActionUnavailable)
        );
        assert_eq!(
            panel.activation(&row),
            Err(ActionPanelInvalidation::ActionUnavailable)
        );
    }

    #[test]
    fn same_item_from_another_owner_cannot_take_over_panel() {
        let row = row();
        let mut replacement = row.clone();
        replacement.target = ActionTarget::Result {
            owner: PluginId("owner-b".into()),
            item: ItemId("item".into()),
        };
        let mut panel = ActionPanel::new(nested(&row));
        assert_eq!(
            panel.activation(&replacement),
            Err(ActionPanelInvalidation::TargetUnavailable)
        );
        let mut panel = ActionPanel::new(nested(&row));
        assert_eq!(
            panel.reconcile(Some(nested(&replacement))),
            Err(ActionPanelInvalidation::TargetUnavailable)
        );
        let mut panel = ActionPanel::new(nested(&row));
        assert_eq!(
            panel.reconcile(None),
            Err(ActionPanelInvalidation::TargetUnavailable)
        );
    }

    #[test]
    fn dispatch_revalidates_action_removal_without_waiting_for_reconciliation() {
        let mut row = row();
        let mut panel = ActionPanel::new(nested(&row));
        panel.set_query("copy".into()).unwrap();
        row.alternate_actions.remove(0);
        assert_eq!(
            panel.activation(&row),
            Err(ActionPanelInvalidation::ActionUnavailable)
        );
    }

    #[test]
    fn changed_search_membership_does_not_select_another_action_on_refresh() {
        let mut row = row();
        let mut panel = ActionPanel::new(ActionTree::from_flat(&row, vec![]).unwrap());
        panel.set_query("o".into()).unwrap();
        panel.select(&id("copy"));
        row.alternate_actions[0].label = "Duplicate".into();
        panel
            .reconcile(Some(ActionTree::from_flat(&row, vec![]).unwrap()))
            .unwrap();
        assert_eq!(panel.query(), "o");
        assert_eq!(panel.selected(), None);
        assert_eq!(panel.activation(&row), Ok(None));
    }

    #[test]
    fn moved_action_preserves_browse_focus_and_reanchors_parent_path() {
        let row = row();
        let mut panel = ActionPanel::new(nested(&row));
        panel.select(&ActionIdentity::Submenu("tools".into()));
        panel.activation(&row).unwrap();
        let tree = ActionTree::new(
            &row,
            section(vec![
                leaf("open", "Open"),
                menu("new", "New", vec![leaf("copy", "Copy")]),
            ]),
        )
        .unwrap();
        panel.reconcile(Some(tree)).unwrap();
        assert_eq!(panel.selected(), Some(&id("copy")));
        assert_eq!(panel.path(), &[ActionIdentity::Submenu("new".into())]);
    }

    #[test]
    fn query_refusal_is_atomic_and_no_matches_have_no_execution() {
        let row = row();
        let mut panel = ActionPanel::new(nested(&row));
        panel.set_query("copy".into()).unwrap();
        assert_eq!(
            panel.set_query("x".repeat(MAX_ACTION_QUERY_BYTES + 1)),
            Err(ActionPanelRefusal::QueryTooLong)
        );
        assert_eq!(panel.query(), "copy");
        assert_eq!(panel.selected(), Some(&id("copy")));
        panel.set_query("no such action".into()).unwrap();
        assert_eq!(panel.visible_entries().len(), 0);
        assert_eq!(panel.selected(), None);
        assert_eq!(panel.activation(&row), Ok(None));
        assert!(!panel.move_selection(isize::MIN));
    }

    #[test]
    fn navigation_is_clamped_and_paint_borrows_prepared_metadata() {
        let row = row();
        let mut panel = ActionPanel::new(nested(&row));
        assert!(!panel.move_selection(isize::MIN));
        assert!(panel.move_selection(isize::MAX));
        assert_eq!(
            panel.selected(),
            Some(&ActionIdentity::Host(HostAction::OpenSettings))
        );
        assert!(!panel.move_selection(1));
        let clone = panel.clone();
        assert!(Arc::ptr_eq(panel.tree(), clone.tree()));
        let first = panel.visible_entries().next().unwrap();
        let second = panel.visible_entries().next().unwrap();
        assert!(std::ptr::eq(first.presentation, second.presentation));
    }

    #[test]
    fn explicit_metadata_survives_preparation() {
        let row = row();
        let mut entry = leaf("remove", "Remove");
        if let ActionEntry::Plugin { presentation, .. } = &mut entry {
            presentation.destructive = true;
            presentation.shortcut = Some("Ctrl+Shift+D".into());
            presentation.icon_reference = Some("trash".into());
        }
        let tree = ActionTree::new(&row, section(vec![entry])).unwrap();
        let presentation = tree.presentation(&id("remove")).unwrap();
        assert!(presentation.destructive);
        assert_eq!(presentation.shortcut.as_deref(), Some("Ctrl+Shift+D"));
        assert_eq!(presentation.icon_reference.as_deref(), Some("trash"));
    }

    #[test]
    fn duplicates_unknown_actions_and_empty_menus_are_refused() {
        let row = row();
        assert!(matches!(
            ActionTree::new(&row, section(vec![leaf("copy", "A"), leaf("copy", "B")])),
            Err(ActionPanelRefusal::DuplicateIdentity)
        ));
        assert!(matches!(
            ActionTree::new(&row, section(vec![leaf("missing", "Missing")])),
            Err(ActionPanelRefusal::UnknownPluginAction(_))
        ));
        assert!(matches!(
            ActionTree::new(&row, section(vec![menu("empty", "Empty", vec![])])),
            Err(ActionPanelRefusal::EmptySubmenu)
        ));
        let mut duplicate = row.clone();
        duplicate
            .alternate_actions
            .push(action("open", "Different label"));
        assert!(matches!(
            ActionTree::from_flat(&duplicate, vec![]),
            Err(ActionPanelRefusal::AmbiguousPluginAction(_))
        ));
        let mut sections = section(vec![]);
        sections.extend(section(vec![]));
        assert!(matches!(
            ActionTree::new(&row, sections),
            Err(ActionPanelRefusal::DuplicateSection)
        ));
    }

    #[test]
    fn depth_four_is_accepted_and_five_is_refused() {
        let row = row();
        let mut entry = leaf("copy", "Copy");
        for depth in 0..3 {
            entry = menu(&format!("m{depth}"), "Menu", vec![entry]);
        }
        assert!(ActionTree::new(&row, section(vec![entry.clone()])).is_ok());
        entry = menu("too-deep", "Menu", vec![entry]);
        assert!(matches!(
            ActionTree::new(&row, section(vec![entry])),
            Err(ActionPanelRefusal::TooDeep)
        ));
    }

    #[test]
    fn entry_count_is_not_silently_truncated() {
        let mut row = row();
        row.default_action = None;
        row.alternate_actions = (0..MAX_ACTION_ENTRIES)
            .map(|i| action(&i.to_string(), "Action"))
            .collect();
        assert_eq!(
            ActionTree::from_flat(&row, vec![]).unwrap().len(),
            MAX_ACTION_ENTRIES
        );
        assert!(matches!(
            ActionTree::from_flat(&row, vec![ActionTree::open_settings_entry()]),
            Err(ActionPanelRefusal::TooManyEntries)
        ));
        row.alternate_actions.push(action("overflow", "Action"));
        assert!(matches!(
            ActionTree::from_flat(&row, vec![]),
            Err(ActionPanelRefusal::TooManyEntries)
        ));
    }

    #[test]
    fn utf8_and_aggregate_text_limits_refuse_explicitly() {
        let mut row = row();
        row.default_action.as_mut().unwrap().label = "é".repeat(MAX_ACTION_TEXT_BYTES / 2);
        assert!(ActionTree::from_flat(&row, vec![]).is_ok());
        row.default_action.as_mut().unwrap().label.push('é');
        assert!(matches!(
            ActionTree::from_flat(&row, vec![]),
            Err(ActionPanelRefusal::TextTooLong("label"))
        ));
        row.default_action = None;
        row.alternate_actions = (0..32)
            .map(|i| {
                let mut action = action(&i.to_string(), "Action");
                action.description = "x".repeat(MAX_ACTION_TEXT_BYTES);
                action
            })
            .collect();
        assert!(matches!(
            ActionTree::from_flat(&row, vec![]),
            Err(ActionPanelRefusal::TotalTextTooLong)
        ));
    }

    #[test]
    fn primary_uses_the_same_execution_validation_as_the_panel() {
        let row = row();
        let tree = nested(&row);
        let primary = tree.primary().unwrap();
        let mut panel = ActionPanel::new(tree.clone());
        assert_eq!(
            panel.activation(&row),
            Ok(tree
                .execution(primary, &row)
                .unwrap()
                .map(ActionActivation::Execute))
        );
        let mut stale = row.clone();
        stale.default_action = None;
        assert_eq!(
            tree.execution(primary, &stale),
            Err(ActionPanelInvalidation::ActionUnavailable)
        );
        stale = row.clone();
        stale.target = ActionTarget::Result {
            owner: PluginId("owner-a".into()),
            item: ItemId("another-item".into()),
        };
        assert_eq!(
            tree.execution(primary, &stale),
            Err(ActionPanelInvalidation::TargetUnavailable)
        );
    }
    #[test]
    fn empty_panel_refuses_selection_and_repeated_updates_do_not_create_focus() {
        let row = super::tests::row();
        let tree = ActionTree::new(&row, Vec::new()).unwrap();
        let mut panel = ActionPanel::new(tree.clone());
        assert!(panel.tree().is_empty());
        assert_eq!(panel.selected(), None);
        assert!(!panel.select(&ActionIdentity::Plugin(ActionId("open".into()))));
        assert!(!panel.move_selection(1));
        assert_eq!(panel.activation(&row), Ok(None));
        panel.reconcile(Some(tree)).unwrap();
        assert_eq!(panel.selected(), None);
    }

    #[test]
    fn repeated_refresh_cannot_restore_lost_search_focus_without_user_navigation() {
        let mut row = row();
        let mut panel = ActionPanel::new(ActionTree::from_flat(&row, vec![]).unwrap());
        panel.set_query("o".into()).unwrap();
        panel.select(&id("copy"));
        row.alternate_actions[0].label = "Duplicate".into();
        let replacement = ActionTree::from_flat(&row, vec![]).unwrap();
        panel.reconcile(Some(replacement.clone())).unwrap();
        panel.reconcile(Some(replacement)).unwrap();
        assert_eq!(panel.selected(), None);
        assert_eq!(panel.activation(&row), Ok(None));
        assert!(!panel.move_selection(0));
        assert!(panel.move_selection(1));
        assert_eq!(panel.selected(), Some(&id("open")));
    }
}
