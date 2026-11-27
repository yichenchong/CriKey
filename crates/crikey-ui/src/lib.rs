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

use crikey_core::{Action, ActionId, Generation, ItemId, PageFrame, PageInput, PluginId};
use crikey_platform::IconImage;

pub mod action_panel;
mod native;
pub mod navigation;
pub mod standard_view;
mod typed_input;
pub use typed_input::{next_inline_input_revision, InlineArguments, TypedInputSubmission};
mod root_presentation;
use action_panel::{ActionActivation, ActionExecution, ActionPanelNavigation, ActionTree, HostAction};
pub use action_panel::{ActionIdentity, ActionPanel, ActionTarget};
pub use navigation::{
    LogicalScroll, NavigationClock, NavigationPresentation, NavigationRestoration, NavigationRoute,
};
pub use root_presentation::{PublicationIdentity, ScopeChoice, ScopeIdentity, SectionHeader};
mod preview_content;
mod session_end;
pub use preview_content::{prepare_preview, PreparedPreview, PreviewBlock, PreviewBlockKind, PreviewSpan};
/// The one Windows message the launcher's window procedure must rewrite before
/// `DefWindowProc` can turn a keystroke into an alert sound.
///
/// Public because the mapping is a pure function of a message id and is
/// therefore compiled and tested on every host, not only the one that can run
/// it.
pub mod system_char;
mod theme;
/// Clipping the launcher window to its rounded shape, where the surface in
/// front of it cannot be composited.
///
/// Clipping the launcher window to its rounded shape on Windows, whose
/// swapchain cannot present a corner as nothing.
///
/// Public for the same reason [`system_char`] is: the clip is Windows-only,
/// but the pixel arithmetic behind it is a pure function of a scale factor and
/// is therefore compiled and tested on every host rather than only on the one
/// that can run it.
pub mod window_shape;

pub use egui;
pub use native::{
    build_launcher_frame, create_launcher_context, initial_page_viewport, page_palette,
    release_launcher_textures, ActivationLatencySnapshot, ActivationLatencyTracker, NativeLauncher,
    NativeLauncherConfig, NativeLauncherEvent, NativeLauncherHandle, NativeUiFrame, RendererError,
    ACTIVATION_SAMPLE_CAPACITY,
};
/// Re-exported so a caller can name [`NativeLauncherConfig::present_mode`]
/// without taking its own `wgpu` dependency and risking a version skew with
/// the one the renderer is built against.
pub use wgpu;

/// One row in the result list (spec 6.4).
#[derive(Debug, Clone)]
pub struct ResultRow {
    pub target: ActionTarget,
    pub source: Option<ResultSource>,
    pub command_state: Option<CommandRowState>,
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

/// Presentation provenance, never command dispatch authority.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResultSource {
    pub owner: PluginId,
    pub item: ItemId,
    pub kind: ResultSourceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResultSourceKind {
    Ordinary,
    DeclaredCatalog {
        epoch: u64,
    },
    Standard {
        logical_view: u64,
        surface: u64,
        revision: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRowState {
    pub registry_revision: u64,
    pub preference_revision: u64,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub favorite: Option<usize>,
    pub favorite_count: usize,
    pub hidden: bool,
    pub fallback: Option<usize>,
    pub fallback_count: usize,
    pub aliases: Vec<String>,
    pub hotkey: Option<String>,
    pub argument_policy: crikey_core::ArgumentPolicy,
    pub argument: Option<String>,
    pub ranking_reset_available: bool,
}

/// Membership edits preserve every other preference record field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandPreferenceEdit {
    Favorite(bool),
    MoveFavorite { position: usize },
    Hidden(bool),
    Fallback(bool),
    MoveFallback { position: usize },
    SetAlias(Option<String>),
    SetHotkey(Option<String>),
}

impl ResultRow {
    pub fn has_primary_action(&self) -> bool {
        match &self.target {
            ActionTarget::Command(_) => self.command_state.as_ref().is_some_and(|state| state.available),
            ActionTarget::Result { .. } => self.default_action.is_some(),
        }
    }
    pub fn primary_identity(&self) -> Option<ActionIdentity> {
        match &self.target {
            ActionTarget::Command(_) => self.has_primary_action().then_some(ActionIdentity::InvokeCommand),
            ActionTarget::Result { .. } => self
                .default_action
                .as_ref()
                .map(|action| ActionIdentity::Plugin(action.action_id.clone())),
        }
    }

    pub fn primary_command(&self, generation: Generation, query: &str) -> Option<UiCommand> {
        if !self.has_primary_action() {
            return None;
        }
        match &self.target {
            ActionTarget::Command(_) => Some(UiCommand::InvokeCommand {
                target: self.target.clone(),
                argument: None,
                generation,
                query: query.to_owned(),
                registry_revision: self.command_state.as_ref()?.registry_revision,
            }),
            ActionTarget::Result { .. } => Some(UiCommand::ActivateAction {
                target: self.target.clone(),
                identity: self.primary_identity()?,
            }),
        }
    }
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
    /// Which control the value is edited with.
    pub control: SettingControl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingEditStatus {
    Saved,
    Failed,
    PersistedReloadRequired,
}

/// An actual persistence result, correlated with the original submitted edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingEditOutcome {
    pub request_id: u64,
    pub key: String,
    pub value: String,
    pub status: SettingEditStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPreferenceEditOutcome {
    pub request_id: u64,
    pub reference: crikey_core::CommandRef,
    pub edit: CommandPreferenceEdit,
    pub status: SettingEditStatus,
}

pub fn command_edit_key(reference: &crikey_core::CommandRef, hotkey: bool) -> String {
    format!(
        "command:{}:{}:{}:{}",
        reference.provider_id.0.len(),
        reference.provider_id.0,
        reference.command_id,
        if hotkey { "hotkey" } else { "alias" }
    )
}

static NEXT_SETTING_EDIT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn next_setting_edit_id() -> u64 {
    use std::sync::atomic::Ordering;
    NEXT_SETTING_EDIT
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| next.checked_add(1))
        .expect("setting edit identity exhausted")
}

/// What kind of control edits a [`SettingRow`].
///
/// Host-supplied like everything else on the row, and for the same reason: the
/// renderer must not decide that a value spelled `true` is a boolean, because
/// a free-text setting is allowed to hold that word and would then be
/// undraggable out of it. The host owns the schema and says which control the
/// value takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingControl {
    /// Free text, edited in a field and committed with Enter or Save.
    Text,
    /// Exactly on or off, shown as a switch that commits the moment it moves.
    ///
    /// The state is carried here rather than parsed from
    /// [`SettingRow::value`], so the renderer never interprets a configuration
    /// value: the host has already read the key the same way the launcher
    /// reads it to decide its own behaviour, which is what keeps the switch
    /// and the thing it switches from disagreeing.
    Toggle { on: bool },
}

/// One complete engine-painted web surface frame.
///
/// Pixels are premultiplied RGBA8, row-major from the top left. The native
/// protocol validates its own message before the launcher constructs this
/// transport-independent value; the view model nevertheless checks the
/// dimensions against the byte count before allowing a renderer to upload it.
///
/// Shared because the same bounded raster crosses the host/view-model boundary
/// and then the view-model/renderer boundary. Neither handoff copies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSurfaceFrame {
    pub surface_id: u64,
    pub generation: u64,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub rgba8: Arc<[u8]>,
}

impl WebSurfaceFrame {
    /// Whether this value is a non-empty, complete RGBA surface.
    pub fn is_valid(&self) -> bool {
        self.surface_id != 0
            && self.generation != 0
            && self.pixel_width != 0
            && self.pixel_height != 0
            && (self.pixel_width as usize)
                .checked_mul(self.pixel_height as usize)
                .and_then(|pixels| pixels.checked_mul(4))
                == Some(self.rgba8.len())
    }
}

/// Raw input for the supervised web engine.
///
/// This is deliberately not `crikey-native-protocol::RawInput`: the UI knows
/// nothing about a transport or its numeric discriminants. The composition
/// root translates these semantic variants at its own boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum WebSurfaceInput {
    Key {
        surface_id: u64,
        timestamp_ms: u64,
        pressed: bool,
        keysym: u32,
        /// XKB keycode on the Linux WPE path (evdev scancode plus eight).
        /// Zero when the input source has no hardware-key identity.
        hardware_keycode: u32,
        repeat: bool,
        modifiers: u32,
    },
    PointerMove {
        surface_id: u64,
        timestamp_ms: u64,
        x: f32,
        y: f32,
        modifiers: u32,
    },
    PointerButton {
        surface_id: u64,
        timestamp_ms: u64,
        x: f32,
        y: f32,
        button: u32,
        pressed: bool,
        press_count: u32,
        modifiers: u32,
    },
    Scroll {
        surface_id: u64,
        timestamp_ms: u64,
        x: f32,
        y: f32,
        delta_x: f32,
        delta_y: f32,
        precise: bool,
        modifiers: u32,
        stop: bool,
    },
    Focus {
        surface_id: u64,
        focused: bool,
    },
}

/// Input-method composition for the supervised web engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebImeEvent {
    /// The current marked text. The optional range is measured in Unicode
    /// scalar values, not UTF-8 bytes.
    Preedit {
        surface_id: u64,
        text: String,
        cursor: Option<(u32, u32)>,
    },
    Commit {
        surface_id: u64,
        text: String,
    },
}

/// Modifier bits used by [`WebSurfaceInput`].
///
/// These are engine semantics rather than protocol discriminants. They remain
/// stable when the host transport changes.
pub const WEB_MODIFIER_SHIFT: u32 = 1 << 0;
pub const WEB_MODIFIER_CONTROL: u32 = 1 << 1;
pub const WEB_MODIFIER_ALT: u32 = 1 << 2;
pub const WEB_MODIFIER_META: u32 = 1 << 3;

/// Engine pointer-button numbering used by [`WebSurfaceInput`].
pub const WEB_POINTER_BUTTON_LEFT: u32 = 1;
pub const WEB_POINTER_BUTTON_MIDDLE: u32 = 2;
pub const WEB_POINTER_BUTTON_RIGHT: u32 = 3;

/// The plugin-owned page the launcher is showing, if any (spec 6.3, 32).
///
/// A page either carries a display list the host draws, or declares a web
/// document whose supervised engine supplies complete frames. The renderer
/// places and clips both kinds inside the same host-owned page rectangle;
/// neither can paint over launcher chrome.
///
/// The declaration and display list are shared rather than copied: a page
/// that redraws on a timer republishes its frame many times, and a repaint
/// that changes nothing about it costs a refcount bump instead of a copy of
/// every node and string. Engine pixels are shared separately in `web_frame`.
#[derive(Debug, Clone, PartialEq)]
pub struct PageSurface {
    /// Which plugin owns the page, so the host can route input back to the
    /// process that asked for it.
    pub plugin: PluginId,
    /// The plugin's own name for this page, echoed in every request so a
    /// plugin that offers several pages knows which one is being drawn.
    pub page_id: String,
    /// The owning plugin's display name, shown in the status line: a surface
    /// that takes the whole launcher must always say whose it is.
    pub plugin_name: String,
    /// The last accepted display list or web declaration. Already validated
    /// by the host transport, so nothing that failed
    /// [`crikey_core::PageFrame::validate`] reaches the renderer.
    pub frame: Arc<PageFrame>,
    /// Whether the host is still waiting for the plugin to answer the request
    /// it has already sent.
    ///
    /// The nodes below are then the previous frame, which is the right thing to
    /// keep drawing -- blanking the surface on every round trip would flicker
    /// a page that redraws on a timer -- but the user has to be able to tell
    /// that what they are looking at is not the answer to what they just did,
    /// so the status line says so.
    pub stale: bool,
    /// Whether any frame from the plugin has ever been accepted for this
    /// page.
    ///
    /// Distinct from the display list being empty, and the distinction
    /// matters: a plugin may legitimately publish a page with no nodes -- a
    /// cleared canvas, a form that has just been submitted -- and that page
    /// has been answered. What has not been answered is the surface the host
    /// puts up the instant it opens one, before the plugin has drawn
    /// anything, which is the only case that should read as loading.
    pub answered: bool,
    /// Launcher-generated identity of the supervised engine surface, once the
    /// declaration has passed permission checks. `None` for display-list pages
    /// and for a declaration that has not been admitted yet.
    pub web_surface_id: Option<u64>,
    /// The newest accepted engine frame. It belongs to `web_surface_id`; a
    /// frame for a closed or replaced id never enters this slot.
    pub web_frame: Option<Arc<WebSurfaceFrame>>,
    /// Highest engine generation accepted for this surface. Kept even when
    /// `web_frame` is cleared, so an older frame already in flight cannot
    /// repaint the sheet afterwards.
    pub web_generation: u64,
    /// Engine-reported caret rectangle in page-local logical pixels. The
    /// native renderer offsets it by the measured page origin before asking
    /// the window system to position an IME candidate window.
    pub web_caret_area: Option<(f32, f32, f32, f32)>,
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
    /// Host view/session identity; results-only frames have no input authority.
    pub view: Option<ViewId>,
    /// One-shot acknowledgment of an ordered native input boundary.
    pub input_receipt: Option<Arc<ActionInputReceipt>>,
    pub generation: Generation,
    pub query: String,
    pub rows: Arc<[ResultRow]>,
    pub selected: usize,
    /// Changes on explicit selection intent or same-generation publication
    /// that changes the selected target or index. Query generations carry
    /// query navigation separately; identical republications do not undo scrolling.
    pub root_selection_revision: u64,
    /// True while at least one plugin is still working on this generation.
    pub pending_plugins: bool,
    /// True while the action list of the selected row is open (spec 6.3).
    ///
    /// The overlay is view-model state rather than renderer state, so which
    /// rung `Cancel` backs out of is decided in one place and every renderer
    /// draws the same launcher.
    pub action_panel: Option<Arc<ActionPanel>>,
    /// True while the settings surface is showing (spec 6.3).
    pub settings_open: bool,
    /// What the settings surface lists, as the host last described it.
    ///
    /// Shared for the same reason the rows are: a frame that changes nothing
    /// about the settings costs a refcount bump rather than a copy of every
    /// key, label and value.
    pub settings: Arc<[SettingRow]>,
    pub setting_edits: Arc<[SettingEditOutcome]>,
    /// Full authoritative management projection, including hidden and unavailable references.
    pub command_management: Arc<[ResultRow]>,
    /// The setting whose editor should take the keyboard when the surface
    /// opens, by key. `None` leaves the focus where the user put it.
    pub settings_focus: Option<String>,
    /// Whether the footer's navigation hint line is drawn.
    ///
    /// Carried on the frame rather than looked up by key in the renderer,
    /// because [`SettingRow`] is the only thing the UI knows about
    /// configuration: a renderer that read `launcher.show-hints` itself would
    /// be a second, silent copy of the configuration schema living in the crate
    /// that is documented not to have one.
    pub show_hints: bool,
    /// Whether the launcher window is drawn with rounded corners.
    ///
    /// Carried for the same reason [`Self::show_hints`] is, and live for the
    /// same reason: a preference the user changes in the settings surface has
    /// to take effect in the next frame, not the next launch.
    ///
    /// A window can only round its corners smoothly where something composites
    /// them. Where nothing does, the launcher stays square rather than cutting
    /// a stepped arc out of itself, so this asks for rounded corners rather
    /// than promising them.
    pub rounded_corners: bool,
    /// The plugin-drawn page the launcher is showing, if any (spec 32).
    ///
    /// Mutually exclusive with [`Self::settings_open`] by construction, not by
    /// convention: both take the whole result area and both claim the
    /// keyboard, so the model closes one when the other opens and the renderer
    /// never has to decide which of two full-height surfaces wins.
    pub page: Option<PageSurface>,
    pub preview: PreviewPane,
    pub navigation: NavigationPresentation,
}

