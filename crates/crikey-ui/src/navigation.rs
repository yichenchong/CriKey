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
    Standard {
        owner: PluginId,
        producer: u64,
        logical_view: std::num::NonZeroU64,
        route: crikey_core::structured_view::RouteId,
        title: String,
        collection: bool,
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
    pub settings_management: Arc<crate::settings_management::Panel>,
    pub inline_arguments: Option<crate::InlineArguments>,
    pub standard_route: bool,
    pub standard_refresh_pending: bool,
    pub standard: Option<Arc<crate::standard_view::StandardSurface>>,
    pub scopes: Arc<[crate::ScopeChoice]>,
    pub selected_scope: Option<crate::ScopeIdentity>,
    pub section_headers: Arc<[crate::SectionHeader]>,
    pub actionless_targets: Arc<[crikey_core::structured_arguments::OwnedItem]>,
    pub title: Arc<str>,
    pub can_go_back: bool,
    pub scroll: LogicalScroll,
    pub restoration: u64,
    /// Existing logical view lifetime; unlike scroll restoration, routine
    /// publications do not retire query/editor focus ownership.
    pub focus_lifetime: Option<crate::ViewId>,
    /// Publication must not pull an offscreen selection back over a restored anchor.
    pub preserve_scroll: bool,
    pub unavailable: Option<String>,
    /// Independent preference and shortcut diagnostics; neither invalidates
    /// the current route or disables its query/editor input.
    pub interface_diagnostics: Arc<[Option<Arc<str>>; 2]>,
    /// Transient picker snapshot from the application's sole history store.
    pub recall: Arc<[String]>,
    pub recall_selected: usize,
}
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub route: NavigationRoute,
    pub rows: Arc<[ResultRow]>,
    pub state: NavigationRestoration,
    pub descriptor: Option<Arc<crikey_core::structured_view::ViewDescriptor>>,
    pub admitted_descriptor: Option<Arc<crikey_core::structured_view::ViewDescriptor>>,
    pub render_standard: bool,
    pub transient_bytes: usize,
    pub transient_pixels: usize,
    pub panel: Option<Arc<crate::ActionPanel>>,
}

// Count immutable result and icon allocations once even if several entries share them.
pub(crate) fn retained_bytes<'a>(entries: impl Iterator<Item = &'a Entry>) -> usize {
    let mut allocations = std::collections::HashSet::new();
    let mut typed = crikey_core::content_accounting::ContentFootprint::new(
        MAX_NAVIGATION_BYTES,
        crikey_core::preview::MAX_RETAINED_PREVIEW_PIXELS,
    );
    let mut renderer_bytes = 0usize;
    let mut renderer_pixels = 0usize;
    // The fixed entry pool includes the current logical entry/suspended slot.
    let mut bytes = MAX_NAVIGATION_ENTRIES * std::mem::size_of::<Entry>();
    for entry in entries {
        renderer_bytes = entry.transient_bytes;
        renderer_pixels = entry.transient_pixels;
        for descriptor in entry.descriptor.iter().chain(entry.admitted_descriptor.iter()) {
            match typed.charge_shared(descriptor, 2 * std::mem::size_of::<usize>(), 0) {
                Ok(true) if typed.include_view(descriptor).is_err() => return usize::MAX,
                Err(_) => return usize::MAX,
                _ => {}
            }
        }
        if let Some(descriptor) = &entry.descriptor {
            let pixels = if entry.render_standard {
                rendered_view_bytes(descriptor)
            } else {
                0
            };
            renderer_bytes = renderer_bytes.saturating_add(pixels);
            renderer_pixels = renderer_pixels.saturating_add(pixels);
        }
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
            let context_bytes = input
                .view_context
                .as_ref()
                .map_or(0, |context| context.route.0.capacity());
            bytes = bytes.saturating_add(context_bytes);
            renderer_bytes = renderer_bytes.saturating_add(context_bytes + key_bytes);
            if let Some((reference, _)) = &input.command {
                bytes += reference.provider_id.0.capacity() + reference.command_id.capacity();
            }
            // Reserve the bounded live editor separately without recording
            // password lengths. Only the current entry has a renderer editor.
            renderer_bytes = renderer_bytes.saturating_add(
                input
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
                    .sum::<usize>(),
            );
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
            NavigationRoute::Standard {
                owner, route, title, ..
            } => owner.0.capacity() + route.0.capacity() + title.capacity(),
            NavigationRoute::CustomPage {
                plugin,
                page_id,
                title,
            } => plugin.0.capacity() + page_id.capacity() + title.capacity(),
        };
        if let Some(panel) = &entry.panel {
            if allocations.insert(Arc::as_ptr(panel) as usize) {
                bytes = bytes.saturating_add(panel.retained_bytes(&mut allocations));
            }
        }
        if !allocations.insert(Arc::as_ptr(&entry.rows).cast::<()>() as usize) {
            continue;
        }
        bytes += std::mem::size_of_val(entry.rows.as_ref()) + 2 * std::mem::size_of::<usize>();
        for row in entry.rows.iter() {
            bytes = bytes.saturating_add(row_retained_bytes(row, &mut allocations));
        }
    }
    if typed.charge(renderer_bytes, renderer_pixels).is_err() {
        return usize::MAX;
    }
    bytes.saturating_add(typed.resources().bytes)
}

