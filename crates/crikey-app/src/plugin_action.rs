//! Shared action-dispatch seam for plugin-owned result actions.
//!
//! The search service validates the selected item and action before handing the
//! request to this router. Providers register their exact loaded plugin ids;
//! dispatch therefore cannot fall through to a sibling runtime or reinterpret
//! a stale owner as another plugin.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use crate::plugin_page::PageUpdate;
use crikey_core::result_presentation::ResultPresentation;
use crikey_core::structured_arguments::{
    ArgumentSchema, ArgumentValues, ExecutableKey, OwnedItem, SubmissionOutcome,
};
use crikey_core::structured_view::{
    StructuredRouteOutcome, ViewExecutionContext, ViewReply, ViewRequest, ViewSessionIdentity,
};
use crikey_core::{ActionId, CoreError, Item, ItemId, PageInput, PluginId, Result as CoreResult};
use crikey_plugin_model::{permissions::admit_web_surface, FilesystemScope, Permissions};

/// Exact owner/item snapshots paired with their physical producing worker.
pub(crate) type OriginItems = Mutex<BTreeMap<(PluginId, ItemId), (Item, u64)>>;
pub(crate) type SharedOriginItems = Arc<OriginItems>;
type CompletionCallback = Arc<dyn Fn() + Send + Sync>;

/// Authority supplied by an accepted provider publication, never item metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootSource {
    Catalog(DeclaredCommandSource),
    Suggestion {
        generation: crikey_core::Generation,
        producer: u64,
    },
}

#[derive(Debug, Clone)]
pub struct RootExecutableBinding {
    pub executable: ExecutableKey,
    pub source: RootSource,
    pub schema: Arc<ArgumentSchema>,
    pub fingerprint: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct RootPresentationSnapshot {
    pub owner: PluginId,
    pub source: RootSource,
    pub presentation: Arc<ResultPresentation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootScope {
    pub owner: PluginId,
    pub id: crikey_core::result_presentation::ScopeId,
    pub source: RootSource,
}

#[derive(Debug)]
struct RootPublication {
    snapshot: RootPresentationSnapshot,
    items: BTreeMap<ItemId, Item>,
}

/// Complete sidecars and frozen executable items share one replacement boundary.
#[derive(Debug, Default)]
pub struct RootSourceLedger {
    catalogs: BTreeMap<PluginId, RootPublication>,
    suggestions: BTreeMap<PluginId, RootPublication>,
    retired: BTreeSet<OwnedItem>,
    retired_owners: BTreeSet<PluginId>,
    captured_scope: Option<RootScope>,
}

impl RootSourceLedger {
    pub(crate) fn actionless_item(
        &self,
        owner: &PluginId,
        item_id: &ItemId,
        source: &RootSource,
    ) -> Option<&Item> {
        let publication = match source {
            RootSource::Catalog(_) => self.catalogs.get(owner)?,
            RootSource::Suggestion { .. } => self.suggestions.get(owner)?,
        };
        let item = publication.items.get(item_id)?;
        if &publication.snapshot.source != source
            || !item.actions.is_empty()
            || item.argument_policy == crikey_core::ArgumentPolicy::Required
        {
            return None;
        }
        if publication.snapshot.presentation.schemas.iter().any(|binding| {
            binding.executable.target.owner == *owner
                && binding.executable.target.item == *item_id
                && binding.executable.action.is_none()
        }) {
            return None;
        }
        Some(item)
    }
    pub(crate) fn actionless_source(
        &self,
        owner: &PluginId,
        item: &ItemId,
        canonical: Option<&DeclaredCommandSource>,
    ) -> Option<RootSource> {
        let publication = if let Some(canonical) = canonical {
            self.catalogs.get(owner).filter(
                |p| matches!(&p.snapshot.source, RootSource::Catalog(source) if source == canonical),
            )?
        } else if let Some(publication) = self.suggestions.get(owner).filter(|p| p.items.contains_key(item)) {
            publication
        } else {
            if self.retired_owners.contains(owner)
                || self
                    .retired
                    .iter()
                    .any(|target| &target.owner == owner && &target.item == item)
            {
                return None;
            }
            self.catalogs.get(owner)?
        };
        self.actionless_item(owner, item, &publication.snapshot.source)?;
        Some(publication.snapshot.source.clone())
    }
    pub fn publish(
        &mut self,
        owner: PluginId,
        source: RootSource,
        items: &[Item],
        presentation: ResultPresentation,
    ) -> CoreResult<()> {
        let invalid = || CoreError::Invalid("invalid root publication".into());
        let mut frozen = BTreeMap::new();
        let mut targets = BTreeSet::new();
        let mut executables = BTreeSet::new();
        for item in items {
            if item.plugin_id != owner {
                return Err(invalid());
            }
            let target = OwnedItem {
                owner: owner.clone(),
                item: item.stable_id.clone(),
            };
            if !targets.insert(target.clone()) {
                return Err(invalid());
            }
            if item.actions.is_empty() {
                executables.insert(ExecutableKey { target, action: None });
            } else {
                let mut actions = BTreeSet::new();
                for action in &item.actions {
                    if !actions.insert(&action.action_id) {
                        return Err(invalid());
                    }
                    if action.execution_policy == crikey_core::ExecutionPolicy::Plugin
                        && (action.applicable_categories.is_empty()
                            || action.applicable_categories.contains(&item.category))
                    {
                        executables.insert(ExecutableKey {
                            target: target.clone(),
                            action: Some(action.action_id.clone()),
                        });
                    }
                }
            }
        }
        presentation
            .validate(&owner, &targets, &executables)
            .map_err(|_| invalid())?;
        for binding in &presentation.schemas {
            binding.schema.fingerprint().map_err(|_| invalid())?;
        }
        for item in items {
            frozen.insert(item.stable_id.clone(), item.clone());
        }
        let dynamic = matches!(source, RootSource::Suggestion { .. });
        if dynamic
            && self.captured_scope.as_ref().is_some_and(|scope| {
                scope.owner == owner
                    && (match (&scope.source, &source) {
                        (
                            RootSource::Suggestion {
                                producer: captured, ..
                            },
                            RootSource::Suggestion { producer, .. },
                        ) => captured != producer,
                        _ => true,
                    } || !presentation
                        .scopes
                        .iter()
                        .any(|declaration| declaration.id == scope.id))
            })
        {
            self.captured_scope = None;
        }
        let publication = RootPublication {
            snapshot: RootPresentationSnapshot {
                owner: owner.clone(),
                source,
                presentation: Arc::new(presentation),
            },
            items: frozen,
        };
        if dynamic {
            if let Some(previous) = self.suggestions.insert(owner.clone(), publication) {
                self.retire_items(&owner, previous.items.into_keys());
            }
            for target in targets {
                self.retired.remove(&target);
            }
        } else {
            self.catalogs.insert(owner, publication);
        }
        Ok(())
    }

    pub fn retire_suggestions(&mut self) {
        for (owner, publication) in std::mem::take(&mut self.suggestions) {
            self.retire_items(&owner, publication.items.into_keys());
        }
    }

    /// Capture one selected declaration independently from executable rows.
    /// Generation retirement removes old bindings, never this query's scope.
    pub fn capture_scope(&mut self, scope: Option<&RootScope>) -> bool {
        let Some(scope) = scope else {
            self.captured_scope = None;
            return true;
        };
        if !self.scope_current(scope) {
            return false;
        }
        self.captured_scope = matches!(&scope.source, RootSource::Suggestion { .. }).then(|| scope.clone());
        true
    }

    /// Endpoints additionally require the dynamic producer to remain live.
    pub fn scope_current(&self, scope: &RootScope) -> bool {
        if self.captured_scope.as_ref() == Some(scope) {
            return true;
        }
        let publication = match &scope.source {
            RootSource::Catalog(_) => self.catalogs.get(&scope.owner),
            RootSource::Suggestion { .. } => self.suggestions.get(&scope.owner),
        };
        publication.is_some_and(|publication| {
            publication.snapshot.source == scope.source
                && publication
                    .snapshot
                    .presentation
                    .scopes
                    .iter()
                    .any(|declaration| declaration.id == scope.id)
        })
    }

    /// Only a deliberately captured declaration may outlive its query source.
    pub fn scope_captured(&self, scope: &RootScope) -> bool {
        self.captured_scope.as_ref() == Some(scope)
    }

    /// Worker death retires only that owner's dynamic authority. Healthy
    /// siblings keep their current generation and presentation.
    pub fn retire_owner_suggestions(&mut self, owner: &PluginId) {
        if self
            .captured_scope
            .as_ref()
            .is_some_and(|scope| &scope.owner == owner)
        {
            self.captured_scope = None;
        }
        if let Some(publication) = self.suggestions.remove(owner) {
            self.retire_items(owner, publication.items.into_keys());
        }
    }

    pub fn invalidate(&mut self, owner: &PluginId) {
        self.catalogs.remove(owner);
        self.retire_owner_suggestions(owner);
    }

    fn retire_items(&mut self, owner: &PluginId, items: impl Iterator<Item = ItemId>) {
        if self.retired_owners.contains(owner) {
            return;
        }
        for item in items {
            self.retired.insert(OwnedItem {
                owner: owner.clone(),
                item,
            });
            if self.retired.len() > crikey_core::result_presentation::MAX_PRESENTATION_ITEMS {
                // Explicit canonical admission remains available. Old result
                // rows fail closed rather than retaining unbounded tombstones.
                self.retired_owners
                    .extend(self.retired.iter().map(|target| target.owner.clone()));
                self.retired.clear();
                break;
            }
        }
    }

    pub fn binding(
        &self,
        key: &ExecutableKey,
        canonical: Option<&DeclaredCommandSource>,
    ) -> Option<RootExecutableBinding> {
        let publication = if let Some(source) = canonical {
            self.catalogs
                .get(&key.target.owner)
                .filter(|p| p.snapshot.source == RootSource::Catalog(source.clone()))?
        } else if let Some(publication) = self
            .suggestions
            .get(&key.target.owner)
            .filter(|p| p.items.contains_key(&key.target.item))
        {
            publication
        } else {
            if self.retired_owners.contains(&key.target.owner) || self.retired.contains(&key.target) {
                return None;
            }
            self.catalogs.get(&key.target.owner)?
        };
        let schema = publication.snapshot.presentation.schema_for(key)?.clone();
        Some(RootExecutableBinding {
            executable: key.clone(),
            source: publication.snapshot.source.clone(),
            fingerprint: schema.fingerprint().ok()?,
            schema,
        })
    }

    pub fn snapshots(&self) -> Vec<RootPresentationSnapshot> {
        self.catalogs
            .values()
            .chain(self.suggestions.values())
            .map(|p| p.snapshot.clone())
            .collect()
    }

    pub fn frozen_item(&self, binding: &RootExecutableBinding) -> Option<Item> {
        let publication = match &binding.source {
            RootSource::Catalog(_) => self.catalogs.get(&binding.executable.target.owner)?,
            RootSource::Suggestion { .. } => self.suggestions.get(&binding.executable.target.owner)?,
        };
        if publication.snapshot.source != binding.source {
            return None;
        }
        let schema = publication
            .snapshot
            .presentation
            .schema_for(&binding.executable)?;
        if schema.as_ref() != binding.schema.as_ref() || schema.fingerprint().ok()? != binding.fingerprint {
            return None;
        }
        publication.items.get(&binding.executable.target.item).cloned()
    }
}

/// Exact provider publication backing an accepted canonical catalog.
/// This is not a UI epoch or a live-worker requirement for command execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredCommandSource {
    pub instance: u64,
    pub generation: crikey_core::Generation,
    pub producer: u64,
    pub publication: u64,
}

/// Ordinary actions remain strict; only declared commands may call the
/// genuinely actionless item callback.
#[derive(Debug)]
pub(crate) enum AdmittedPluginWork {
    Action(ActionId),
    Actionless {
        source: RootSource,
    },
    Command {
        declaration: crikey_core::command::CommandDeclaration,
        action_id: Option<ActionId>,
        source: DeclaredCommandSource,
    },
    Typed {
        binding: RootExecutableBinding,
        values: ArgumentValues,
    },
}

impl AdmittedPluginWork {
    pub(crate) fn action_id(&self) -> Option<&ActionId> {
        match self {
            Self::Action(id) => Some(id),
            Self::Command { action_id, .. } => action_id.as_ref(),
            Self::Actionless { .. } => None,
            Self::Typed { binding, .. } => binding.executable.action.as_ref(),
        }
    }

