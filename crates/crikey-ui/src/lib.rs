//! Launcher window, view model and keyboard command handling (spec 6, 25.5).
//!
//! The UI owns no plugin state: it renders a view model produced for one query
//! generation and never blocks on plugin work.
//!
//! [`LauncherViewModel`] is the renderer-independent state machine. The native
//! [`NativeLauncher`] implements ADR-0002 with a retained `winit` window,
//! `wgpu` surface, and egui widget frame while preserving [`LauncherWindow`] as
//! the presentation seam. [`build_launcher_frame`] stays independently
//! callable for deterministic headless rendering checks.

use std::sync::Arc;

use crikey_core::{Action, ActionId, Generation, ItemId};

mod native;
mod theme;

pub use egui;
pub use native::{
    build_launcher_frame, create_launcher_context, ActivationLatencySnapshot, ActivationLatencyTracker,
    NativeLauncher, NativeLauncherConfig, NativeLauncherEvent, NativeLauncherHandle, NativeUiFrame,
    RendererError, ACTIVATION_SAMPLE_CAPACITY,
};
/// Re-exported so a caller can name [`NativeLauncherConfig::present_mode`]
/// without taking its own `wgpu` dependency and risking a version skew with
/// the one the renderer is built against.
pub use wgpu;

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
///
/// The row set is *shared* with the view model, never copied into the frame: a
/// keystroke that leaves the results standing costs one refcount bump instead
/// of a deep copy of every row and every string in it (spec 25.5). Frames of
/// one result set therefore all point at the single allocation
/// [`LauncherViewModel::publish`] built.
#[derive(Debug, Clone)]
pub struct ViewModel {
    pub generation: Generation,
    pub query: String,
    pub rows: Arc<[ResultRow]>,
    pub selected: usize,
    /// True while at least one plugin is still working on this generation.
    pub pending_plugins: bool,
    /// True while the action list of the selected row is open (spec 6.3).
    ///
    /// The overlay is view-model state rather than renderer state, so which
    /// rung `Cancel` backs out of is decided in one place and every renderer
    /// draws the same launcher.
    pub actions_open: bool,
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
    /// Opens the action list of the selected row (spec 6.3).
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

/// Rows a page key moves the selection by (spec 6.3 "page navigation").
///
/// One page is the launcher's visible list height, so `PageDown` lands on the
/// row a page of scrolling would have revealed rather than on an arbitrary
/// offset.
pub const PAGE_SIZE: usize = 8;

/// Work the host must do about a command the view model has already applied to
/// its own state (spec 6.2.10, 6.3).
///
/// The view model schedules no query, runs no action and drives no window: it
/// reports the intent and leaves the work to the composition root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEffect {
    /// The query text changed; schedule a generation for it (spec 6.2.2).
    Query(String),
    /// Run `action` on `item` (spec 6.2.10). The launcher stays open —
    /// dismissing after an execution is the host's decision, not the UI's.
    Execute { item: ItemId, action: ActionId },
    /// The launcher closed itself and is warm again for the next hotkey.
    Dismissed,
}

