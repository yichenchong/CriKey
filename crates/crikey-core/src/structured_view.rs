//! Bounded standard views, route-local executable definitions and host admission.
use crate::content_accounting::{account_owned, account_schema, ContentFootprint};
use crate::preview::{self, PreviewBlock, PreviewContent, PreviewText};
use crate::result_presentation::{
    account_declarations, account_item, validate_memberships, ItemPresentation, Scope, ScopeId, Section,
    SectionId, MAX_SECTIONS,
};
use crate::structured_arguments::*;
use crate::structured_arguments::{fail, id, owned, text, Check};
use crate::{Action, ActionId, ArgumentPolicy, Category, ExecutionPolicy, Item, PluginId};
use std::{collections::BTreeSet, mem::size_of, num::NonZeroU64, sync::Arc};

pub const STRUCTURED_VIEW_CAPABILITY: &str = "structured_view_v1";
pub const MAX_VIEW_ITEMS: usize = 4096;
/// Individual descriptor ceiling; never an independent session allowance.
pub const MAX_DESCRIPTOR_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ACTIONS_PER_TARGET: usize = 256;
pub const MAX_ACTION_DEPTH: usize = 4;
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RouteId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewRequestMode {
    Open,
    Refresh,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewRefreshReason {
    Explicit,
    Query,
    Selection,
    Scope,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewSessionIdentity {
    pub logical_view: NonZeroU64,
    pub surface: NonZeroU64,
}
/// Exact descriptor binding. Revision is the successful response request ID,
/// never a root query generation or a separately maintained route clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewExecutionContext {
    pub route: RouteId,
    pub session: ViewSessionIdentity,
    pub revision: NonZeroU64,
}
impl ViewExecutionContext {
    pub fn validate(&self) -> Check {
        id(&self.route.0)
    }
    pub fn surface(&self) -> SurfaceIdentity {
        SurfaceIdentity {
            logical_view: self.session.logical_view,
            surface: self.session.surface,
            generation: self.revision,
        }
    }
}
/// Host-owned request state is restore intent, never executable authority.
/// Live typed values do not belong in this request or any retained frame.
pub struct ViewRequest {
    pub mode: ViewRequestMode,
    pub route: RouteId,
    pub session: ViewSessionIdentity,
    pub previous_revision: Option<NonZeroU64>,
    pub state: Option<HostViewState>,
    pub query: String,
    pub scroll: Option<ScrollAnchor>,
    pub reason: ViewRefreshReason,
}
impl std::fmt::Debug for ViewRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewRequest")
            .field("mode", &self.mode)
            .field("route", &self.route)
            .field("session", &self.session)
            .field("previous_revision", &self.previous_revision)
            .field("state", &self.state)
            .field("query", &"[REDACTED]")
            .field("scroll", &self.scroll)
            .field("reason", &self.reason)
            .finish()
    }
}
impl ViewRequest {
    pub fn validate(&self) -> Check {
        id(&self.route.0)?;
        text(&self.query)?;
        match self.mode {
            ViewRequestMode::Open if self.previous_revision.is_some() => {
                return Err(fail(ValidationCode::InvalidReference));
            }
            ViewRequestMode::Refresh if self.previous_revision.is_none() || self.state.is_none() => {
                return Err(fail(ValidationCode::InvalidReference));
            }
            _ => {}
        }
        if let Some(state) = &self.state {
            state.validate()?;
        }
        if let Some(scroll) = &self.scroll {
            owned(&scroll.item)?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone)]
pub struct ViewReply {
    pub context: ViewExecutionContext,
    pub descriptor: Arc<ViewDescriptor>,
}
impl ViewReply {
    /// The caller supplies the correlated transport request ID and admission
    /// constructed by the actual producing endpoint, not by the guest.
    pub fn validate(
        &self,
        request: &ViewRequest,
        response_revision: NonZeroU64,
        admission: &RouteAdmission,
    ) -> Check {
        request.validate()?;
        self.context.validate()?;
        if self.context.route != request.route
            || self.context.session != request.session
            || self.context.revision != response_revision
            || request.previous_revision == Some(response_revision)
            || admission.identity.route != self.context.route
            || admission.surface != self.context.surface()
        {
            return Err(fail(ValidationCode::StaleSurface));
        }
        // Restore intent may name selection, input or anchors removed by this
        // response. Only reconciled host state can authorize current events;
        // validating it here would reject ordinary filtering and refresh.
        self.descriptor.validate(admission)
    }
}

/// Presentation and argument schema for one exact executable definition.
/// None is genuinely actionless, never an alias for the primary action.
#[derive(Debug, Clone)]
pub struct ExecutablePresentation {
    pub action: Option<ActionId>,
    pub schema: Option<Arc<ArgumentSchema>>,
    pub shortcut: Option<String>,
    pub destructive: bool,
}
#[derive(Debug, Clone)]
pub enum ActionMenuEntry {
    Action(ExecutablePresentation),
    Submenu {
        id: String,
        label: String,
        description: String,
        icon_reference: Option<String>,
        entries: Vec<ActionMenuEntry>,
    },
}
#[derive(Debug, Clone)]
pub struct ActionSection {
    pub id: String,
    pub label: String,
    pub entries: Vec<ActionMenuEntry>,
}
/// Route-local executable definitions, admitted into the existing endpoint's
/// exact owner/route/incarnation table before publishing the descriptor.
/// Item and Action retain their existing meanings; IDs alone grant nothing.
#[derive(Debug, Clone)]
pub struct RouteItemDefinition {
    pub item: Item,
    /// Explicit actionless authority. Valid only when the item has no actions.
    /// Worker definitions may grant only Plugin execution.
    pub actionless_policy: Option<ExecutionPolicy>,
    pub primary_action: Option<ActionId>,
    pub action_sections: Vec<ActionSection>,
}
impl RouteItemDefinition {
    pub fn action(&self, action: &ActionId) -> Option<&Action> {
        self.item.actions.iter().find(|a| &a.action_id == action)
    }
    pub fn presentation(&self, action: Option<&ActionId>) -> Option<&ExecutablePresentation> {
        self.action_sections
            .iter()
            .find_map(|s| find_binding(&s.entries, action, 1))
    }
}
fn find_binding<'a>(
    entries: &'a [ActionMenuEntry],
    action: Option<&ActionId>,
    depth: usize,
) -> Option<&'a ExecutablePresentation> {
    if depth > MAX_ACTION_DEPTH {
        return None;
    }
    entries.iter().find_map(|entry| match entry {
        ActionMenuEntry::Action(binding) if binding.action.as_ref() == action => Some(binding),
        ActionMenuEntry::Submenu { entries, .. } => find_binding(entries, action, depth + 1),
        _ => None,
    })
}

#[derive(Debug, Clone)]
pub struct CollectionView {
    /// Provider order, including within each explicit section, is preserved.
    pub items: Arc<[ItemPresentation]>,
    pub sections: Vec<Section>,
    pub scopes: Vec<Scope>,
    /// Initial hints only. They never override admitted host state on updates.
    pub initial_selection: Option<OwnedItem>,
    pub initial_scope: Option<ScopeId>,
}
#[derive(Debug, Clone)]
pub struct GridView {
    pub collection: CollectionView,
    pub columns: u8,
}

/// One preparation pass through the existing safe preview parser, off-paint.
#[derive(Debug, Clone)]
pub struct DetailView {
    content: Arc<PreviewContent>,
    blocks: Arc<[PreviewBlock]>,
    pub target: Option<OwnedItem>,
}
impl DetailView {
    pub fn prepare(content: PreviewContent, target: Option<OwnedItem>) -> Check<Self> {
        content
            .validate()
            .map_err(|_| fail(ValidationCode::InvalidPreview))?;
        let metadata_bytes = content
            .values
            .iter()
            .try_fold(0usize, |n, v| {
                n.checked_add(v.label.len())
                    .and_then(|n| n.checked_add(v.value.len()))
            })
            .ok_or_else(|| fail(ValidationCode::Overflow))?;
        let blocks = if let Some(PreviewText::Markdown(source)) = &content.text {
            preview::parse_preview_markdown_with_budget(
                source,
                preview::MAX_PREVIEW_NODES - content.values.len(),
                preview::MAX_PREVIEW_TEXT_BYTES - metadata_bytes,
            )
            .map_err(|_| fail(ValidationCode::InvalidPreview))?
        } else {
            Vec::new()
        };
        if let Some(target) = &target {
            owned(target)?;
        }
        Ok(Self {
            content: Arc::new(content),
            blocks: blocks.into(),
            target,
        })
    }
    pub fn content(&self) -> &Arc<PreviewContent> {
        &self.content
    }
    pub fn blocks(&self) -> &Arc<[PreviewBlock]> {
        &self.blocks
    }
}
/// Only an executable reference, never a competing copy of its schema.
#[derive(Debug, Clone)]
pub struct FormView {
    pub submit: ExecutableKey,
}
#[derive(Debug, Clone)]
pub enum StandardView {
    List(CollectionView),
    Grid(GridView),
    Detail(DetailView),
    Form(FormView),
}
#[derive(Debug, Clone)]
pub struct ViewDescriptor {
    pub title: String,
    pub definitions: Arc<[RouteItemDefinition]>,
    pub view: StandardView,
}