    pub(crate) fn command_source(
        &self,
    ) -> Option<(&crikey_core::command::CommandDeclaration, &DeclaredCommandSource)> {
        match self {
            Self::Command {
                declaration, source, ..
            } => Some((declaration, source)),
            Self::Action(_) | Self::Actionless { .. } | Self::Typed { .. } => None,
        }
    }

    pub(crate) fn typed_binding(&self) -> Option<&RootExecutableBinding> {
        match self {
            Self::Typed { binding, .. } => Some(binding),
            _ => None,
        }
    }

    pub(crate) fn take_typed_values(&mut self) -> Option<ArgumentValues> {
        match self {
            Self::Typed { values, .. } => Some(std::mem::take(values)),
            _ => None,
        }
    }

    pub(crate) fn validate(&self, item: &Item, argument: Option<&str>) -> CoreResult<()> {
        match self {
            Self::Actionless { .. } => {
                if !item.actions.is_empty()
                    || argument.is_some()
                    || item.argument_policy == crikey_core::ArgumentPolicy::Required
                {
                    return Err(CoreError::Invalid("actionless root target changed".into()));
                }
            }
            Self::Typed { binding, values } => {
                if argument.is_some()
                    || item.plugin_id != binding.executable.target.owner
                    || item.stable_id != binding.executable.target.item
                {
                    return Err(CoreError::Invalid("typed executable source changed".into()));
                }
                if let Some(id) = &binding.executable.action {
                    let mut matching = item.actions.iter().filter(|a| &a.action_id == id);
                    if matching.next().is_none_or(|a| {
                        a.execution_policy != crikey_core::ExecutionPolicy::Plugin
                            || (!a.applicable_categories.is_empty()
                                && !a.applicable_categories.contains(&item.category))
                    }) || matching.next().is_some()
                    {
                        return Err(CoreError::Invalid("typed executable action unavailable".into()));
                    }
                } else if !item.actions.is_empty() {
                    return Err(CoreError::Invalid("typed actionless target changed".into()));
                }
                if binding.schema.validate_values(values).is_err() {
                    return Err(CoreError::Invalid("invalid typed arguments".into()));
                }
            }
            Self::Action(id) => {
                let (action, _) = validate_action_item(item, id, argument)?;
                if action.execution_policy != crikey_core::ExecutionPolicy::Plugin {
                    return Err(CoreError::Invalid("selected action is not plugin-owned".into()));
                }
            }
            Self::Command {
                declaration,
                action_id,
                ..
            } => {
                if validate_declared_command_item(declaration, item, argument)?.as_ref() != action_id.as_ref()
                {
                    return Err(CoreError::Invalid(
                        "declared command action changed after admission".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Validate the canonical frozen target at both admission and execution.
pub(crate) fn validate_declared_command_item(
    declaration: &crikey_core::command::CommandDeclaration,
    item: &Item,
    argument: Option<&str>,
) -> CoreResult<Option<ActionId>> {
    use crikey_core::command::{CommandAvailability, ExecutableTarget};
    declaration
        .validate()
        .map_err(|reason| CoreError::Invalid(reason.into()))?;
    let ExecutableTarget::Item { item_id, action_id } = &declaration.executable_target else {
        return Err(CoreError::Invalid(
            "host command cannot be dispatched to a plugin".into(),
        ));
    };
    if declaration.availability != CommandAvailability::Available
        || declaration.reference.provider_id != item.plugin_id
        || item_id != &item.stable_id
        || declaration.argument_policy != item.argument_policy
    {
        return Err(CoreError::Invalid(
            "declared command target or policy is no longer current".into(),
        ));
    }
    match item.argument_policy {
        crikey_core::ArgumentPolicy::Forbidden if argument.is_some() => {
            return Err(CoreError::Invalid(
                "this command does not accept an argument".into(),
            ));
        }
        crikey_core::ArgumentPolicy::Required if argument.is_none_or(str::is_empty) => {
            return Err(CoreError::Invalid("this command requires an argument".into()));
        }
        _ => {}
    }
    let mut ids = BTreeSet::new();
    if item.actions.iter().any(|action| !ids.insert(&action.action_id)) {
        return Err(CoreError::Invalid(
            "declared command catalog has duplicate action IDs".into(),
        ));
    }
    let selected = action_id
        .as_ref()
        .or_else(|| item.actions.first().map(|action| &action.action_id));
    if let Some(selected) = selected {
        let (action, _) = validate_action_item(item, selected, argument)?;
        if action.execution_policy != crikey_core::ExecutionPolicy::Plugin {
            return Err(CoreError::Invalid(
                "declared command action is not plugin-owned".into(),
            ));
        }
    }
    Ok(selected.cloned())
}

/// Process-scoped terminal notification, independent of result/session wakes.
/// Ring only after releasing the completion mailbox lock, so the receiver may
/// drain immediately without blocking the producer.
#[derive(Clone, Default)]
pub(crate) struct CompletionWake(Arc<Mutex<Option<CompletionCallback>>>);

impl CompletionWake {
    pub(crate) fn install(&self, wake: CompletionCallback) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = Some(wake);
    }

    pub(crate) fn ring(&self) {
        let wake = self.0.lock().unwrap_or_else(|error| error.into_inner()).clone();
        if let Some(wake) = wake {
            wake();
        }
    }
}

impl fmt::Debug for CompletionWake {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("CompletionWake").finish_non_exhaustive()
    }
}

/// Identifier assigned when a plugin action is admitted to an endpoint.
///
/// The plugin namespace is part of the id so a router can cancel an action or
/// attribute its completion without relying on a process-global counter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActionRequestId {
    pub plugin: PluginId,
    pub sequence: u64,
}

/// What a plugin action did, beyond succeeding.
///
/// An action that opens a page is not a plain success: the launcher must hand
/// the screen to that plugin rather than dismiss, and it can only do that if
/// the completion says so and names the surface. Reporting it as `Ok(())`
/// would leave the caller guessing from a side channel which action, if any,
/// wanted the window kept open (spec 32.2).
#[derive(Debug, Clone)]
pub enum ActionEffect {
    /// The action ran and is finished.
    Completed,
    /// Fixed-code typed outcome; values and callback diagnostics never return.
    Typed(SubmissionOutcome),
    /// Host-correlated descriptor from the exact admitted producer.
    View(ViewReply),
    /// Navigation keeps the physical completion source, never a guest owner.
    Navigate {
        outcome: StructuredRouteOutcome,
        producer: u64,
    },
    /// The action asked the host to open this plugin-drawn page. The owning
    /// plugin is [`PluginActionCompletion::plugin`].
    ShowPage { page_id: String },
}

impl PartialEq for ActionEffect {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Completed, Self::Completed) => true,
            (Self::Typed(a), Self::Typed(b)) => a == b,
            (Self::ShowPage { page_id: a }, Self::ShowPage { page_id: b }) => a == b,
            (
                Self::Navigate {
                    outcome: a,
                    producer: x,
                },
                Self::Navigate {
                    outcome: b,
                    producer: y,
                },
            ) => a == b && x == y,
            (Self::View(a), Self::View(b)) => {
                a.context == b.context && Arc::ptr_eq(&a.descriptor, &b.descriptor)
            }
            _ => false,
        }
    }
}
impl Eq for ActionEffect {}

/// Terminal result emitted by a plugin action endpoint.
///
/// The completion owns its error, so polling is the only operation that
/// transfers a worker outcome back to the UI thread.
#[derive(Debug)]
pub struct PluginActionCompletion {
    pub request_id: ActionRequestId,
    pub plugin: PluginId,
    pub item_id: ItemId,
    pub action_id: Option<ActionId>,
    pub outcome: CoreResult<ActionEffect>,
}

/// What a validated action did at submission time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionSubmission {
    /// A host-mediated action completed synchronously.
    Completed,
    /// A plugin-owned action was admitted and will complete asynchronously.
    Pending(ActionRequestId),
}

/// Admission-time privacy evidence from the exact frozen execution snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginActionReceipt {
    pub request_id: ActionRequestId,
    pub query_history_allowed: bool,
}

/// Success-only item learning uses this exact admitted frozen snapshot. Typed
/// values are never attached to the receipt or query-history evidence.
#[derive(Debug)]
pub struct TypedActionReceipt {
    pub receipt: PluginActionReceipt,
    pub item: Item,
}

/// A successful host/provider admission and its query-collection authorization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionReceipt {
    pub submission: ActionSubmission,
    pub query_history_allowed: bool,
}

