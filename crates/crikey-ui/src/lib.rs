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
use crikey_platform::IconImage;

mod native;
mod session_end;
/// The one Windows message the launcher's window procedure must rewrite before
/// `DefWindowProc` can turn a keystroke into an alert sound.
///
/// Public because the mapping is a pure function of a message id and is
/// therefore compiled and tested on every host, not only the one that can run
/// it.
pub mod system_char;
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
    /// What the producing plugin or backend said about this item's icon, which
    /// only the platform that wrote it can interpret (spec 10.1).
    pub icon_reference: Option<String>,
    /// The pixels that reference resolved to, where a platform resolved it.
    ///
    /// Decoded before the row reaches the renderer, never by it: resolution
    /// stats theme directories and decodes an image, and the UI thread may do
    /// neither. `None` covers every ordinary reason a row has no icon -- the
    /// item named none, the platform knows of none, the file would not decode --
    /// and the row is drawn identically for all of them.
    ///
    /// Shared rather than owned because one icon answers many rows: an
    /// application appears in several result sets over a session, and every
    /// generation would otherwise carry its own copy of the same 9 KiB.
    pub icon: Option<Arc<IconImage>>,
    pub category: String,
    pub plugin_name: String,
    /// Byte ranges within `label` to highlight.
    pub highlights: Vec<(usize, usize)>,
    pub argument_hint: Option<String>,
    pub status: Option<String>,
    pub default_action: Option<Action>,
    pub alternate_actions: Vec<Action>,
}