/// Opaque ownership checkpoint for a split native input batch.
#[derive(Debug, Clone)]
pub struct ActionInputCheckpoint {
    pub(crate) view: Option<ViewId>,
    pub(crate) generation: Generation,
    pub(crate) query: String,
    pub(crate) target: Option<ActionTarget>,
    pub(crate) primary: Option<ActionId>,
    pub(crate) panel: Option<Arc<ActionPanel>>,
    pub(crate) preview_focus: (bool, bool),
    // The successful response revision identifies the exact definitions/schema.
    // Retain only its binding, never a descriptor outside navigation accounting.
    pub(crate) standard: Option<(PluginId, u64, crikey_core::structured_view::ViewExecutionContext)>,
    pub(crate) standard_refresh_pending: bool,
    pub(crate) typed_revision: Option<(u64, u64)>,
}

impl ActionInputCheckpoint {
    pub(crate) fn of(model: &ViewModel) -> Self {
        Self {
            view: model.view,
            generation: model.generation,
            query: model.query.clone(),
            target: model.rows.get(model.selected).map(ActionTarget::of),
            primary: model
                .rows
                .get(model.selected)
                .and_then(|row| row.default_action.as_ref())
                .map(|action| action.action_id.clone()),
            panel: model.action_panel.clone(),
            preview_focus: (model.preview.focused, model.preview.detail),
            standard: model.navigation.standard.as_ref().map(|surface| {
                (surface.owner.clone(), surface.producer, surface.context.clone())
            }),
            standard_refresh_pending: model.navigation.standard_refresh_pending,
            typed_revision: model.navigation.inline_arguments.as_ref().map(|input| (input.revision, input.draft_revision)),
        }
    }

    fn same_panel(&self, panel: Option<&Arc<ActionPanel>>) -> bool {
        match (&self.panel, panel) {
            (Some(expected), Some(current)) => {
                expected.focus_token() == current.focus_token()
                    && expected.layout_revision() == current.layout_revision()
                    && expected.query() == current.query()
                    && expected.path() == current.path()
                    && expected.selected() == current.selected()
                    && expected.invalidation() == current.invalidation()
            }
            (None, None) => true,
            _ => false,
        }
    }

    pub(crate) fn matches(
        &self,
        view: Option<ViewId>,
        generation: Generation,
        query: &str,
        row: Option<&ResultRow>,
        panel: Option<&Arc<ActionPanel>>,
        preview_focus: (bool, bool),
        standard: Option<&Arc<standard_view::StandardSurface>>,
        standard_refresh_pending: bool,
        typed_revision: Option<(u64, u64)>,
    ) -> bool {
        self.view.is_some()
            && self.view == view
            && self.generation == generation
            && self.query == query
            && self.standard.as_ref().map(|(owner, producer, context)| (owner, *producer, context))
                == standard.map(|surface| (&surface.owner, surface.producer, &surface.context))
            && self.standard_refresh_pending == standard_refresh_pending
            && !standard_refresh_pending
            && self.typed_revision == typed_revision
            && self.primary.as_ref()
                == row
                    .and_then(|row| row.default_action.as_ref())
                    .map(|action| &action.action_id)
            && self.preview_focus == preview_focus
            && self.same_panel(panel)
            && match (&self.target, row) {
                (Some(target), Some(row)) => target.matches(row),
                (None, None) => true,
                _ => false,
            }
    }
}

impl PartialEq for ActionInputCheckpoint {
    fn eq(&self, other: &Self) -> bool {
        self.view == other.view
            && self.generation == other.generation
            && self.query == other.query
            && self.target == other.target
            && self.primary == other.primary
            && self.preview_focus == other.preview_focus
            && self.standard == other.standard
            && self.standard_refresh_pending == other.standard_refresh_pending
            && self.typed_revision == other.typed_revision
            && self.same_panel(other.panel.as_ref())
    }
}

/// Proof that the host applied an ordered input boundary.
#[derive(Debug, Clone)]
pub struct ActionInputReceipt {
    pub(crate) token: u64,
    pub(crate) accepted: bool,
    pub(crate) owner: ActionInputCheckpoint,
}

/// Keyboard-only interaction surface (spec 6.3).
///
/// Not [`Eq`]: a page input carries pointer coordinates, which are floats.
#[derive(Debug, Clone, PartialEq)]
pub enum UiCommand {
    StandardSelect {
        context: crikey_core::structured_view::ViewExecutionContext,
        target: crikey_core::structured_arguments::OwnedItem,
    },
    StandardActivate {
        context: crikey_core::structured_view::ViewExecutionContext,
        executable: crikey_core::structured_arguments::ExecutableKey,
    },
    /// A deliberate grid gesture, deferrable only across its own selection refresh.
    StandardDoubleClick {
        context: crikey_core::structured_view::ViewExecutionContext,
        executable: crikey_core::structured_arguments::ExecutableKey,
    },
    StandardLink {
        context: crikey_core::structured_view::ViewExecutionContext,
        url: String,
    },
    StandardHorizontal(bool),
    SetTypedDraft {
        revision: u64,
        draft_revision: u64,
        draft: std::collections::BTreeMap<
            crikey_core::structured_arguments::FieldId,
            crikey_core::structured_arguments::NonSecretValue,
        >,
    },
    TypedInputActivity {
        revision: u64,
        draft_revision: u64,
    },
    SetScope(Option<ScopeIdentity>),
    ActivateActionless {
        target: ActionTarget,
    },
    InvokeCommand {
        target: ActionTarget,
        argument: Option<String>,
        generation: Generation,
        query: String,
        registry_revision: u64,
    },
    EditCommandPreference {
        target: ActionTarget,
        expected_revision: u64,
        edit: CommandPreferenceEdit,
    },
    EditCommandShortcut {
        request_id: u64,
        target: ActionTarget,
        expected_revision: u64,
        edit: CommandPreferenceEdit,
    },
    ResetCommandRanking {
        target: ActionTarget,
        registry_revision: u64,
    },
    Back,
    SetLogicalScroll(LogicalScroll),
    RecallQuery,
    ClearQueryHistory,
    /// Apply this editor prefix and host key atomically in the composition root.
    ApplyInputBatch {
        continuation: Option<u64>,
        source: Arc<ActionInputCheckpoint>,
        commands: Vec<UiCommand>,
    },
    FocusPreview,
    LeavePreview,
    ShowPreviewDetail,
    SetPreviewRatio(f32),
    BeginShortcutCapture(String),
    RecordShortcut(String),
    CancelShortcutCapture,
    /// Native input owned by a focused editor still supersedes asynchronous presentation.
    UserActivity,
    /// Pointer press may complete the grid's captured selection gesture.
    PointerActivity,
    OpenPreviewLink {
        /// Rare link activation owns its large source identity off the hot command enum.
        target: Box<PreviewLinkTarget>,
        url: String,
    },
    SetQuery(String),
    SelectNext,
    SelectPrevious,
    PageDown,
    PageUp,
    Complete,
    /// Opens the action list of the selected row (spec 6.3).
    ShowActions,
    ExecuteDefault,
    SetActionQuery(String),
    ActivateAction {
        target: ActionTarget,
        identity: ActionIdentity,
    },
    ActivateSelectedAction,
    ActionBack,
    Cancel,
    Dismiss,
    /// Shows the settings surface (spec 6.3).
    OpenSettings,
    /// Hides the settings surface again.
    CloseSettings,
    /// One pointer, key or focus event the open page must be told about
    /// (spec 32). The renderer has already hit-tested it and translated its
    /// coordinates into the page's own space.
    PageInput(PageInput),
    /// Raw input for the supervised engine behind an admitted web page.
    WebInput(WebSurfaceInput),
    /// Input-method composition for the supervised engine.
    WebIme(WebImeEvent),
    /// Closes the open page. Reserved by the host -- Escape produces this and
    /// is never forwarded -- so a plugin can never trap the user inside its
    /// own surface.
    ClosePage,
    /// The renderer measured the page area. Sent when the size changes so the
    /// plugin can lay out against the surface it actually has (spec 32.3).
    ResizePage {
        width: u32,
        height: u32,
    },
    /// Asks the host to persist one setting; the UI neither validates nor
    /// stores it.
    SetSetting {
        request_id: u64,
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
///
/// Not [`Eq`], for the same reason [`UiCommand`] is not.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEffect {
    ExecuteStandard {
        owner: PluginId,
        context: crikey_core::structured_view::ViewExecutionContext,
        executable: crikey_core::structured_arguments::ExecutableKey,
    },
    RefreshStandard(crikey_core::structured_view::ViewRefreshReason),
    OpenStandardLink {
        owner: PluginId,
        context: crikey_core::structured_view::ViewExecutionContext,
        url: String,
    },
    ExecuteActionless {
        owner: PluginId,
        item: ItemId,
        view: ViewId,
        generation: Generation,
    },
    RecallQuery,
    InvokeCommand {
        reference: crikey_core::CommandRef,
        argument: Option<String>,
        registry_revision: u64,
        view: ViewId,
        generation: Generation,
        query: String,
    },
    MutateCommandPreferences {
        request_id: u64,
        reference: crikey_core::CommandRef,
        expected_revision: u64,
        edit: CommandPreferenceEdit,
    },
    ResetCommandRanking {
        reference: crikey_core::CommandRef,
        registry_revision: u64,
    },
    ClearQueryHistory,
    /// Apply serially without polling foreign completions between commands,
    /// then acknowledge a retained continuation with `finish_input_batch`.
    InputBatch {
        continuation: Option<u64>,
        commands: Vec<UiCommand>,
    },
    /// The query text changed; schedule a generation for it (spec 6.2.2).
    Query(String),
    /// Run `action` on `item` (spec 6.2.10). The launcher stays open —
    /// dismissing after an execution is the host's decision, not the UI's.
    Execute {
        owner: PluginId,
        item: ItemId,
        action: ActionId,
    },
    OpenPreviewLink {
        target: PreviewLinkTarget,
        url: String,
    },
    /// The launcher closed itself and is warm again for the next hotkey.
    Dismissed,
    /// Persist `value` under `key` (spec 6.3). Opening and closing the
    /// settings surface is the UI's own business, but the value behind a row
    /// belongs to the host's configuration store.
    SetSetting {
        request_id: u64,
        key: String,
        value: String,
    },
    /// Deliver `PageInput` to the plugin that owns the open page (spec 32.9).
    /// The model holds no transport, so the round trip is the host's.
    PageInput(PageInput),
    /// Deliver raw input to the supervised engine behind the open web page.
    WebInput(WebSurfaceInput),
    /// Deliver input-method composition to the supervised web engine.
    WebIme(WebImeEvent),
    /// Tear the page session down with the plugin that owns it.
    ///
    /// Produced even when the model had no page open, and even while the
    /// launcher is hidden: the model forgets a page the instant the user asks
    /// it to close, and if that also swallowed the effect the plugin would be
    /// left holding a session nobody will ever answer again.
    ClosePage,
    /// Tell the owning plugin how large its surface actually is, so the next
    /// frame it draws is laid out for the space it has.
    ResizePage {
        width: u32,
        height: u32,
    },
    /// The user asked the launcher to exit for good, not to hide until the
    /// next hotkey press.
    Quit,
}

/// Logical launcher surface, independent of any query or engine generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    RootList,
    Settings,
    CustomPage,
    Standard,
}