/// Ephemeral endpoint admission identity, NOT a durable navigation route.
/// Back/resume resolves an owner route anew and allocates fresh admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteIdentity {
    pub owner: PluginId,
    pub route: RouteId,
    pub incarnation: NonZeroU64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceIdentity {
    pub logical_view: NonZeroU64,
    pub surface: NonZeroU64,
    pub generation: NonZeroU64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefinitionSource {
    Worker,
    TrustedHost,
}
/// Constructed by host composition, never decoded from a provider payload.
#[derive(Debug, Clone)]
pub struct RouteAdmission {
    identity: RouteIdentity,
    surface: SurfaceIdentity,
    source: DefinitionSource,
}
impl RouteAdmission {
    pub fn worker(identity: RouteIdentity, surface: SurfaceIdentity) -> Check<Self> {
        Self::new(identity, surface, DefinitionSource::Worker)
    }
    /// Only host-owned definitions may use this constructor. No wire trust bit.
    pub fn trusted_host(identity: RouteIdentity, surface: SurfaceIdentity) -> Check<Self> {
        Self::new(identity, surface, DefinitionSource::TrustedHost)
    }
    fn new(identity: RouteIdentity, surface: SurfaceIdentity, source: DefinitionSource) -> Check<Self> {
        id(&identity.owner.0)?;
        id(&identity.route.0)?;
        Ok(Self {
            identity,
            surface,
            source,
        })
    }
    pub fn identity(&self) -> &RouteIdentity {
        &self.identity
    }
    pub fn surface(&self) -> SurfaceIdentity {
        self.surface
    }
}
/// Projection of the authoritative host model, supplied on EVERY event.
/// This owns no navigation entry, query generation, query text or stack.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostViewState {
    pub selected: Option<OwnedItem>,
    pub scope: Option<ScopeId>,
    /// Focused inline/form executable. Its single attached schema is reused.
    pub input: Option<ExecutableKey>,
}
impl HostViewState {
    pub fn validate(&self) -> Check {
        if let Some(selected) = &self.selected {
            owned(selected)?;
        }
        if let Some(scope) = &self.scope {
            id(&scope.0)?;
        }
        if let Some(input) = &self.input {
            input.validate()?;
            if self.selected.as_ref() != Some(&input.target) {
                return Err(fail(ValidationCode::InvalidReference));
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollAnchor {
    pub item: OwnedItem,
    pub row_offset: u32,
}
/// Smallest action-completion navigation seam. Owner/incarnation come from the
/// admitted completion, not from this payload. Back remains entirely host-owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructuredRouteOutcome {
    Push(RouteId),
    Replace(RouteId),
}
impl StructuredRouteOutcome {
    pub fn validate(&self) -> Check {
        match self {
            Self::Push(route) | Self::Replace(route) => id(&route.0),
        }
    }
}
/// Shared by root and route typed callbacks; navigation is intercepted before
/// publishing the submission-only inline editor result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedExecutionOutcome {
    Submission(SubmissionOutcome),
    Navigate(StructuredRouteOutcome),
}
impl From<SubmissionOutcome> for TypedExecutionOutcome {
    fn from(outcome: SubmissionOutcome) -> Self {
        Self::Submission(outcome)
    }
}
impl TypedExecutionOutcome {
    pub fn validate(&self, schema: Option<&ArgumentSchema>) -> Check {
        match self {
            Self::Submission(outcome) => outcome.validate(schema),
            Self::Navigate(outcome) => outcome.validate(),
        }
    }
}
#[derive(Debug)]
pub struct ViewEvent {
    pub route: RouteIdentity,
    pub surface: SurfaceIdentity,
    pub kind: ViewEventKind,
}
pub enum ViewEventKind {
    Select(OwnedItem),
    /// Discovery intent: schema-bearing actions open input, not execution.
    Activate(ExecutableKey),
    ScopeChanged(Option<ScopeId>),
    QueryChanged(String),
    ScrollChanged(Option<ScrollAnchor>),
    JumpSection(SectionId),
    ArgumentsChanged {
        executable: ExecutableKey,
        values: ArgumentValues,
    },
    Submit(ActionSubmission),
    OpenLink(String),
    Back,
    Cancel,
}
impl std::fmt::Debug for ViewEventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Select(target) => f.debug_tuple("Select").field(target).finish(),
            Self::Activate(target) => f.debug_tuple("Activate").field(target).finish(),
            Self::ScopeChanged(scope) => f.debug_tuple("ScopeChanged").field(scope).finish(),
            Self::QueryChanged(_) => f.write_str("QueryChanged([REDACTED])"),
            Self::ScrollChanged(anchor) => f.debug_tuple("ScrollChanged").field(anchor).finish(),
            Self::JumpSection(section) => f.debug_tuple("JumpSection").field(section).finish(),
            Self::ArgumentsChanged { executable, .. } => f
                .debug_struct("ArgumentsChanged")
                .field("executable", executable)
                .field("values", &"[REDACTED]")
                .finish(),
            Self::Submit(_) => f.write_str("Submit([REDACTED])"),
            Self::OpenLink(_) => f.write_str("OpenLink([REDACTED])"),
            Self::Back => f.write_str("Back"),
            Self::Cancel => f.write_str("Cancel"),
        }
    }
}