fn rendered_view_bytes(descriptor: &crikey_core::structured_view::ViewDescriptor) -> usize {
    use crikey_core::structured_view::StandardView;
    let collection = match &descriptor.view {
        StandardView::List(collection) => Some(collection),
        StandardView::Grid(grid) => Some(&grid.collection),
        StandardView::Detail(detail) => {
            return detail
                .content()
                .image
                .as_ref()
                .map_or(0, |image| image.rgba.len())
        }
        StandardView::Form(_) => None,
    };
    let mut shared = std::collections::HashSet::new();
    collection.map_or(0, |collection| {
        collection
            .items
            .iter()
            .filter_map(|item| item.image.as_ref())
            .filter(|image| shared.insert(Arc::as_ptr(image) as usize))
            .map(|image| image.rgba.len())
            .sum()
    })
}

pub(crate) fn transient_bytes(
    page: Option<&crate::PageSurface>,
    preview: Option<&crate::PreviewStatus>,
) -> usize {
    use std::mem::size_of;
    let mut bytes = 0usize;
    if let Some(page) = page {
        bytes += page.plugin.0.capacity() + page.page_id.capacity() + page.plugin_name.capacity();
        bytes += size_of::<crikey_core::PageFrame>() + 2 * size_of::<usize>() + page.frame.title.capacity();
        bytes += page.frame.nodes.capacity() * size_of::<crikey_core::PageNode>();
        for node in &page.frame.nodes {
            bytes += node.text.capacity() + node.label.capacity();
            if let Some(image) = &node.image {
                bytes += image.rgba.capacity() + image.rgba.len();
            }
        }
        if let Some(web) = &page.frame.web {
            bytes += web.url.capacity();
        }
        if let Some(frame) = &page.web_frame {
            bytes += size_of::<crate::WebSurfaceFrame>() + 4 * size_of::<usize>() + 2 * frame.rgba8.len();
        }
    }
    if let Some(crate::PreviewStatus::Content(prepared)) = preview {
        let content = &prepared.content;
        bytes += size_of::<crikey_core::PreviewContent>() + 4 * size_of::<usize>();
        if let Some(text) = &content.text {
            bytes += match text {
                crikey_core::PreviewText::Plain(text) | crikey_core::PreviewText::Markdown(text) => {
                    text.capacity()
                }
            };
        }
        if let Some(image) = &content.image {
            bytes += image.rgba.capacity() + image.rgba.len();
        }
        bytes += content.values.capacity() * size_of::<crikey_core::PreviewValue>();
        bytes += content
            .values
            .iter()
            .map(|value| value.label.capacity() + value.value.capacity())
            .sum::<usize>();
        bytes += std::mem::size_of_val(prepared.blocks.as_ref());
        for block in prepared.blocks.iter() {
            bytes += block.spans.capacity() * size_of::<crate::PreviewSpan>();
            for span in &block.spans {
                bytes += span.text.capacity() + span.link.as_ref().map_or(0, String::capacity);
            }
        }
    }
    bytes
}

