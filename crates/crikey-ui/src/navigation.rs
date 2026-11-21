//! Bounded memory-only logical routes. No plugin frame, engine identity or secret is retained.
use crate::ResultRow;
use crikey_core::PluginId;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub const MAX_NAVIGATION_ENTRIES: usize = 16;
pub const MAX_NAVIGATION_BYTES: usize = 8 * 1024 * 1024;

pub trait NavigationClock: Send + Sync + std::fmt::Debug {
    fn now(&self) -> Duration;
}
#[derive(Debug)]
pub struct MonotonicNavigationClock(Instant);
impl Default for MonotonicNavigationClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl NavigationClock for MonotonicNavigationClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationRoute {
    Root,
    Settings {
        focus: Option<String>,
    },
    CustomPage {
        plugin: PluginId,
        page_id: String,
        title: String,
    },
}
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogicalScroll {
    pub row: usize,
    pub offset: f32,
}
/// Reusable host-view restoration payload. Secret drafts stay outside this
/// payload; a scope is a logical identifier, never arbitrary editor contents.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NavigationRestoration {
    pub inline_arguments: Option<crate::InlineArguments>,
    pub query: String,
    pub selected: Option<crate::ActionTarget>,
    pub fallback_index: usize,
    pub navigated: bool,
    pub scope: Option<crate::ScopeIdentity>,
    pub scroll: LogicalScroll,
}
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NavigationPresentation {
    pub inline_arguments: Option<crate::InlineArguments>,
    pub scopes: Arc<[crate::ScopeChoice]>,
    pub selected_scope: Option<crate::ScopeIdentity>,
    pub section_headers: Arc<[crate::SectionHeader]>,
    pub actionless_targets: Arc<[crikey_core::structured_arguments::OwnedItem]>,
    pub title: Arc<str>,
    pub can_go_back: bool,
    pub scroll: LogicalScroll,
    pub restoration: u64,
    /// Publication must not pull an offscreen selection back over a restored anchor.
    pub preserve_scroll: bool,
    pub unavailable: Option<String>,
    /// Transient picker snapshot from the application's sole history store.
    pub recall: Arc<[String]>,
    pub recall_selected: usize,
}
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub route: NavigationRoute,
    pub rows: Arc<[ResultRow]>,
    pub state: NavigationRestoration,
}