fn validate_category(category: &Category) -> Check {
    if let Category::PluginDefined(value) = category {
        id(value)?;
    }
    Ok(())
}
fn validate_definition(definition: &RouteItemDefinition, admission: &RouteAdmission) -> Check {
    let item = &definition.item;
    id(&item.stable_id.0)?;
    id(&item.plugin_id.0)?;
    if item.plugin_id != admission.identity.owner {
        return Err(fail(ValidationCode::WrongOwner));
    }
    validate_category(&item.category)?;
    text(&item.label)?;
    text(&item.description)?;
    text(&item.target)?;
    if let Some(icon) = &item.icon_reference {
        text(icon)?;
    }
    if item.search_terms.len() > preview::MAX_PREVIEW_VALUES
        || item.metadata.len() > preview::MAX_PREVIEW_VALUES
        || item.actions.len() > MAX_ACTIONS_PER_TARGET
        || definition.action_sections.len() > MAX_SECTIONS
    {
        return Err(fail(ValidationCode::TooMany));
    }
    for term in &item.search_terms {
        text(term)?;
    }
    for (key, value) in &item.metadata {
        text(key)?;
        text(value)?;
    }
    let mut defined = BTreeSet::new();
    if let Some(policy) = definition.actionless_policy {
        if !item.actions.is_empty() {
            return Err(fail(ValidationCode::InvalidReference));
        }
        if admission.source == DefinitionSource::Worker && policy != ExecutionPolicy::Plugin {
            return Err(fail(ValidationCode::ForbiddenExecutionPolicy));
        }
        defined.insert(None);
    }
    for action in &item.actions {
        id(&action.action_id.0)?;
        text(&action.label)?;
        text(&action.description)?;
        if !defined.insert(Some(&action.action_id)) {
            return Err(fail(ValidationCode::DuplicateId));
        }
        if admission.source == DefinitionSource::Worker && action.execution_policy != ExecutionPolicy::Plugin
        {
            return Err(fail(ValidationCode::ForbiddenExecutionPolicy));
        }
        if let Some(icon) = &action.icon_reference {
            text(icon)?;
        }
        if action.applicable_categories.len() > preview::MAX_PREVIEW_VALUES {
            return Err(fail(ValidationCode::TooMany));
        }
        for category in &action.applicable_categories {
            validate_category(category)?;
        }
    }
    if definition
        .primary_action
        .as_ref()
        .is_some_and(|a| !defined.contains(&Some(a)))
    {
        return Err(fail(ValidationCode::InvalidReference));
    }
    let mut presented = BTreeSet::new();
    let mut submenu_ids = BTreeSet::new();
    let mut section_ids = BTreeSet::new();
    let mut count = 0usize;
    for section in &definition.action_sections {
        id(&section.id)?;
        text(&section.label)?;
        if !section_ids.insert(&section.id) {
            return Err(fail(ValidationCode::DuplicateId));
        }
        validate_menu(
            &section.entries,
            1,
            &mut count,
            &defined,
            &mut presented,
            &mut submenu_ids,
        )?;
    }
    if defined != presented {
        return Err(fail(ValidationCode::InvalidReference));
    }
    Ok(())
}
fn validate_menu<'a>(
    entries: &'a [ActionMenuEntry],
    depth: usize,
    count: &mut usize,
    defined: &BTreeSet<Option<&'a ActionId>>,
    presented: &mut BTreeSet<Option<&'a ActionId>>,
    submenu_ids: &mut BTreeSet<&'a str>,
) -> Check {
    if depth > MAX_ACTION_DEPTH {
        return Err(fail(ValidationCode::InvalidBounds));
    }
    if entries.len() > MAX_ACTIONS_PER_TARGET {
        return Err(fail(ValidationCode::TooMany));
    }
    for entry in entries {
        *count = count
            .checked_add(1)
            .ok_or_else(|| fail(ValidationCode::Overflow))?;
        if *count > MAX_ACTIONS_PER_TARGET {
            return Err(fail(ValidationCode::TooMany));
        }
        match entry {
            ActionMenuEntry::Action(binding) => {
                if let Some(action) = &binding.action {
                    id(&action.0)?;
                }
                if !defined.contains(&binding.action.as_ref()) {
                    return Err(fail(ValidationCode::InvalidReference));
                }
                if !presented.insert(binding.action.as_ref()) {
                    return Err(fail(ValidationCode::DuplicateId));
                }
                if let Some(shortcut) = &binding.shortcut {
                    text(shortcut)?;
                }
                if let Some(schema) = &binding.schema {
                    schema.validate()?;
                }
            }
            ActionMenuEntry::Submenu {
                id: key,
                label,
                description,
                icon_reference,
                entries,
            } => {
                id(key)?;
                text(label)?;
                text(description)?;
                if !submenu_ids.insert(key) {
                    return Err(fail(ValidationCode::DuplicateId));
                }
                if let Some(icon) = icon_reference {
                    text(icon)?;
                }
                validate_menu(entries, depth + 1, count, defined, presented, submenu_ids)?;
            }
        }
    }
    Ok(())
}
impl CollectionView {
    fn visible(&self, target: &OwnedItem, scope: Option<&ScopeId>) -> bool {
        self.items
            .iter()
            .any(|i| &i.target == target && scope.is_none_or(|s| i.scopes.contains(s)))
    }
    fn validate(&self, descriptor: &ViewDescriptor) -> Check {
        let available: BTreeSet<_> = descriptor
            .definitions
            .iter()
            .map(|d| (&d.item.plugin_id, &d.item.stable_id))
            .collect();
        validate_memberships(&self.sections, &self.scopes, &self.items, |target| {
            available.contains(&(&target.owner, &target.item))
        })?;
        if self
            .initial_scope
            .as_ref()
            .is_some_and(|s| !self.scopes.iter().any(|scope| &scope.id == s))
        {
            return Err(fail(ValidationCode::InvalidSelection));
        }
        if self
            .initial_selection
            .as_ref()
            .is_some_and(|s| !self.visible(s, self.initial_scope.as_ref()))
        {
            return Err(fail(ValidationCode::InvalidSelection));
        }
        Ok(())
    }
}
impl ViewDescriptor {
    pub fn definition(&self, target: &OwnedItem) -> Option<&RouteItemDefinition> {
        self.definitions
            .iter()
            .find(|d| d.item.plugin_id == target.owner && d.item.stable_id == target.item)
    }
    pub fn executable(&self, target: &ExecutableKey) -> Option<(ExecutionPolicy, &ExecutablePresentation)> {
        let definition = self.definition(&target.target)?;
        let policy = match &target.action {
            Some(action) => definition.action(action)?.execution_policy,
            None if definition.item.actions.is_empty() => definition.actionless_policy?,
            None => return None,
        };
        Some((policy, definition.presentation(target.action.as_ref())?))
    }
    pub fn schema_for(&self, target: &ExecutableKey) -> Option<&Arc<ArgumentSchema>> {
        self.executable(target)?.1.schema.as_ref()
    }
    fn collection(&self) -> Option<&CollectionView> {
        match &self.view {
            StandardView::List(c) => Some(c),
            StandardView::Grid(g) => Some(&g.collection),
            _ => None,
        }
    }
    pub fn validate(&self, admission: &RouteAdmission) -> Check {
        text(&self.title)?;
        if self.definitions.len() > MAX_VIEW_ITEMS {
            return Err(fail(ValidationCode::TooMany));
        }
        let mut seen = BTreeSet::new();
        for definition in self.definitions.iter() {
            validate_definition(definition, admission)?;
            if !seen.insert(&definition.item.stable_id) {
                return Err(fail(ValidationCode::DuplicateId));
            }
        }
        match &self.view {
            StandardView::List(c) => c.validate(self)?,
            StandardView::Grid(g) => {
                if g.columns == 0 || g.columns > 16 {
                    return Err(fail(ValidationCode::InvalidBounds));
                }
                g.collection.validate(self)?;
            }
            StandardView::Detail(d) => {
                if d.target.as_ref().is_some_and(|t| self.definition(t).is_none()) {
                    return Err(fail(ValidationCode::InvalidReference));
                }
            }
            StandardView::Form(f) => {
                if self.schema_for(&f.submit).is_none() {
                    return Err(fail(ValidationCode::InvalidReference));
                }
            }
        }
        // Individual content bounds do not replace mixed-stack admission.
        ContentFootprint::new(MAX_DESCRIPTOR_BYTES, preview::MAX_RETAINED_PREVIEW_PIXELS)
            .include_view(self)?;
        Ok(())
    }
    /// Used once when opening a new view; never reapply on provider refresh.
    pub fn initial_state(&self, admission: &RouteAdmission) -> Check<HostViewState> {
        self.validate(admission)?;
        let state = match &self.view {
            StandardView::List(c) => HostViewState {
                selected: c.initial_selection.clone(),
                scope: c.initial_scope.clone(),
                input: None,
            },
            StandardView::Grid(g) => HostViewState {
                selected: g.collection.initial_selection.clone(),
                scope: g.collection.initial_scope.clone(),
                input: None,
            },
            StandardView::Detail(d) => HostViewState {
                selected: d.target.clone(),
                ..Default::default()
            },
            StandardView::Form(f) => HostViewState {
                selected: Some(f.submit.target.clone()),
                input: Some(f.submit.clone()),
                scope: None,
            },
        };
        self.validate_state(admission, &state)?;
        Ok(state)
    }
    pub fn validate_state(&self, admission: &RouteAdmission, state: &HostViewState) -> Check {
        state.validate()?;
        self.validate(admission)?;
        if let Some(c) = self.collection() {
            if state
                .scope
                .as_ref()
                .is_some_and(|s| !c.scopes.iter().any(|v| &v.id == s))
                || state
                    .selected
                    .as_ref()
                    .is_some_and(|s| !c.visible(s, state.scope.as_ref()))
            {
                return Err(fail(ValidationCode::InvalidSelection));
            }
        } else {
            if state.scope.is_some() {
                return Err(fail(ValidationCode::InvalidSelection));
            }
            let target = match &self.view {
                StandardView::Detail(d) => d.target.as_ref(),
                StandardView::Form(f) => Some(&f.submit.target),
                _ => None,
            };
            if state.selected.as_ref() != target {
                return Err(fail(ValidationCode::InvalidSelection));
            }
        }
        if let Some(input) = &state.input {
            if state.selected.as_ref() != Some(&input.target) || self.schema_for(input).is_none() {
                return Err(fail(ValidationCode::InvalidReference));
            }
        }
        if let StandardView::Form(f) = &self.view {
            if state.input.as_ref() != Some(&f.submit) {
                return Err(fail(ValidationCode::InvalidReference));
            }
        }
        Ok(())
    }
    pub fn validate_draft(&self, state: &HostViewState, draft: &NonSecretDraft) -> Check {
        if state.input.as_ref() != Some(&draft.executable) {
            return Err(fail(ValidationCode::InvalidReference));
        }
        let schema = self
            .schema_for(&draft.executable)
            .ok_or_else(|| fail(ValidationCode::InvalidReference))?;
        schema.validate_draft(draft)?;
        Ok(())
    }
    pub fn snapshot_draft(
        &self,
        state: &HostViewState,
        executable: &ExecutableKey,
        values: &ArgumentValues,
    ) -> Result<NonSecretDraft, Vec<FieldError>> {
        if state.input.as_ref() != Some(executable) {
            return Err(vec![FieldError {
                field: None,
                code: ValidationCode::InvalidReference,
            }]);
        }
        let schema = self.schema_for(executable).ok_or_else(|| {
            vec![FieldError {
                field: None,
                code: ValidationCode::InvalidReference,
            }]
        })?;
        Ok(NonSecretDraft {
            executable: executable.clone(),
            values: schema.snapshot_values(values)?,
        })
    }
    fn selected_executable(&self, state: &HostViewState, target: &ExecutableKey) -> Check {
        if state.selected.as_ref() != Some(&target.target) || self.executable(target).is_none() {
            return Err(fail(ValidationCode::InvalidReference));
        }
        if let StandardView::Form(form) = &self.view {
            if &form.submit != target {
                return Err(fail(ValidationCode::InvalidReference));
            }
        }
        Ok(())
    }
    fn validate_submission(&self, state: &HostViewState, submission: &ActionSubmission) -> Check {
        self.selected_executable(state, &submission.executable)?;
        let definition = self
            .definition(&submission.executable.target)
            .ok_or_else(|| fail(ValidationCode::InvalidReference))?;
        match (self.schema_for(&submission.executable), &submission.arguments) {
            (Some(schema), SubmissionArguments::Structured(values)) => {
                if state.input.as_ref() != Some(&submission.executable) {
                    return Err(fail(ValidationCode::InvalidReference));
                }
                values_result(schema.validate_values(values), false)
            }
            (Some(_), _) | (None, SubmissionArguments::Structured(_)) => {
                Err(fail(ValidationCode::InvalidArguments))
            }
            (None, arguments) => {
                let value = match arguments {
                    SubmissionArguments::LegacyText(v) => Some(v.as_str()),
                    _ => None,
                };
                if let Some(value) = value {
                    text(value)?;
                }
                match (definition.item.argument_policy, value) {
                    (ArgumentPolicy::Forbidden, Some(_)) | (ArgumentPolicy::Required, None | Some("")) => {
                        Err(fail(ValidationCode::InvalidArguments))
                    }
                    _ => Ok(()),
                }
            }
        }
    }
    /// The host state and exact owner/route/incarnation/surface are admission
    /// inputs. Provider initial hints are never consulted for event admission.
    pub fn validate_event(
        &self,
        admission: &RouteAdmission,
        state: &HostViewState,
        event: &ViewEvent,
    ) -> Check {
        if event.route != admission.identity || event.surface != admission.surface {
            return Err(fail(ValidationCode::StaleSurface));
        }
        self.validate_state(admission, state)?;
        let collection = self.collection();
        match &event.kind {
            ViewEventKind::Back | ViewEventKind::Cancel => Ok(()),
            ViewEventKind::QueryChanged(query) => {
                collection.ok_or_else(|| fail(ValidationCode::UnsupportedEvent))?;
                text(query)
            }
            ViewEventKind::Select(target) => {
                let c = collection.ok_or_else(|| fail(ValidationCode::UnsupportedEvent))?;
                if c.visible(target, state.scope.as_ref()) {
                    Ok(())
                } else {
                    Err(fail(ValidationCode::InvalidSelection))
                }
            }
            ViewEventKind::Activate(target) => self.selected_executable(state, target),
            ViewEventKind::ScopeChanged(scope) => {
                let c = collection.ok_or_else(|| fail(ValidationCode::UnsupportedEvent))?;
                if scope.as_ref().is_none_or(|s| c.scopes.iter().any(|v| &v.id == s)) {
                    Ok(())
                } else {
                    Err(fail(ValidationCode::InvalidSelection))
                }
            }
            ViewEventKind::ScrollChanged(anchor) => {
                let c = collection.ok_or_else(|| fail(ValidationCode::UnsupportedEvent))?;
                if anchor
                    .as_ref()
                    .is_none_or(|a| c.visible(&a.item, state.scope.as_ref()))
                {
                    Ok(())
                } else {
                    Err(fail(ValidationCode::InvalidSelection))
                }
            }
            ViewEventKind::JumpSection(section) => {
                let c = collection.ok_or_else(|| fail(ValidationCode::UnsupportedEvent))?;
                if c.sections.iter().any(|s| &s.id == section) {
                    Ok(())
                } else {
                    Err(fail(ValidationCode::InvalidReference))
                }
            }
            ViewEventKind::ArgumentsChanged { executable, values } => {
                self.selected_executable(state, executable)?;
                if state.input.as_ref() != Some(executable) {
                    return Err(fail(ValidationCode::InvalidReference));
                }
                let schema = self
                    .schema_for(executable)
                    .ok_or_else(|| fail(ValidationCode::InvalidArguments))?;
                values_result(schema.validate_values(values), true)
            }
            ViewEventKind::Submit(submission) => self.validate_submission(state, submission),
            ViewEventKind::OpenLink(url) => {
                let StandardView::Detail(detail) = &self.view else {
                    return Err(fail(ValidationCode::UnsupportedEvent));
                };
                preview::validate_preview_link(url).map_err(|_| fail(ValidationCode::InvalidReference))?;
                let metadata = detail
                    .content
                    .values
                    .iter()
                    .any(|v| v.kind == preview::PreviewValueKind::Link && &v.value == url);
                let block = detail
                    .blocks
                    .iter()
                    .flat_map(|b| &b.spans)
                    .any(|s| s.link.as_ref() == Some(url));
                if metadata || block {
                    Ok(())
                } else {
                    Err(fail(ValidationCode::InvalidReference))
                }
            }
        }
    }
}
pub(crate) fn values_result(result: Result<(), Vec<FieldError>>, draft: bool) -> Check {
    if let Err(errors) = result {
        if let Some(error) = errors
            .into_iter()
            .find(|e| !draft || !matches!(e.code, ValidationCode::Required | ValidationCode::TooShort))
        {
            return Err(ValidationError {
                field: error.field,
                code: error.code,
            });
        }
    }
    Ok(())
}
fn account_category(category: &Category, budget: &mut ContentFootprint<'_>) -> Check {
    if let Category::PluginDefined(v) = category {
        budget.string(v)?;
    }
    Ok(())
}
fn account_menu<'a>(
    entries: &'a Vec<ActionMenuEntry>,
    budget: &mut ContentFootprint<'a>,
    depth: usize,
) -> Check {
    if depth > MAX_ACTION_DEPTH {
        return Err(fail(ValidationCode::InvalidBounds));
    }
    budget.array::<ActionMenuEntry>(entries.capacity())?;
    for entry in entries {
        match entry {
            ActionMenuEntry::Action(binding) => {
                if let Some(action) = &binding.action {
                    budget.string(&action.0)?;
                }
                if let Some(v) = &binding.shortcut {
                    budget.string(v)?;
                }
                if let Some(schema) = &binding.schema {
                    if budget.begin_shared(schema, size_of::<ArgumentSchema>())? {
                        account_schema(schema, budget)?;
                    }
                }
            }
            ActionMenuEntry::Submenu {
                id,
                label,
                description,
                icon_reference,
                entries,
            } => {
                budget.string(id)?;
                budget.string(label)?;
                budget.string(description)?;
                if let Some(v) = icon_reference {
                    budget.string(v)?;
                }
                account_menu(entries, budget, depth + 1)?;
            }
        }
    }
    Ok(())
}
fn account_definition<'a>(definition: &'a RouteItemDefinition, budget: &mut ContentFootprint<'a>) -> Check {
    let item = &definition.item;
    budget.string(&item.stable_id.0)?;
    budget.string(&item.plugin_id.0)?;
    account_category(&item.category, budget)?;
    budget.string(&item.label)?;
    budget.string(&item.description)?;
    budget.string(&item.target)?;
    if let Some(v) = &item.icon_reference {
        budget.string(v)?;
    }
    budget.array::<String>(item.search_terms.capacity())?;
    for term in &item.search_terms {
        budget.string(term)?;
    }
    for (key, value) in &item.metadata {
        budget.map_record::<String, String>()?;
        budget.string(key)?;
        budget.string(value)?;
    }
    budget.array::<Action>(item.actions.capacity())?;
    for action in &item.actions {
        budget.string(&action.action_id.0)?;
        budget.string(&action.label)?;
        budget.string(&action.description)?;
        if let Some(v) = &action.icon_reference {
            budget.string(v)?;
        }
        budget.array::<Category>(action.applicable_categories.capacity())?;
        for category in &action.applicable_categories {
            account_category(category, budget)?;
        }
    }
    if let Some(v) = &definition.primary_action {
        budget.string(&v.0)?;
    }
    budget.array::<ActionSection>(definition.action_sections.capacity())?;
    for section in &definition.action_sections {
        budget.string(&section.id)?;
        budget.string(&section.label)?;
        account_menu(&section.entries, budget, 1)?;
    }
    Ok(())
}
fn account_collection<'a>(collection: &'a CollectionView, budget: &mut ContentFootprint<'a>) -> Check {
    account_declarations(&collection.sections, &collection.scopes, budget)?;
    if let Some(v) = &collection.initial_selection {
        account_owned(v, budget)?;
    }
    if let Some(v) = &collection.initial_scope {
        budget.string(&v.0)?;
    }
    let inline = collection
        .items
        .len()
        .checked_mul(size_of::<ItemPresentation>())
        .ok_or_else(|| fail(ValidationCode::Overflow))?;
    if budget.begin_shared(&collection.items, inline)? {
        for item in collection.items.iter() {
            account_item(item, budget)?;
        }
    }
    Ok(())
}
impl<'a> ContentFootprint<'a> {
    pub fn include_view(&mut self, view: &'a ViewDescriptor) -> Check {
        account_view(view, self)
    }
}
fn account_view<'a>(view: &'a ViewDescriptor, budget: &mut ContentFootprint<'a>) -> Check {
    budget.bytes(size_of::<ViewDescriptor>())?;
    budget.string(&view.title)?;
    let inline = view
        .definitions
        .len()
        .checked_mul(size_of::<RouteItemDefinition>())
        .ok_or_else(|| fail(ValidationCode::Overflow))?;
    if budget.begin_shared(&view.definitions, inline)? {
        for definition in view.definitions.iter() {
            account_definition(definition, budget)?;
        }
    }
    match &view.view {
        StandardView::List(c) => account_collection(c, budget)?,
        StandardView::Grid(g) => account_collection(&g.collection, budget)?,
        StandardView::Form(f) => {
            account_owned(&f.submit.target, budget)?;
            if let Some(action) = &f.submit.action {
                budget.string(&action.0)?;
            }
        }
        StandardView::Detail(d) => {
            if let Some(target) = &d.target {
                account_owned(target, budget)?;
            }
            if budget.begin_shared(&d.content, size_of::<PreviewContent>())? {
                if let Some(t) = &d.content.text {
                    match t {
                        PreviewText::Plain(v) | PreviewText::Markdown(v) => budget.string(v)?,
                    }
                }
                budget.array::<preview::PreviewValue>(d.content.values.capacity())?;
                for v in &d.content.values {
                    budget.string(&v.label)?;
                    budget.string(&v.value)?;
                }
                if let Some(image) = &d.content.image {
                    budget.charge(image.rgba.capacity(), image.rgba.capacity())?;
                }
            }
            let inline = d
                .blocks
                .len()
                .checked_mul(size_of::<PreviewBlock>())
                .ok_or_else(|| fail(ValidationCode::Overflow))?;
            if budget.begin_shared(&d.blocks, inline)? {
                for block in d.blocks.iter() {
                    budget.array::<preview::PreviewSpan>(block.spans.capacity())?;
                    for span in &block.spans {
                        budget.string(&span.text)?;
                        if let Some(v) = &span.link {
                            budget.string(v)?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ItemId;
    use std::collections::BTreeMap;
    fn target(name: &str) -> OwnedItem {
        OwnedItem {
            owner: PluginId("owner".into()),
            item: ItemId(name.into()),
        }
    }
    fn executable(name: &str) -> ExecutableKey {
        ExecutableKey {
            target: target(name),
            action: Some(ActionId("run".into())),
        }
    }
    fn admission(surface: u64) -> RouteAdmission {
        RouteAdmission::worker(
            RouteIdentity {
                owner: PluginId("owner".into()),
                route: RouteId("route".into()),
                incarnation: NonZeroU64::new(1).unwrap(),
            },
            SurfaceIdentity {
                logical_view: NonZeroU64::new(1).unwrap(),
                surface: NonZeroU64::new(surface).unwrap(),
                generation: NonZeroU64::new(1).unwrap(),
            },
        )
        .unwrap()
    }
    fn event(kind: ViewEventKind) -> ViewEvent {
        let admission = admission(1);
        ViewEvent {
            route: admission.identity.clone(),
            surface: admission.surface,
            kind,
        }
    }
    fn field(name: &str, kind: FieldKind) -> ArgumentField {
        ArgumentField {
            id: FieldId(name.into()),
            label: name.into(),
            required: true,
            placeholder: None,
            validation_message: Some("Check input".into()),
            kind,
        }
    }
    fn schema() -> ArgumentSchema {
        ArgumentSchema {
            fields: vec![
                field(
                    "text",
                    FieldKind::Text {
                        min_bytes: 2,
                        max_bytes: MAX_TEXT_BYTES,
                        default: Some("ok".into()),
                        multiline: false,
                    },
                ),
                field(
                    "password",
                    FieldKind::Password {
                        min_bytes: 1,
                        max_bytes: MAX_TEXT_BYTES,
                    },
                ),
                field(
                    "choice",
                    FieldKind::Dropdown {
                        options: vec![DropdownOption {
                            id: OptionId("one".into()),
                            label: "One".into(),
                        }],
                        default: Some(OptionId("one".into())),
                    },
                ),
            ],
        }
    }
    fn values() -> ArgumentValues {
        let mut values = schema().default_values().unwrap();
        values.0.insert(
            FieldId("password".into()),
            StructuredValue::Password(Password::new("secret-unique".into()).unwrap()),
        );
        values
    }
    fn definition(name: &str, schema: Option<Arc<ArgumentSchema>>) -> RouteItemDefinition {
        RouteItemDefinition {
            item: Item {
                stable_id: ItemId(name.into()),
                plugin_id: PluginId("owner".into()),
                category: Category::Application,
                label: name.into(),
                description: String::new(),
                target: name.into(),
                search_terms: vec![],
                icon_reference: None,
                argument_policy: ArgumentPolicy::Optional,
                hit_policy: crate::HitPolicy::Ignored,
                score_hint: 0,
                metadata: BTreeMap::new(),
                actions: vec![Action {
                    action_id: ActionId("run".into()),
                    label: "Run".into(),
                    description: String::new(),
                    applicable_categories: vec![],
                    icon_reference: None,
                    execution_policy: ExecutionPolicy::Plugin,
                }],
            },
            actionless_policy: None,
            primary_action: Some(ActionId("run".into())),
            action_sections: vec![ActionSection {
                id: "main".into(),
                label: "Actions".into(),
                entries: vec![ActionMenuEntry::Action(ExecutablePresentation {
                    action: Some(ActionId("run".into())),
                    schema,
                    shortcut: None,
                    destructive: false,
                })],
            }],
        }
    }
    fn list(
        schema: Option<Arc<ArgumentSchema>>,
        image: Option<Arc<preview::PreviewImage>>,
    ) -> ViewDescriptor {
        ViewDescriptor {
            title: "List".into(),
            definitions: vec![definition("a", schema.clone()), definition("b", schema)].into(),
            view: StandardView::List(CollectionView {
                items: vec![
                    ItemPresentation {
                        target: target("a"),
                        section: Some(SectionId("section".into())),
                        scopes: vec![ScopeId("A".into())],
                        image: image.clone(),
                    },
                    ItemPresentation {
                        target: target("b"),
                        section: Some(SectionId("section".into())),
                        scopes: vec![ScopeId("B".into())],
                        image,
                    },
                ]
                .into(),
                sections: vec![Section {
                    id: SectionId("section".into()),
                    label: "Section".into(),
                }],
                scopes: vec![
                    Scope {
                        id: ScopeId("A".into()),
                        label: "A".into(),
                    },
                    Scope {
                        id: ScopeId("B".into()),
                        label: "B".into(),
                    },
                ],
                initial_selection: Some(target("a")),
                initial_scope: None,
            }),
        }
    }
    #[test]
    fn host_scope_wins_over_mismatched_descriptor_hints() {
        let view = list(None, None);
        let admission = admission(1);
        let state = HostViewState {
            selected: Some(target("a")),
            scope: Some(ScopeId("A".into())),
            input: None,
        };
        view.validate_state(&admission, &state).unwrap();
        assert_eq!(
            view.validate_event(&admission, &state, &event(ViewEventKind::Select(target("b"))))
                .unwrap_err()
                .code,
            ValidationCode::InvalidSelection
        );
        assert_eq!(
            view.validate_event(
                &admission,
                &state,
                &event(ViewEventKind::Activate(executable("b")))
            )
            .unwrap_err()
            .code,
            ValidationCode::InvalidReference
        );
        let mut refreshed = view.clone();
        let StandardView::List(c) = &mut refreshed.view else {
            panic!()
        };
        c.initial_selection = Some(target("b"));
        c.initial_scope = Some(ScopeId("B".into()));
        refreshed
            .validate_event(
                &admission,
                &state,
                &event(ViewEventKind::Activate(executable("a"))),
            )
            .unwrap();
        assert_eq!(
            refreshed
                .validate_event(&admission, &state, &event(ViewEventKind::Select(target("b"))))
                .unwrap_err()
                .code,
            ValidationCode::InvalidSelection
        );
    }
    #[test]
    fn selection_is_host_authoritative_even_without_scope() {
        let view = list(None, None);
        let admission = admission(1);
        let state = view.initial_state(&admission).unwrap();
        assert_eq!(
            view.validate_event(
                &admission,
                &state,
                &event(ViewEventKind::Activate(executable("b")))
            )
            .unwrap_err()
            .code,
            ValidationCode::InvalidReference
        );
        view.validate_event(&admission, &state, &event(ViewEventKind::Select(target("b"))))
            .unwrap();
    }
    #[test]
    fn exact_route_incarnation_surface_and_executable_definitions_are_required() {
        StructuredRouteOutcome::Push(RouteId("detail".into()))
            .validate()
            .unwrap();
        StructuredRouteOutcome::Replace(RouteId("form".into()))
            .validate()
            .unwrap();
        assert_eq!(
            StructuredRouteOutcome::Push(RouteId(String::new()))
                .validate()
                .unwrap_err()
                .code,
            ValidationCode::EmptyId
        );
        let view = list(None, None);
        let admission = admission(1);
        let state = view.initial_state(&admission).unwrap();
        let mut e = event(ViewEventKind::Activate(executable("a")));
        e.route.route = RouteId("other".into());
        assert_eq!(
            view.validate_event(&admission, &state, &e).unwrap_err().code,
            ValidationCode::StaleSurface
        );
        e.route = admission.identity.clone();
        e.route.incarnation = NonZeroU64::new(2).unwrap();
        assert_eq!(
            view.validate_event(&admission, &state, &e).unwrap_err().code,
            ValidationCode::StaleSurface
        );
        e.route = admission.identity.clone();
        e.surface.surface = NonZeroU64::new(2).unwrap();
        assert_eq!(
            view.validate_event(&admission, &state, &e).unwrap_err().code,
            ValidationCode::StaleSurface
        );
        let mut missing = view.clone();
        missing.definitions = Arc::from([]);
        assert_eq!(
            missing.validate(&admission).unwrap_err().code,
            ValidationCode::InvalidReference
        );
    }
    #[test]
    fn worker_definitions_cannot_grant_host_execution() {
        let mut view = list(None, None);
        let mut definitions = view.definitions.to_vec();
        definitions[0].item.actions[0].execution_policy = ExecutionPolicy::HostMediated;
        view.definitions = definitions.into();
        let admission = admission(1);
        assert_eq!(
            view.validate(&admission).unwrap_err().code,
            ValidationCode::ForbiddenExecutionPolicy
        );
        let trusted = RouteAdmission::trusted_host(admission.identity.clone(), admission.surface).unwrap();
        view.validate(&trusted).unwrap();
    }
    #[test]
    fn action_quota_is_per_target_not_across_all_rows() {
        let definitions: Vec<_> = (0..257).map(|n| definition(&n.to_string(), None)).collect();
        let items: Vec<_> = (0..257)
            .map(|n| ItemPresentation {
                target: target(&n.to_string()),
                section: None,
                scopes: vec![],
                image: None,
            })
            .collect();
        let view = ViewDescriptor {
            title: "Many actionable rows".into(),
            definitions: definitions.into(),
            view: StandardView::List(CollectionView {
                items: items.into(),
                sections: vec![],
                scopes: vec![],
                initial_selection: None,
                initial_scope: None,
            }),
        };
        view.validate(&admission(1)).unwrap();
        let mut definition = definition("too-many", None);
        definition.item.actions = (0..=MAX_ACTIONS_PER_TARGET)
            .map(|n| Action {
                action_id: ActionId(n.to_string()),
                label: "Action".into(),
                description: String::new(),
                applicable_categories: vec![],
                icon_reference: None,
                execution_policy: ExecutionPolicy::Plugin,
            })
            .collect();
        assert_eq!(
            validate_definition(&definition, &admission(1)).unwrap_err().code,
            ValidationCode::TooMany
        );
    }
    #[test]
    fn submenu_nodes_share_selected_target_quota_and_depth() {
        let mut d = definition("a", None);
        let leaf = d.action_sections[0].entries.pop().unwrap();
        let mut entry = leaf;
        for depth in 0..MAX_ACTION_DEPTH {
            entry = ActionMenuEntry::Submenu {
                id: depth.to_string(),
                label: "Submenu".into(),
                description: String::new(),
                icon_reference: None,
                entries: vec![entry],
            };
        }
        d.action_sections[0].entries.push(entry);
        assert_eq!(
            validate_definition(&d, &admission(1)).unwrap_err().code,
            ValidationCode::InvalidBounds
        );
        let mut d = definition("a", None);
        for n in 0..MAX_ACTIONS_PER_TARGET {
            d.action_sections[0].entries.push(ActionMenuEntry::Submenu {
                id: n.to_string(),
                label: "Submenu".into(),
                description: String::new(),
                icon_reference: None,
                entries: vec![],
            });
        }
        assert_eq!(
            validate_definition(&d, &admission(1)).unwrap_err().code,
            ValidationCode::TooMany
        );
    }
    #[test]
    fn form_and_inline_inputs_resolve_identical_executable_schema() {
        let shared = Arc::new(schema());
        let inline = list(Some(shared.clone()), None);
        let admission = admission(1);
        let mut state = inline.initial_state(&admission).unwrap();
        state.input = Some(executable("a"));
        let values = values();
        let snapshot = inline.snapshot_draft(&state, &executable("a"), &values).unwrap();
        assert!(!snapshot.values.contains_key(&FieldId("password".into())));
        inline.validate_draft(&state, &snapshot).unwrap();
        let form = ViewDescriptor {
            title: "Form".into(),
            definitions: inline.definitions.clone(),
            view: StandardView::Form(FormView {
                submit: executable("a"),
            }),
        };
        assert!(Arc::ptr_eq(
            inline.schema_for(&executable("a")).unwrap(),
            form.schema_for(&executable("a")).unwrap()
        ));
        let form_state = form.initial_state(&admission).unwrap();
        let submit = event(ViewEventKind::Submit(ActionSubmission {
            executable: executable("a"),
            arguments: SubmissionArguments::Structured(values),
        }));
        inline.validate_event(&admission, &state, &submit).unwrap();
        form.validate_event(&admission, &form_state, &submit).unwrap();
        let legacy = event(ViewEventKind::Submit(ActionSubmission {
            executable: executable("a"),
            arguments: SubmissionArguments::LegacyText("not-typed".into()),
        }));
        assert_eq!(
            inline
                .validate_event(&admission, &state, &legacy)
                .unwrap_err()
                .code,
            ValidationCode::InvalidArguments
        );
    }
    #[test]
    fn incomplete_drafts_are_allowed_but_submit_and_password_snapshots_are_checked() {
        let view = list(Some(Arc::new(schema())), None);
        let admission = admission(1);
        let mut state = view.initial_state(&admission).unwrap();
        state.input = Some(executable("a"));
        let draft = event(ViewEventKind::ArgumentsChanged {
            executable: executable("a"),
            values: ArgumentValues::default(),
        });
        view.validate_event(&admission, &state, &draft).unwrap();
        let submit = event(ViewEventKind::Submit(ActionSubmission {
            executable: executable("a"),
            arguments: SubmissionArguments::Structured(ArgumentValues::default()),
        }));
        assert_eq!(
            view.validate_event(&admission, &state, &submit).unwrap_err().code,
            ValidationCode::Required
        );
        let mut snapshot = NonSecretDraft {
            executable: executable("a"),
            values: BTreeMap::new(),
        };
        snapshot.values.insert(
            FieldId("password".into()),
            NonSecretValue::Text("forbidden".into()),
        );
        assert_eq!(
            view.validate_draft(&state, &snapshot).unwrap_err().code,
            ValidationCode::WrongType
        );
    }
    #[test]
    fn legacy_arguments_remain_separate_and_respect_existing_policy() {
        let view = list(None, None);
        let admission = admission(1);
        let state = view.initial_state(&admission).unwrap();
        let legacy = event(ViewEventKind::Submit(ActionSubmission {
            executable: executable("a"),
            arguments: SubmissionArguments::LegacyText("legacy".into()),
        }));
        view.validate_event(&admission, &state, &legacy).unwrap();
        let typed = event(ViewEventKind::Submit(ActionSubmission {
            executable: executable("a"),
            arguments: SubmissionArguments::Structured(ArgumentValues::default()),
        }));
        assert_eq!(
            view.validate_event(&admission, &state, &typed).unwrap_err().code,
            ValidationCode::InvalidArguments
        );
    }
    #[test]
    fn detail_reuses_preview_parser_and_link_membership() {
        let detail = DetailView::prepare(
            PreviewContent {
                text: Some(PreviewText::Markdown("[Docs](https://example.test/)".into())),
                ..Default::default()
            },
            Some(target("a")),
        )
        .unwrap();
        assert!(!detail.blocks().is_empty());
        let view = ViewDescriptor {
            title: "Detail".into(),
            definitions: vec![definition("a", None)].into(),
            view: StandardView::Detail(detail),
        };
        let admission = admission(1);
        let state = view.initial_state(&admission).unwrap();
        view.validate_event(
            &admission,
            &state,
            &event(ViewEventKind::OpenLink("https://example.test/".into())),
        )
        .unwrap();
        assert!(view
            .validate_event(
                &admission,
                &state,
                &event(ViewEventKind::OpenLink("https://other.test/".into()))
            )
            .is_err());
        let unsafe_content = PreviewContent {
            text: Some(PreviewText::Markdown("<script>bad</script>".into())),
            ..Default::default()
        };
        assert_eq!(
            DetailView::prepare(unsafe_content, None).unwrap_err().code,
            ValidationCode::InvalidPreview
        );
    }
    #[test]
    fn mixed_host_content_and_shared_pixels_use_one_caller_owned_budget() {
        let image = Arc::new(preview::PreviewImage {
            width: 512,
            height: 512,
            rgba: vec![0; 1024 * 1024],
        });
        let view = list(None, Some(image.clone()));
        let second = view.clone();
        let mut budget = ContentFootprint::new(MAX_DESCRIPTOR_BYTES, preview::MAX_RETAINED_PREVIEW_PIXELS);
        budget.charge(128 * 1024, 0).unwrap(); // Existing root/settings content.
        budget.charge(1024 * 1024, 1024 * 1024).unwrap(); // Renderer copy.
        budget.include_view(&view).unwrap();
        budget.include_view(&second).unwrap();
        assert_eq!(budget.resources().pixels, 2 * 1024 * 1024);
        // Shared-allocation seam used by a non-structured owner sees the same Arc.
        assert!(!budget.charge_shared(&image, 1024 * 1024, 1024 * 1024).unwrap());
        assert_eq!(
            budget.charge(3 * 1024 * 1024, 3 * 1024 * 1024).unwrap_err().code,
            ValidationCode::RetainedPixels
        );
        let mut mixed = ContentFootprint::new(100, 0);
        mixed.charge(100, 0).unwrap();
        assert_eq!(
            mixed.include_view(&view).unwrap_err().code,
            ValidationCode::RetainedBytes
        );
    }
    #[test]
    fn capacity_checked_arithmetic_and_live_secret_retention_are_counted() {
        let mut view = list(None, None);
        view.title = String::with_capacity(MAX_DESCRIPTOR_BYTES + 1);
        assert_eq!(
            view.validate(&admission(1)).unwrap_err().code,
            ValidationCode::RetainedBytes
        );
        let mut budget = ContentFootprint::new(usize::MAX, usize::MAX);
        assert_eq!(
            budget.array::<ItemPresentation>(usize::MAX).unwrap_err().code,
            ValidationCode::Overflow
        );
        budget.charge(usize::MAX, 0).unwrap();
        assert_eq!(budget.charge(1, 0).unwrap_err().code, ValidationCode::Overflow);
        let mut budget = ContentFootprint::new(MAX_DESCRIPTOR_BYTES, 0);
        let values = values();
        budget.include_values(&values).unwrap();
        assert!(budget.resources().bytes >= "secret-unique".len());
    }
    fn actionless_definition(schema: Option<Arc<ArgumentSchema>>) -> RouteItemDefinition {
        let mut d = definition("a", schema);
        d.item.actions.clear();
        d.primary_action = None;
        d.actionless_policy = Some(ExecutionPolicy::Plugin);
        let ActionMenuEntry::Action(binding) = &mut d.action_sections[0].entries[0] else {
            panic!()
        };
        binding.action = None;
        d
    }
    #[test]
    fn actionless_routes_activate_submit_and_resolve_the_same_form_schema() {
        let shared = Arc::new(schema());
        let key = ExecutableKey {
            target: target("a"),
            action: None,
        };
        let mut inline = list(Some(shared.clone()), None);
        inline.definitions = vec![actionless_definition(Some(shared.clone())), definition("b", None)].into();
        let admission = admission(1);
        let mut state = inline.initial_state(&admission).unwrap();
        inline
            .validate_event(&admission, &state, &event(ViewEventKind::Activate(key.clone())))
            .unwrap();
        state.input = Some(key.clone());
        assert!(Arc::ptr_eq(inline.schema_for(&key).unwrap(), &shared));
        let form = ViewDescriptor {
            title: "Actionless form".into(),
            definitions: inline.definitions.clone(),
            view: StandardView::Form(FormView { submit: key.clone() }),
        };
        let form_state = form.initial_state(&admission).unwrap();
        let submission = event(ViewEventKind::Submit(ActionSubmission {
            executable: key.clone(),
            arguments: SubmissionArguments::Structured(values()),
        }));
        inline.validate_event(&admission, &state, &submission).unwrap();
        form.validate_event(&admission, &form_state, &submission).unwrap();
        assert!(inline.schema_for(&executable("a")).is_none());
        let mut stale = event(ViewEventKind::Activate(key.clone()));
        stale.route.incarnation = NonZeroU64::new(2).unwrap();
        assert_eq!(
            inline
                .validate_event(&admission, &state, &stale)
                .unwrap_err()
                .code,
            ValidationCode::StaleSurface
        );
        state.selected = Some(target("b"));
        state.input = None;
        assert_eq!(
            inline
                .validate_event(&admission, &state, &event(ViewEventKind::Activate(key)))
                .unwrap_err()
                .code,
            ValidationCode::InvalidReference
        );
    }
    #[test]
    fn actionless_routes_require_explicit_policy_and_matching_binding() {
        let admission = admission(1);
        let mut d = actionless_definition(None);
        validate_definition(&d, &admission).unwrap();
        d.actionless_policy = None;
        assert_eq!(
            validate_definition(&d, &admission).unwrap_err().code,
            ValidationCode::InvalidReference
        );
        d.actionless_policy = Some(ExecutionPolicy::HostMediated);
        assert_eq!(
            validate_definition(&d, &admission).unwrap_err().code,
            ValidationCode::ForbiddenExecutionPolicy
        );
        let trusted = RouteAdmission::trusted_host(admission.identity.clone(), admission.surface).unwrap();
        validate_definition(&d, &trusted).unwrap();
        d.item.actions = definition("a", None).item.actions;
        assert_eq!(
            validate_definition(&d, &trusted).unwrap_err().code,
            ValidationCode::InvalidReference
        );
        let mut missing = actionless_definition(None);
        missing.action_sections.clear();
        assert_eq!(
            validate_definition(&missing, &admission).unwrap_err().code,
            ValidationCode::InvalidReference
        );
        let mut duplicate = actionless_definition(None);
        let binding = duplicate.action_sections[0].entries[0].clone();
        duplicate.action_sections[0].entries.push(binding);
        assert_eq!(
            validate_definition(&duplicate, &admission).unwrap_err().code,
            ValidationCode::DuplicateId
        );
    }
    #[test]
    fn actionless_untyped_submission_remains_separate_from_named_primary() {
        let key = ExecutableKey {
            target: target("a"),
            action: None,
        };
        let mut view = list(None, None);
        view.definitions = vec![actionless_definition(None), definition("b", None)].into();
        let admission = admission(1);
        let state = view.initial_state(&admission).unwrap();
        let submit = event(ViewEventKind::Submit(ActionSubmission {
            executable: key.clone(),
            arguments: SubmissionArguments::None,
        }));
        view.validate_event(&admission, &state, &submit).unwrap();
        assert_eq!(view.executable(&key).unwrap().0, ExecutionPolicy::Plugin);
        assert!(view.executable(&executable("a")).is_none());
        assert!(view
            .executable(&ExecutableKey {
                target: target("b"),
                action: None
            })
            .is_none());
    }
    #[test]
    fn request_mode_state_bounds_and_debug_are_host_safe() {
        let mut request = request();
        request.validate().unwrap();
        request.previous_revision = NonZeroU64::new(1);
        assert_eq!(
            request.validate().unwrap_err().code,
            ValidationCode::InvalidReference
        );
        request.mode = ViewRequestMode::Refresh;
        assert_eq!(
            request.validate().unwrap_err().code,
            ValidationCode::InvalidReference
        );
        request.state = Some(HostViewState::default());
        request.validate().unwrap();
        request.query = "private-query-marker".into();
        assert!(!format!("{request:?}").contains("private-query-marker"));
        let event = event(ViewEventKind::QueryChanged(request.query.clone()));
        assert!(!format!("{event:?}").contains("private-query-marker"));
        request.query = "x".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(request.validate().unwrap_err().code, ValidationCode::TooLong);
        request.query.clear();
        request.state.as_mut().unwrap().input = Some(executable("a"));
        assert_eq!(
            request.validate().unwrap_err().code,
            ValidationCode::InvalidReference
        );
    }
    fn request() -> ViewRequest {
        ViewRequest {
            mode: ViewRequestMode::Open,
            route: RouteId("route".into()),
            session: ViewSessionIdentity {
                logical_view: NonZeroU64::new(1).unwrap(),
                surface: NonZeroU64::new(1).unwrap(),
            },
            previous_revision: None,
            state: None,
            query: String::new(),
            scroll: None,
            reason: ViewRefreshReason::Explicit,
        }
    }
    #[test]
    fn reply_requires_exact_correlation_endpoint_and_descriptor_revision() {
        let request = request();
        let mut reply = ViewReply {
            context: ViewExecutionContext {
                route: request.route.clone(),
                session: request.session,
                revision: NonZeroU64::new(7).unwrap(),
            },
            descriptor: Arc::new(list(None, None)),
        };
        let mut endpoint = admission(1);
        endpoint.surface = reply.context.surface();
        reply
            .validate(&request, reply.context.revision, &endpoint)
            .unwrap();
        assert_eq!(endpoint.surface.generation.get(), 7);
        assert_eq!(
            reply
                .validate(&request, NonZeroU64::new(8).unwrap(), &endpoint)
                .unwrap_err()
                .code,
            ValidationCode::StaleSurface
        );
        endpoint.identity.owner = PluginId("other".into());
        assert_eq!(
            reply
                .validate(&request, reply.context.revision, &endpoint)
                .unwrap_err()
                .code,
            ValidationCode::WrongOwner
        );
        endpoint.identity.owner = PluginId("owner".into());
        reply.context.session.surface = NonZeroU64::new(2).unwrap();
        assert_eq!(
            reply
                .validate(&request, reply.context.revision, &endpoint)
                .unwrap_err()
                .code,
            ValidationCode::StaleSurface
        );
    }
    #[test]
    fn removed_retained_selection_and_anchor_do_not_reject_a_valid_reply() {
        let mut request = request();
        request.mode = ViewRequestMode::Refresh;
        request.previous_revision = NonZeroU64::new(1);
        request.state = Some(HostViewState {
            selected: Some(target("a")),
            scope: Some(ScopeId("A".into())),
            input: Some(executable("a")),
        });
        request.scroll = Some(ScrollAnchor {
            item: target("a"),
            row_offset: 0,
        });
        let mut replacement = list(None, None);
        replacement.definitions = vec![definition("b", None)].into();
        let StandardView::List(collection) = &mut replacement.view else {
            panic!()
        };
        collection.items = vec![ItemPresentation {
            target: target("b"),
            section: None,
            scopes: vec![],
            image: None,
        }]
        .into();
        collection.initial_selection = Some(target("b"));
        collection.sections.clear();
        collection.scopes.clear();
        let reply = ViewReply {
            context: ViewExecutionContext {
                route: request.route.clone(),
                session: request.session,
                revision: NonZeroU64::new(2).unwrap(),
            },
            descriptor: Arc::new(replacement),
        };
        let mut endpoint = admission(1);
        endpoint.surface = reply.context.surface();
        // Both refresh and fresh Back/open accept bounded restoration intent.
        for mode in [ViewRequestMode::Refresh, ViewRequestMode::Open] {
            request.mode = mode;
            request.previous_revision = if mode == ViewRequestMode::Refresh {
                NonZeroU64::new(1)
            } else {
                None
            };
            reply
                .validate(&request, reply.context.revision, &endpoint)
                .unwrap();
        }
        assert_eq!(
            reply
                .descriptor
                .validate_state(&endpoint, request.state.as_ref().unwrap())
                .unwrap_err()
                .code,
            ValidationCode::InvalidSelection
        );
        let state = HostViewState::default();
        for kind in [
            ViewEventKind::Activate(executable("a")),
            ViewEventKind::Submit(ActionSubmission {
                executable: executable("a"),
                arguments: SubmissionArguments::Structured(values()),
            }),
        ] {
            let event = ViewEvent {
                route: endpoint.identity.clone(),
                surface: endpoint.surface,
                kind,
            };
            assert_eq!(
                reply
                    .descriptor
                    .validate_event(&endpoint, &state, &event)
                    .unwrap_err()
                    .code,
                ValidationCode::InvalidReference
            );
        }
        let mut old_event = event(ViewEventKind::Activate(executable("b")));
        old_event.route = endpoint.identity.clone();
        assert_eq!(
            reply
                .descriptor
                .validate_event(&endpoint, &state, &old_event)
                .unwrap_err()
                .code,
            ValidationCode::StaleSurface
        );
        request.mode = ViewRequestMode::Refresh;
        request.previous_revision = Some(reply.context.revision);
        assert_eq!(
            reply
                .validate(&request, reply.context.revision, &endpoint)
                .unwrap_err()
                .code,
            ValidationCode::StaleSurface
        );
    }
    #[test]
    fn removed_schema_is_restore_intent_not_current_typed_authority() {
        let mut request = request();
        request.mode = ViewRequestMode::Refresh;
        request.previous_revision = NonZeroU64::new(1);
        request.state = Some(HostViewState {
            selected: Some(target("a")),
            scope: None,
            input: Some(executable("a")),
        });
        let mut reply = ViewReply {
            context: ViewExecutionContext {
                route: request.route.clone(),
                session: request.session,
                revision: NonZeroU64::new(2).unwrap(),
            },
            descriptor: Arc::new(list(None, None)),
        };
        let mut endpoint = admission(1);
        endpoint.surface = reply.context.surface();
        reply
            .validate(&request, reply.context.revision, &endpoint)
            .unwrap();
        assert_eq!(
            reply
                .descriptor
                .validate_state(&endpoint, request.state.as_ref().unwrap())
                .unwrap_err()
                .code,
            ValidationCode::InvalidReference
        );
        let state = HostViewState {
            selected: Some(target("a")),
            ..Default::default()
        };
        let event = ViewEvent {
            route: endpoint.identity.clone(),
            surface: endpoint.surface,
            kind: ViewEventKind::Submit(ActionSubmission {
                executable: executable("a"),
                arguments: SubmissionArguments::Structured(values()),
            }),
        };
        assert_eq!(
            reply
                .descriptor
                .validate_event(&endpoint, &state, &event)
                .unwrap_err()
                .code,
            ValidationCode::InvalidArguments
        );
        // Restore intent never exempts new content from resource admission.
        Arc::make_mut(&mut reply.descriptor).title = String::with_capacity(MAX_DESCRIPTOR_BYTES + 1);
        assert_eq!(
            reply
                .validate(&request, reply.context.revision, &endpoint)
                .unwrap_err()
                .code,
            ValidationCode::RetainedBytes
        );
    }
}