pub(crate) fn row_retained_bytes(
    row: &ResultRow,
    allocations: &mut std::collections::HashSet<usize>,
) -> usize {
    let mut bytes = row.target.retained_bytes()
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
            + state.aliases.capacity() * std::mem::size_of::<String>()
            + state.aliases.iter().map(String::capacity).sum::<usize>();
    }
    bytes += row.icon_reference.as_ref().map_or(0, String::capacity)
        + row.argument_hint.as_ref().map_or(0, String::capacity)
        + row.status.as_ref().map_or(0, String::capacity)
        + row.highlights.capacity() * std::mem::size_of::<(usize, usize)>()
        + row.alternate_actions.capacity() * std::mem::size_of::<crikey_core::Action>();
    for action in row.default_action.iter().chain(row.alternate_actions.iter()) {
        bytes += action.action_id.0.capacity()
            + action.label.capacity()
            + action.description.capacity()
            + action.icon_reference.as_ref().map_or(0, String::capacity)
            + action.applicable_categories.capacity() * std::mem::size_of::<crikey_core::Category>();
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
    bytes
}

pub(crate) fn transient_pixels(
    page: Option<&crate::PageSurface>,
    preview: Option<&crate::PreviewStatus>,
) -> usize {
    let mut pixels = 0;
    if let Some(page) = page {
        pixels += page
            .frame
            .nodes
            .iter()
            .filter_map(|node| node.image.as_ref())
            .map(|image| image.rgba.capacity() + image.rgba.len())
            .sum::<usize>();
        if let Some(frame) = &page.web_frame {
            pixels += 2 * frame.rgba8.len();
        }
    }
    if let Some(crate::PreviewStatus::Content(prepared)) = preview {
        if let Some(image) = &prepared.content.image {
            pixels += image.rgba.capacity() + image.rgba.len();
        }
    }
    pixels
}

/// Maximum UTF-8 size of the atomic navigation override object.
pub const MAX_NAVIGATION_BINDINGS_BYTES: usize = 8192;
pub const MAX_BINDINGS_PER_ACTION: usize = 4;
pub const MAX_NAVIGATION_CHORD_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NavigationStyle {
    #[default]
    Default,
    Emacs,
    Vim,
}