// Count immutable result and icon allocations once even if several entries share them.
pub(crate) fn retained_bytes<'a>(entries: impl Iterator<Item = &'a Entry>) -> usize {
    let mut allocations = std::collections::HashSet::new();
    let mut typed = crikey_core::content_accounting::ContentFootprint::new(MAX_NAVIGATION_BYTES, 0);
    let mut renderer_bytes = 0usize;
    // The fixed entry pool includes the current logical entry/suspended slot.
    let mut bytes = MAX_NAVIGATION_ENTRIES * std::mem::size_of::<Entry>();
    for entry in entries {
        renderer_bytes = 0;
        if let Some(input) = &entry.state.inline_arguments {
            if typed.include_schema(&input.schema).is_err() {
                return usize::MAX;
            }
            // Logical snapshots share the explicit nonsecret draft allocation.
            let map_bytes = input
                .draft
                .values
                .iter()
                .map(|(id, value)| {
                    id.0.capacity()
                        + std::mem::size_of_val(value)
                        + 6 * std::mem::size_of::<usize>()
                        + match value {
                            crikey_core::structured_arguments::NonSecretValue::Text(text) => text.capacity(),
                            crikey_core::structured_arguments::NonSecretValue::Dropdown(option) => {
                                option.0.capacity()
                            }
                        }
                })
                .sum::<usize>();
            let key_bytes = input.executable.target.owner.0.capacity()
                + input.executable.target.item.0.capacity()
                + input
                    .executable
                    .action
                    .as_ref()
                    .map_or(0, |action| action.0.capacity());
            let draft_bytes = map_bytes
                + key_bytes
                + std::mem::size_of_val(input.draft.as_ref())
                + 2 * std::mem::size_of::<usize>();
            if typed.charge_shared(&input.draft, draft_bytes, 0).is_err() {
                return usize::MAX;
            }
            bytes = bytes.saturating_add(key_bytes);
            if let Some((reference, _)) = &input.command {
                bytes += reference.provider_id.0.capacity() + reference.command_id.capacity();
            }
            // Reserve the bounded live editor separately without recording
            // password lengths. Only the current entry has a renderer editor.
            renderer_bytes = input
                .schema
                .fields
                .iter()
                .map(|field| {
                    field.id.0.capacity()
                        + 6 * std::mem::size_of::<usize>()
                        + match field.kind {
                            crikey_core::structured_arguments::FieldKind::Text { .. } => {
                                crikey_core::structured_arguments::MAX_TEXT_BYTES
                            }
                            crikey_core::structured_arguments::FieldKind::Password { .. } => {
                                crikey_core::structured_arguments::MAX_TEXT_BYTES
                            }
                            crikey_core::structured_arguments::FieldKind::Dropdown { .. } => {
                                crikey_core::structured_arguments::MAX_ID_BYTES
                            }
                        }
                })
                .sum();
        }
        bytes = bytes.saturating_add(entry.state.query.capacity());
        bytes += entry.state.scope.as_ref().map_or(0, |scope| {
            scope.owner.0.capacity() + scope.id.0.capacity() + std::mem::size_of_val(scope)
        });
        if let Some(selected) = &entry.state.selected {
            bytes += selected.retained_bytes();
        }
        bytes += match &entry.route {
            NavigationRoute::Root => 0,
            NavigationRoute::Settings { focus } => focus.as_ref().map_or(0, String::capacity),
            NavigationRoute::CustomPage {
                plugin,
                page_id,
                title,
            } => plugin.0.capacity() + page_id.capacity() + title.capacity(),
        };
        if !allocations.insert(Arc::as_ptr(&entry.rows).cast::<()>() as usize) {
            continue;
        }
        bytes += std::mem::size_of_val(entry.rows.as_ref()) + 2 * std::mem::size_of::<usize>();
        for row in entry.rows.iter() {
            bytes += row.target.retained_bytes()
                + row.label.capacity()
                + row.description.capacity()
                + row.category.capacity()
                + row.plugin_name.capacity();
            if let Some(source) = &row.source {
                bytes += source.owner.0.capacity() + source.item.0.capacity();
            }
            if let Some(state) = &row.command_state {
                bytes += state.unavailable_reason.as_ref().map_or(0, String::capacity)
                    + state.argument.as_ref().map_or(0, String::capacity)
                    + state.hotkey.as_ref().map_or(0, String::capacity)
                    + state.aliases.capacity() * std::mem::size_of::<String>();
                bytes += state.aliases.iter().map(String::capacity).sum::<usize>();
            }
            bytes += row.icon_reference.as_ref().map_or(0, String::capacity)
                + row.argument_hint.as_ref().map_or(0, String::capacity)
                + row.status.as_ref().map_or(0, String::capacity);
            bytes += row.highlights.capacity() * std::mem::size_of::<(usize, usize)>();
            bytes += row.alternate_actions.capacity() * std::mem::size_of::<crikey_core::Action>();
            for action in row.default_action.iter().chain(row.alternate_actions.iter()) {
                bytes += action.action_id.0.capacity()
                    + action.label.capacity()
                    + action.description.capacity()
                    + action.icon_reference.as_ref().map_or(0, String::capacity);
                bytes +=
                    action.applicable_categories.capacity() * std::mem::size_of::<crikey_core::Category>();
                for category in &action.applicable_categories {
                    if let crikey_core::Category::PluginDefined(name) = category {
                        bytes += name.capacity();
                    }
                }
            }
            if let Some(icon) = &row.icon {
                if allocations.insert(Arc::as_ptr(icon) as usize) {
                    bytes += icon.retained_bytes() + 2 * std::mem::size_of::<usize>();
                }
            }
        }
    }
    bytes
        .saturating_add(typed.resources().bytes)
        .saturating_add(renderer_bytes)
}
