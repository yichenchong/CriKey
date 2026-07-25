//! Launcher window and presentation contracts (spec 6, 25.5).
//!
//! The UI owns no plugin state: it renders a view model produced for one query
//! generation and never blocks on plugin work.

use crikey_core::{Action, Generation, ItemId};

/// One row in the result list (spec 6.4).
#[derive(Debug, Clone)]
pub struct ResultRow {
    pub item: ItemId,
    pub label: String,
    pub description: String,
    pub icon_reference: Option<String>,
    pub category: String,
    pub plugin_name: String,
    /// Byte ranges within `label` to highlight.
    pub highlights: Vec<(usize, usize)>,
    pub argument_hint: Option<String>,
    pub status: Option<String>,
    pub default_action: Option<Action>,
    pub alternate_actions: Vec<Action>,
}

/// Everything the renderer needs for one frame.
#[derive(Debug, Clone)]
pub struct ViewModel {
    pub generation: Generation,
    pub query: String,
    pub rows: Vec<ResultRow>,
    pub selected: usize,
    /// True while at least one plugin is still working on this generation.
    pub pending_plugins: bool,
}

/// Keyboard-only interaction surface (spec 6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    SetQuery(String),
    SelectNext,
    SelectPrevious,
    PageDown,
    PageUp,
    Complete,
    ShowActions,
    ExecuteDefault,
    ExecuteAlternate(usize),
    Cancel,
    Dismiss,
}

pub trait LauncherWindow {
    fn show(&mut self);
    fn hide(&mut self);
    fn is_visible(&self) -> bool;
    /// Presents a frame. Must never block on plugin traffic.
    fn present(&mut self, model: &ViewModel);
}