/// Validate the item that will actually be queued, not a previously displayed row.
pub(crate) fn validate_action_item<'a>(
    item: &'a Item,
    action_id: &ActionId,
    argument: Option<&str>,
) -> CoreResult<(&'a crikey_core::Action, bool)> {
    let action = item
        .actions
        .iter()
        .find(|action| &action.action_id == action_id)
        .ok_or_else(|| CoreError::Invalid("selected action is no longer available".into()))?;
    if !action.applicable_categories.is_empty() && !action.applicable_categories.contains(&item.category) {
        return Err(CoreError::Invalid(format!(
            "action `{}` is not applicable to item category `{}`",
            action.action_id.0,
            item.category.as_str()
        )));
    }
    match item.argument_policy {
        crikey_core::ArgumentPolicy::Forbidden if argument.is_some() => {
            return Err(CoreError::Invalid("this item does not accept an argument".into()))
        }
        crikey_core::ArgumentPolicy::Required if argument.is_none_or(str::is_empty) => {
            return Err(CoreError::Invalid("this item requires an argument".into()))
        }
        _ => {}
    }
    let primary = item.actions.iter().find(|action| {
        action.applicable_categories.is_empty() || action.applicable_categories.contains(&item.category)
    });
    Ok((
        action,
        item.argument_policy == crikey_core::ArgumentPolicy::Forbidden
            && argument.is_none()
            && primary.is_some_and(|action| &action.action_id == action_id),
    ))
}

/// A live suggestion overrides its canonical catalog entry. Released static
/// catalogs remain executable without borrowing a preview/query incarnation.
pub(crate) fn resolve_action_item(
    plugin: &PluginId,
    item_id: &ItemId,
    live_origin: Option<u64>,
    suggestions: &OriginItems,
    catalog: &OriginItems,
    static_catalog: &Mutex<BTreeMap<(PluginId, ItemId), Item>>,
) -> Option<(Item, Option<u64>)> {
    let key = (plugin.clone(), item_id.clone());
    {
        let suggestions = suggestions.lock().unwrap_or_else(|error| error.into_inner());
        if let Some((item, origin)) = suggestions.get(&key) {
            if *origin != 0 && Some(*origin) == live_origin {
                return Some((item.clone(), Some(*origin)));
            }
            // Its presence identifies the selected dynamic target. Losing that
            // origin must refuse admission, not silently retarget the same ID
            // to canonical content with a different policy/action.
            return None;
        }
    }
    if let Some((item, _)) = catalog
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&key)
    {
        return Some((item.clone(), None));
    }
    static_catalog
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&key)
        .map(|item| (item.clone(), None))
}

/// Runtime entry point for one provider's plugin-owned actions.
///
/// Implementations own the worker and the shared per-plugin budget. Submission
/// must only validate/admit and enqueue work; it must never wait for the plugin
/// callback. The admitted action slot remains owned by the endpoint until a
/// terminal completion is emitted, including transport failures, timeout,
/// cancellation and worker reaping.
pub trait PluginActionExecutor: Send + Sync {
    fn request_view(
        &self,
        _owner: &PluginId,
        _producer: u64,
        _request: ViewRequest,
    ) -> CoreResult<ActionRequestId> {
        Err(CoreError::Invalid(
            "runtime does not support standard views".into(),
        ))
    }
    fn submit_view_action(
        &self,
        _owner: &PluginId,
        _context: &ViewExecutionContext,
        _state: &crikey_core::structured_view::HostViewState,
        _executable: &ExecutableKey,
        _argument: Option<&str>,
    ) -> CoreResult<ActionRequestId> {
        Err(CoreError::Invalid(
            "runtime does not support standard views".into(),
        ))
    }
    fn submit_view_typed(
        &self,
        _owner: &PluginId,
        _context: &ViewExecutionContext,
        _state: &crikey_core::structured_view::HostViewState,
        _executable: &ExecutableKey,
        _values: ArgumentValues,
    ) -> CoreResult<ActionRequestId> {
        Err(CoreError::Invalid(
            "runtime does not support standard views".into(),
        ))
    }
    fn retire_view(&self, _owner: &PluginId, _session: ViewSessionIdentity) {}
    fn view_current(&self, _owner: &PluginId, _context: &ViewExecutionContext) -> bool {
        false
    }
    fn submit_actionless(
        &self,
        _owner: &PluginId,
        _item: &ItemId,
        _source: &RootSource,
    ) -> CoreResult<ActionRequestId> {
        Err(CoreError::Invalid("actionless root target unavailable".into()))
    }
    fn root_actionless_source(
        &self,
        _owner: &PluginId,
        _item: &ItemId,
        _canonical: Option<&DeclaredCommandSource>,
    ) -> Option<RootSource> {
        None
    }
    fn validate_view_link(
        &self,
        _owner: &PluginId,
        _context: &ViewExecutionContext,
        _state: &crikey_core::structured_view::HostViewState,
        _url: &str,
    ) -> CoreResult<()> {
        Err(CoreError::Invalid("view link unavailable".into()))
    }
    fn root_binding(
        &self,
        _key: &ExecutableKey,
        _canonical: Option<&DeclaredCommandSource>,
    ) -> Option<RootExecutableBinding> {
        None
    }
    fn root_presentations(&self) -> Vec<RootPresentationSnapshot> {
        Vec::new()
    }
    fn root_scope_current(&self, _scope: &RootScope) -> bool {
        false
    }
    fn submit_typed(
        &self,
        _binding: &RootExecutableBinding,
        _values: ArgumentValues,
    ) -> CoreResult<TypedActionReceipt> {
        Err(CoreError::Invalid(
            "runtime does not support typed arguments".into(),
        ))
    }

    /// Resolve and freeze only the exact canonical catalog publication.
    /// Adapters without declaration support refuse, never use ordinary lookup.
    fn submit_declared_command_with_receipt(
        &self,
        _declaration: &crikey_core::command::CommandDeclaration,
        _source: &DeclaredCommandSource,
        _argument: Option<&str>,
    ) -> CoreResult<PluginActionReceipt> {
        Err(CoreError::Invalid(
            "runtime does not support declared commands".into(),
        ))
    }

    /// Admits `action_id` for an item belonging to `plugin`.
    fn submit_plugin_action(
        &self,
        plugin: &PluginId,
        item: &Item,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> CoreResult<ActionRequestId>;

    /// Older/legacy adapters cannot prove that free-form execution text is
    /// nonsecret. They remain executable but do not authorize query collection.
    fn submit_plugin_action_with_receipt(
        &self,
        plugin: &PluginId,
        item: &Item,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> CoreResult<PluginActionReceipt> {
        self.submit_plugin_action(plugin, item, action_id, argument)
            .map(|request_id| PluginActionReceipt {
                request_id,
                query_history_allowed: false,
            })
    }

    fn submit_plugin_action_by_id_with_receipt(
        &self,
        plugin: &PluginId,
        item_id: &ItemId,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> CoreResult<PluginActionReceipt> {
        self.submit_plugin_action_by_id(plugin, item_id, action_id, argument)
            .map(|request_id| PluginActionReceipt {
                request_id,
                query_history_allowed: false,
            })
    }

    /// Drains terminal outcomes without waiting for plugin work.
    fn poll_plugin_actions(&self) -> Vec<PluginActionCompletion> {
        Vec::new()
    }

    /// Requests cancellation of an admitted action. Cancellation is
    /// cooperative once a provider worker has started the host call.
    fn cancel_plugin_action(&self, _request_id: &ActionRequestId) -> bool {
        false
    }

    /// Whether this endpoint has a current item snapshot with exact ownership.
    ///
    /// The live UI receives async provider rows separately from
    /// `SearchService`; this lookup lets the composition root route one of
    /// those rows without guessing an owner from an opaque stable id.
    fn owns_item(&self, _plugin: &PluginId, _item_id: &ItemId) -> bool {
        false
    }

    /// Admits an action by an item id retained by the provider runtime.
    fn submit_plugin_action_by_id(
        &self,
        _plugin: &PluginId,
        _item_id: &ItemId,
        _action_id: &ActionId,
        _argument: Option<&str>,
    ) -> CoreResult<ActionRequestId> {
        Err(CoreError::Invalid(
            "plugin action item snapshot is unavailable".to_owned(),
        ))
    }

    /// Opens the named plugin-drawn page and starts asking for frames.
    ///
    /// Returning immediately is part of the contract: a page is a stream of
    /// round trips to a child process, and none of them may happen on the
    /// caller's thread. A runtime with no page support refuses rather than
    /// pretending to open one, so the launcher never shows an empty surface
    /// nothing will ever draw into.
    fn open_plugin_page(
        &self,
        _plugin: &PluginId,
        _page_id: &str,
        _width: u32,
        _height: u32,
        _palette: crikey_core::PagePalette,
    ) -> CoreResult<()> {
        Err(CoreError::Invalid(
            "this plugin runtime cannot draw pages".to_owned(),
        ))
    }

    /// Queues one host-hit-tested page event for the open page.
    fn send_plugin_page_input(&self, _input: PageInput) -> CoreResult<()> {
        Err(CoreError::Invalid("no plugin page is open".to_owned()))
    }

    /// Tells the open page the viewport changed.
    fn resize_plugin_page(&self, _width: u32, _height: u32) {}

    /// Identity of the current page opening, if this runtime supports updates.
    fn plugin_page_session_id(&self) -> Option<crate::PageSessionId> {
        None
    }

    /// Updates only the identified live page; never opens or restarts a guest.
    fn update_plugin_page_palette(
        &self,
        _session: crate::PageSessionId,
        _palette: crikey_core::PagePalette,
    ) -> CoreResult<()> {
        Err(CoreError::Invalid(
            "no matching live plugin page is open".to_owned(),
        ))
    }

    /// Closes the open page, telling its plugin the surface is gone.
    fn close_plugin_page(&self) {}

    /// Takes the newest finished page frame, if one is waiting.
    fn poll_plugin_page(&self) -> Option<PageUpdate> {
        None
    }
}
/// One privileged operation the host performs on a plugin's behalf.
///
/// The variants are exactly the operations a plugin can reach through the
/// host today, and no more. A permission with no operation behind it is not
/// given a variant here: a gate nothing can call would read as enforcement
/// while enforcing nothing, which is the defect this type exists to close.
/// Such permissions are named by `Manifest::unhonoured_declarations` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCapability {
    /// Starting an application through the platform launcher.
    ProcessLaunch,
    /// Handing a path the *user* picked to whatever their desktop registered
    /// for it, through the platform opener.
    ///
    /// Its own variant rather than a second use of [`Self::ProcessLaunch`]
    /// because the two are different questions and a gate that cannot tell
    /// them apart cannot ever be tightened: "start this program with this
    /// argument vector" is a decision this workspace made, while "open this
    /// document with whatever is registered" is a decision the user's desktop
    /// made. A refusal has to be able to name which one was refused.
    ///
    /// It is nonetheless backed by the same `process` permission, and that is
    /// deliberate rather than a shortcut: `xdg-open`, `/usr/bin/open` and
    /// `ShellExecuteExW` all run a `.desktop`, a `.app` or an `.exe` when
    /// handed one, so the authority is genuinely the same authority. Giving it
    /// a weaker field of its own would be a gate that reads as a restriction
    /// while restricting nothing.
    DocumentOpen,
    /// Reading a resource file shipped inside the plugin's own package.
    PackageFileRead,
}

impl HostCapability {
    /// The manifest field an author has to change to grant this.
    fn permission(self) -> &'static str {
        match self {
            Self::ProcessLaunch | Self::DocumentOpen => "process",
            Self::PackageFileRead => "filesystem",
        }
    }

    /// The operation as an operator reading a refusal would name it.
    fn operation(self) -> &'static str {
        match self {
            Self::ProcessLaunch => "process launch",
            Self::DocumentOpen => "document open",
            Self::PackageFileRead => "package resource read",
        }
    }