/// The renderer-free launcher state machine (spec 6.1 - 6.5; ADR-0002).
///
/// Constructed once at startup and kept alive for the process lifetime:
/// [`activate`](Self::activate) shows an object that already exists, so the
/// hotkey path never pays construction cost, and [`dismiss`](Self::dismiss)
/// empties the launcher without dropping it.
///
/// While hidden the model is completely inert — every command, generation
/// change and publish is discarded and [`frame`](Self::frame) yields nothing —
/// so late plugin traffic can never resurrect a closed launcher. Dismissal is
/// also a session boundary: it retires the accept target, so results still in
/// flight when the launcher closed are dropped rather than landing in the next
/// session, while the generation floor it leaves behind keeps those retired
/// generations rejected forever (spec 6.5).
#[derive(Debug)]
pub struct LauncherViewModel {
    visible: bool,
    /// Set by every accepted mutation and cleared by `frame`, so mutations
    /// between two frames coalesce into one view model (spec 25.5).
    dirty: bool,
    query: String,
    /// The published rows, shared with every frame handed out since. Replaced
    /// wholesale by `publish`; never mutated in place, because a frame the
    /// renderer is still holding must keep describing what it was given.
    rows: Arc<[ResultRow]>,
    selected: usize,
    /// Whether the selected row's action list is open (spec 6.3).
    actions_open: bool,
    pending_plugins: bool,
    /// Highest generation ever begun, across every session of this launcher.
    /// Monotonic and deliberately kept by `dismiss`, so a generation retired
    /// before the launcher closed can never be begun again after it reopens
    /// (spec 6.5).
    floor: Option<Generation>,
    /// Generation the live query text belongs to, and the only one `publish`
    /// accepts. `None` before the first `begin_generation` and again after
    /// every `dismiss`, which is why neither a pre-first-generation publish nor
    /// a pre-dismiss one can reach the row set.
    active: Option<Generation>,
    /// Generation `rows` were published for. A publish whose generation differs
    /// from this one is the first of its generation and starts at the top.
    published: Option<Generation>,
}

impl Default for LauncherViewModel {
    fn default() -> Self {
        Self::new()
    }
}