/// One configurable value the settings surface shows (spec 6.3).
///
/// Entirely host-supplied: the UI knows nothing about the configuration
/// schema, its layers or its validation, so a row carries its own label and
/// the name of the layer the value came from and the renderer only draws
/// what it was handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingRow {
    /// The configuration key an edit is reported against, such as
    /// `launcher.activation-hotkey`.
    pub key: String,
    pub label: String,
    pub value: String,
    /// Which configuration layer supplied `value`, shown so the user can tell
    /// a default apart from something they set themselves.
    pub source: String,
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
    /// True while the settings surface is showing (spec 6.3).
    pub settings_open: bool,
    /// What the settings surface lists, as the host last described it.
    ///
    /// Shared for the same reason the rows are: a frame that changes nothing
    /// about the settings costs a refcount bump rather than a copy of every
    /// key, label and value.
    pub settings: Arc<[SettingRow]>,
    /// The setting whose editor should take the keyboard when the surface
    /// opens, by key. `None` leaves the focus where the user put it.
    pub settings_focus: Option<String>,
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
    /// Shows the settings surface (spec 6.3).
    OpenSettings,
    /// Hides the settings surface again.
    CloseSettings,
    /// Asks the host to persist one setting; the UI neither validates nor
    /// stores it.
    SetSetting {
        key: String,
        value: String,
    },
    /// Asks the launcher to exit for good, rather than to hide until the next
    /// hotkey press.
    Quit,
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
/// A fixed step, not a measurement: the state machine has no window and the
/// rendered list holds a variable number of rows depending on window size and
/// how much text each row carries, so a page here is a constant the renderer
/// scrolls to follow rather than the other way round.
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
    /// Persist `value` under `key` (spec 6.3). Opening and closing the
    /// settings surface is the UI's own business, but the value behind a row
    /// belongs to the host's configuration store.
    SetSetting { key: String, value: String },
    /// The user asked the launcher to exit for good, not to hide until the
    /// next hotkey press.
    Quit,
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
    /// Whether the user has moved the selection themselves within the current
    /// generation.
    ///
    /// Row zero is where every generation starts, and until an arrow key is
    /// pressed it is a *default* rather than a choice. The distinction decides
    /// what a republish does: results arrive in waves -- the catalog answers
    /// first and a provider's ranked rows can land afterwards and outrank it --
    /// so following the previously selected item would drag the highlight down
    /// the list as better rows appear above it, which is the launcher moving
    /// the selection the user never touched. Once they have chosen a row, the
    /// opposite is true and the row must be followed wherever it moves.
    navigated: bool,
    /// Whether the selected row's action list is open (spec 6.3).
    actions_open: bool,
    pending_plugins: bool,
    /// Whether the settings surface is showing (spec 6.3).
    ///
    /// Not session state: the surface is opened over whatever the launcher is
    /// already showing and closes without disturbing the query or the rows.
    settings_open: bool,
    /// What the settings surface lists, as the host last described it. The
    /// host owns the configuration; the model only carries its description to
    /// the renderer, which is why this survives `dismiss`.
    settings: Arc<[SettingRow]>,
    /// The setting an opening surface should put the keyboard in, by key.
    settings_focus: Option<String>,
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
            navigated: false,
            actions_open: false,
            pending_plugins: false,
            settings_open: false,
            settings: Arc::default(),
            settings_focus: None,
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
    ///
    /// The settings *rows* survive, because they describe the host's
    /// configuration rather than this session, but the surface itself closes:
    /// the next activation is a fresh launcher, not the panel the user left
    /// open.
    pub fn dismiss(&mut self) {
        if !self.visible {
            return;
        }

        self.visible = false;
        self.dirty = false;
        self.query.clear();
        self.rows = Arc::default();
        self.selected = 0;
        self.navigated = false;
        self.actions_open = false;
        self.pending_plugins = false;
        self.settings_open = false;
        self.settings_focus = None;
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
        // A new query is a new default. Whatever the user had chosen belonged
        // to the answer they were choosing from.
        self.navigated = false;
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
    ///
    /// A publish that lands while the query is empty carries no rows at all,
    /// whatever the host ranked for it: an untyped launcher is the query field
    /// and nothing else, and dropping the rows here is what makes that true
    /// for every renderer instead of only for the one that remembers to check.
    pub fn publish(&mut self, generation: Generation, rows: Vec<ResultRow>, pending_plugins: bool) {
        if !self.visible || self.active != Some(generation) {
            return;
        }

        let rows: Vec<ResultRow> = if self.query.is_empty() { Vec::new() } else { rows };
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
            settings_open: self.settings_open,
            // Shared for the same reason the rows are.
            settings: Arc::clone(&self.settings),
            settings_focus: self.settings_focus.clone(),
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
            // Opening and closing the settings surface is pure UI state, so
            // the host has nothing to do about either; only the value behind a
            // row is the host's to keep.
            UiCommand::OpenSettings => {
                self.open_settings(None);
                None
            }
            UiCommand::CloseSettings => {
                self.close_settings();
                None
            }
            UiCommand::SetSetting { key, value } => Some(UiEffect::SetSetting { key, value }),
            UiCommand::Quit => Some(UiEffect::Quit),
            // Cancel backs out one rung at a time: it closes the settings
            // surface first, then an open action list, then clears a non-empty
            // query, and closes only an already-bare launcher. Dismiss skips
            // the ladder entirely.
            UiCommand::Cancel if self.settings_open => {
                self.close_settings();
                None
            }
            UiCommand::Cancel if self.actions_open => {
                self.close_actions();
                None
            }
            // Clearing the query is an ordinary edit, so it goes through the
            // same path a backspace to nothing would take.
            UiCommand::Cancel if !self.query.is_empty() => self.retype(String::new()),
            UiCommand::Cancel | UiCommand::Dismiss => {
                self.dismiss();
                Some(UiEffect::Dismissed)
            }
        }
    }

    /// Applies a query edit. Retyping the identical text is not a new query
    /// state, so it changes nothing and schedules nothing.
    ///
    /// An edit back to the empty query drops the rows rather than leaving the
    /// previous ones standing: an empty query shows nothing but the text
    /// field, and a row the user cannot see must not still be the one Enter
    /// runs.
    fn retype(&mut self, text: String) -> Option<UiEffect> {
        if self.query == text {
            return None;
        }

        // Reuse the buffer rather than swapping it in: a keystroke on the hot
        // path must not allocate, and `text` is already owned by the effect.
        self.query.clear();
        self.query.push_str(&text);
        if self.query.is_empty() {
            self.rows = Arc::default();
            self.selected = 0;
            self.navigated = false;
            self.actions_open = false;
        }
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

    /// Tries to attach an actionable failure message to the selected row, and
    /// reports whether the row is now carrying it.
    ///
    /// There is nothing to attach a message to when the result list is empty,
    /// which happens when a republish empties the list while the action the
    /// message describes is still running. `false` means exactly that: the
    /// message was dropped and the caller still owns the job of telling the
    /// user. `true` means the selected row carries the message, including the
    /// case where it already did and nothing had to change.
    ///
    /// This copies the current row slice only on an action failure, never on
    /// query or navigation hot paths. The next frame renders the message
    /// through the row's existing `status` field.
    #[must_use = "a dropped status message is invisible to the user unless the caller reports it"]
    pub fn set_selected_status(&mut self, status: String) -> bool {
        let Some(selected) = self.rows.get(self.selected) else {
            return false;
        };
        if selected.status.as_deref() == Some(status.as_str()) {
            return true;
        }

        let mut rows = self.rows.to_vec();
        rows[self.selected].status = Some(status);
        self.rows = rows.into();
        self.dirty = true;
        true
    }

    /// Replaces what the settings surface lists.
    ///
    /// Accepted while hidden, unlike everything else here, because these rows
    /// describe the host's configuration rather than a launcher session: the
    /// host publishes them at startup and again after each write, and the
    /// launcher must already know them the first time it is shown.
    pub fn set_settings(&mut self, settings: Vec<SettingRow>) {
        if *self.settings == *settings {
            return;
        }

        self.settings = settings.into();
        // A hidden launcher has no frame to dirty; the next activation carries
        // the new rows anyway.
        self.dirty |= self.visible;
    }

    /// The settings the surface would list right now.
    pub fn settings(&self) -> &[SettingRow] {
        &self.settings
    }

    pub fn is_settings_open(&self) -> bool {
        self.settings_open
    }

    /// Shows the settings surface, optionally putting the keyboard in the
    /// editor for `focus_key` (spec 6.3).
    ///
    /// The host opens it directly when something is misconfigured — an
    /// activation hotkey that would not register, say — so the user lands on
    /// the row that needs their attention instead of being told to go looking
    /// for it. Reopening an open surface still re-aims the focus, because the
    /// second reason to open it need not be the first one.
    pub fn open_settings(&mut self, focus_key: Option<&str>) {
        let focus = focus_key.map(str::to_owned);
        if self.settings_open && self.settings_focus == focus {
            return;
        }

        self.settings_open = true;
        self.settings_focus = focus;
        self.dirty |= self.visible;
    }

    /// Hides the settings surface, leaving the query and the rows alone. An
    /// already closed surface is not a change, so this produces no frame.
    pub fn close_settings(&mut self) {
        if !self.settings_open {
            return;
        }

        self.settings_open = false;
        self.settings_focus = None;
        self.dirty |= self.visible;
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
        // The only path a person's arrow or page key reaches, so this is where
        // a default becomes a choice.
        self.navigated = true;
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

        // Untouched selections stay at the top. A later wave of results can
        // outrank what the first wave put in row zero, and following the item
        // the user never chose would walk the highlight down the list under
        // them.
        if !self.navigated {
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