    fn granted_by(self, permissions: &Permissions) -> bool {
        match self {
            Self::ProcessLaunch | Self::DocumentOpen => permissions.process,
            Self::PackageFileRead => permissions.allows_filesystem_read(FilesystemScope::Package),
        }
    }
}

/// Exact-owner action registry used by [`crate::SearchService`].
///
/// A provider endpoint is registered once for every plugin it loaded. The
/// lookup is exact on the namespaced `PluginId`; no prefix or fallback routing
/// is permitted. Every owner carries a [`Permissions`] value, including the
/// legacy ones that ship no manifest: "no declaration" must not resolve to a
/// skipped check, so the legacy compatibility baseline is written down and
/// consulted like any other grant.
#[derive(Default)]
pub struct PluginActionRouter {
    providers: BTreeMap<PluginId, Arc<dyn PluginActionExecutor>>,
    host_permissions: BTreeMap<PluginId, Permissions>,
}

impl fmt::Debug for PluginActionRouter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginActionRouter")
            .field("plugins", &self.providers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl PluginActionRouter {
    pub fn submit_actionless(
        &self,
        owner: &PluginId,
        item: &ItemId,
        source: &RootSource,
    ) -> CoreResult<ActionRequestId> {
        self.providers
            .get(owner)
            .ok_or_else(|| CoreError::Invalid("root owner unavailable".into()))?
            .submit_actionless(owner, item, source)
    }
    pub fn root_actionless_source(
        &self,
        owner: &PluginId,
        item: &ItemId,
        canonical: Option<&DeclaredCommandSource>,
    ) -> Option<RootSource> {
        self.providers
            .get(owner)?
            .root_actionless_source(owner, item, canonical)
    }
    pub fn request_view(
        &self,
        owner: &PluginId,
        producer: u64,
        request: ViewRequest,
    ) -> CoreResult<ActionRequestId> {
        self.providers
            .get(owner)
            .ok_or_else(|| CoreError::Invalid("view owner unavailable".into()))?
            .request_view(owner, producer, request)
    }
    pub fn submit_view_action(
        &self,
        owner: &PluginId,
        context: &ViewExecutionContext,
        state: &crikey_core::structured_view::HostViewState,
        executable: &ExecutableKey,
        argument: Option<&str>,
    ) -> CoreResult<ActionRequestId> {
        self.providers
            .get(owner)
            .ok_or_else(|| CoreError::Invalid("view owner unavailable".into()))?
            .submit_view_action(owner, context, state, executable, argument)
    }
    pub fn submit_view_typed(
        &self,
        owner: &PluginId,
        context: &ViewExecutionContext,
        state: &crikey_core::structured_view::HostViewState,
        executable: &ExecutableKey,
        values: ArgumentValues,
    ) -> CoreResult<ActionRequestId> {
        self.providers
            .get(owner)
            .ok_or_else(|| CoreError::Invalid("view owner unavailable".into()))?
            .submit_view_typed(owner, context, state, executable, values)
    }
    pub fn retire_view(&self, owner: &PluginId, session: ViewSessionIdentity) {
        if let Some(endpoint) = self.providers.get(owner) {
            endpoint.retire_view(owner, session);
        }
    }
    pub fn view_current(&self, owner: &PluginId, context: &ViewExecutionContext) -> bool {
        self.providers
            .get(owner)
            .is_some_and(|endpoint| endpoint.view_current(owner, context))
    }
    pub fn validate_view_link(
        &self,
        owner: &PluginId,
        context: &ViewExecutionContext,
        state: &crikey_core::structured_view::HostViewState,
        url: &str,
    ) -> CoreResult<()> {
        self.providers
            .get(owner)
            .ok_or_else(|| CoreError::Invalid("view owner unavailable".into()))?
            .validate_view_link(owner, context, state, url)
    }
    /// Registers a legacy endpoint under the legacy compatibility baseline.
    ///
    /// A Keypirinha package has no `crikey.toml` and therefore no author
    /// declaration. It is still registered with an explicit grant set —
    /// [`Permissions::legacy_compatibility_baseline`] — rather than with a
    /// bypass, so a legacy owner travels the same gate as a manifest-governed
    /// one and an operator can read the posture out of `crikey plugin doctor`.
    pub fn register<I>(&mut self, plugins: I, executor: Arc<dyn PluginActionExecutor>) -> CoreResult<()>
    where
        I: IntoIterator<Item = PluginId>,
    {
        self.register_inner(
            plugins
                .into_iter()
                .map(|plugin| (plugin, Permissions::legacy_compatibility_baseline())),
            executor,
        )
    }

    /// Registers a manifest-governed endpoint with its host-mediated grants.
    ///
    /// The grants are checked before a plugin-owned result asks the host to
    /// perform a privileged operation. Keeping this map at the composition
    /// root prevents a provider worker from bypassing the host decision.
    pub fn register_with_permissions<I>(
        &mut self,
        permissions: I,
        executor: Arc<dyn PluginActionExecutor>,
    ) -> CoreResult<()>
    where
        I: IntoIterator<Item = (PluginId, Permissions)>,
    {
        self.register_inner(permissions, executor)
    }

    fn register_inner<I>(&mut self, plugins: I, executor: Arc<dyn PluginActionExecutor>) -> CoreResult<()>
    where
        I: IntoIterator<Item = (PluginId, Permissions)>,
    {
        let plugins = plugins.into_iter().collect::<Vec<_>>();
        let mut unique = BTreeSet::new();
        if plugins.is_empty() {
            return Ok(());
        }
        // A collision on either map is a duplicate: grants can exist without a
        // provider (a host catalog), so checking only the provider map would
        // let a plugin registration silently overwrite the host's own entry.
        if plugins.iter().any(|(plugin, _)| {
            !unique.insert(plugin)
                || self.providers.contains_key(plugin)
                || self.host_permissions.contains_key(plugin)
        }) {
            return Err(CoreError::Invalid(
                "plugin action provider is already registered".to_owned(),
            ));
        }
        for (plugin, permissions) in plugins {
            self.providers.insert(plugin.clone(), Arc::clone(&executor));
            self.host_permissions.insert(plugin, permissions);
        }
        Ok(())
    }

    /// Records the grants of a catalog the host itself produces.
    ///
    /// Discovered applications are published under a builtin owner that has no
    /// plugin runtime and must never receive plugin-owned dispatch, so it gets
    /// no executor here. It still needs an entry: an owner absent from the
    /// grant map is refused, and refusing the host's own launch action would
    /// leave the launcher unable to launch anything. Its one grant is
    /// `process`, written down for the same reason the legacy baseline is —
    /// so the exception is a line of code an auditor can find.
    pub fn register_host_catalog(&mut self, plugin: PluginId) -> CoreResult<()> {
        if self.host_permissions.contains_key(&plugin) {
            return Err(CoreError::Invalid(format!(
                "action grants for plugin `{}` are already registered",
                plugin.0
            )));
        }
        self.host_permissions.insert(
            plugin,
            Permissions {
                process: true,
                ..Permissions::default()
            },
        );
        Ok(())
    }

    /// Whether the exact owner may have the host perform `capability`.
    ///
    /// An owner this router does not know is denied: an unattributable request
    /// is refused rather than resolved to some other plugin's grants.
    pub fn permits(&self, plugin: &PluginId, capability: HostCapability) -> bool {
        self.host_permissions
            .get(plugin)
            .is_some_and(|permissions| capability.granted_by(permissions))
    }

    /// [`Self::permits`] as the refusal the caller should propagate.
    ///
    /// One spelling for every host-mediated seam, so a refusal names the owner
    /// and the manifest field to change wherever it came from, and so no two
    /// call sites drift into two different diagnostics for one decision.
    pub fn authorize(&self, plugin: &PluginId, capability: HostCapability) -> CoreResult<()> {
        if self.permits(plugin, capability) {
            return Ok(());
        }
        Err(CoreError::Invalid(format!(
            "plugin `{}` lacks the {} permission for host-mediated {}",
            plugin.0,
            capability.permission(),
            capability.operation()
        )))
    }

    /// Decides whether the exact owner may create the web surface its page
    /// declared.
    ///
    /// The page frame carries what the surface wants, while this registry
    /// carries the owning manifest's grants. Keeping the decision here makes
    /// an absent or unattributable owner a refusal rather than a route around
    /// [`admit_web_surface`].
    pub fn admit_web_surface(
        &self,
        plugin: &PluginId,
        wants_navigation: bool,
        wants_persistent_storage: bool,
    ) -> CoreResult<()> {
        let permissions = self.host_permissions.get(plugin).ok_or_else(|| {
            CoreError::Invalid(format!(
                "plugin `{}` has no registered permissions for a web surface",
                plugin.0
            ))
        })?;
        admit_web_surface(permissions, wants_navigation, wants_persistent_storage).map_err(|error| {
            CoreError::Invalid(format!(
                "plugin `{}` cannot open its web surface: {error}",
                plugin.0
            ))
        })
    }

    pub fn root_binding(
        &self,
        key: &ExecutableKey,
        canonical: Option<&DeclaredCommandSource>,
    ) -> Option<RootExecutableBinding> {
        let binding = self
            .providers
            .get(&key.target.owner)?
            .root_binding(key, canonical)?;
        if binding.executable != *key || binding.schema.fingerprint().ok()? != binding.fingerprint {
            return None;
        }
        Some(binding)
    }

    pub fn root_presentations(&self) -> Vec<RootPresentationSnapshot> {
        let mut seen = BTreeSet::new();
        self.providers
            .iter()
            .filter(|(_, executor)| seen.insert(Arc::as_ptr(executor) as *const () as usize))
            .flat_map(|(_, executor)| {
                executor.root_presentations().into_iter().filter(move |snapshot| {
                    self.providers
                        .get(&snapshot.owner)
                        .is_some_and(|owner| Arc::ptr_eq(owner, executor))
                })
            })
            .collect()
    }

    pub fn root_scope_current(&self, scope: &RootScope) -> bool {
        self.providers
            .get(&scope.owner)
            .is_some_and(|provider| provider.root_scope_current(scope))
    }

    pub fn submit_typed(
        &self,
        binding: &RootExecutableBinding,
        values: ArgumentValues,
    ) -> CoreResult<TypedActionReceipt> {
        if binding.schema.fingerprint().ok() != Some(binding.fingerprint)
            || binding.schema.validate_values(&values).is_err()
        {
            return Err(CoreError::Invalid("invalid typed arguments".into()));
        }
        self.providers
            .get(&binding.executable.target.owner)
            .ok_or_else(|| CoreError::Invalid("typed owner unavailable".into()))?
            .submit_typed(binding, values)
            .map(|mut receipt| {
                receipt.receipt.query_history_allowed = false;
                receipt
            })
    }

    /// Canonical commands never resolve through suggestion-first item lookup.
    pub fn submit_declared_command_with_receipt(
        &self,
        declaration: &crikey_core::command::CommandDeclaration,
        source: &DeclaredCommandSource,
        argument: Option<&str>,
    ) -> CoreResult<PluginActionReceipt> {
        let owner = &declaration.reference.provider_id;
        self.providers
            .get(owner)
            .ok_or_else(|| CoreError::Invalid(format!("no action runtime owns plugin `{}`", owner.0)))?
            .submit_declared_command_with_receipt(declaration, source, argument)
    }

    /// Dispatches and obtains privacy evidence from the same accepted snapshot.
    pub fn submit(
        &self,
        plugin: &PluginId,
        item: &Item,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> CoreResult<PluginActionReceipt> {
        if item.plugin_id != *plugin {
            return Err(CoreError::Invalid(format!(
                "plugin action item is owned by `{}`, not `{}`",
                item.plugin_id.0, plugin.0
            )));
        }
        self.providers
            .get(plugin)
            .ok_or_else(|| CoreError::Invalid(format!("no action runtime owns plugin `{}`", plugin.0)))?
            .submit_plugin_action_with_receipt(plugin, item, action_id, argument)
    }

    /// Resolves exactly one owner and returns its admission-time privacy evidence.
    pub fn submit_owned_item(
        &self,
        plugin: &PluginId,
        item_id: &ItemId,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> CoreResult<PluginActionReceipt> {
        self.providers
            .get(plugin)
            .filter(|executor| executor.owns_item(plugin, item_id))
            .ok_or_else(|| CoreError::Invalid("selected owner's result is no longer current".into()))?
            .submit_plugin_action_by_id_with_receipt(plugin, item_id, action_id, argument)
    }

    /// Drains all terminal action outcomes without waiting for a plugin.
    pub fn poll(&self) -> Vec<PluginActionCompletion> {
        let mut completions = Vec::new();
        let mut seen = BTreeSet::new();
        for executor in self.providers.values() {
            let key = Arc::as_ptr(executor) as *const () as usize;
            if seen.insert(key) {
                completions.extend(executor.poll_plugin_actions());
            }
        }
        completions
    }

    /// Requests cancellation of one exact-owner action.
    pub fn cancel(&self, request_id: &ActionRequestId) -> bool {
        self.providers
            .get(&request_id.plugin)
            .is_some_and(|executor| executor.cancel_plugin_action(request_id))
    }

    /// Finds a current provider-owned item snapshot by exact item id and
    /// submits to its unique owner.
    ///
    /// Stable ids are opaque and may collide across independently loaded
    /// providers, so an ambiguous match is refused rather than routed to an
    /// arbitrary sibling.
    pub fn submit_by_item_id(
        &self,
        item_id: &ItemId,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> CoreResult<ActionRequestId> {
        let mut matches = self
            .providers
            .iter()
            .filter(|(plugin, executor)| executor.owns_item(plugin, item_id));
        let Some((plugin, executor)) = matches.next() else {
            return Err(CoreError::Invalid(
                "selected plugin result is no longer current".to_owned(),
            ));
        };
        if matches.next().is_some() {
            return Err(CoreError::Invalid(
                "selected plugin result has ambiguous ownership".to_owned(),
            ));
        }
        executor.submit_plugin_action_by_id(plugin, item_id, action_id, argument)
    }

    /// Returns whether this registry has an exact endpoint for `plugin`.
    pub fn owns(&self, plugin: &PluginId) -> bool {
        self.providers.contains_key(plugin)
    }

    /// Opens a page on the runtime registered for `plugin`.
    ///
    /// An owner this router does not know is refused rather than routed to a
    /// sibling runtime, for the same reason action dispatch is: a page is a
    /// plugin drawing on the user's screen, and the wrong plugin drawing is
    /// worse than no page at all.
    pub fn open_page(
        &self,
        plugin: &PluginId,
        page_id: &str,
        width: u32,
        height: u32,
        palette: crikey_core::PagePalette,
    ) -> CoreResult<()> {
        self.providers
            .get(plugin)
            .ok_or_else(|| CoreError::Invalid(format!("no action runtime owns plugin `{}`", plugin.0)))?
            .open_plugin_page(plugin, page_id, width, height, palette)
    }

    /// Queues one page event for the page `plugin` has open.
    pub fn send_page_input(&self, plugin: &PluginId, input: PageInput) -> CoreResult<()> {
        self.providers
            .get(plugin)
            .ok_or_else(|| CoreError::Invalid(format!("no action runtime owns plugin `{}`", plugin.0)))?
            .send_plugin_page_input(input)
    }

    /// Tells the page `plugin` has open that the viewport changed.
    pub fn resize_page(&self, plugin: &PluginId, width: u32, height: u32) {
        if let Some(executor) = self.providers.get(plugin) {
            executor.resize_plugin_page(width, height);
        }
    }

    /// Identity of the page opening on the exact owning runtime.
    pub fn page_session_id(&self, plugin: &PluginId) -> Option<crate::PageSessionId> {
        self.providers.get(plugin)?.plugin_page_session_id()
    }

    /// Queues a palette update only for the identified live session.
    pub fn update_page_palette(
        &self,
        plugin: &PluginId,
        session: crate::PageSessionId,
        palette: crikey_core::PagePalette,
    ) -> CoreResult<()> {
        self.providers
            .get(plugin)
            .ok_or_else(|| CoreError::Invalid(format!("no action runtime owns plugin `{}`", plugin.0)))?
            .update_plugin_page_palette(session, palette)
    }

    /// Closes the page `plugin` has open.
    pub fn close_page(&self, plugin: &PluginId) {
        if let Some(executor) = self.providers.get(plugin) {
            executor.close_plugin_page();
        }
    }

    /// Takes the newest frame the page `plugin` has open produced.
    pub fn poll_page(&self, plugin: &PluginId) -> Option<PageUpdate> {
        self.providers.get(plugin)?.poll_plugin_page()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use crikey_core::{
        ActionId, ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId, Result as CoreResult,
    };
    use crikey_plugin_model::{
        ConcurrencySection, FilesystemAccess, FilesystemPermission, FilesystemScope, Permissions,
    };
    use crikey_plugin_supervisor::{shared_budget_from_section, BudgetKind, PluginBudgetHandle};

    use super::{ArgumentSchema, ArgumentValues, ExecutableKey, OwnedItem, ResultPresentation};
    use super::{HostCapability, PluginActionExecutor, PluginActionRouter};

    fn item(plugin: &str) -> Item {
        Item {
            stable_id: ItemId("item".to_owned()),
            plugin_id: PluginId(plugin.to_owned()),
            category: Category::Keyword,
            label: "item".to_owned(),
            description: String::new(),
            target: String::new(),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: HitPolicy::Recorded,
            score_hint: 0,
            metadata: BTreeMap::new(),
            actions: Vec::new(),
        }
    }

    fn root_sidecar(target: &Item) -> (super::RootSource, ExecutableKey, ResultPresentation) {
        let key = ExecutableKey {
            target: OwnedItem {
                owner: target.plugin_id.clone(),
                item: target.stable_id.clone(),
            },
            action: None,
        };
        let source = super::RootSource::Catalog(super::DeclaredCommandSource {
            instance: 1,
            generation: crikey_core::Generation::from_raw(1),
            producer: 7,
            publication: 1,
        });
        let presentation = ResultPresentation {
            schemas: vec![crikey_core::result_presentation::ExecutableSchemaBinding {
                executable: key.clone(),
                schema: Arc::new(ArgumentSchema::default()),
            }],
            ..Default::default()
        };
        (source, key, presentation)
    }

    #[test]
    fn root_sidecar_replacement_is_transactional_and_owner_qualified() {
        let target = item("native.root");
        let (source, key, presentation) = root_sidecar(&target);
        let mut ledger = super::RootSourceLedger::default();
        ledger
            .publish(
                target.plugin_id.clone(),
                source.clone(),
                &[target.clone()],
                presentation.clone(),
            )
            .unwrap();
        let binding = ledger.binding(&key, None).unwrap();
        let mut invalid = presentation;
        invalid.schemas[0].executable.target.owner = PluginId("sibling".into());
        assert!(ledger
            .publish(target.plugin_id.clone(), source, &[target.clone()], invalid)
            .is_err());
        assert_eq!(ledger.frozen_item(&binding).unwrap().plugin_id, target.plugin_id);
        let mut sibling = key;
        sibling.target.owner = PluginId("sibling".into());
        assert!(ledger.binding(&sibling, None).is_none());
    }

    #[test]
    fn retired_dynamic_binding_never_borrows_same_id_catalog() {
        let target = item("native.root");
        let (catalog, key, presentation) = root_sidecar(&target);
        let mut ledger = super::RootSourceLedger::default();
        ledger
            .publish(
                target.plugin_id.clone(),
                catalog.clone(),
                &[target.clone()],
                presentation.clone(),
            )
            .unwrap();
        let canonical = match &catalog {
            super::RootSource::Catalog(source) => source,
            _ => unreachable!(),
        };
        let static_binding = ledger.binding(&key, Some(canonical)).unwrap();
        let dynamic = super::RootSource::Suggestion {
            generation: crikey_core::Generation::from_raw(2),
            producer: 7,
        };
        ledger
            .publish(target.plugin_id.clone(), dynamic, &[target.clone()], presentation)
            .unwrap();
        let selected = ledger.binding(&key, None).unwrap();
        ledger.retire_suggestions();
        assert!(ledger.frozen_item(&selected).is_none());
        assert!(ledger.binding(&key, None).is_none());
        assert!(ledger.frozen_item(&static_binding).is_some());
        assert!(ledger.binding(&key, Some(canonical)).is_some());
    }

    #[test]
    fn owner_retirement_preserves_sibling_binding_and_presentation() {
        let failing = item("modern.failing");
        let healthy = item("modern.healthy");
        let mut ledger = super::RootSourceLedger::default();
        let (_, failing_key, failing_sidecar) = root_sidecar(&failing);
        let (_, healthy_key, healthy_sidecar) = root_sidecar(&healthy);
        let generation = crikey_core::Generation::from_raw(2);
        ledger
            .publish(
                failing.plugin_id.clone(),
                super::RootSource::Suggestion {
                    generation,
                    producer: 7,
                },
                &[failing.clone()],
                failing_sidecar,
            )
            .unwrap();
        ledger
            .publish(
                healthy.plugin_id.clone(),
                super::RootSource::Suggestion {
                    generation,
                    producer: 8,
                },
                &[healthy.clone()],
                healthy_sidecar,
            )
            .unwrap();
        let healthy_binding = ledger.binding(&healthy_key, None).unwrap();
        ledger.retire_owner_suggestions(&failing.plugin_id);
        assert!(ledger.binding(&failing_key, None).is_none());
        assert!(ledger.frozen_item(&healthy_binding).is_some());
        assert!(ledger.binding(&healthy_key, None).is_some());
        let snapshots = ledger.snapshots();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].owner, healthy.plugin_id);
        assert_eq!(snapshots[0].source, healthy_binding.source);
    }

    #[test]
    fn captured_scope_survives_execution_retirement_but_not_withdrawal_or_producer_replacement() {
        use crikey_core::result_presentation::{Scope, ScopeId};
        let target = item("native.scope");
        let (_, key, mut presentation) = root_sidecar(&target);
        presentation.scopes.push(Scope {
            id: ScopeId("secure".into()),
            label: "Secure".into(),
        });
        let source = super::RootSource::Suggestion {
            generation: crikey_core::Generation::from_raw(1),
            producer: 7,
        };
        let scope = super::RootScope {
            owner: target.plugin_id.clone(),
            id: ScopeId("secure".into()),
            source: source.clone(),
        };
        let mut ledger = super::RootSourceLedger::default();
        ledger
            .publish(
                target.plugin_id.clone(),
                source.clone(),
                &[target.clone()],
                presentation.clone(),
            )
            .unwrap();
        let binding = ledger.binding(&key, None).unwrap();
        assert!(ledger.capture_scope(Some(&scope)));
        ledger.retire_suggestions();
        assert!(ledger.scope_current(&scope));
        assert!(ledger.frozen_item(&binding).is_none());
        assert!(ledger.binding(&key, None).is_none());
        let fresh = super::RootSource::Suggestion {
            generation: crikey_core::Generation::from_raw(2),
            producer: 7,
        };
        ledger
            .publish(
                target.plugin_id.clone(),
                fresh,
                &[target.clone()],
                presentation.clone(),
            )
            .unwrap();
        assert!(ledger.scope_current(&scope));
        assert!(ledger.capture_scope(Some(&scope)));
        let undeclared = super::RootSource::Suggestion {
            generation: crikey_core::Generation::from_raw(3),
            producer: 7,
        };
        let mut withdrawn = presentation.clone();
        withdrawn.scopes.clear();
        ledger
            .publish(target.plugin_id.clone(), undeclared, &[target.clone()], withdrawn)
            .unwrap();
        assert!(!ledger.scope_current(&scope));
        assert!(!ledger.capture_scope(Some(&scope)));
        ledger
            .publish(
                target.plugin_id.clone(),
                source.clone(),
                &[target.clone()],
                presentation.clone(),
            )
            .unwrap();
        assert!(ledger.capture_scope(Some(&scope)));
        let replacement = super::RootSource::Suggestion {
            generation: crikey_core::Generation::from_raw(4),
            producer: 8,
        };
        ledger
            .publish(
                target.plugin_id.clone(),
                replacement,
                &[target.clone()],
                presentation.clone(),
            )
            .unwrap();
        assert!(!ledger.scope_current(&scope));
        ledger
            .publish(target.plugin_id.clone(), source, &[target], presentation)
            .unwrap();
        assert!(ledger.capture_scope(Some(&scope)));
        ledger.capture_scope(None);
        ledger.retire_suggestions();
        assert!(!ledger.scope_current(&scope));
    }

    #[test]
    fn changed_schema_and_source_revoke_frozen_binding() {
        use crikey_core::structured_arguments::{ArgumentField, FieldId, FieldKind};
        let target = item("modern.root");
        let (source, key, mut presentation) = root_sidecar(&target);
        let mut ledger = super::RootSourceLedger::default();
        ledger
            .publish(
                target.plugin_id.clone(),
                source.clone(),
                &[target.clone()],
                presentation.clone(),
            )
            .unwrap();
        let binding = ledger.binding(&key, None).unwrap();
        presentation.schemas[0].schema = Arc::new(ArgumentSchema {
            fields: vec![ArgumentField {
                id: FieldId("password".into()),
                label: "Password".into(),
                required: true,
                placeholder: None,
                validation_message: None,
                kind: FieldKind::Password {
                    min_bytes: 1,
                    max_bytes: 64,
                },
            }],
        });
        ledger
            .publish(target.plugin_id.clone(), source, &[target], presentation)
            .unwrap();
        assert!(ledger.frozen_item(&binding).is_none());
        let current = ledger.binding(&key, None).unwrap();
        assert_ne!(binding.fingerprint, current.fingerprint);
        assert!(current
            .schema
            .validate_values(&ArgumentValues::default())
            .is_err());
    }

    #[test]
    fn sidecar_none_means_actionless_not_an_invented_primary() {
        use crikey_core::{Action, ExecutionPolicy};
        let mut target = item("native.root");
        let (source, key, mut presentation) = root_sidecar(&target);
        target.actions.push(Action {
            action_id: ActionId("actual-primary".into()),
            label: "Run".into(),
            description: String::new(),
            applicable_categories: vec![target.category.clone()],
            icon_reference: None,
            execution_policy: ExecutionPolicy::Plugin,
        });
        let mut ledger = super::RootSourceLedger::default();
        assert!(ledger
            .publish(
                target.plugin_id.clone(),
                source.clone(),
                &[target.clone()],
                presentation.clone()
            )
            .is_err());
        presentation.schemas[0].executable.action = Some(ActionId("actual-primary".into()));
        ledger
            .publish(target.plugin_id.clone(), source, &[target], presentation)
            .unwrap();
        assert!(ledger.binding(&key, None).is_none());
        let mut actual = key;
        actual.action = Some(ActionId("actual-primary".into()));
        assert!(ledger.binding(&actual, None).is_some());
    }

    #[test]
    fn retired_origin_tombstones_are_bounded_and_never_reenable_catalog_borrowing() {
        let target = item("native.root");
        let (catalog, key, presentation) = root_sidecar(&target);
        let canonical = match &catalog {
            super::RootSource::Catalog(source) => source.clone(),
            _ => unreachable!(),
        };
        let mut ledger = super::RootSourceLedger::default();
        ledger
            .publish(
                target.plugin_id.clone(),
                catalog,
                &[target.clone()],
                presentation.clone(),
            )
            .unwrap();
        for index in 0..=crikey_core::result_presentation::MAX_PRESENTATION_ITEMS {
            let mut dynamic = target.clone();
            dynamic.stable_id = ItemId(format!("dynamic-{index}"));
            ledger
                .publish(
                    target.plugin_id.clone(),
                    super::RootSource::Suggestion {
                        generation: crikey_core::Generation::from_raw(index as u64 + 2),
                        producer: 7,
                    },
                    &[dynamic],
                    ResultPresentation::default(),
                )
                .unwrap();
            ledger.retire_suggestions();
        }
        assert!(ledger.retired.len() <= crikey_core::result_presentation::MAX_PRESENTATION_ITEMS);
        assert!(ledger.binding(&key, None).is_none());
        assert!(ledger.binding(&key, Some(&canonical)).is_some());
    }

    #[test]
    fn typed_router_moves_values_and_never_grants_query_collection() {
        use crikey_core::structured_arguments::{
            ArgumentField, FieldId, FieldKind, Password, StructuredValue,
        };
        struct TypedExecutor(AtomicBool);
        impl PluginActionExecutor for TypedExecutor {
            fn submit_plugin_action(
                &self,
                _: &PluginId,
                _: &Item,
                _: &ActionId,
                _: Option<&str>,
            ) -> CoreResult<super::ActionRequestId> {
                panic!("typed input must never fall through to legacy execution")
            }
            fn submit_typed(
                &self,
                binding: &super::RootExecutableBinding,
                values: ArgumentValues,
            ) -> CoreResult<super::TypedActionReceipt> {
                assert!(binding.schema.validate_values(&values).is_ok());
                self.0.store(true, Ordering::Release);
                Ok(super::TypedActionReceipt {
                    receipt: super::PluginActionReceipt {
                        request_id: super::ActionRequestId {
                            plugin: binding.executable.target.owner.clone(),
                            sequence: 1,
                        },
                        query_history_allowed: true,
                    },
                    item: item(&binding.executable.target.owner.0),
                })
            }
        }
        let target = item("modern.private");
        let (source, key, _) = root_sidecar(&target);
        let schema = Arc::new(ArgumentSchema {
            fields: vec![ArgumentField {
                id: FieldId("password".into()),
                label: "Password".into(),
                required: true,
                placeholder: None,
                validation_message: None,
                kind: FieldKind::Password {
                    min_bytes: 1,
                    max_bytes: 64,
                },
            }],
        });
        let binding = super::RootExecutableBinding {
            executable: key,
            source,
            fingerprint: schema.fingerprint().unwrap(),
            schema,
        };
        let executor = Arc::new(TypedExecutor(AtomicBool::new(false)));
        let mut router = PluginActionRouter::default();
        router.register([target.plugin_id], executor.clone()).unwrap();
        assert!(router.submit_typed(&binding, ArgumentValues::default()).is_err());
        assert!(!executor.0.load(Ordering::Acquire));
        let mut values = ArgumentValues::default();
        values.0.insert(
            FieldId("password".into()),
            StructuredValue::Password(Password::new("private-value".into()).unwrap()),
        );
        assert!(!format!("{values:?}").contains("private-value"));
        let receipt = router.submit_typed(&binding, values).unwrap();
        assert!(!receipt.receipt.query_history_allowed);
        assert!(executor.0.load(Ordering::Acquire));
    }

    #[test]
    fn declared_validation_keeps_exact_primary_action_policy_and_arguments() {
        use crikey_core::command::{CommandAvailability, CommandDeclaration, CommandRef, ExecutableTarget};
        use crikey_core::{Action, ExecutionPolicy};
        let mut target = item("native.commands");
        let mut declaration = CommandDeclaration {
            reference: CommandRef {
                provider_id: target.plugin_id.clone(),
                command_id: "run".into(),
            },
            label: "Run".into(),
            executable_target: ExecutableTarget::Item {
                item_id: target.stable_id.clone(),
                action_id: None,
            },
            availability: CommandAvailability::Available,
            argument_policy: ArgumentPolicy::Forbidden,
        };
        let validate = |declaration: &CommandDeclaration, item: &Item, argument| {
            super::validate_declared_command_item(declaration, item, argument)
        };
        assert_eq!(validate(&declaration, &target, None).unwrap(), None);
        assert!(validate(&declaration, &target, Some("not allowed")).is_err());
        target.actions.push(Action {
            action_id: ActionId("primary".into()),
            label: "Primary".into(),
            description: String::new(),
            applicable_categories: vec![Category::Keyword],
            icon_reference: None,
            execution_policy: ExecutionPolicy::Plugin,
        });
        let mut alternate = target.actions[0].clone();
        alternate.action_id = ActionId("alternate".into());
        target.actions.push(alternate);
        assert_eq!(
            validate(&declaration, &target, None).unwrap(),
            Some(ActionId("primary".into()))
        );
        declaration.executable_target = ExecutableTarget::Item {
            item_id: target.stable_id.clone(),
            action_id: Some(ActionId("alternate".into())),
        };
        assert_eq!(
            validate(&declaration, &target, None).unwrap(),
            Some(ActionId("alternate".into()))
        );
        target.actions[1].applicable_categories = vec![Category::Application];
        assert!(validate(&declaration, &target, None).is_err());
        target.actions[1].applicable_categories.clear();
        target.actions[1].execution_policy = ExecutionPolicy::HostMediated;
        assert!(validate(&declaration, &target, None).is_err());
        target.actions[1].execution_policy = ExecutionPolicy::Plugin;
        target.actions.push(target.actions[1].clone());
        assert!(
            validate(&declaration, &target, None).is_err(),
            "ambiguous IDs never select the first match"
        );
        target.actions.pop();
        target.argument_policy = ArgumentPolicy::Required;
        assert!(
            validate(&declaration, &target, Some("captured")).is_err(),
            "policy equality is authoritative"
        );
        declaration.argument_policy = ArgumentPolicy::Required;
        assert!(validate(&declaration, &target, None).is_err());
        assert!(validate(&declaration, &target, Some("")).is_err());
        assert!(validate(&declaration, &target, Some("captured as one string")).is_ok());
        target.plugin_id = PluginId("other.owner".into());
        assert!(validate(&declaration, &target, Some("captured")).is_err());
    }

    #[test]
    fn retired_dynamic_override_refuses_admission_even_with_canonical_same_owner_and_id() {
        let owner = PluginId("native.override".into());
        let mut dynamic = item(&owner.0);
        dynamic.target = "selected dynamic target".into();
        let key = (owner.clone(), dynamic.stable_id.clone());
        let mut canonical = dynamic.clone();
        canonical.target = "canonical replacement target".into();
        canonical.argument_policy = ArgumentPolicy::Required;

        // Cover both retained-producer and released-static canonical sources.
        for released_static in [false, true] {
            let suggestions = Mutex::new(BTreeMap::from([(key.clone(), (dynamic.clone(), 11))]));
            let catalog = Mutex::new(if released_static {
                BTreeMap::new()
            } else {
                BTreeMap::from([(key.clone(), (canonical.clone(), 9))])
            });
            let static_catalog = Mutex::new(if released_static {
                BTreeMap::from([(key.clone(), canonical.clone())])
            } else {
                BTreeMap::new()
            });

            // Retirement/replacement happened before admission, not after enqueue.
            for live_origin in [None, Some(12), Some(0)] {
                assert!(
                    super::resolve_action_item(
                        &owner,
                        &dynamic.stable_id,
                        live_origin,
                        &suggestions,
                        &catalog,
                        &static_catalog,
                    )
                    .is_none(),
                    "a stale dynamic selection cannot become a canonical command"
                );
            }
            let (live, origin) = super::resolve_action_item(
                &owner,
                &dynamic.stable_id,
                Some(11),
                &suggestions,
                &catalog,
                &static_catalog,
            )
            .unwrap();
            assert_eq!(live.target, "selected dynamic target");
            assert_eq!(origin, Some(11));

            // Only removal of the override permits a fresh canonical admission.
            suggestions.lock().unwrap().clear();
            let (resolved, origin) = super::resolve_action_item(
                &owner,
                &dynamic.stable_id,
                None,
                &suggestions,
                &catalog,
                &static_catalog,
            )
            .unwrap();
            assert_eq!(resolved.target, "canonical replacement target");
            assert_eq!(resolved.argument_policy, ArgumentPolicy::Required);
            assert_eq!(origin, None);
        }
    }

    #[derive(Debug, Default)]
    struct RecordingExecutor {
        calls: Mutex<Vec<PluginId>>,
        next: AtomicU64,
    }

    impl PluginActionExecutor for RecordingExecutor {
        fn submit_plugin_action(
            &self,
            plugin: &PluginId,
            _item: &Item,
            _action_id: &ActionId,
            _argument: Option<&str>,
        ) -> CoreResult<super::ActionRequestId> {
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(plugin.clone());
            Ok(super::ActionRequestId {
                plugin: plugin.clone(),
                sequence: self.next.fetch_add(1, Ordering::Relaxed) + 1,
            })
        }
    }

    #[test]
    fn unattested_legacy_or_custom_adapters_never_authorize_query_collection() {
        let executor = Arc::new(RecordingExecutor::default());
        let owner = PluginId("legacy.freeform".into());
        let mut router = PluginActionRouter::default();
        router.register([owner.clone()], executor.clone()).unwrap();
        let receipt = router
            .submit(
                &owner,
                &item(&owner.0),
                &ActionId("run".into()),
                Some("free-form value"),
            )
            .unwrap();
        assert!(!receipt.query_history_allowed);
        assert_eq!(executor.calls.lock().unwrap().as_slice(), [owner]);
    }

    #[test]
    fn routes_only_to_exact_registered_owner() {
        let executor = Arc::new(RecordingExecutor::default());
        let mut router = PluginActionRouter::default();
        router
            .register([PluginId("modern.alpha".to_owned())], executor.clone())
            .unwrap();
        assert!(router.owns(&PluginId("modern.alpha".to_owned())));
        assert!(!router.owns(&PluginId("native.alpha".to_owned())));
        let request = router
            .submit(
                &PluginId("modern.alpha".to_owned()),
                &item("modern.alpha"),
                &ActionId("run".to_owned()),
                None,
            )
            .unwrap()
            .request_id;
        assert_eq!(request.plugin, PluginId("modern.alpha".to_owned()));
        let error = router
            .submit(
                &PluginId("native.alpha".to_owned()),
                &item("native.alpha"),
                &ActionId("run".to_owned()),
                None,
            )
            .expect_err("an unknown owner must not fall through to another provider");
        assert!(error.to_string().contains("no action runtime owns plugin"));
        assert_eq!(
            executor
                .calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_slice(),
            &[PluginId("modern.alpha".to_owned())]
        );
    }

    #[test]
    fn web_permissions_follow_exact_manifest_owners_not_the_shared_executor() {
        let executor = Arc::new(RecordingExecutor::default());
        let denied = PluginId("modern.page".to_owned());
        let networked = PluginId("native.page".to_owned());
        let persistent = PluginId("native.persistent".to_owned());
        let mut router = PluginActionRouter::default();
        router
            .register_with_permissions(
                [
                    (denied.clone(), Permissions::default()),
                    (
                        networked.clone(),
                        Permissions {
                            network: true,
                            ..Permissions::default()
                        },
                    ),
                    (
                        persistent.clone(),
                        Permissions {
                            network: true,
                            web_storage: true,
                            ..Permissions::default()
                        },
                    ),
                ],
                executor,
            )
            .expect("owners share one endpoint");
        assert!(router.admit_web_surface(&denied, true, false).is_err());
        assert!(router.admit_web_surface(&networked, true, false).is_ok());
        let refusal = router
            .admit_web_surface(&networked, true, true)
            .expect_err("network alone does not authorize durable cookies");
        assert!(refusal.to_string().contains("native.page"));
        assert!(refusal.to_string().contains("permissions.web-storage"));
        assert!(router.admit_web_surface(&persistent, true, true).is_ok());
        assert!(router
            .admit_web_surface(&PluginId("page".to_owned()), true, true)
            .is_err());
        assert!(router
            .admit_web_surface(&PluginId("legacy.page".to_owned()), false, false)
            .is_err());
    }
    #[test]
    fn host_process_permission_is_enforced_per_manifest_owner() {
        let executor = Arc::new(RecordingExecutor::default());
        let modern = PluginId("modern.denied".to_owned());
        let native = PluginId("native.granted".to_owned());
        let legacy = PluginId("legacy.trusted".to_owned());
        let mut router = PluginActionRouter::default();
        router
            .register_with_permissions([(modern.clone(), Permissions::default())], executor.clone())
            .unwrap();
        router
            .register_with_permissions(
                [(
                    native.clone(),
                    Permissions {
                        process: true,
                        ..Permissions::default()
                    },
                )],
                executor.clone(),
            )
            .unwrap();
        router.register([legacy.clone()], executor).unwrap();

        assert!(!router.permits(&modern, HostCapability::ProcessLaunch));
        assert!(router.permits(&native, HostCapability::ProcessLaunch));
        // A legacy package declares nothing, and the baseline the host applies
        // in its place grants exactly this.
        assert!(router.permits(&legacy, HostCapability::ProcessLaunch));
        assert!(!router.permits(&PluginId("unknown".to_owned()), HostCapability::ProcessLaunch));

        let refusal = router
            .authorize(&modern, HostCapability::ProcessLaunch)
            .expect_err("a plugin without the process grant must be refused");
        assert_eq!(
            refusal.to_string(),
            "plugin `modern.denied` lacks the process permission for host-mediated process launch",
            "a refusal must name the owner and the manifest field that would grant it"
        );
        router
            .authorize(&native, HostCapability::ProcessLaunch)
            .expect("a plugin that declared the process grant must be admitted");
    }

    /// The refusal has to be attributable to one owner. A router that answered
    /// per provider endpoint would deny two plugins sharing one worker because
    /// the stricter of the two declared nothing.
    #[test]
    fn a_refusal_is_scoped_to_one_owner_and_not_to_its_provider_endpoint() {
        let executor = Arc::new(RecordingExecutor::default());
        let denied = PluginId("modern.denied".to_owned());
        let granted = PluginId("modern.granted".to_owned());
        let mut router = PluginActionRouter::default();
        router
            .register_with_permissions(
                [
                    (denied.clone(), Permissions::default()),
                    (
                        granted.clone(),
                        Permissions {
                            process: true,
                            ..Permissions::default()
                        },
                    ),
                ],
                executor,
            )
            .unwrap();

        assert!(router.authorize(&denied, HostCapability::ProcessLaunch).is_err());
        assert!(router.authorize(&granted, HostCapability::ProcessLaunch).is_ok());
    }

    /// The one filesystem read the host performs for a plugin. Undeclared must
    /// keep working, or every plugin written before this gate loses its icons;
    /// an author who declares `none` and nothing else is taken at their word.
    #[test]
    fn the_package_read_grant_is_implicit_but_an_explicit_none_scope_refuses_it() {
        let executor = Arc::new(RecordingExecutor::default());
        let silent = PluginId("modern.silent".to_owned());
        let renouncing = PluginId("modern.renouncing".to_owned());
        let scoped = PluginId("modern.scoped".to_owned());
        let mut router = PluginActionRouter::default();
        router
            .register_with_permissions(
                [
                    (silent.clone(), Permissions::default()),
                    (
                        renouncing.clone(),
                        Permissions {
                            filesystem: vec![FilesystemPermission {
                                scope: FilesystemScope::None,
                                access: FilesystemAccess::Read,
                            }],
                            ..Permissions::default()
                        },
                    ),
                    (
                        scoped.clone(),
                        Permissions {
                            filesystem: vec![FilesystemPermission {
                                scope: FilesystemScope::Package,
                                access: FilesystemAccess::Read,
                            }],
                            ..Permissions::default()
                        },
                    ),
                ],
                executor,
            )
            .unwrap();

        assert!(router.permits(&silent, HostCapability::PackageFileRead));
        assert!(router.permits(&scoped, HostCapability::PackageFileRead));
        let refusal = router
            .authorize(&renouncing, HostCapability::PackageFileRead)
            .expect_err("a plugin that declared no filesystem scope must be refused");
        assert_eq!(
            refusal.to_string(),
            "plugin `modern.renouncing` lacks the filesystem permission for host-mediated \
             package resource read"
        );
    }

    #[derive(Debug)]
    struct BudgetExecutor {
        budget: PluginBudgetHandle,
        entered: AtomicBool,
        release: AtomicBool,
    }

    impl PluginActionExecutor for BudgetExecutor {
        fn submit_plugin_action(
            &self,
            plugin: &PluginId,
            _item: &Item,
            _action_id: &ActionId,
            _argument: Option<&str>,
        ) -> CoreResult<super::ActionRequestId> {
            let guard = self
                .budget
                .try_acquire_owned(BudgetKind::Action)
                .ok_or(crikey_core::CoreError::CapacityExceeded("action budget"))?;
            self.entered.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                thread::yield_now();
            }
            drop(guard);
            Ok(super::ActionRequestId {
                plugin: plugin.clone(),
                sequence: 1,
            })
        }
    }

    #[test]
    fn action_budget_refuses_a_concurrent_second_call() {
        let budget = shared_budget_from_section(&ConcurrencySection {
            max_action_requests: Some(1),
            ..ConcurrencySection::default()
        });
        let executor = Arc::new(BudgetExecutor {
            budget,
            entered: AtomicBool::new(false),
            release: AtomicBool::new(false),
        });
        let mut router = PluginActionRouter::default();
        router
            .register([PluginId("modern.alpha".to_owned())], executor.clone())
            .unwrap();
        let router = Arc::new(router);
        let worker_router = Arc::clone(&router);
        let worker = thread::spawn(move || {
            worker_router
                .submit(
                    &PluginId("modern.alpha".to_owned()),
                    &item("modern.alpha"),
                    &ActionId("run".to_owned()),
                    None,
                )
                .unwrap();
        });
        while !executor.entered.load(Ordering::Acquire) {
            thread::yield_now();
        }
        let refused = router
            .submit(
                &PluginId("modern.alpha".to_owned()),
                &item("modern.alpha"),
                &ActionId("run".to_owned()),
                None,
            )
            .expect_err("the configured action budget must refuse concurrent work");
        assert!(refused.to_string().contains("action budget"));
        executor.release.store(true, Ordering::Release);
        worker.join().unwrap();
        assert_eq!(executor.budget.in_flight(BudgetKind::Action), 0);
    }

    #[derive(Debug)]
    struct SlowExecutor {
        release: Arc<AtomicBool>,
        completions: Mutex<std::sync::mpsc::Receiver<super::PluginActionCompletion>>,
        sender: std::sync::mpsc::SyncSender<super::PluginActionCompletion>,
        next: AtomicU64,
    }

    impl SlowExecutor {
        fn new() -> Arc<Self> {
            let (sender, completions) = std::sync::mpsc::sync_channel(2);
            Arc::new(Self {
                release: Arc::new(AtomicBool::new(false)),
                completions: Mutex::new(completions),
                sender,
                next: AtomicU64::new(0),
            })
        }
    }

    impl PluginActionExecutor for SlowExecutor {
        fn submit_plugin_action(
            &self,
            plugin: &PluginId,
            item: &Item,
            action_id: &ActionId,
            _argument: Option<&str>,
        ) -> CoreResult<super::ActionRequestId> {
            let request_id = super::ActionRequestId {
                plugin: plugin.clone(),
                sequence: self.next.fetch_add(1, Ordering::Relaxed) + 1,
            };
            let release = Arc::clone(&self.release);
            let sender = self.sender.clone();
            let request_id_for_thread = request_id.clone();
            let plugin = plugin.clone();
            let item_id = item.stable_id.clone();
            let action_id = action_id.clone();
            thread::spawn(move || {
                while !release.load(Ordering::Acquire) {
                    thread::yield_now();
                }
                let _ = sender.send(super::PluginActionCompletion {
                    request_id: request_id_for_thread,
                    plugin,
                    item_id,
                    action_id: Some(action_id),
                    outcome: Ok(super::ActionEffect::Completed),
                });
            });
            Ok(request_id)
        }

        fn poll_plugin_actions(&self) -> Vec<super::PluginActionCompletion> {
            let receiver = self.completions.lock().unwrap_or_else(|error| error.into_inner());
            std::iter::from_fn(|| receiver.try_recv().ok()).collect()
        }
    }

    #[test]
    fn slow_action_submission_does_not_block_following_event() {
        let executor = SlowExecutor::new();
        let mut router = PluginActionRouter::default();
        router
            .register([PluginId("modern.slow".to_owned())], executor.clone())
            .unwrap();

        let request = router
            .submit(
                &PluginId("modern.slow".to_owned()),
                &item("modern.slow"),
                &ActionId("slow".to_owned()),
                None,
            )
            .unwrap()
            .request_id;
        // A second event/submission is accepted while the first action is
        // deliberately held before completion.
        let second_request = router
            .submit(
                &PluginId("modern.slow".to_owned()),
                &item("modern.slow"),
                &ActionId("second".to_owned()),
                None,
            )
            .unwrap()
            .request_id;
        assert_ne!(second_request, request);
        assert!(router.poll().is_empty());

        executor.release.store(true, Ordering::Release);
        let mut completions = Vec::new();
        for _ in 0..10_000 {
            completions.extend(router.poll());
            if completions.len() == 2 {
                break;
            }
            thread::yield_now();
        }
        assert_eq!(completions.len(), 2);
        assert!(completions.iter().all(|completion| completion.outcome.is_ok()));
        assert!(completions
            .iter()
            .any(|completion| completion.request_id == request));
        assert!(completions
            .iter()
            .any(|completion| completion.request_id == second_request));
    }

    #[test]
    fn direct_result_dispatch_never_borrows_a_colliding_siblings_snapshot() {
        struct Snapshot {
            available: AtomicBool,
            retire_on_lookup: AtomicBool,
            calls: Mutex<Vec<(PluginId, ActionId)>>,
        }
        impl PluginActionExecutor for Snapshot {
            fn submit_plugin_action(
                &self,
                _owner: &PluginId,
                _item: &Item,
                _action: &ActionId,
                _argument: Option<&str>,
            ) -> CoreResult<super::ActionRequestId> {
                panic!("direct rows must use the provider's current snapshot");
            }
            fn owns_item(&self, _owner: &PluginId, item: &ItemId) -> bool {
                let live = item.0 == "shared" && self.available.load(Ordering::SeqCst);
                if self.retire_on_lookup.load(Ordering::SeqCst) {
                    self.available.store(false, Ordering::SeqCst);
                }
                live
            }
            fn submit_plugin_action_by_id(
                &self,
                owner: &PluginId,
                _item: &ItemId,
                action: &ActionId,
                _argument: Option<&str>,
            ) -> CoreResult<super::ActionRequestId> {
                if !self.available.load(Ordering::SeqCst) || action.0 != "reveal" {
                    return Err(crikey_core::CoreError::Invalid("snapshot retired".into()));
                }
                self.calls.lock().unwrap().push((owner.clone(), action.clone()));
                Ok(super::ActionRequestId {
                    plugin: owner.clone(),
                    sequence: 1,
                })
            }
        }
        let snapshot = || {
            Arc::new(Snapshot {
                available: AtomicBool::new(true),
                retire_on_lookup: AtomicBool::new(false),
                calls: Mutex::new(Vec::new()),
            })
        };
        let first = snapshot();
        let sibling = snapshot();
        let owner = PluginId("first".into());
        let other = PluginId("sibling".into());
        let item = ItemId("shared".into());
        let action = ActionId("reveal".into());
        let mut router = PluginActionRouter::default();
        router.register([owner.clone()], first.clone()).unwrap();
        router.register([other], sibling.clone()).unwrap();
        router.submit_owned_item(&owner, &item, &action, None).unwrap();
        assert_eq!(
            *first.calls.lock().unwrap(),
            vec![(owner.clone(), action.clone())]
        );
        assert!(sibling.calls.lock().unwrap().is_empty());
        first.retire_on_lookup.store(true, Ordering::SeqCst);
        assert!(router.submit_owned_item(&owner, &item, &action, None).is_err());
        assert!(router.submit_owned_item(&owner, &item, &action, None).is_err());
        assert_eq!(first.calls.lock().unwrap().len(), 1);
        assert!(sibling.calls.lock().unwrap().is_empty());
    }
}