impl LauncherViewModel {
    /// Builds the warm launcher: fully constructed, hidden, presenting nothing.
    ///
    /// Whatever this costs is paid once at startup; activation itself is a
    /// visibility flip that allocates nothing (ADR-0002).
    pub fn new() -> Self {
        Self {
            visible: false,
            dirty: false,
            query: String::new(),
            rows: Arc::default(),
            selected: 0,
            actions_open: false,
            pending_plugins: false,
            floor: None,
            active: None,
            published: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Shows the launcher. Idempotent: activating an open one keeps its query,
    /// rows and selection untouched and produces no frame.
    pub fn activate(&mut self) {
        if self.visible {
            return;
        }

        self.visible = true;
        self.dirty = true;
    }

    /// Hides the launcher and clears the query, the rows, the selection, the
    /// action list and the pending flag. The object survives for the next
    /// hotkey press and the query buffer keeps its capacity, so reactivation
    /// allocates nothing.
    ///
    /// Clearing the accept target is what makes dismissal a session boundary:
    /// plugin results still in flight for the generation that was live when the
    /// launcher closed are discarded instead of publishing rows into the
    /// reopened launcher. The generation *floor* survives untouched, so those
    /// same generations also stay rejected by
    /// [`begin_generation`](Self::begin_generation) (spec 6.5).
    pub fn dismiss(&mut self) {
        if !self.visible {
            return;
        }

        self.visible = false;
        self.dirty = false;
        self.query.clear();
        self.rows = Arc::default();
        self.selected = 0;
        self.actions_open = false;
        self.pending_plugins = false;
        self.active = None;
        self.published = None;
    }

    /// Makes `generation` the active one and marks results outstanding.
    ///
    /// `Generation::ZERO` is the core sentinel for "no query has begun", so it
    /// is never a live target. Only a generation strictly newer than every
    /// generation this launcher has ever begun is accepted, so results can
    /// never be reordered across generations and a generation retired in an
    /// earlier session can never come back (spec 6.5). The rows keep showing
    /// the previous generation's results until its first publish arrives:
    /// editing the query must never flicker the list empty.
    pub fn begin_generation(&mut self, generation: Generation) {
        if !self.visible
            || generation == Generation::ZERO
            || self.floor.is_some_and(|floor| generation <= floor)
        {
            return;
        }

        self.floor = Some(generation);
        self.active = Some(generation);
        self.pending_plugins = true;
        self.dirty = true;
    }

    /// Replaces the row set and the pending flag for `generation`.
    ///
    /// Accepted only when `generation` is exactly the accept target: the
    /// generation begun in this session and not yet superseded. Older,
    /// never-begun, pre-first-generation and pre-dismiss publishes are
    /// discarded whole and never partially applied (spec 6.2.7).
    pub fn publish(&mut self, generation: Generation, rows: Vec<ResultRow>, pending_plugins: bool) {
        if !self.visible || self.active != Some(generation) {
            return;
        }

        self.selected = self.resolve_selection(&rows, generation);
        // One move into a shared allocation per accepted publish; from here on
        // every frame of this result set is a refcount bump.
        self.rows = rows.into();
        // The overlay describes the alternates of a row snapshot that no longer
        // exists, so a republish closes it rather than offering stale actions.
        self.actions_open = false;
        self.pending_plugins = pending_plugins;
        self.published = Some(generation);
        self.dirty = true;
    }

    /// The newest state, once per accepted mutation.
    ///
    /// `None` while hidden and whenever nothing changed since the last call, so
    /// the list is drawn once per batch and never once per plugin item
    /// (spec 25.5).
    pub fn frame(&mut self) -> Option<ViewModel> {
        if !self.visible || !self.dirty {
            return None;
        }

        self.dirty = false;
        Some(ViewModel {
            // The active generation, not the published one: the frame carries
            // the generation the query text belongs to.
            generation: self.active.unwrap_or(Generation::ZERO),
            query: self.query.clone(),
            // A refcount bump, not a copy of the row set (spec 25.5).
            rows: Arc::clone(&self.rows),
            selected: self.selected,
            pending_plugins: self.pending_plugins,
            actions_open: self.actions_open,
        })
    }

    /// The sole command entry point (spec 6.3).
    ///
    /// Returns the work the host must do, or `None` when the command changed
    /// nothing. A hidden launcher ignores every command: only `activate` moves
    /// it.
    pub fn apply(&mut self, command: UiCommand) -> Option<UiEffect> {
        if !self.visible {
            return None;
        }

        match command {
            UiCommand::SetQuery(text) => self.retype(text),
            UiCommand::SelectNext => {
                self.select(self.selected.saturating_add(1));
                None
            }
            UiCommand::SelectPrevious => {
                self.select(self.selected.saturating_sub(1));
                None
            }
            UiCommand::PageDown => {
                self.select(self.selected.saturating_add(PAGE_SIZE));
                None
            }
            UiCommand::PageUp => {
                self.select(self.selected.saturating_sub(PAGE_SIZE));
                None
            }
            UiCommand::Complete => self.complete(),
            // Opening the action list schedules nothing and runs nothing: it is
            // pure UI state, so the host has no work and the change reaches the
            // renderer through the next frame.
            UiCommand::ShowActions => {
                self.open_actions();
                None
            }
            UiCommand::ExecuteDefault => {
                let row = self.rows.get(self.selected)?;
                let effect = execute(row, row.default_action.as_ref()?);
                self.close_actions();
                Some(effect)
            }
            UiCommand::ExecuteAlternate(index) => {
                let row = self.rows.get(self.selected)?;
                let effect = execute(row, row.alternate_actions.get(index)?);
                self.close_actions();
                Some(effect)
            }
            // Cancel backs out one rung at a time: it closes an open action
            // list first, then clears a non-empty query, and closes only an
            // already-bare launcher. Dismiss skips the ladder entirely.
            UiCommand::Cancel if self.actions_open => {
                self.close_actions();
                None
            }
            UiCommand::Cancel if !self.query.is_empty() => {
                self.query.clear();
                self.dirty = true;
                // Clearing the query is an edit, not a blanking: the host
                // replaces the rows, the UI keeps showing the old ones.
                Some(UiEffect::Query(String::new()))
            }
            UiCommand::Cancel | UiCommand::Dismiss => {
                self.dismiss();
                Some(UiEffect::Dismissed)
            }
        }
    }

    /// Applies a query edit. Retyping the identical text is not a new query
    /// state, so it changes nothing and schedules nothing.
    fn retype(&mut self, text: String) -> Option<UiEffect> {
        if self.query == text {
            return None;
        }

        // Reuse the buffer rather than swapping it in: a keystroke on the hot
        // path must not allocate, and `text` is already owned by the effect.
        self.query.clear();
        self.query.push_str(&text);
        self.dirty = true;
        Some(UiEffect::Query(text))
    }

    /// Fills the query from the selected row (spec 6.3 "query completion").
    /// The completed text is an ordinary edit, so the host schedules a
    /// generation for it exactly as if the user had typed it.
    fn complete(&mut self) -> Option<UiEffect> {
        let row = self.rows.get(self.selected)?;
        if self.query == row.label {
            return None;
        }

        self.query.clear();
        self.query.push_str(&row.label);
        self.dirty = true;
        Some(UiEffect::Query(self.query.clone()))
    }

    /// Attaches an actionable failure message to the selected row.
    ///
    /// This copies the current row slice only on an action failure, never on
    /// query or navigation hot paths. The next frame renders the message
    /// through the row's existing `status` field.
    pub fn set_selected_status(&mut self, status: String) {
        let Some(selected) = self.rows.get(self.selected) else {
            return;
        };
        if selected.status.as_deref() == Some(status.as_str()) {
            return;
        }

        let mut rows = self.rows.to_vec();
        rows[self.selected].status = Some(status);
        self.rows = rows.into();
        self.dirty = true;
    }

    /// Opens the action list of the selected row (spec 6.3).
    ///
    /// Only a row that carries alternate actions has a list worth showing: the
    /// default action already answers to `ExecuteDefault`, so an overlay over
    /// nothing is not a state the launcher can be left stuck in. Reopening an
    /// open list changes nothing.
    fn open_actions(&mut self) {
        if self.actions_open {
            return;
        }

        let has_alternates = self
            .rows
            .get(self.selected)
            .is_some_and(|row| !row.alternate_actions.is_empty());
        if !has_alternates {
            return;
        }

        self.actions_open = true;
        self.dirty = true;
    }

    /// Closes the action list. An already closed list is not a change, so this
    /// produces no frame of its own.
    fn close_actions(&mut self) {
        if !self.actions_open {
            return;
        }

        self.actions_open = false;
        self.dirty = true;
    }

    /// Moves the selection to `target`, clamped into range. Navigation never
    /// wraps, and an empty list has nothing to move.
    fn select(&mut self, target: usize) {
        let Some(last) = self.rows.len().checked_sub(1) else {
            return;
        };

        let target = target.min(last);
        if target == self.selected {
            return;
        }

        self.selected = target;
        // The action list belongs to the row it was opened over, so moving off
        // that row closes it instead of silently retargeting it.
        self.actions_open = false;
        self.dirty = true;
    }

    /// Where the selection lands once `incoming` replaces the current rows.
    ///
    /// Within one generation the selection is anchored by [`ItemId`]: a
    /// republish follows the selected item wherever it moved to and falls back
    /// to the previous index clamped into range when the anchor is gone. The
    /// first publish of a new generation is a new query state and starts at the
    /// top, because the anchor lives inside one generation only.
    fn resolve_selection(&self, incoming: &[ResultRow], generation: Generation) -> usize {
        let Some(last) = incoming.len().checked_sub(1) else {
            return 0;
        };

        if self.published != Some(generation) {
            return 0;
        }

        let Some(anchor) = self.rows.get(self.selected).map(|row| &row.item) else {
            return 0;
        };

        incoming
            .iter()
            .position(|row| row.item == *anchor)
            .unwrap_or(self.selected.min(last))
    }
}

fn execute(row: &ResultRow, action: &Action) -> UiEffect {
    UiEffect::Execute {
        item: row.item.clone(),
        action: action.action_id.clone(),
    }
}