/// Process-local identity of a logical surface lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewId(u64);

impl ViewId {
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Host content only: never a custom page or another embedded engine.
#[derive(Debug, Clone, PartialEq)]
pub enum PreviewStatus {
    Loading,
    Content(Arc<PreparedPreview>),
    Empty,
    Unavailable(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreviewPane {
    pub status: Option<PreviewStatus>,
    pub focused: bool,
    pub detail: bool,
    pub ratio: f32,
    pub link_target: Option<PreviewLinkTarget>,
}

impl Default for PreviewPane {
    fn default() -> Self {
        Self {
            status: None,
            focused: false,
            detail: false,
            ratio: 0.55,
            link_target: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewSelection {
    pub view: ViewId,
    pub target: ActionTarget,
    pub source: ResultSource,
    pub generation: Generation,
    pub plugin: PluginId,
    pub item: ItemId,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewLinkTarget {
    pub selection: PreviewSelection,
    pub plugin: PluginId,
    pub incarnation: u64,
    pub request: u64,
}

/// The renderer-free launcher state machine (spec 6.1 - 6.5; ADR-0002).
///
/// Constructed once at startup and kept alive for the process lifetime:
/// [`activate`](Self::activate) shows an object that already exists, so the
/// hotkey path never pays construction cost, and [`dismiss`](Self::dismiss)
/// empties the launcher without dropping it.
///
/// Logical view lifetimes are independent of query generations: covering the
/// root retires its publications, and returning requests a fresh search.
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
    view_id: ViewId,
    publication_view: Option<ViewId>,
    input_receipt: Option<Arc<ActionInputReceipt>>,
    input_batch: Option<u64>,
    query_refresh: bool,
    restore_selection: bool,
    /// Set by every accepted mutation and cleared by `frame`, so mutations
    /// between two frames coalesce into one view model (spec 25.5).
    dirty: bool,
    query: String,
    /// The published rows, shared with every frame handed out since. Replaced
    /// wholesale by `publish`; never mutated in place, because a frame the
    /// renderer is still holding must keep describing what it was given.
    rows: Arc<[ResultRow]>,
    selected: usize,
    root_selection_revision: u64,
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
    action_panel: Option<Arc<ActionPanel>>,
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
    setting_edits: Arc<[SettingEditOutcome]>,
    /// The setting an opening surface should put the keyboard in, by key.
    settings_focus: Option<String>,
    /// The plugin-drawn page the launcher is showing, if any (spec 32).
    ///
    /// Session state, unlike the settings rows above: a page belongs to the
    /// plugin round trip that opened it, so `dismiss` drops it and the host
    /// tears the plugin's side down when it sees [`UiEffect::Dismissed`].
    page: Option<PageSurface>,
    /// Whether published frames draw the footer's navigation hint line. Host
    /// configuration rather than session state, like `settings` above, so it
    /// survives `dismiss`.
    show_hints: bool,
    /// Whether published frames ask for a rounded window. Host configuration
    /// too, and kept across a dismissal for the same reason: it describes the
    /// launcher rather than the query being typed into it.
    rounded_corners: bool,
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
    preview: PreviewPane,
    preview_selection: Option<PreviewSelection>,
    selection_revision: u64,
    navigation_stack: Vec<navigation::Entry>,
    navigation_scroll: LogicalScroll,
    navigation_restore: u64,
    navigation_unavailable: Option<String>,
    navigation_notice_revision: u64,
    navigation_notice_token: Option<NavigationNoticeToken>,
    page_reopen: Option<NavigationRoute>,
    reopening: bool,
    resume_delay: std::time::Duration,
    suspended_at: Option<std::time::Duration>,
    suspended: Option<navigation::Entry>,
    command_management: Arc<[ResultRow]>,
    navigation_clock: Arc<dyn NavigationClock>,
    recall: Arc<[String]>,
    recall_selected: usize,
    navigation_scope: Option<ScopeIdentity>,
    restoration_target: Option<ActionTarget>,
    navigation_title: Arc<str>,
    preserve_scroll: bool,
    inline_arguments: Option<InlineArguments>,
    standard_restoring: bool,
    standard_refresh_pending: bool,
    /// Keyboard follow survives intermediate admissions until final publication.
    standard_follow_selection: bool,
    standard_pointer_source: Option<Arc<standard_view::StandardSurface>>,
    standard_deferred_activation: Option<crikey_core::structured_arguments::ExecutableKey>,
    standard_ready_activation: Option<UiCommand>,
    scope_choices: Arc<[ScopeChoice]>,
    section_headers: Arc<[SectionHeader]>,
    actionless_targets: Arc<[crikey_core::structured_arguments::OwnedItem]>,
    standard: Option<Arc<standard_view::StandardSurface>>,
}

impl Default for LauncherViewModel {
    fn default() -> Self {
        Self::new()
    }
}

impl LauncherViewModel {
    /// Builds the warm launcher: fully constructed, hidden, presenting nothing.
    ///
    /// Construction and the bounded navigation entry pool are paid once at
    /// startup. Default reset-mode activation needs no reconstruction.
    pub fn new() -> Self {
        Self {
            command_management: Arc::default(),
            scope_choices: Arc::default(),
            standard_restoring: false,
            standard_refresh_pending: false,
            section_headers: Arc::default(),
            actionless_targets: Arc::default(),
            inline_arguments: None,
            standard: None,
            visible: false,
            view_id: ViewId(0),
            publication_view: None,
            input_receipt: None,
            input_batch: None,
            query_refresh: false,
            restore_selection: false,
            dirty: false,
            query: String::new(),
            rows: Arc::default(),
            selected: 0,
            root_selection_revision: 0,
            navigated: false,
            action_panel: None,
            pending_plugins: false,
            settings_open: false,
            settings: Arc::default(),
            setting_edits: Arc::default(),
            settings_focus: None,
            page: None,
            show_hints: true,
            rounded_corners: true,
            floor: None,
            active: None,
            published: None,
            preview: PreviewPane::default(),
            preview_selection: None,
            selection_revision: 0,
            navigation_stack: Vec::with_capacity(navigation::MAX_NAVIGATION_ENTRIES - 1),
            navigation_scroll: LogicalScroll::default(),
            navigation_restore: 0,
            navigation_unavailable: None,
            navigation_notice_revision: 0,
            navigation_notice_token: None,
            page_reopen: None,
            reopening: false,
            resume_delay: std::time::Duration::ZERO,
            suspended_at: None,
            suspended: None,
            navigation_clock: Arc::new(navigation::MonotonicNavigationClock::default()),
            recall: Arc::default(),
            recall_selected: 0,
            navigation_scope: None,
            restoration_target: None,
            navigation_title: Arc::from("Search"),
            preserve_scroll: false,
            standard_follow_selection: false,
            standard_pointer_source: None,
            standard_deferred_activation: None,
            standard_ready_activation: None,
        }
    }
    const INLINE_SOURCE_CHANGED_NOTICE: &str =
        "Command input source changed; select the current command again";

    /// Install only an exact source-pinned schema resolved by the application.
    pub fn open_inline_arguments(
        &mut self,
        revision: u64,
        executable: crikey_core::structured_arguments::ExecutableKey,
        schema: Arc<crikey_core::structured_arguments::ArgumentSchema>,
        source: PublicationIdentity,
        command: Option<(crikey_core::CommandRef, u64)>,
    ) -> bool {
        if self
            .view()
            .is_some_and(|(_, kind)| matches!(kind, ViewKind::RootList | ViewKind::Standard))
            && schema.validate().is_ok()
        {
            let command = command.or_else(|| {
                let row = self.rows.get(self.selected)?;
                let ActionTarget::Command(reference) = &row.target else {
                    return None;
                };
                Some((reference.clone(), row.command_state.as_ref()?.registry_revision))
            });
            let Ok(fingerprint) = schema.fingerprint() else {
                return false;
            };
            let draft = Arc::new(crikey_core::structured_arguments::NonSecretDraft {
                executable: executable.clone(),
                values: Default::default(),
            });
            let previous = self.inline_arguments.replace(InlineArguments {
                revision,
                draft_revision: 0,
                draft_retained: true,
                executable,
                schema,
                source,
                fingerprint,
                command,
                view: self.view_id,
                generation: self.active.unwrap_or(Generation::ZERO),
                view_context: self.standard.as_ref().map(|surface| surface.context.clone()),
                outcome: None,
                draft,
            });
            let entry = self.capture_entry();
            if navigation::retained_bytes(self.navigation_stack.iter().chain(std::iter::once(&entry)))
                > navigation::MAX_NAVIGATION_BYTES
            {
                self.inline_arguments = previous;
                self.set_navigation_notice(
                    "Command input exceeds the 8 MiB navigation limit. Go Back before opening it.".into(),
                );
                return false;
            }
            // This refusal belongs to the retired input, not the newly admitted
            // exact context. Preserve every other diagnostic, including notices
            // published after that refusal and notices restored through Back.
            if self.navigation_unavailable.as_deref() == Some(Self::INLINE_SOURCE_CHANGED_NOTICE) {
                self.navigation_unavailable = None;
                self.navigation_notice_token = None;
            }
            self.action_panel = None;
            self.preview.focused = false;
            self.preview.detail = false;
            self.recall = Arc::default();
            self.dirty = true;
            return true;
        }
        false
    }
    /// Explicit command input is another root entry in the same bounded P3 stack.
    pub fn open_root_input(&mut self) -> bool {
        self.activate();
        if self.view().is_none() {
            return false;
        }
        if (!self.query.is_empty()
            || self.inline_arguments.is_some()
            || self.navigation_scope.is_some()
            || self.settings_open
            || self.page.is_some()
            || self.page_reopen.is_some())
            && !self.push_navigation()
        {
            return false;
        }
        self.change_view();
        self.page = None;
        self.page_reopen = None;
        self.reopening = false;
        self.settings_open = false;
        self.settings_focus = None;
        self.navigation_scope = None;
        self.navigation_title = Arc::from("Command input");
        self.navigation_unavailable = None;
        self.navigation_notice_token = None;
        self.active = None;
        self.published = None;
        self.query.clear();
        self.rows = Arc::default();
        self.selected = 0;
        self.query_refresh = false;
        self.dirty = true;
        true
    }
    pub fn retire_inline_arguments(&mut self) {
        self.inline_arguments = None;
        self.dirty = true;
    }
    pub fn retire_unavailable_inline_arguments(&mut self) {
        self.retire_inline_arguments();
        self.set_navigation_notice(Self::INLINE_SOURCE_CHANGED_NOTICE.into());
    }
    pub fn inline_arguments(&self) -> Option<&InlineArguments> {
        self.inline_arguments.as_ref()
    }
    pub fn accepts_typed_input(&self, revision: u64, view: ViewId, generation: Generation) -> bool {
        self.view().is_some_and(|(current, kind)| {
            current == view && matches!(kind, ViewKind::RootList | ViewKind::Standard)
        }) && self.active == Some(generation)
            && !self.standard_refresh_pending
            && self.inline_arguments.as_ref().is_some_and(|input| {
                input.revision == revision
                    && input.view == view
                    && input.generation == generation
                    && input.view_context.as_ref() == self.standard.as_ref().map(|surface| &surface.context)
            })
    }
    pub fn accepts_typed_draft(
        &self,
        revision: u64,
        draft_revision: u64,
        view: ViewId,
        generation: Generation,
    ) -> bool {
        self.accepts_typed_input(revision, view, generation)
            && self
                .inline_arguments
                .as_ref()
                .is_some_and(|input| input.draft_retained && input.draft_revision == draft_revision)
    }
    pub fn inline_arguments_completed(
        &mut self,
        revision: u64,
        draft_revision: u64,
        outcome: crikey_core::structured_arguments::SubmissionOutcome,
    ) {
        if let Some(input) = self
            .inline_arguments
            .as_mut()
            .filter(|input| input.revision == revision && input.draft_revision == draft_revision)
        {
            if outcome.validate(Some(&input.schema)).is_ok() {
                input.outcome = Some(outcome);
                self.dirty = true;
            }
        }
    }
    pub fn selected_scope(&self) -> Option<&ScopeIdentity> {
        self.navigation_scope.as_ref()
    }
    /// Presentation text only; provider admission separately validates its captured source.
    pub fn selected_scope_choice(&self) -> Option<&ScopeChoice> {
        let selected = self.navigation_scope.as_ref()?;
        self.scope_choices
            .iter()
            .find(|choice| &choice.identity == selected)
    }
    pub fn admits_root_generation(&self, generation: Generation) -> bool {
        self.active == Some(generation)
            && self.publication_view == Some(self.view_id)
            && self.view().is_some_and(|(_, kind)| kind == ViewKind::RootList)
    }
    pub fn set_root_presentation(
        &mut self,
        generation: Generation,
        choices: Vec<ScopeChoice>,
        headers: Vec<SectionHeader>,
        actionless: Vec<crikey_core::structured_arguments::OwnedItem>,
    ) {
        if !self.admits_root_generation(generation) {
            return;
        }
        self.scope_choices = choices.into();
        self.section_headers = headers.into();
        self.actionless_targets = actionless.into();
        self.dirty |= self.visible;
    }

    /// Refresh from the same authoritative registry/preferences projection as discovery.
    pub fn set_command_management_rows(&mut self, mut rows: Vec<ResultRow>) {
        rows.sort_by(|left, right| match (&left.target, &right.target) {
            (ActionTarget::Command(left), ActionTarget::Command(right)) => left.cmp(right),
            (ActionTarget::Command(_), _) => std::cmp::Ordering::Less,
            (_, ActionTarget::Command(_)) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        });
        rows.dedup_by(|left, right| left.target == right.target);
        self.command_management = rows.into();
        if self.standard.is_none() {
            reconcile_action_panel(&mut self.action_panel, &self.rows, &self.command_management);
        }
        self.dirty |= self.visible;
    }

    /// Inject before first activation; expiry is monotonic and exact at the boundary.
    pub fn set_navigation_clock(&mut self, clock: Arc<dyn NavigationClock>) {
        self.suspended = None;
        self.suspended_at = None;
        self.navigation_clock = clock;
    }

    pub fn set_resume_delay(&mut self, delay: std::time::Duration) {
        self.resume_delay = delay;
        if delay.is_zero() {
            self.suspended = None;
            self.suspended_at = None;
            if !self.visible {
                self.navigation_stack.clear();
            }
        }
    }

    pub fn navigation_restoration(&self) -> NavigationRestoration {
        // Legacy result queries can contain free-form arguments. Command
        // discovery queries are not argument drafts; their editor drafts stay
        // in renderer state and are never copied into this restoration payload.
        if self.rows.get(self.selected).is_some_and(|row| {
            matches!(row.target, ActionTarget::Result { .. }) && row.argument_hint.is_some()
        }) {
            return NavigationRestoration {
                inline_arguments: self.inline_arguments.clone(),
                scope: self.navigation_scope.clone(),
                ..Default::default()
            };
        }
        NavigationRestoration {
            inline_arguments: self.inline_arguments.clone(),
            query: self.query.clone(),
            selected: self.rows.get(self.selected).map(ActionTarget::of),
            fallback_index: self.selected,
            navigated: self.navigated,
            scope: self.navigation_scope.clone(),
            scroll: self.navigation_scroll.clone(),
        }
    }

    /// Restores logical host state only. The caller owns route/resource cleanup;
    /// the ordinary refresh drain issues fresh work instead of reviving answers.
    pub fn restore_navigation_state(&mut self, state: NavigationRestoration) {
        if !self.visible {
            return;
        }
        self.query = state.query;
        self.inline_arguments = state.inline_arguments;
        self.selected = state
            .selected
            .as_ref()
            .and_then(|target| self.rows.iter().position(|row| target.matches(row)))
            .unwrap_or(state.fallback_index.min(self.rows.len().saturating_sub(1)));
        self.navigated = state.navigated;
        self.restoration_target = state.selected;
        self.navigation_scope = state.scope;
        self.navigation_scroll = if state.scroll.offset.is_finite() && state.scroll.offset >= 0.0 {
            state.scroll
        } else {
            LogicalScroll::default()
        };
        self.preserve_scroll = true;
        self.navigation_restore = self
            .navigation_restore
            .checked_add(1)
            .expect("restoration identity exhausted");
        self.active = None;
        self.published = None;
        self.publication_view = None;
        self.query_refresh = self.visible && self.view().is_some_and(|(_, kind)| kind == ViewKind::RootList);
        self.restore_selection = self.navigated;
        self.preview_selection();
        self.dirty |= self.visible;
    }

    fn capture_entry(&self) -> navigation::Entry {
        let route = if self.settings_open {
            NavigationRoute::Settings {
                // Recorder capture is transient input ownership, never a resumable route.
                focus: self
                    .settings_focus
                    .as_ref()
                    .filter(|focus| !focus.starts_with("record:"))
                    .cloned(),
            }
        } else if let Some(page) = &self.page {
            NavigationRoute::CustomPage {
                plugin: page.plugin.clone(),
                page_id: page.page_id.clone(),
                title: page.plugin_name.clone(),
            }
        } else if let Some(surface) = &self.standard {
            NavigationRoute::Standard {
                owner: surface.owner.clone(),
                producer: surface.producer,
                logical_view: surface.context.session.logical_view,
                route: surface.context.route.clone(),
                title: self.navigation_title.to_string(),
                collection: matches!(
                    surface.descriptor.view,
                    crikey_core::structured_view::StandardView::List(_)
                        | crikey_core::structured_view::StandardView::Grid(_)
                ),
            }
        } else if let Some(route) = &self.page_reopen {
            route.clone()
        } else {
            NavigationRoute::Root
        };
        let state = self.navigation_restoration();
        let rows = self.rows.clone();
        navigation::Entry {
            route,
            rows,
            state,
            descriptor: self.standard.as_ref().map(|s| s.descriptor.clone()),
            admitted_descriptor: self
                .standard
                .as_ref()
                .map(|surface| surface.admitted_descriptor.clone()),
            panel: self.action_panel.clone(),
            render_standard: self.standard.is_some()
                && (!self.standard_refresh_pending || self.standard_pointer_source.is_some()),
            transient_pixels: navigation::transient_pixels(self.page.as_ref(), self.preview.status.as_ref()),
            transient_bytes: navigation::transient_bytes(self.page.as_ref(), self.preview.status.as_ref())
                + self.standard.as_ref().map_or(0, |surface| {
                    std::mem::size_of::<standard_view::StandardSurface>()
                        + 2 * std::mem::size_of::<usize>()
                        + surface.owner.0.capacity()
                        + surface.context.route.0.capacity()
                })
                + self.standard_activation_bytes(),
        }
    }

    fn push_navigation(&mut self) -> bool {
        let mut entry = self.capture_entry();
        if self.navigation_stack.len() + 1 >= navigation::MAX_NAVIGATION_ENTRIES
            || navigation::retained_bytes(self.navigation_stack.iter().chain(std::iter::once(&entry)))
                > navigation::MAX_NAVIGATION_BYTES
        {
            self.navigation_unavailable =
                Some("Navigation limit reached. Go Back before opening another view.".into());
            self.navigation_notice_token = None;
            self.dirty = true;
            return false;
        }
        entry.transient_bytes = 0;
        entry.transient_pixels = 0;
        entry.panel = None;
        entry.render_standard = false;
        entry.admitted_descriptor = None;
        self.navigation_stack.push(entry);
        self.inline_arguments = None;
        self.navigation_unavailable = None;
        self.navigation_notice_token = None;
        self.navigation_scroll = LogicalScroll::default();
        true
    }

    fn restore_entry(&mut self, entry: navigation::Entry) {
        self.page = None;
        self.standard = None;
        self.settings_open = false;
        self.settings_focus = None;
        self.page_reopen = None;
        let no_standard_selection = matches!(entry.route, NavigationRoute::Standard { collection: true, .. })
            && entry.state.selected.is_none();
        self.rows = entry.rows;
        self.restore_navigation_state(entry.state);
        if no_standard_selection {
            self.selected = usize::MAX;
        }
        self.query_refresh = false;
        self.navigation_unavailable = None;
        self.navigation_notice_token = None;
        match entry.route {
            NavigationRoute::Root => {
                self.navigation_title = Arc::from("Search");
                self.query_refresh = self.visible;
            }
            NavigationRoute::Settings { focus } => {
                self.navigation_title = Arc::from("Settings");
                self.settings_open = true;
                self.settings_focus = focus.filter(|focus| !focus.starts_with("record:"));
            }
            route @ NavigationRoute::Standard { .. } => {
                if let NavigationRoute::Standard { title, .. } = &route {
                    self.navigation_title = Arc::from(title.as_str());
                }
                self.page_reopen = Some(route);
            }
            route @ NavigationRoute::CustomPage { .. } => {
                if let NavigationRoute::CustomPage { title, page_id, .. } = &route {
                    self.navigation_title = Arc::from(format!("{title} — {page_id}"));
                }
                self.page_reopen = Some(route);
            }
        }
        self.preview_selection();
        self.dirty |= self.visible;
    }

    fn pop_navigation(&mut self) {
        if let Some(entry) = self.navigation_stack.pop() {
            self.change_view();
            self.restore_entry(entry);
        }
    }

    /// Drain after closing the old surface. The descriptor has no engine state.
    pub fn take_page_reopen(&mut self) -> Option<NavigationRoute> {
        if !self.visible || self.navigation_unavailable.is_some() || self.reopening {
            return None;
        }
        let route = self.page_reopen.take()?;
        if matches!(route, NavigationRoute::Standard { .. }) {
            self.page_reopen = Some(route.clone());
        }
        self.reopening = true;
        Some(route)
    }

    /// An unavailable owner stays a recoverable route with host-owned Back.
    pub fn page_reopen_failed(&mut self, route: NavigationRoute, reason: String) {
        self.reopening = false;
        self.page = None;
        self.page_reopen = Some(route);
        self.set_navigation_notice(format!("Page unavailable: {reason}"));
        self.dirty = true;
    }

    fn navigation_presentation(&self) -> NavigationPresentation {
        NavigationPresentation {
            standard: self.standard.clone(),
            standard_refresh_pending: self.standard_refresh_pending,
            standard_pointer_source: self.standard_pointer_source.clone(),
            scopes: self.scope_choices.clone(),
            selected_scope: self.navigation_scope.clone(),
            section_headers: self.section_headers.clone(),
            standard_route: self.standard.is_some()
                || matches!(self.page_reopen, Some(NavigationRoute::Standard { .. })),
            actionless_targets: self.actionless_targets.clone(),
            // A restored route input is draft intent, not a live editor, until
            // the fresh endpoint descriptor and exact schema have been admitted.
            inline_arguments: if matches!(self.page_reopen, Some(NavigationRoute::Standard { .. }))
                || self.standard_refresh_pending
            {
                None
            } else {
                self.inline_arguments.clone()
            },
            title: self.navigation_title.clone(),
            can_go_back: !self.navigation_stack.is_empty(),
            scroll: self.navigation_scroll.clone(),
            // A keyboard intent supersedes even an anchor not painted yet.
            restoration: if self.preserve_scroll {
                self.navigation_restore
            } else {
                0
            },
            preserve_scroll: self.preserve_scroll,
            unavailable: self.navigation_unavailable.clone(),
            recall: self.recall.clone(),
            recall_selected: self.recall_selected,
        }
    }

    /// One bounded transient picker snapshot, never retained in navigation.
    pub fn set_query_recall(&mut self, queries: Vec<String>) {
        if !self.visible
            || self.view().is_none_or(|(_, kind)| kind != ViewKind::RootList)
            || !self.query.is_empty()
            || !self.rows.is_empty()
        {
            return;
        }
        let mut bytes = 0usize;
        self.recall = queries
            .into_iter()
            .take(100)
            .filter(|query| {
                if query.is_empty() || query.len() > 4096 || query.chars().any(char::is_control) {
                    return false;
                }
                bytes += query.len();
                bytes <= 256 * 1024
            })
            .collect::<Vec<_>>()
            .into();
        self.recall_selected = 0;
        self.dirty = true;
    }

    pub fn set_navigation_notice(&mut self, message: String) {
        let _ = self.publish_navigation_notice(message);
    }

    /// Publishes a notice whose source may later retire only this exact notice.
    pub fn publish_navigation_notice(&mut self, mut message: String) -> Option<NavigationNoticeToken> {
        let mut end = message.len().min(action_panel::MAX_ACTION_TEXT_BYTES);
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        let token = self.navigation_notice_revision.checked_add(1).map(|revision| {
            self.navigation_notice_revision = revision;
            NavigationNoticeToken(revision)
        });
        self.navigation_notice_token = token;
        self.navigation_unavailable = Some(message);
        self.dirty |= self.visible;
        token
    }

    pub fn clear_navigation_notice(&mut self, token: NavigationNoticeToken) {
        if self.navigation_notice_token == Some(token) {
            self.navigation_unavailable = None;
            self.navigation_notice_token = None;
            self.dirty |= self.visible;
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// The current logical surface. Hidden launchers have no live view.
    pub fn view(&self) -> Option<(ViewId, ViewKind)> {
        self.visible.then_some((
            self.view_id,
            if self.settings_open {
                ViewKind::Settings
            } else if self.standard.is_some() {
                ViewKind::Standard
            } else if self.page.is_some() || self.page_reopen.is_some() || self.reopening {
                ViewKind::CustomPage
            } else {
                ViewKind::RootList
            },
        ))
    }

    /// A deliberate Enter on a non-argument root result, not an action search.
    pub fn submitted_root_query(&self) -> Option<(&str, &PluginId, &ItemId)> {
        let row = self.current_action_row()?;
        (self.view().is_some_and(|(_, kind)| kind == ViewKind::RootList)
            && self.action_panel.is_none()
            && !self.query.trim().is_empty()
            && row.argument_hint.is_none()
            && row.default_action.is_some())
        .then_some(())?;
        let ActionTarget::Result { owner, item } = &row.target else {
            return None;
        };
        Some((self.query.as_str(), owner, item))
    }

    /// Root-query recall candidate for a deliberate command Enter. Argument
    /// editors and action search never supply recall text; the host still waits
    /// for successful, privacy-admitted completion before recording this query.
    pub fn submitted_command_query(&self) -> Option<&str> {
        let row = self.current_action_row()?;
        if self.view().is_none_or(|(_, kind)| kind != ViewKind::RootList)
            || self.action_panel.is_some()
            || self.query.trim().is_empty()
            || !matches!(row.target, ActionTarget::Command(_))
        {
            return None;
        }
        let state = row.command_state.as_ref()?;
        if !state.available
            || (state.argument_policy == crikey_core::ArgumentPolicy::Required
                && state.argument.as_ref().is_none_or(|argument| argument.is_empty()))
        {
            return None;
        }
        Some(self.query.as_str())
    }

    /// Drains the fresh search owed after uncovering the root list.
    ///
    /// Separate from `UiEffect` because closing a page already owes the host
    /// a lifecycle effect. The host drains this after applying commands and
    /// asynchronous page transitions, through its ordinary query dispatch.
    pub fn take_query_refresh(&mut self) -> Option<String> {
        if !self.visible
            || self.settings_open
            || self.page.is_some()
            || self.page_reopen.is_some()
            || self.reopening
            || !self.query_refresh
        {
            return None;
        }
        self.query_refresh = false;
        self.restore_selection = self.navigated;
        Some(self.query.clone())
    }

    /// Refreshes host-owned discovery inputs without resetting user selection.
    pub fn refresh_root_query(&mut self) -> Option<String> {
        if !self.view().is_some_and(|(_, kind)| kind == ViewKind::RootList) {
            return None;
        }
        if self.navigated {
            self.restoration_target = self.rows.get(self.selected).map(|row| row.target.clone());
        }
        self.query_refresh = true;
        self.take_query_refresh()
    }

    fn change_view(&mut self) {
        self.clear_standard_activation();
        self.standard_refresh_pending = false;
        self.view_id = ViewId(self.view_id.0.checked_add(1).expect("view identity exhausted"));
        self.recall = Arc::default();
        self.publication_view = None;
        self.input_receipt = None;
        self.pending_plugins = false;
        self.close_actions();
    }

    /// Shows the launcher. Idempotent: activating an open one keeps its query,
    /// rows and selection untouched and produces no frame.
    pub fn activate(&mut self) {
        if self.visible {
            return;
        }

        self.visible = true;
        self.change_view();
        self.query_refresh = true;
        if let Some(entry) = self.suspended.take() {
            if self
                .suspended_at
                .take()
                .and_then(|at| self.navigation_clock.now().checked_sub(at))
                .is_some_and(|age| age < self.resume_delay)
            {
                self.restore_entry(entry);
            } else {
                self.navigation_stack.clear();
            }
        }
        self.dirty = true;
    }

    /// Hides and retires all live admission. Immediate reset is the default;
    /// an optional bounded, memory-only logical snapshot can resume before its
    /// monotonic deadline, but it never retains a page frame or engine.
    ///
    /// Clearing the accept target is what makes dismissal a session boundary:
    /// plugin results still in flight for the generation that was live when the
    /// launcher closed are discarded instead of publishing rows into the
    /// reopened launcher. The generation *floor* survives untouched, so those
    /// same generations also stay rejected by
    /// [`begin_generation`](Self::begin_generation) (spec 6.5).
    ///
    /// Host configuration survives. Page surfaces always close; the host's
    /// handling of [`UiEffect::Dismissed`] ends their resources. A resumed
    /// custom route requests a new session rather than restoring those resources.
    pub fn dismiss(&mut self) {
        if !self.visible {
            return;
        }

        self.navigation_unavailable = None;
        self.navigation_notice_token = None;
        self.navigation_title = Arc::from("Search");
        if !self.resume_delay.is_zero() {
            let mut entry = self.capture_entry();
            if navigation::retained_bytes(self.navigation_stack.iter().chain(std::iter::once(&entry)))
                <= navigation::MAX_NAVIGATION_BYTES
            {
                entry.transient_bytes = 0;
                entry.transient_pixels = 0;
                entry.panel = None;
                entry.render_standard = false;
                entry.admitted_descriptor = None;
                self.suspended = Some(entry);
                self.suspended_at = Some(self.navigation_clock.now());
            } else {
                self.suspended = None;
                self.suspended_at = None;
                self.navigation_stack.clear();
                self.navigation_unavailable =
                    Some("Previous view exceeded the 8 MiB resume limit; search was reset.".into());
                self.navigation_notice_token = None;
            }
        } else {
            self.suspended = None;
            self.suspended_at = None;
            self.navigation_stack.clear();
        }
        self.inline_arguments = None;
        self.clear_standard_activation();
        self.standard = None;
        self.page_reopen = None;
        self.navigation_scroll = LogicalScroll::default();
        self.preserve_scroll = false;
        self.input_receipt = None;
        self.input_batch = None;
        self.visible = false;
        self.dirty = false;
        self.query.clear();
        self.rows = Arc::default();
        self.selected = 0;
        self.navigated = false;
        self.action_panel = None;
        self.pending_plugins = false;
        self.settings_open = false;
        self.settings_focus = None;
        self.page = None;
        self.active = None;
        self.published = None;
        self.publication_view = None;
        self.query_refresh = false;
        self.restore_selection = false;
        self.recall = Arc::default();
        self.reopening = false;
        self.navigation_scope = None;
        self.restoration_target = None;
        self.preview_selection();
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
            || self.settings_open
            || self.page.is_some()
            || self.standard.is_some()
            || self.page_reopen.is_some()
            || self.reopening
            || generation == Generation::ZERO
            || self.floor.is_some_and(|floor| generation <= floor)
        {
            return;
        }

        self.floor = Some(generation);
        // An ordinary query starts at the top; a return search reconciles the
        // existing user selection by item identity through fresh publications.
        if self.restore_selection {
            self.published = Some(generation);
            self.restore_selection = false;
        } else {
            self.navigated = false;
        }
        self.active = Some(generation);
        if let Some(input) = &mut self.inline_arguments {
            input.view = self.view_id;
            input.generation = generation;
        }
        self.publication_view = Some(self.view_id);
        self.query_refresh = false;
        self.pending_plugins = true;
        self.actionless_targets = Arc::default();
        self.section_headers = Arc::default();
        self.dirty = true;
    }

    /// Replaces the row set and the pending flag for `generation`.
    ///
    /// Accepted only when `generation` is exactly the accept target: the
    /// generation begun in this session and not yet superseded or covered.
    /// Older, never-begun, pre-first-generation and pre-dismiss publications
    /// are discarded whole and never partially applied (spec 6.2.7).
    ///
    /// A publish that lands while the query is empty carries no rows at all,
    /// whatever the host ranked for it: an untyped launcher is the query field
    /// and nothing else, and dropping the rows here is what makes that true
    /// for every renderer instead of only for the one that remembers to check.
    pub fn publish(&mut self, generation: Generation, rows: Vec<ResultRow>, pending_plugins: bool) {
        if !self.admits_root_generation(generation) {
            return;
        }

        let selected = self.resolve_selection(&rows, generation);
        // The first ordinary query publication replaces a prior generation,
        // not a choice already painted for this query. Its generation/index
        // already drive native follow, including direct provider first answers.
        // Return searches deliberately retag `published` in `begin_generation`.
        if self.published == Some(generation)
            && (self.selected != selected
                || self.rows.get(self.selected).map(|row| &row.target)
                    != rows.get(selected).map(|row| &row.target))
        {
            self.root_selection_revision = self.root_selection_revision.wrapping_add(1);
        }
        self.selected = selected;
        // One move into a shared allocation per accepted publish; from here on
        // every frame of this result set is a refcount bump.
        self.rows = rows.into();
        reconcile_action_panel(&mut self.action_panel, &self.rows, &self.command_management);
        self.pending_plugins = pending_plugins;
        self.published = Some(generation);
        if !pending_plugins {
            self.restoration_target = None;
            if self.rows.is_empty() {
                // Empty root results have no scroll renderer to report their
                // clamp. Retire the anchor here before another route saves it.
                self.navigation_scroll = LogicalScroll::default();
                self.preserve_scroll = false;
            }
        }
        self.dirty = true;
    }

    /// The newest state, once per accepted mutation.
    ///
    /// `None` while hidden and whenever nothing changed since the last call, so
    /// the list is drawn once per batch and never once per plugin item
    /// (spec 25.5).
    pub fn frame(&mut self) -> Option<ViewModel> {
        self.preview_selection();
        if !self.visible || !self.dirty {
            return None;
        }

        self.dirty = false;
        Some(ViewModel {
            view: Some(self.view_id),
            input_receipt: self.input_receipt.clone(),
            // The active generation, not the published one: the frame carries
            // the generation the query text belongs to.
            generation: self.active.unwrap_or(Generation::ZERO),
            query: self.query.clone(),
            // A refcount bump, not a copy of the row set (spec 25.5).
            rows: Arc::clone(&self.rows),
            selected: if self.standard.is_some()
                && !self.rows.is_empty()
                && !self.standard_selection_available()
            {
                usize::MAX
            } else {
                self.selected
            },
            root_selection_revision: self.root_selection_revision,
            pending_plugins: self.pending_plugins,
            action_panel: self.action_panel.clone(),
            command_management: Arc::clone(&self.command_management),
            settings_open: self.settings_open,
            // Shared for the same reason the rows are.
            settings: Arc::clone(&self.settings),
            setting_edits: Arc::clone(&self.setting_edits),
            settings_focus: self.settings_focus.clone(),
            // One refcount bump on the node list plus the surface's own three
            // short strings, so a page redrawing on a timer does not copy its
            // display list once per frame.
            page: self.page.clone(),
            show_hints: self.show_hints,
            rounded_corners: self.rounded_corners,
            preview: self.preview.clone(),
            navigation: self.navigation_presentation(),
        })
    }

    /// Completes a validated native command batch after all ordinary host effects.
    pub fn finish_input_batch(&mut self, token: u64) {
        if self.input_batch != Some(token) || !self.visible {
            return;
        }
        self.input_batch = None;
        let owner = ActionInputCheckpoint {
            view: Some(self.view_id),
            generation: self.active.unwrap_or(Generation::ZERO),
            query: self.query.clone(),
            target: self.rows.get(self.selected).map(ActionTarget::of),
            primary: self
                .rows
                .get(self.selected)
                .and_then(|row| row.default_action.as_ref())
                .map(|action| action.action_id.clone()),
            panel: self.action_panel.clone(),
            preview_focus: (self.preview.focused, self.preview.detail),
            standard: self.standard.as_ref().map(|surface| {
                (surface.owner.clone(), surface.producer, surface.context.clone())
            }),
            standard_refresh_pending: self.standard_refresh_pending,
            typed_revision: self.inline_arguments.as_ref().map(|input| (input.revision, input.draft_revision)),
        };
        self.input_receipt = Some(Arc::new(ActionInputReceipt {
            token,
            accepted: true,
            owner,
        }));
        self.dirty = true;
    }

    /// The sole command entry point (spec 6.3).
    ///
    /// Returns the work the host must do, or `None` when the command changed
    /// nothing. A hidden launcher ignores every command: only `activate` moves
    /// it, and [`UiCommand::ClosePage`] -- which has a plugin session to end
    /// however the launcher got into this state.
    pub fn shortcut_capture(&self) -> Option<&str> {
        self.visible
            .then_some(self.settings_focus.as_deref())
            .flatten()?
            .strip_prefix("record:")
    }

    pub fn apply(&mut self, command: UiCommand) -> Option<UiEffect> {
        // Closing a page is the one command whose work outlives the launcher
        // session, so it is the one command a hidden launcher still answers:
        // the model may already have forgotten the page -- a dismissal clears
        // it -- but the plugin has not, and swallowing this would leave it
        // holding a session nothing will ever answer again.
        if !self.visible && !matches!(command, UiCommand::ClosePage) {
            return None;
        }
        if !matches!(
            command,
            UiCommand::StandardDoubleClick { .. }
                | UiCommand::SetLogicalScroll(_)
                | UiCommand::PointerActivity
        ) {
            self.clear_standard_activation();
        }
        if matches!(command, UiCommand::PointerActivity) {
            self.standard_deferred_activation = None;
            self.standard_ready_activation = None;
        }
        if let Some(effect) = self.apply_standard_command(&command) {
            return effect;
        }

        if matches!(
            &command,
            UiCommand::SetQuery(_)
                | UiCommand::SelectNext
                | UiCommand::SelectPrevious
                | UiCommand::PageDown
                | UiCommand::PageUp
                | UiCommand::Complete
        ) && self.inline_arguments.take().is_some()
        {
            self.dirty = true;
        }
        match command {
            UiCommand::StandardActivate { .. }
            | UiCommand::StandardDoubleClick { .. }
            | UiCommand::StandardLink { .. }
            | UiCommand::StandardHorizontal(_)
            | UiCommand::StandardSelect { .. } => None,
            UiCommand::SetScope(scope) => {
                if self.navigation_scope == scope {
                    return None;
                }
                if scope.as_ref().is_some_and(|identity| {
                    !self
                        .scope_choices
                        .iter()
                        .any(|choice| &choice.identity == identity)
                }) {
                    return None;
                }
                self.inline_arguments = None;
                self.navigation_scope = scope;
                self.navigation_unavailable = None;
                self.navigation_notice_token = None;
                self.publication_view = None;
                self.active = None;
                self.rows = Arc::default();
                self.selected = 0;
                self.dirty = true;
                Some(UiEffect::Query(self.query.clone()))
            }
            UiCommand::TypedInputActivity {
                revision,
                draft_revision,
            } => {
                if matches!(self.page_reopen, Some(NavigationRoute::Standard { .. }))
                    || self.standard_refresh_pending
                {
                    return None;
                }
                if let Some(input) = self
                    .inline_arguments
                    .as_mut()
                    .filter(|input| input.revision == revision && input.draft_revision < draft_revision)
                {
                    input.draft_revision = draft_revision;
                    input.draft_retained = false;
                    input.outcome = None;
                    self.dirty = true;
                }
                None
            }
            UiCommand::SetTypedDraft {
                revision,
                draft_revision,
                draft,
            } => {
                if matches!(self.page_reopen, Some(NavigationRoute::Standard { .. }))
                    || self.standard_refresh_pending
                {
                    return None;
                }
                let mut previous = None;
                if let Some(input) = self
                    .inline_arguments
                    .as_mut()
                    .filter(|input| input.revision == revision && input.draft_revision <= draft_revision)
                {
                    use crikey_core::structured_arguments::{
                        ArgumentValues, NonSecretDraft, StructuredValue, ValidationCode,
                    };
                    let values = ArgumentValues(
                        draft
                            .into_iter()
                            .map(|(id, value)| (id, StructuredValue::NonSecret(value)))
                            .collect(),
                    );
                    let valid = input.schema.validate_values(&values).err().is_none_or(|errors| {
                        errors.iter().all(|error| {
                            matches!(error.code, ValidationCode::Required | ValidationCode::TooShort)
                        })
                    });
                    if valid {
                        let values = values
                            .0
                            .into_iter()
                            .filter_map(|(id, value)| match value {
                                StructuredValue::NonSecret(value) => Some((id, value)),
                                StructuredValue::Password(_) => None,
                            })
                            .collect();
                        previous = Some(std::mem::replace(
                            &mut input.draft,
                            Arc::new(NonSecretDraft {
                                executable: input.executable.clone(),
                                values,
                            }),
                        ));
                        input.draft_revision = draft_revision;
                        input.draft_retained = true;
                        input.outcome = None;
                        self.dirty = true;
                    }
                }
                if let Some(previous) = previous {
                    let entry = self.capture_entry();
                    if navigation::retained_bytes(self.navigation_stack.iter().chain(std::iter::once(&entry)))
                        > navigation::MAX_NAVIGATION_BYTES
                    {
                        if let Some(input) = &mut self.inline_arguments {
                            input.draft = previous;
                            input.draft_retained = false;
                            input.outcome =
                                Some(crikey_core::structured_arguments::SubmissionOutcome::Failed(
                                    crikey_core::structured_arguments::SubmissionFailureCode::ResourceLimit,
                                ));
                        }
                        self.set_navigation_notice("Draft exceeds the 8 MiB navigation limit. Go Back or shorten the input before submitting.".into());
                    }
                }
                None
            }
            UiCommand::UserActivity | UiCommand::PointerActivity => None,
            UiCommand::BeginShortcutCapture(key) => {
                self.settings_focus = Some(format!("record:{key}"));
                self.dirty = true;
                None
            }
            UiCommand::RecordShortcut(chord) => {
                if let Some(key) = self.shortcut_capture().map(str::to_owned) {
                    self.setting_edit_completed(SettingEditOutcome {
                        request_id: next_setting_edit_id(),
                        key: format!("capture:{key}"),
                        value: chord,
                        status: SettingEditStatus::Saved,
                    });
                    self.settings_focus = None;
                    self.dirty = true;
                }
                None
            }
            UiCommand::CancelShortcutCapture => {
                if self.shortcut_capture().is_some() {
                    self.settings_focus = None;
                    self.dirty = true;
                }
                None
            }
            UiCommand::EditCommandPreference {
                target,
                expected_revision,
                edit,
            } => self.apply(UiCommand::EditCommandShortcut {
                request_id: next_setting_edit_id(),
                target,
                expected_revision,
                edit,
            }),
            UiCommand::Back => {
                let had_page = self.page.is_some();
                self.pop_navigation();
                had_page.then_some(UiEffect::ClosePage)
            }
            UiCommand::SetLogicalScroll(scroll) => {
                if scroll.offset.is_finite() && scroll.offset >= 0.0 {
                    self.navigation_scroll = scroll;
                }
                None
            }
            UiCommand::RecallQuery => (self.view().is_some_and(|(_, kind)| kind == ViewKind::RootList)
                && self.query.is_empty()
                && self.rows.is_empty()
                && self.action_panel.is_none())
            .then_some(UiEffect::RecallQuery),
            UiCommand::ClearQueryHistory => {
                self.recall = Arc::default();
                self.dirty = true;
                Some(UiEffect::ClearQueryHistory)
            }
            UiCommand::ApplyInputBatch {
                continuation,
                source,
                commands,
            } => {
                let source_valid = source.matches(
                    Some(self.view_id),
                    self.active.unwrap_or(Generation::ZERO),
                    &self.query,
                    self.rows.get(self.selected),
                    self.action_panel.as_ref(),
                    (self.preview.focused, self.preview.detail),
                    self.standard.as_ref(),
                    self.standard_refresh_pending,
                    self.inline_arguments.as_ref().map(|input| (input.revision, input.draft_revision)),
                );
                let query_refused = source_valid
                    && commands.iter().any(|command| {
                        matches!(command, UiCommand::SetActionQuery(query)
                        if query.len() > action_panel::MAX_ACTION_QUERY_BYTES)
                    });
                if source_valid && !query_refused {
                    self.input_batch = continuation;
                    Some(UiEffect::InputBatch {
                        continuation,
                        commands,
                    })
                } else {
                    if query_refused {
                        let _ = self
                            .set_selected_status(action_panel::ActionPanelRefusal::QueryTooLong.to_string());
                    }
                    if let Some(token) = continuation {
                        self.input_receipt = Some(Arc::new(ActionInputReceipt {
                            token,
                            accepted: false,
                            owner: (*source).clone(),
                        }));
                        self.dirty = true;
                    }
                    None
                }
            }
            UiCommand::OpenPreviewLink { target, url } => {
                if self.preview_selection().as_ref() == Some(&target.selection)
                    && self.preview.link_target.as_ref() == Some(target.as_ref())
                {
                    Some(UiEffect::OpenPreviewLink { target: *target, url })
                } else {
                    None
                }
            }
            UiCommand::FocusPreview | UiCommand::ShowPreviewDetail => {
                if self.preview.status.is_some() {
                    self.preview.focused = true;
                    self.preview.detail = matches!(command, UiCommand::ShowPreviewDetail);
                    self.action_panel = None;
                    self.dirty = true;
                }
                None
            }
            UiCommand::LeavePreview => {
                self.preview.focused = false;
                self.preview.detail = false;
                self.dirty = true;
                None
            }
            UiCommand::SetPreviewRatio(ratio) => {
                self.set_preview_ratio(ratio);
                Some(UiEffect::SetSetting {
                    request_id: next_setting_edit_id(),
                    key: "launcher.preview-split".into(),
                    value: self.preview.ratio.to_string(),
                })
            }
            UiCommand::SetQuery(text) => self.retype(text),
            UiCommand::SelectNext | UiCommand::SelectPrevious | UiCommand::PageDown | UiCommand::PageUp => {
                let delta = match command {
                    UiCommand::SelectNext => 1,
                    UiCommand::SelectPrevious => -1,
                    UiCommand::PageDown => PAGE_SIZE as isize,
                    _ => -(PAGE_SIZE as isize),
                };
                if !self.recall.is_empty() {
                    self.recall_selected = self
                        .recall_selected
                        .saturating_add_signed(delta)
                        .min(self.recall.len() - 1);
                    self.dirty = true;
                    return None;
                }
                if let Some(panel) = &mut self.action_panel {
                    self.dirty |= Arc::make_mut(panel).move_selection(delta);
                } else {
                    self.select(self.selected.saturating_add_signed(delta));
                }
                None
            }
            UiCommand::Complete if self.action_panel.is_some() => None,
            UiCommand::Complete => self.complete(),
            // Opening the action list schedules nothing and runs nothing: it is
            // pure UI state, so the host has no work and the change reaches the
            // renderer through the next frame.
            UiCommand::ShowActions => {
                self.open_actions();
                None
            }
            UiCommand::SetActionQuery(query) => {
                if let Some(panel) = &mut self.action_panel {
                    match Arc::make_mut(panel).set_query(query) {
                        Ok(changed) => self.dirty |= changed,
                        Err(reason) => {
                            let _ = self.set_selected_status(reason.to_string());
                        }
                    }
                }
                if !self.within_navigation_budget() {
                    self.action_panel = None;
                    self.set_navigation_notice("Actions exceed the shared navigation resource limit".into());
                }
                None
            }
            UiCommand::InvokeCommand {
                target,
                argument,
                generation,
                query,
                registry_revision,
            } => {
                if self.active != Some(generation) || self.query != query {
                    return None;
                }
                self.current_action_row()?;
                let row = self.command_row(&target)?;
                if row.target != target {
                    return None;
                }
                let ActionTarget::Command(reference) = &row.target else {
                    return None;
                };
                let state = row.command_state.as_ref()?;
                if state.registry_revision != registry_revision {
                    return None;
                }
                if !state.available {
                    return None;
                }
                let argument = argument.or_else(|| state.argument.clone());
                match state.argument_policy {
                    crikey_core::ArgumentPolicy::Required
                        if argument.as_ref().is_none_or(|value| value.is_empty()) =>
                    {
                        self.open_actions();
                        return None;
                    }
                    crikey_core::ArgumentPolicy::Forbidden if argument.is_some() => return None,
                    _ => {}
                }
                Some(UiEffect::InvokeCommand {
                    reference: reference.clone(),
                    argument,
                    registry_revision: state.registry_revision,
                    view: self.view_id,
                    generation,
                    query,
                })
            }
            UiCommand::EditCommandShortcut {
                request_id,
                target,
                expected_revision,
                edit,
            } => {
                let row = self.command_row(&target)?;
                if row.target != target {
                    return None;
                }
                let ActionTarget::Command(reference) = &row.target else {
                    return None;
                };
                let state = row.command_state.as_ref()?;
                if state.preference_revision != expected_revision {
                    return None;
                }
                if matches!(edit, CommandPreferenceEdit::Fallback(true))
                    && state.available
                    && state.argument_policy == crikey_core::ArgumentPolicy::Forbidden
                {
                    return None;
                }
                Some(UiEffect::MutateCommandPreferences {
                    request_id,
                    reference: reference.clone(),
                    expected_revision,
                    edit,
                })
            }
            UiCommand::ResetCommandRanking {
                target,
                registry_revision,
            } => {
                let row = self.command_row(&target)?;
                if row.target != target {
                    return None;
                }
                let ActionTarget::Command(reference) = &row.target else {
                    return None;
                };
                let state = row.command_state.as_ref()?;
                if !state.ranking_reset_available || state.registry_revision != registry_revision {
                    return None;
                }
                Some(UiEffect::ResetCommandRanking {
                    reference: reference.clone(),
                    registry_revision,
                })
            }
            UiCommand::ActivateActionless { target } => {
                if self.current_action_row().is_some_and(|row| target.matches(row)) {
                    self.apply(UiCommand::ExecuteDefault)
                } else {
                    None
                }
            }
            UiCommand::ActivateAction { target, identity } => self.dispatch_action(&target, &identity),
            UiCommand::ActivateSelectedAction => self.activate_panel(),
            UiCommand::ActionBack => {
                self.action_back();
                None
            }
            UiCommand::ExecuteDefault => {
                if let Some(query) = self.recall.get(self.recall_selected).cloned() {
                    return self.retype(query);
                }
                if self.action_panel.is_some() {
                    return self.activate_panel();
                }
                if let Some(row) = self.current_action_row() {
                    if let ActionTarget::Result { owner, item } = &row.target {
                        if self
                            .actionless_targets
                            .iter()
                            .any(|target| &target.owner == owner && &target.item == item)
                        {
                            return Some(UiEffect::ExecuteActionless {
                                owner: owner.clone(),
                                item: item.clone(),
                                view: self.view_id,
                                generation: self.active?,
                            });
                        }
                    }
                }
                let row = self.current_action_row()?;
                let target = ActionTarget::of(row);
                let identity = row.primary_identity()?;
                self.dispatch_action(&target, &identity)
            }
            UiCommand::OpenSettings => {
                // The one settings transition that is not purely UI state: it
                // evicts a page, and the plugin behind that page has a session
                // to end.
                self.open_settings(None).then_some(UiEffect::ClosePage)
            }
            UiCommand::CloseSettings => {
                self.close_settings();
                None
            }
            // The page's own input is the host's to deliver: the model holds no
            // transport and the plugin is the only thing that can answer.
            UiCommand::PageInput(input) => Some(UiEffect::PageInput(input)),
            UiCommand::WebInput(input) => Some(UiEffect::WebInput(input)),
            UiCommand::WebIme(event) => Some(UiEffect::WebIme(event)),
            // Pure measurement: nothing in the model changes, and the frame
            // already on screen stays exactly as it is.
            UiCommand::ResizePage { width, height } => {
                self.page.as_ref().map(|_| UiEffect::ResizePage { width, height })
            }
            // The surface goes the moment the user asks, without waiting on a
            // plugin round trip: a page the plugin has stopped answering must
            // still close.
            UiCommand::ClosePage => {
                self.close_page();
                Some(UiEffect::ClosePage)
            }
            UiCommand::SetSetting {
                request_id,
                key,
                value,
            } => Some(UiEffect::SetSetting {
                request_id,
                key,
                value,
            }),
            UiCommand::Quit => Some(UiEffect::Quit),
            // Cancel backs out one rung at a time: it closes an open page
            // first, then the settings surface, then an open action list, then
            // clears a non-empty query, and closes only an already-bare
            // launcher. Dismiss skips the ladder entirely.
            //
            // A page rung ends the plugin's session as well as the surface,
            // which is why this one carries an effect where the other UI-state
            // rungs do not.
            UiCommand::Cancel if self.inline_arguments.is_some() => {
                self.retire_inline_arguments();
                None
            }
            UiCommand::Cancel if self.preview.focused || self.preview.detail => {
                self.preview.focused = false;
                self.preview.detail = false;
                self.dirty = true;
                None
            }
            UiCommand::Cancel if !self.recall.is_empty() => {
                self.recall = Arc::default();
                self.dirty = true;
                None
            }
            UiCommand::Cancel if self.page.is_some() => {
                self.close_page();
                Some(UiEffect::ClosePage)
            }
            UiCommand::Cancel if self.settings_open => {
                self.close_settings();
                None
            }
            UiCommand::Cancel if self.action_panel.is_some() => {
                self.action_back();
                None
            }
            UiCommand::Cancel if !self.navigation_stack.is_empty() => {
                self.pop_navigation();
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
        if self.view().is_none_or(|(_, kind)| kind != ViewKind::RootList) {
            return None;
        }
        self.dirty |= !self.recall.is_empty();
        self.recall = Arc::default();
        if self.query == text {
            return None;
        }
        self.restore_selection = false;
        self.restoration_target = None;
        self.navigation_scroll = LogicalScroll::default();
        self.preserve_scroll = false;
        self.close_actions();
        self.publication_view = None;
        self.preview_selection();

        // Reuse the buffer rather than swapping it in: a keystroke on the hot
        // path must not allocate, and `text` is already owned by the effect.
        self.query.clear();
        self.query.push_str(&text);
        if self.query.is_empty() {
            self.rows = Arc::default();
            self.selected = 0;
            self.navigated = false;
            self.action_panel = None;
        }
        self.dirty = true;
        Some(UiEffect::Query(text))
    }

    /// Fills the query from the selected row (spec 6.3 "query completion").
    /// The completed text is an ordinary edit, so the host schedules a
    /// generation for it exactly as if the user had typed it.
    fn complete(&mut self) -> Option<UiEffect> {
        if self.view().is_none_or(|(_, kind)| kind != ViewKind::RootList) {
            return None;
        }
        let row = self.rows.get(self.selected)?;
        if self.query == row.label {
            return None;
        }
        self.restore_selection = false;
        self.restoration_target = None;
        self.preserve_scroll = false;
        self.navigation_scroll = LogicalScroll::default();

        self.query.clear();
        self.query.push_str(&row.label);
        self.publication_view = None;
        self.preview_selection();
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
        // Status is host feedback, not an unbounded copy of a worker exception.
        // Bound once at admission; paint and hover share the same UTF-8 text.
        let mut status = status;
        let limit = action_panel::MAX_ACTION_TEXT_BYTES;
        if status.len() > limit {
            let mut end = limit - '…'.len_utf8();
            while !status.is_char_boundary(end) {
                end -= 1;
            }
            status.truncate(end);
            status.push('…');
        }
        if selected.status.as_deref() == Some(status.as_str()) {
            return true;
        }

        let mut rows = self.rows.to_vec();
        rows[self.selected].status = Some(status);
        self.rows = rows.into();
        self.dirty = true;
        true
    }

    pub fn command_edit_completed(&mut self, outcome: CommandPreferenceEditOutcome) {
        let (hotkey, value) = match outcome.edit {
            CommandPreferenceEdit::SetAlias(value) => (false, value),
            CommandPreferenceEdit::SetHotkey(value) => (true, value),
            _ => return,
        };
        self.setting_edit_completed(SettingEditOutcome {
            request_id: outcome.request_id,
            key: command_edit_key(&outcome.reference, hotkey),
            value: value.unwrap_or_default(),
            status: outcome.status,
        });
    }
    pub fn setting_edit_completed(&mut self, outcome: SettingEditOutcome) {
        if self
            .setting_edits
            .iter()
            .find(|edit| edit.key == outcome.key)
            .is_some_and(|previous| previous.request_id > outcome.request_id || previous == &outcome)
        {
            return;
        }
        let mut edits = self.setting_edits.to_vec();
        if let Some(previous) = edits.iter_mut().find(|edit| edit.key == outcome.key) {
            *previous = outcome;
        } else {
            edits.push(outcome);
        }
        self.setting_edits = edits.into();
        self.dirty |= self.visible;
    }

    /// Replaces what the settings surface lists.
    ///
    /// Accepted while hidden, unlike everything else here, because these rows
    /// describe the host's configuration rather than a launcher session: the
    /// host publishes them at startup and again after each write, and the
    /// launcher must already know them the first time it is shown.
    /// Active renderer text drafts are keyed by setting identity separately
    /// from this snapshot and survive reloads until acknowledged saved or cancelled.
    pub fn set_settings(&mut self, settings: Vec<SettingRow>) {
        if *self.settings == *settings {
            return;
        }

        self.settings = settings.into();
        // A hidden launcher has no frame to dirty; the next activation carries
        // the new rows anyway.
        self.dirty |= self.visible;
    }

    /// Chooses whether published frames draw the footer's hint line, and
    /// reports whether that changed the model.
    ///
    /// Accepted while hidden for the same reason `set_settings` is: the host
    /// reads the key once at startup, long before the first activation, and a
    /// launcher that only learned the answer on its second showing would come
    /// up with the hints the user turned off.
    pub fn set_show_hints(&mut self, show: bool) -> bool {
        if self.show_hints == show {
            return false;
        }

        self.show_hints = show;
        // A hidden launcher has no frame to dirty; the next activation carries
        // the new value anyway.
        self.dirty |= self.visible;
        true
    }

    /// Chooses whether published frames ask for a rounded window, and reports
    /// whether that changed the model.
    ///
    /// Accepted while hidden for the same reason [`Self::set_show_hints`] is:
    /// the host reads the key once at startup, and the shape has to be right
    /// on the first frame the user ever sees rather than the second.
    pub fn set_rounded_corners(&mut self, rounded: bool) -> bool {
        if self.rounded_corners == rounded {
            return false;
        }

        self.rounded_corners = rounded;
        // A hidden launcher has no frame to dirty; the next activation carries
        // the new value anyway.
        self.dirty |= self.visible;
        true
    }

    pub fn set_preview_ratio(&mut self, ratio: f32) {
        if ratio.is_finite() {
            let ratio = ratio.clamp(0.3, 0.7);
            self.dirty |= self.visible && self.preview.ratio != ratio;
            self.preview.ratio = ratio;
        }
    }

    /// Reconciles selection once after a host event, coalescing publications.
    /// Retire presentation before the caller signals cancellation.
    pub fn preview_selection(&mut self) -> Option<PreviewSelection> {
        let current = if self.view().is_some_and(|(_, kind)| kind == ViewKind::RootList)
            && self.active == self.published
            && self.publication_view == Some(self.view_id)
        {
            self.rows.get(self.selected).and_then(|row| {
                row.source
                    .as_ref()
                    .map(|source| (self.view_id, self.active.unwrap(), &row.target, source))
            })
        } else {
            None
        };
        let unchanged = match (&self.preview_selection, &current) {
            (Some(old), Some((view, generation, target, source))) => {
                old.view == *view
                    && old.generation == *generation
                    && &old.target == *target
                    && &old.source == *source
            }
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            let Some(revision) = self.selection_revision.checked_add(1) else {
                self.preview_selection = None;
                self.preview.link_target = None;
                self.preview.focused = false;
                self.preview.detail = false;
                if !matches!(&self.preview.status, Some(PreviewStatus::Unavailable(reason)) if reason == "Preview selection identity exhausted")
                {
                    self.preview.status = Some(PreviewStatus::Unavailable(
                        "Preview selection identity exhausted".into(),
                    ));
                    self.dirty |= self.visible;
                }
                return None;
            };
            self.selection_revision = revision;
            self.preview_selection = current.map(|(view, generation, target, source)| PreviewSelection {
                view,
                generation,
                target: target.clone(),
                source: source.clone(),
                plugin: source.owner.clone(),
                item: source.item.clone(),
                revision: self.selection_revision,
            });
            self.preview.status = None;
            self.preview.link_target = None;
            self.preview.focused = false;
            self.preview.detail = false;
            self.dirty |= self.visible;
        }
        self.preview_selection.clone()
    }

    pub fn publish_preview(&mut self, selection: &PreviewSelection, status: PreviewStatus) -> bool {
        if self.preview_selection().as_ref() != Some(selection) {
            return false;
        }
        self.preview.status = Some(status);
        self.preview.link_target = None;
        if !self.within_navigation_budget() {
            self.preview.status = Some(PreviewStatus::Unavailable(
                "Shared navigation resource limit reached".into(),
            ));
            self.dirty = true;
            return false;
        }
        self.dirty = true;
        true
    }

    pub fn set_preview_link_target(&mut self, target: PreviewLinkTarget) {
        if target.plugin == target.selection.plugin
            && self.preview_selection().as_ref() == Some(&target.selection)
        {
            self.preview.link_target = Some(target);
            self.dirty = true;
        }
    }

    pub fn selected_preview_row(&self) -> Option<&ResultRow> {
        self.rows.get(self.selected)
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
    ///
    /// An open page closes: both surfaces take the whole result area and both
    /// claim the keyboard, so only one of them can exist. Reports whether a
    /// page was closed, because the plugin holding it still has to be told;
    /// [`UiCommand::OpenSettings`] turns that into
    /// [`UiEffect::ClosePage`], and a host reaching this directly -- the
    /// startup path, where a misconfiguration opens the surface before any
    /// page could exist -- owns the same duty.
    pub fn open_settings(&mut self, focus_key: Option<&str>) -> bool {
        let focus = focus_key.map(str::to_owned);
        if self.settings_open && self.settings_focus == focus {
            return false;
        }

        if !self.settings_open {
            if !self.push_navigation() {
                return false;
            }
            self.change_view();
        }
        let had_page = self.page.take().is_some();
        self.standard = None;
        self.page_reopen = None;
        self.reopening = false;
        self.navigation_unavailable = None;
        self.navigation_notice_token = None;
        self.settings_open = true;
        self.settings_focus = focus;
        self.navigation_title = Arc::from("Settings");
        self.dirty |= self.visible;
        had_page
    }

    /// Hides settings while preserving root query and rows. The host must drain
    /// [`Self::take_query_refresh`] to replace retired work with a fresh search.
    /// An already closed surface is not a change.
    pub fn close_settings(&mut self) {
        if !self.settings_open {
            return;
        }

        self.pop_navigation();
    }

    /// The page the launcher is showing right now, if any (spec 32).
    pub fn page(&self) -> Option<&PageSurface> {
        self.page.as_ref()
    }

    /// Shows `surface` as the launcher's page, reporting whether the model
    /// took it.
    ///
    /// Refused while hidden, like `publish` and unlike `set_settings`: a page
    /// is one plugin's session rather than a description of the host's
    /// configuration, and a surface opened into a launcher nobody can see
    /// would own a keyboard that is not there.
    ///
    /// An open settings surface closes for the reason
    /// [`open_settings`](Self::open_settings) gives. Every accepted call is a
    /// change: opening a page replaces whatever page was open rather than
    /// comparing display lists, because reopening is the host answering a new
    /// action and [`update_page`](Self::update_page) is how the same page
    /// redraws.
    pub fn open_page(&mut self, surface: PageSurface) -> bool {
        if !self.visible {
            return false;
        }

        if !self.reopening && !self.push_navigation() {
            return false;
        }
        self.reopening = false;
        self.change_view();
        self.standard = None;
        self.settings_open = false;
        self.settings_focus = None;
        self.navigation_title = Arc::from(format!("{} — {}", surface.plugin_name, surface.page_id));
        self.page = Some(surface);
        if !self.within_navigation_budget() {
            self.pop_navigation();
            self.set_navigation_notice("Page exceeds the shared navigation resource limit".into());
            return false;
        }
        self.dirty = true;
        true
    }

    /// Replaces the open page's display list, reporting whether the model took
    /// it.
    ///
    /// `stale` is whether the host is still waiting on a request it has
    /// already sent, which the status line shows: the nodes keep standing
    /// across a round trip so a page that redraws on a timer does not flicker,
    /// and the user has to be able to tell the difference.
    ///
    /// Two frames are refused. One that arrives with no page open belongs to a
    /// session the user has already closed. One whose generation is older than
    /// the frame already standing answers a request that has been superseded,
    /// and drawing it would walk the page backwards -- generations are
    /// monotonic per page precisely so that this is decidable here rather than
    /// left to whichever renderer notices.
    pub fn update_page(&mut self, frame: Arc<PageFrame>, stale: bool) -> bool {
        let Some(page) = self.page.as_mut() else {
            return false;
        };
        if frame.generation < page.frame.generation {
            return false;
        }
        // Redrawing the identical display list at the identical staleness is
        // not a change, so a page that answers a poll with the frame it
        // already published costs no repaint.
        if page.stale == stale && Arc::ptr_eq(&page.frame, &frame) {
            return false;
        }

        let changed_rendering_model = page.frame.web.is_some() != frame.web.is_some();
        page.frame = frame;
        page.stale = stale;
        // The plugin has now drawn something, whatever it drew. An empty
        // display list still counts: the page answered.
        page.answered = true;
        // A display list and an engine surface are mutually exclusive. A
        // frame that crosses that boundary retires the old engine identity as
        // well as its pixels, generation floor, and caret; otherwise a late
        // frame from the page's previous rendering model could be admitted if
        // the plugin later crossed back.
        if changed_rendering_model {
            page.web_surface_id = None;
            page.web_frame = None;
            page.web_generation = 0;
            page.web_caret_area = None;
        }
        if !self.within_navigation_budget() {
            self.close_page();
            self.set_navigation_notice("Page exceeds the shared navigation resource limit".into());
            return false;
        }
        self.dirty = true;
        true
    }

    /// Attaches an admitted, launcher-generated engine surface to the current
    /// web declaration.
    ///
    /// Permission checks and process creation stay in the composition root.
    /// The UI accepts only a nonzero identity and only after a validated
    /// `PageFrame.web` declaration is standing. Replacing the identity drops
    /// every pixel and caret belonging to its predecessor.
    pub fn declare_web_surface(&mut self, surface_id: u64) -> bool {
        let Some(page) = self.page.as_mut() else {
            return false;
        };
        if surface_id == 0 || page.frame.web.is_none() {
            return false;
        }
        if page.web_surface_id == Some(surface_id) {
            return false;
        }

        page.web_surface_id = Some(surface_id);
        page.web_frame = None;
        page.web_generation = 0;
        page.web_caret_area = None;
        self.dirty = true;
        true
    }

    /// Accepts the next complete engine frame for the current web surface.
    ///
    /// Surface identity drops pixels still in flight from a closed engine.
    /// Generations increase strictly, so a duplicate or stale delivery cannot
    /// walk the page backwards. Geometry is checked before the renderer sees
    /// it even though the transport already validated the same invariant.
    pub fn update_web_frame(&mut self, frame: Arc<WebSurfaceFrame>) -> bool {
        let Some(page) = self.page.as_mut() else {
            return false;
        };
        if !frame.is_valid()
            || page.frame.web.is_none()
            || page.web_surface_id != Some(frame.surface_id)
            || frame.generation <= page.web_generation
        {
            return false;
        }

        page.web_generation = frame.generation;
        page.web_frame = Some(frame);
        if !self.within_navigation_budget() {
            if let Some(page) = &mut self.page {
                page.web_frame = None;
            }
            self.set_navigation_notice("Web pixels exceed the shared navigation resource limit".into());
            return false;
        }
        self.dirty = true;
        true
    }

    /// Clears the pixels and caret for a live web surface without retiring its
    /// generation floor.
    ///
    /// This is the `Gone`/load-failure path. Keeping the floor means a frame
    /// queued before the failure cannot restore pixels after the host reported
    /// them gone. The accepted generation also records that the surface did
    /// produce a first frame, so clearing it does not put the page back into
    /// its initial loading state.
    pub fn clear_web_frame(&mut self, surface_id: u64) -> bool {
        let Some(page) = self.page.as_mut() else {
            return false;
        };
        if page.web_surface_id != Some(surface_id) {
            return false;
        }
        let changed = page.web_frame.take().is_some() | page.web_caret_area.take().is_some();
        if !changed {
            return false;
        }

        self.dirty = true;
        true
    }

    /// Updates the web engine's page-local caret rectangle.
    ///
    /// Non-finite coordinates, overflowing endpoints, and negative extents are
    /// refused rather than passed to the window system. A duplicate costs no repaint.
    pub fn update_web_caret_area(
        &mut self,
        surface_id: u64,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    ) -> bool {
        let Some(page) = self.page.as_mut() else {
            return false;
        };
        let area = (x, y, width, height);
        if page.web_surface_id != Some(surface_id)
            || ![x, y, width, height].iter().all(|value| value.is_finite())
            || !(x + width).is_finite()
            || !(y + height).is_finite()
            || width < 0.0
            || height < 0.0
            || page.web_caret_area == Some(area)
        {
            return false;
        }

        page.web_caret_area = Some(area);
        self.dirty = true;
        true
    }

    /// Closes the open page, reporting whether there was one.
    ///
    /// The surface goes immediately and unconditionally. Ending the plugin's
    /// side of the session is the host's work, which is why every route into
    /// here reports [`UiEffect::ClosePage`] whatever this answers: the model
    /// forgetting a page is not the plugin learning of it.
    pub fn close_page(&mut self) -> bool {
        if self.page.take().is_none() {
            return false;
        }
        self.pop_navigation();
        true
    }

    fn open_actions(&mut self) {
        if self.action_panel.is_some() {
            return;
        }
        let Some(row) = self.current_action_row() else {
            return;
        };
        match self.standard.as_ref().map_or_else(
            || action_tree(row, &self.command_management),
            |surface| standard_view::action_tree(surface, row),
        ) {
            Ok(tree) => {
                self.action_panel = Some(Arc::new(ActionPanel::new(tree)));
                if !self.within_navigation_budget() {
                    self.action_panel = None;
                    self.set_navigation_notice("Actions exceed the shared navigation resource limit".into());
                    return;
                }
                self.navigated = true;
                self.preview.focused = false;
                self.preview.detail = false;
                self.dirty = true;
            }
            Err(reason) => {
                let _ = self.set_selected_status(reason.to_string());
            }
        }
    }

    fn current_action_row(&self) -> Option<&ResultRow> {
        if self.settings_open
            || self.page.is_some()
            || self.active != self.published
            || self.publication_view != Some(self.view_id)
        {
            return None;
        }
        self.rows.get(self.selected)
    }

    fn action_back(&mut self) {
        if self
            .action_panel
            .as_mut()
            .is_none_or(|panel| Arc::make_mut(panel).back() == ActionPanelNavigation::Close)
        {
            self.close_actions();
        }
        self.dirty = true;
    }

    fn activate_panel(&mut self) -> Option<UiEffect> {
        self.current_action_row()?;
        let panel = self.action_panel.as_mut()?;
        let row = self.rows.iter().find(|row| panel.tree().target().matches(row))?;
        let activation = Arc::make_mut(panel).activation(row).ok().flatten();
        self.dirty = true;
        match activation {
            Some(ActionActivation::Execute(execution)) => self.execute_action(execution),
            _ => None,
        }
    }

    fn dispatch_action(&mut self, target: &ActionTarget, identity: &ActionIdentity) -> Option<UiEffect> {
        self.current_action_row()?;
        let row = self.rows.get(self.selected)?;
        if !target.matches(row) {
            return None;
        }
        if let Some(panel) = &mut self.action_panel {
            if panel.tree().target() != target || panel.invalidation().is_some() {
                return None;
            }
            if let Some(execution) = panel.tree().execution(identity, row).ok().flatten() {
                return self.execute_action(execution);
            }
            if !Arc::make_mut(panel).select(identity) && panel.selected() != Some(identity) {
                return None;
            }
            return self.activate_panel();
        }
        let tree = self
            .standard
            .as_ref()
            .map_or_else(
                || action_tree(row, &self.command_management),
                |surface| standard_view::action_tree(surface, row),
            )
            .ok()?;
        let execution = tree.execution(identity, row).ok().flatten()?;
        self.execute_action(execution)
    }

    fn execute_action(&mut self, execution: ActionExecution) -> Option<UiEffect> {
        self.close_actions();
        match execution {
            ActionExecution::Actionless { owner, item } => {
                if self.standard.is_some() {
                    self.standard_execute(None)
                } else {
                    Some(UiEffect::ExecuteActionless {
                        owner,
                        item,
                        view: self.view_id,
                        generation: self.active?,
                    })
                }
            }
            ActionExecution::Command { reference } => {
                let state = self.current_action_row()?.command_state.as_ref()?;
                self.apply(UiCommand::InvokeCommand {
                    target: ActionTarget::Command(reference),
                    argument: None,
                    registry_revision: state.registry_revision,
                    generation: self.active?,
                    query: self.query.clone(),
                })
            }
            ActionExecution::Plugin { owner, item, action } => {
                if self.standard.is_some() {
                    self.standard_execute(Some(action))
                } else {
                    Some(UiEffect::Execute { owner, item, action })
                }
            }
            ActionExecution::Host(HostAction::OpenSettings) => {
                self.open_settings(None).then_some(UiEffect::ClosePage)
            }
        }
    }

    fn command_row(&self, target: &ActionTarget) -> Option<&ResultRow> {
        if self.visible && self.settings_open {
            return self.command_management.iter().find(|row| &row.target == target);
        }
        let selected = self.current_action_row()?;
        if &selected.target == target {
            return Some(selected);
        }
        let ActionTarget::Result { owner, item } = &selected.target else {
            return None;
        };
        self.command_management.iter().find(|row| {
            &row.target == target
                && row
                    .source
                    .as_ref()
                    .is_some_and(|source| &source.owner == owner && &source.item == item)
        })
    }

    /// Closes the action list. An already closed list is not a change, so this
    /// produces no frame of its own.
    fn close_actions(&mut self) {
        if self.action_panel.is_none() {
            return;
        }

        self.action_panel = None;
        self.dirty = true;
    }

    /// Moves the selection to `target`, clamped into range. Navigation never
    /// wraps, and an empty list has nothing to move.
    fn select(&mut self, target: usize) {
        self.dirty |= self.preserve_scroll;
        self.preserve_scroll = false;
        // Even a clamped navigation command supersedes a pending return choice.
        self.restoration_target = None;
        let Some(last) = self.rows.len().checked_sub(1) else {
            return;
        };

        let target = target.min(last);
        // Only leaving the row retires its action list; a boundary key still
        // reveals the same selected row without closing its actions.
        if target != self.selected {
            self.close_actions();
        }

        self.selected = target;
        self.preview_selection();
        // A boundary key is still an explicit request to reveal this choice.
        self.root_selection_revision = self.root_selection_revision.wrapping_add(1);
        // The only path a person's arrow or page key reaches, so this is where
        // a default becomes a choice.
        self.navigated = true;
        self.dirty = true;
    }

    /// Where the selection lands once `incoming` replaces the current rows.
    ///
    /// Within one generation the selection is anchored by owner and [`ItemId`]:
    /// a republish follows the selected item wherever it moved to and falls
    /// back to the previous index clamped into range when the anchor is gone.
    /// Return searches retain a separate target across partial publications;
    /// an ordinary new query starts at the top.
    fn resolve_selection(&self, incoming: &[ResultRow], generation: Generation) -> usize {
        let Some(last) = incoming.len().checked_sub(1) else {
            return 0;
        };

        if let Some(target) = &self.restoration_target {
            if let Some(index) = incoming.iter().position(|row| target.matches(row)) {
                return index;
            }
        }
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

        let Some(anchor) = self.rows.get(self.selected) else {
            return 0;
        };

        incoming
            .iter()
            .position(|row| row.target == anchor.target)
            .unwrap_or(self.selected.min(last))
    }
}

/// Rebuilds an exact target's panel with the host's authoritative management projection.
/// Reconciliation retains the panel's sticky invalidation and never revives retired actions.
fn reconcile_action_panel(
    panel: &mut Option<Arc<ActionPanel>>,
    rows: &[ResultRow],
    management: &[ResultRow],
) {
    if let Some(panel) = panel {
        if panel.invalidation().is_some() {
            return;
        }
        let replacement = rows
            .iter()
            .find(|row| panel.tree().target().matches(row))
            .and_then(|row| action_tree(row, management).ok());
        let _ = Arc::make_mut(panel).reconcile(replacement);
    }
}

fn action_tree(
    row: &ResultRow,
    management: &[ResultRow],
) -> Result<Arc<ActionTree>, action_panel::ActionPanelRefusal> {
    use action_panel::{ActionEntry, ActionPresentation};
    let host = ActionEntry::Submenu {
        id: "launcher-settings".into(),
        presentation: ActionPresentation {
            label: "Launcher Settings".into(),
            description: "Open launcher settings.".into(),
            icon_reference: Some("host:settings".into()),
            ..Default::default()
        },
        entries: vec![ActionTree::open_settings_entry()],
    };
    ActionTree::from_flat(row, vec![host]).map(|tree| ActionTree::with_management(tree, row, management))
}

/// Opaque authority to retire one host notice, never a newer unrelated notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavigationNoticeToken(u64);