impl NavigationStyle {
    pub fn parse(value: &str) -> Result<Self, NavigationError> {
        match value {
            "default" => Ok(Self::Default),
            "emacs" => Ok(Self::Emacs),
            "vim" => Ok(Self::Vim),
            _ => Err(NavigationError::InvalidStyle),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationAction {
    Next,
    Previous,
    PageDown,
    PageUp,
    Complete,
    Activate,
    Cancel,
    ShowActions,
    TogglePreview,
    Left,
    Right,
}

impl NavigationAction {
    pub const ALL: [Self; 11] = [
        Self::Next,
        Self::Previous,
        Self::PageDown,
        Self::PageUp,
        Self::Complete,
        Self::Activate,
        Self::Cancel,
        Self::ShowActions,
        Self::TogglePreview,
        Self::Left,
        Self::Right,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Next => "next",
            Self::Previous => "previous",
            Self::PageDown => "page-down",
            Self::PageUp => "page-up",
            Self::Complete => "complete",
            Self::Activate => "activate",
            Self::Cancel => "cancel",
            Self::ShowActions => "show-actions",
            Self::TogglePreview => "toggle-preview",
            Self::Left => "left",
            Self::Right => "right",
        }
    }

    fn admits(self, scope: NavigationScope) -> bool {
        use NavigationScope as S;
        match self {
            Self::Cancel => true,
            Self::Next | Self::Previous | Self::PageDown | Self::PageUp | Self::Activate => {
                matches!(scope, S::Results | S::Actions | S::Recall | S::Standard)
            }
            Self::Complete => scope == S::Results,
            Self::ShowActions => matches!(scope, S::Results | S::Actions | S::Recall | S::Standard),
            Self::TogglePreview => matches!(scope, S::Results | S::Standard | S::Preview),
            Self::Left | Self::Right => scope == S::Standard,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationScope {
    Results,
    Actions,
    Recall,
    Standard,
    Preview,
    Settings,
    Page,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationOwner {
    Host,
    /// Launcher query, action filter or query recall; not an arbitrary editor.
    Query,
    Editor,
    Password,
    Popup,
    Document,
    Web,
    ShortcutCapture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavigationContext {
    pub scope: NavigationScope,
    pub owner: NavigationOwner,
    pub composing: bool,
}

impl NavigationContext {
    fn admits(self) -> bool {
        !self.composing && matches!(self.owner, NavigationOwner::Host | NavigationOwner::Query)
    }

    fn admits_chord(self, action: NavigationAction, chord: crikey_platform::Accelerator) -> bool {
        self.admits() && (self.owner != NavigationOwner::Query || !query_editing_chord(action, chord))
    }
}

/// Query TextEdit retains caret/selection, clipboard and deletion ownership.
/// Existing host discovery (Ctrl+K for ShowActions) and explicit Emacs N/P
/// navigation remain reserved; other editor chords cannot be reassigned away.
fn query_editing_chord(action: NavigationAction, chord: crikey_platform::Accelerator) -> bool {
    if matches!(
        chord.key(),
        "Left" | "Right" | "Home" | "End" | "Backspace" | "Delete" | "Insert"
    ) {
        return true;
    }
    let modifiers = chord.modifiers();
    let command = if cfg!(target_os = "macos") {
        modifiers.meta
    } else {
        modifiers.ctrl
    };
    if command && matches!(chord.key(), "A" | "C" | "V" | "X" | "Z" | "Y") {
        return true;
    }
    if modifiers.ctrl {
        if matches!(chord.key(), "H" | "U" | "W")
            || (chord.key() == "K" && action != NavigationAction::ShowActions)
        {
            return true;
        }
        if cfg!(target_os = "macos") && matches!(chord.key(), "A" | "B" | "E" | "F") {
            return true;
        }
    }
    false
}

/// Origin stays explicit: a failed OS replacement can leave an applied chord
/// held even though the desired preference has changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutSource {
    DesiredActivation,
    SavedCommand,
    AppliedActivation,
    AppliedCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavigationConflict {
    pub action: NavigationAction,
    pub chord: crikey_platform::Accelerator,
    pub source: ShortcutSource,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NavigationError {
    #[error("navigation style must be default, emacs or vim")]
    InvalidStyle,
    #[error("navigation bindings exceed their byte, action or chord limit")]
    Limit,
    #[error("navigation bindings must be an object of known, unique actions and chord arrays")]
    InvalidOverrides,
    #[error("invalid navigation chord: use known modifiers and exactly one known key")]
    InvalidChord,
    #[error("navigation bindings cannot capture unmodified text")]
    TextChord,
    #[error("navigation actions {first:?} and {second:?} overlap on {chord}")]
    Overlap {
        first: NavigationAction,
        second: NavigationAction,
        chord: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationBinding {
    pub action: NavigationAction,
    pub chord: crikey_platform::Accelerator,
    /// Cached canonical rendering for dynamic hints; never format per frame.
    pub hint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledNavigation {
    bindings: Vec<NavigationBinding>,
}

struct NavigationOverrides(Vec<(NavigationAction, Vec<String>)>);

impl<'de> serde::Deserialize<'de> for NavigationOverrides {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OverridesVisitor;
        impl<'de> serde::de::Visitor<'de> for OverridesVisitor {
            type Value = NavigationOverrides;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object of unique known actions and bounded chord arrays")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                use serde::de::Error;
                let mut entries = Vec::new();
                while let Some((key, chords)) = map.next_entry::<String, Vec<String>>()? {
                    let action = NavigationAction::ALL
                        .into_iter()
                        .find(|action| action.name() == key)
                        .ok_or_else(|| M::Error::custom("unknown navigation action"))?;
                    if entries.iter().any(|(existing, _)| *existing == action)
                        || chords.len() > MAX_BINDINGS_PER_ACTION
                        || chords
                            .iter()
                            .any(|chord| chord.len() > MAX_NAVIGATION_CHORD_BYTES)
                    {
                        return Err(M::Error::custom("duplicate action or navigation override limit"));
                    }
                    entries.push((action, chords));
                }
                Ok(NavigationOverrides(entries))
            }
        }
        deserializer.deserialize_map(OverridesVisitor)
    }
}

impl NavigationOverrides {
    fn parse(json: &str) -> Result<Self, NavigationError> {
        if json.len() > MAX_NAVIGATION_BINDINGS_BYTES {
            return Err(NavigationError::Limit);
        }
        serde_json::from_str(if json.is_empty() { "{}" } else { json })
            .map_err(|_| NavigationError::InvalidOverrides)
    }
}

/// Profile-independent scalar validation. Cross-action overlap with retained
/// profile defaults is checked only by `CompiledNavigation::compile`.
pub fn validate_bindings_json(json: &str) -> Result<(), NavigationError> {
    let overrides = NavigationOverrides::parse(json)?;
    for (_, chords) in overrides.0 {
        for chord in chords {
            parse_navigation_chord(&chord)?;
        }
    }
    Ok(())
}

fn parse_navigation_chord(text: &str) -> Result<crikey_platform::Accelerator, NavigationError> {
    let chord = crikey_platform::Accelerator::parse(text).map_err(|_| NavigationError::InvalidChord)?;
    let modifiers = chord.modifiers();
    if !modifiers.ctrl
        && !modifiers.alt
        && !modifiers.meta
        && (chord.key().len() == 1 || chord.key() == "Space")
    {
        return Err(NavigationError::TextChord);
    }
    Ok(chord)
}

impl CompiledNavigation {
    /// Compile once on preference changes. Failure is atomic: callers retain
    /// their last-good resolver (or explicitly compile the default on startup).
    /// Each object entry replaces all chords for that action; [] disables it.
    pub fn compile(style: NavigationStyle, overrides_json: &str) -> Result<Self, NavigationError> {
        let overrides = NavigationOverrides::parse(overrides_json)?;
        let mut compiled = Self { bindings: Vec::new() };
        for (action, chord) in [
            (NavigationAction::Next, "Down"),
            (NavigationAction::Previous, "Up"),
            (NavigationAction::PageDown, "PageDown"),
            (NavigationAction::PageUp, "PageUp"),
            (NavigationAction::Complete, "Tab"),
            (NavigationAction::Activate, "Enter"),
            (NavigationAction::Cancel, "Escape"),
            (NavigationAction::ShowActions, "Alt+Enter"),
            (
                NavigationAction::ShowActions,
                if cfg!(target_os = "macos") {
                    "Meta+K"
                } else {
                    "Ctrl+K"
                },
            ),
            (NavigationAction::TogglePreview, "F6"),
            (NavigationAction::Left, "Left"),
            (NavigationAction::Right, "Right"),
        ] {
            compiled.push(action, chord)?;
        }
        let additions: &[(NavigationAction, &str)] = match style {
            NavigationStyle::Default => &[],
            NavigationStyle::Emacs => &[
                (NavigationAction::Next, "Ctrl+N"),
                (NavigationAction::Previous, "Ctrl+P"),
                (NavigationAction::PageDown, "Ctrl+Alt+N"),
                (NavigationAction::PageUp, "Ctrl+Alt+P"),
            ],
            NavigationStyle::Vim => &[
                (NavigationAction::Next, "Alt+J"),
                (NavigationAction::Previous, "Alt+K"),
                (NavigationAction::PageDown, "Alt+D"),
                (NavigationAction::PageUp, "Alt+U"),
            ],
        };
        for &(action, chord) in additions {
            compiled.push(action, chord)?;
        }
        // Remove every replaced action first, permitting atomic swaps.
        compiled
            .bindings
            .retain(|binding| !overrides.0.iter().any(|(action, _)| *action == binding.action));
        for (action, chords) in overrides.0 {
            for chord in chords {
                compiled.push(action, &chord)?;
            }
        }
        for (index, first) in compiled.bindings.iter().enumerate() {
            for second in &compiled.bindings[index + 1..] {
                if first.chord == second.chord
                    && (first.action == second.action
                        || [
                            NavigationScope::Results,
                            NavigationScope::Actions,
                            NavigationScope::Recall,
                            NavigationScope::Standard,
                            NavigationScope::Preview,
                            NavigationScope::Settings,
                            NavigationScope::Page,
                        ]
                        .into_iter()
                        .any(|scope| first.action.admits(scope) && second.action.admits(scope)))
                {
                    return Err(NavigationError::Overlap {
                        first: first.action,
                        second: second.action,
                        chord: first.hint.clone(),
                    });
                }
            }
        }
        Ok(compiled)
    }

    fn push(&mut self, action: NavigationAction, text: &str) -> Result<(), NavigationError> {
        let chord = parse_navigation_chord(text)?;
        self.bindings.push(NavigationBinding {
            action,
            chord,
            hint: chord.canonical(),
        });
        Ok(())
    }

    pub fn bindings(&self) -> &[NavigationBinding] {
        &self.bindings
    }

    /// Derives the runtime map from a retained desired map and actually-held
    /// OS grabs, including cleanup failures after a partial replacement.
    /// Removes every matching binding and hint without changing preferences or
    /// registering shortcuts. Re-derive from the desired map when grabs change.
    pub fn without_chords(&self, unavailable: &[crikey_platform::Accelerator]) -> Self {
        Self {
            bindings: self
                .bindings
                .iter()
                .filter(|binding| !unavailable.contains(&binding.chord))
                .cloned()
                .collect(),
        }
    }

    /// Exact canonical modifier matching: additional modifiers never silently
    /// execute another command. Call in event order, after web/editor ownership.
    /// Admit queued query text before executing Activate; do not pre-scan keys.
    pub fn resolve(
        &self,
        chord: crikey_platform::Accelerator,
        context: NavigationContext,
    ) -> Option<NavigationAction> {
        if !context.admits() {
            return None;
        }
        self.bindings
            .iter()
            .find(|binding| {
                binding.chord == chord
                    && context.admits_chord(binding.action, chord)
                    && binding.action.admits(context.scope)
            })
            .map(|binding| binding.action)
    }

    pub fn hints(&self, action: NavigationAction, context: NavigationContext) -> impl Iterator<Item = &str> {
        self.bindings
            .iter()
            .filter(move |binding| {
                context.admits_chord(binding.action, binding.chord)
                    && binding.action == action
                    && action.admits(context.scope)
            })
            .map(|binding| binding.hint.as_str())
    }

    /// Reuses the same parsed chords as runtime. The caller owns preference
    /// revisions, command identities, OS receipts and registration policy.
    pub fn conflict(
        &self,
        chord: crikey_platform::Accelerator,
        source: ShortcutSource,
    ) -> Option<NavigationConflict> {
        self.bindings
            .iter()
            .find(|binding| binding.chord == chord)
            .map(|binding| NavigationConflict {
                action: binding.action,
                chord,
                source,
            })
    }
}

#[cfg(test)]
#[path = "navigation_binding_tests.rs"]
mod binding_tests;

impl Default for CompiledNavigation {
    fn default() -> Self {
        Self::compile(NavigationStyle::Default, "{}").expect("built-in navigation bindings are valid")
    }
}
