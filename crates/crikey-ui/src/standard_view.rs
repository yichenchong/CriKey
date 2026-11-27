//! Host-owned standard surfaces on the launcher's existing navigation stack.
use super::*;
use crikey_core::structured_arguments::{ExecutableKey, OwnedItem};
use crikey_core::structured_view::*;
use std::num::NonZeroU64;

#[derive(Debug, Clone)]
pub struct StandardSurface {
    pub owner: PluginId,
    pub producer: u64,
    /// Latest successful endpoint correlation; presentation may lag only while refresh is pending.
    pub context: ViewExecutionContext,
    /// Last final-published presentation; also defines the current P3 intent namespace.
    pub descriptor: Arc<ViewDescriptor>,
    /// Exact latest endpoint definitions, retained only for correlation and accounting.
    pub(crate) admitted_descriptor: Arc<ViewDescriptor>,
}
impl PartialEq for StandardSurface {
    fn eq(&self, other: &Self) -> bool {
        self.owner == other.owner
            && self.producer == other.producer
            && self.context == other.context
            && Arc::ptr_eq(&self.descriptor, &other.descriptor)
            && Arc::ptr_eq(&self.admitted_descriptor, &other.admitted_descriptor)
    }
}
impl LauncherViewModel {
    /// Retire pointer gestures on a new user intent or launcher lifecycle transition.
    pub fn clear_standard_activation(&mut self) {
        self.standard_pointer_source = None;
        self.standard_deferred_activation = None;
        self.standard_ready_activation = None;
    }

    pub(crate) fn standard_activation_bytes(&self) -> usize {
        let source = self
            .standard_pointer_source
            .as_ref()
            .filter(|source| {
                self.standard
                    .as_ref()
                    .is_none_or(|current| !Arc::ptr_eq(source, current))
            })
            .map_or(0, |source| standard_surface_bytes(source));
        let deferred = self
            .standard_deferred_activation
            .as_ref()
            .map_or(0, executable_bytes);
        let ready = match &self.standard_ready_activation {
            Some(UiCommand::StandardActivate { context, executable }) => {
                std::mem::size_of::<UiCommand>() + context.route.0.capacity() + executable_bytes(executable)
                    - std::mem::size_of::<ExecutableKey>()
            }
            _ => 0,
        };
        source + deferred + ready
    }

    /// Consume a revalidated deliberate gesture through ordinary execution admission.
    pub fn take_standard_activation(&mut self) -> Option<UiEffect> {
        if self.standard_refresh_pending {
            return None;
        }
        let command = self.standard_ready_activation.take()?;
        self.apply(command)
    }

    fn reconcile_standard_activation(&mut self) {
        let source = self.standard_pointer_source.take();
        let executable = self.standard_deferred_activation.take();
        let (Some(source), Some(executable), Some(current)) = (source, executable, &self.standard) else {
            return;
        };
        if source.owner == current.owner
            && source.producer == current.producer
            && source.context.session == current.context.session
            && source.context.route == current.context.route
            && source.context.revision < current.context.revision
            && self.standard_selected_target().as_ref() == Some(&executable.target)
            && unchanged_executable(&source.descriptor, &current.descriptor, &executable)
        {
            self.standard_ready_activation = Some(UiCommand::StandardActivate {
                context: current.context.clone(),
                executable,
            });
        }
    }

    pub fn set_standard_refresh_pending(&mut self, pending: bool) {
        self.standard_refresh_pending = pending;
        self.dirty |= self.visible;
    }

    /// Synchronize the installed endpoint revision without painting a superseded
    /// query response or replacing the user's current P3 restoration intent.
    pub fn adopt_standard_revision(&mut self, owner: &PluginId, producer: u64, reply: ViewReply) -> bool {
        let Some(current) = &self.standard else {
            return false;
        };
        if !self.visible
            || &current.owner != owner
            || current.producer != producer
            || current.context.session != reply.context.session
            || current.context.route != reply.context.route
            || current.context.revision >= reply.context.revision
        {
            return false;
        }
        let presentation = current.descriptor.clone();
        let candidate = Arc::new(StandardSurface {
            owner: owner.clone(),
            producer,
            context: reply.context,
            descriptor: presentation,
            admitted_descriptor: reply.descriptor,
        });
        let mut entry = self.capture_entry();
        entry.admitted_descriptor = Some(candidate.admitted_descriptor.clone());
        entry.render_standard = self.standard_pointer_source.is_some();
        entry.transient_bytes = entry
            .transient_bytes
            .saturating_sub(current.owner.0.capacity() + current.context.route.0.capacity())
            .saturating_add(candidate.owner.0.capacity() + candidate.context.route.0.capacity());
        if self
            .standard_pointer_source
            .as_ref()
            .is_some_and(|source| Arc::ptr_eq(source, current))
        {
            entry.transient_bytes = entry
                .transient_bytes
                .saturating_add(standard_surface_bytes(current));
        }
        if navigation::retained_bytes(self.navigation_stack.iter().chain(std::iter::once(&entry)))
            > navigation::MAX_NAVIGATION_BYTES
        {
            self.standard_unavailable();
            return false;
        }
        self.standard = Some(candidate);
        self.set_standard_refresh_pending(true);
        true
    }

    fn standard_refresh_effect(&mut self, reason: ViewRefreshReason) -> UiEffect {
        self.close_actions();
        self.set_standard_refresh_pending(true);
        UiEffect::RefreshStandard(reason)
    }

    fn select_standard(&mut self, index: usize, follow: bool) {
        let target = self
            .rows
            .get(index.min(self.rows.len().saturating_sub(1)))
            .map(|row| &row.target);
        if self.inline_arguments.as_ref().is_some_and(|input| {
            !matches!(target, Some(ActionTarget::Result { owner, item }) if owner == &input.executable.target.owner && item == &input.executable.target.item)
        }) {
            self.inline_arguments = None;
        }
        self.select(index);
        self.standard_follow_selection = follow;
        self.preserve_scroll = !follow;
    }
    pub(crate) fn standard_selection_available(&self) -> bool {
        if self
            .restoration_target
            .as_ref()
            .is_some_and(|target| !self.rows.iter().any(|row| target.matches(row)))
        {
            return false;
        }
        let Some(ActionTarget::Result { owner, item }) = self.rows.get(self.selected).map(|row| &row.target)
        else {
            return false;
        };
        let Some(surface) = &self.standard else {
            return false;
        };
        let matches = |target: &OwnedItem| &target.owner == owner && &target.item == item;
        let collection = match &surface.descriptor.view {
            StandardView::List(collection) => collection,
            StandardView::Grid(grid) => &grid.collection,
            StandardView::Detail(detail) => return detail.target.as_ref().is_some_and(matches),
            StandardView::Form(form) => return matches(&form.submit.target),
        };
        collection.items.iter().any(|p| {
            matches(&p.target)
                && self
                    .navigation_scope
                    .as_ref()
                    .is_none_or(|scope| p.scopes.contains(&scope.id))
        })
    }
    pub(crate) fn standard_selected_target(&self) -> Option<OwnedItem> {
        if !self.standard_selection_available() {
            return None;
        }
        let ActionTarget::Result { owner, item } = &self.rows.get(self.selected)?.target else {
            return None;
        };
        Some(OwnedItem {
            owner: owner.clone(),
            item: item.clone(),
        })
    }
    pub(crate) fn within_navigation_budget(&self) -> bool {
        let entry = self.capture_entry();
        navigation::retained_bytes(self.navigation_stack.iter().chain(std::iter::once(&entry)))
            <= navigation::MAX_NAVIGATION_BYTES
    }
    pub fn standard_unavailable(&mut self) {
        self.clear_standard_activation();
        let Some(surface) = self.standard.take() else {
            return;
        };
        self.page_reopen = Some(NavigationRoute::Standard {
            owner: surface.owner.clone(),
            producer: surface.producer,
            logical_view: surface.context.session.logical_view,
            route: surface.context.route.clone(),
            title: self.navigation_title.to_string(),
            collection: matches!(
                surface.descriptor.view,
                StandardView::List(_) | StandardView::Grid(_)
            ),
        });
        self.inline_arguments = None;
        self.rows = Arc::default();
        self.action_panel = None;
        self.publication_view = None;
        self.set_navigation_notice("View unavailable. Go Back or open Settings.".into());
        self.set_standard_refresh_pending(false);
    }
    pub(crate) fn apply_standard_command(&mut self, command: &UiCommand) -> Option<Option<UiEffect>> {
        let surface = self.standard.as_ref()?;
        if let UiCommand::StandardDoubleClick { context, executable } = command {
            if self.standard_refresh_pending {
                let source = self.standard_pointer_source.as_ref()?;
                if context == &source.context
                    && source.owner == surface.owner
                    && source.producer == surface.producer
                    && source.context.session == surface.context.session
                    && source.context.route == surface.context.route
                    && self.standard_selected_target().as_ref() == Some(&executable.target)
                    && source.descriptor.executable(executable).is_some()
                {
                    self.standard_deferred_activation = Some(executable.clone());
                    if !self.within_navigation_budget() {
                        self.clear_standard_activation();
                    }
                }
                return Some(None);
            }
        }
        if self.standard_refresh_pending
            && matches!(
                command,
                UiCommand::StandardActivate { .. }
                    | UiCommand::StandardLink { .. }
                    | UiCommand::ExecuteDefault
                    | UiCommand::ShowActions
                    | UiCommand::ActivateAction { .. }
                    | UiCommand::ActivateSelectedAction
            )
        {
            return Some(None);
        }
        match command {
            UiCommand::StandardSelect { context, target } => {
                if context != &surface.context || self.standard_refresh_pending {
                    return Some(None);
                }
                let index = self.rows.iter().position(|row| matches!(&row.target, ActionTarget::Result { owner, item } if owner == &target.owner && item == &target.item))?;
                if index == self.selected {
                    return Some(None);
                }
                let pointer_source =
                    matches!(surface.descriptor.view, StandardView::Grid(_)).then(|| surface.clone());
                self.restoration_target = None;
                self.select_standard(index, false);
                self.standard_pointer_source = pointer_source;
                Some(Some(self.standard_refresh_effect(ViewRefreshReason::Selection)))
            }
            UiCommand::StandardActivate { context, executable }
            | UiCommand::StandardDoubleClick { context, executable } => {
                if context != &surface.context || executable.target.owner != surface.owner {
                    return Some(None);
                }
                // One deliberate pointer activation owns selection and execution.
                // Validate the exact current executable before changing host state.
                if surface.descriptor.executable(executable).is_none() {
                    return Some(None);
                }
                if let Some(index) = self.rows.iter().position(|row| {
                    matches!(&row.target, ActionTarget::Result { owner, item }
                        if owner == &executable.target.owner && item == &executable.target.item)
                }) {
                    self.restoration_target = None;
                    self.select_standard(index, false);
                }
                self.standard_ready_activation = None;
                Some(self.activate_standard_executable(executable.clone()))
            }
            UiCommand::StandardLink { context, url } => {
                Some((context == &surface.context).then(|| UiEffect::OpenStandardLink {
                    owner: surface.owner.clone(),
                    context: context.clone(),
                    url: url.clone(),
                }))
            }
            UiCommand::StandardHorizontal(forward) => {
                let StandardView::Grid(grid) = &surface.descriptor.view else {
                    return Some(None);
                };
                let index = grid_neighbor(
                    self.selected,
                    self.rows.len(),
                    grid.columns as usize,
                    &self.section_headers,
                    if *forward { 1 } else { -1 },
                );
                self.restoration_target = None;
                self.select_standard(index, true);
                Some(Some(self.standard_refresh_effect(ViewRefreshReason::Selection)))
            }
            UiCommand::SetQuery(query) => {
                self.query = query.clone();
                self.standard_follow_selection = false;
                self.inline_arguments = None;
                self.dirty = true;
                Some(Some(self.standard_refresh_effect(ViewRefreshReason::Query)))
            }
            UiCommand::SetScope(scope) => {
                if scope
                    .as_ref()
                    .is_some_and(|scope| !self.scope_choices.iter().any(|choice| &choice.identity == scope))
                {
                    return Some(None);
                }
                self.navigation_scope = scope.clone();
                self.standard_follow_selection = false;
                self.inline_arguments = None;
                if !self.standard_selection_available() {
                    self.selected = usize::MAX;
                    self.restoration_target = None;
                }
                if self.standard_scroll_anchor().is_none() {
                    self.navigation_scroll = LogicalScroll {
                        row: usize::MAX,
                        offset: 0.0,
                    };
                }
                self.dirty = true;
                Some(Some(self.standard_refresh_effect(ViewRefreshReason::Scope)))
            }
            UiCommand::ExecuteDefault => {
                let action = if let StandardView::Form(form) = &self.standard.as_ref()?.descriptor.view {
                    form.submit.action.clone()
                } else {
                    self.rows
                        .get(self.selected)
                        .and_then(|row| row.default_action.as_ref())
                        .map(|action| action.action_id.clone())
                };
                Some(self.standard_execute(action))
            }
            UiCommand::SelectNext | UiCommand::SelectPrevious | UiCommand::PageDown | UiCommand::PageUp => {
                if self.action_panel.is_some() {
                    return None;
                }
                let forward = matches!(command, UiCommand::SelectNext | UiCommand::PageDown);
                if self.rows.get(self.selected).is_none() {
                    let index = if forward {
                        0
                    } else {
                        self.rows.len().saturating_sub(1)
                    };
                    self.select_standard(index, true);
                    return Some(Some(self.standard_refresh_effect(ViewRefreshReason::Selection)));
                }
                let mut index = self.selected;
                for _ in 0..if matches!(command, UiCommand::PageDown | UiCommand::PageUp) {
                    PAGE_SIZE
                } else {
                    1
                } {
                    index = match &surface.descriptor.view {
                        StandardView::Grid(grid) => grid_neighbor(
                            index,
                            self.rows.len(),
                            grid.columns as usize,
                            &self.section_headers,
                            if forward { 2 } else { -2 },
                        ),
                        _ if forward => index.saturating_add(1),
                        _ => index.saturating_sub(1),
                    };
                }
                self.restoration_target = None;
                self.select_standard(index, true);
                Some(Some(self.standard_refresh_effect(ViewRefreshReason::Selection)))
            }
            _ => None,
        }
    }
    pub fn standard_surface(&self) -> Option<&Arc<StandardSurface>> {
        self.standard.as_ref()
    }

    /// Begins a fresh physical surface; Back supplies restoration intent only.
    pub fn open_standard_route(
        &mut self,
        owner: PluginId,
        producer: u64,
        outcome: StructuredRouteOutcome,
    ) -> Option<ViewRequest> {
        if !self.visible || outcome.validate().is_err() {
            return None;
        }
        let restoring = self.reopening;
        self.standard_follow_selection = false;
        self.set_standard_refresh_pending(false);
        let restored_collection = restoring
            && matches!(
                self.page_reopen,
                Some(NavigationRoute::Standard { collection: true, .. })
            );
        self.standard_restoring = restoring;
        let restored_logical = if restoring {
            match &self.page_reopen {
                Some(NavigationRoute::Standard { logical_view, .. }) => Some(*logical_view),
                _ => None,
            }
        } else {
            None
        };
        if matches!(outcome, StructuredRouteOutcome::Push(_)) && !self.reopening && !self.push_navigation() {
            return None;
        }
        let route = match outcome {
            StructuredRouteOutcome::Push(route) | StructuredRouteOutcome::Replace(route) => route,
        };
        self.standard = None;
        self.page = None;
        self.settings_open = false;
        self.change_view();
        if !restoring {
            self.inline_arguments = None;
            self.query.clear();
            self.rows = Arc::default();
            self.navigation_scope = None;
            self.restoration_target = None;
            self.selected = 0;
            self.navigation_scroll = LogicalScroll::default();
        }
        self.reopening = true;
        self.navigation_unavailable = None;
        let identity = NonZeroU64::new(self.view_id.0)?;
        let logical_view = restored_logical.unwrap_or(identity);
        self.page_reopen = Some(NavigationRoute::Standard {
            owner,
            producer,
            logical_view,
            route: route.clone(),
            title: "Loading view".into(),
            collection: restored_collection,
        });
        self.dirty = true;
        Some(ViewRequest {
            mode: ViewRequestMode::Open,
            route,
            session: ViewSessionIdentity {
                logical_view,
                surface: identity,
            },
            previous_revision: None,
            state: restoring.then(|| HostViewState {
                selected: self.restoration_target.as_ref().and_then(|target| match target {
                    ActionTarget::Result { owner, item } => Some(OwnedItem {
                        owner: owner.clone(),
                        item: item.clone(),
                    }),
                    _ => None,
                }),
                scope: self.navigation_scope.as_ref().map(|scope| scope.id.clone()),
                input: self
                    .inline_arguments
                    .as_ref()
                    .map(|input| input.executable.clone()),
            }),
            query: self.query.clone(),
            scroll: restoring.then(|| self.standard_scroll_anchor()).flatten(),
            reason: ViewRefreshReason::Explicit,
        })
    }

    pub fn publish_standard_view(&mut self, owner: PluginId, producer: u64, reply: ViewReply) -> bool {
        if !self.visible || reply.context.session.surface.get() != self.view_id.0 {
            return false;
        }
        let matches_route = match &self.page_reopen {
            Some(NavigationRoute::Standard {
                owner: expected,
                route,
                producer: physical,
                logical_view,
                ..
            }) => {
                expected == &owner
                    && route == &reply.context.route
                    && *physical == producer
                    && *logical_view == reply.context.session.logical_view
            }
            _ => self.standard.as_ref().is_some_and(|old| {
                old.owner == owner
                    && old.producer == producer
                    && old.context.session == reply.context.session
                    && old.context.route == reply.context.route
                    && old.context.revision < reply.context.revision
            }),
        };
        if !matches_route {
            return false;
        }
        self.set_standard_refresh_pending(false);
        let old = self.standard.replace(Arc::new(StandardSurface {
            owner: owner.clone(),
            producer,
            context: reply.context,
            admitted_descriptor: reply.descriptor.clone(),
            descriptor: reply.descriptor,
        }));
        let entry = self.capture_entry();
        if navigation::retained_bytes(self.navigation_stack.iter().chain(std::iter::once(&entry)))
            > navigation::MAX_NAVIGATION_BYTES
        {
            self.standard_unavailable();
            self.set_navigation_notice("View exceeds the shared 8 MiB navigation limit. Go Back.".into());
            return false;
        }
        let mut previous_selection = self
            .restoration_target
            .clone()
            .or_else(|| self.rows.get(self.selected).map(|row| row.target.clone()));
        let previous_scroll = self
            .rows
            .get(self.navigation_scroll.row)
            .map(|row| row.target.clone());
        let surface = self.standard.as_ref().unwrap();
        let collection = match &surface.descriptor.view {
            StandardView::List(collection) => Some(collection),
            StandardView::Grid(grid) => Some(&grid.collection),
            _ => None,
        };
        if self.navigation_scope.as_ref().is_some_and(|scope| {
            scope.owner != owner
                || collection.is_none_or(|collection| {
                    !collection
                        .scopes
                        .iter()
                        .any(|declaration| declaration.id == scope.id)
                })
        }) {
            self.standard_unavailable();
            return false;
        }
        let mut rows = Vec::new();
        let mut headers = Vec::new();
        let mut scopes = Vec::new();
        let source = PublicationIdentity::Suggestion {
            generation: Generation::from_raw(surface.context.revision.get()),
            producer,
        };
        if let Some(collection) = collection {
            if old.is_none() && !self.standard_restoring && previous_selection.is_none() {
                previous_selection =
                    collection
                        .initial_selection
                        .as_ref()
                        .map(|target| ActionTarget::Result {
                            owner: target.owner.clone(),
                            item: target.item.clone(),
                        });
            }
            if old.is_none() && !self.standard_restoring && self.navigation_scope.is_none() {
                self.navigation_scope = collection.initial_scope.as_ref().map(|id| ScopeIdentity {
                    owner: owner.clone(),
                    id: id.clone(),
                    source: source.clone(),
                });
            }
            for scope in &collection.scopes {
                scopes.push(ScopeChoice {
                    identity: ScopeIdentity {
                        owner: owner.clone(),
                        id: scope.id.clone(),
                        source: source.clone(),
                    },
                    label: scope.label.clone(),
                });
            }
            if let Some(current) = &mut self.navigation_scope {
                if current.owner == owner && collection.scopes.iter().any(|scope| scope.id == current.id) {
                    current.source = source.clone();
                }
            }
            let selected_scope = self.navigation_scope.as_ref().map(|scope| &scope.id);
            let mut section = None;
            for presentation in collection.items.iter() {
                if selected_scope.is_some_and(|scope| !presentation.scopes.contains(scope)) {
                    continue;
                }
                let Some(definition) = surface
                    .descriptor
                    .definitions
                    .iter()
                    .find(|definition| definition.item.stable_id == presentation.target.item)
                else {
                    continue;
                };
                if presentation.section != section {
                    section = presentation.section.clone();
                    if let Some(declaration) = collection
                        .sections
                        .iter()
                        .find(|s| Some(&s.id) == section.as_ref())
                    {
                        headers.push(SectionHeader {
                            row: rows.len(),
                            owner: owner.clone(),
                            id: declaration.id.clone(),
                            label: declaration.label.clone(),
                        });
                    }
                }
                rows.push(result_row(&owner, definition, &surface.context));
            }
        }
        if collection.is_none() {
            let target = match &surface.descriptor.view {
                StandardView::Detail(detail) => detail.target.as_ref(),
                StandardView::Form(form) => Some(&form.submit.target),
                _ => None,
            };
            if let Some(definition) = target.and_then(|target| surface.descriptor.definition(target)) {
                rows.push(result_row(&owner, definition, &surface.context));
            }
            previous_selection = rows.first().map(|row| row.target.clone());
        }
        self.navigation_title = Arc::from(surface.descriptor.title.as_str());
        self.scope_choices = scopes.into();
        self.section_headers = headers.into();
        self.rows = rows.into();
        self.selected = previous_selection
            .as_ref()
            .and_then(|target| self.rows.iter().position(|row| target.matches(row)))
            .unwrap_or(usize::MAX);
        self.restoration_target = None;
        self.navigation_scroll = previous_scroll
            .and_then(|target| self.rows.iter().position(|row| target.matches(row)))
            .filter(|_| collection.is_some())
            .map(|row| LogicalScroll {
                row,
                offset: self.navigation_scroll.offset,
            })
            .unwrap_or_default();
        self.preserve_scroll = !self.standard_follow_selection;
        self.standard_follow_selection = false;
        if self.preserve_scroll {
            self.navigation_restore = self
                .navigation_restore
                .checked_add(1)
                .expect("restoration identity exhausted");
        }
        self.active = Some(Generation::from_raw(surface.context.revision.get()));
        self.published = self.active;
        self.publication_view = Some(self.view_id);
        self.page_reopen = None;
        self.reopening = false;
        self.query_refresh = false;
        let input = match &surface.descriptor.view {
            StandardView::Form(form) => Some(form.submit.clone()),
            _ => self
                .inline_arguments
                .as_ref()
                .filter(|input| {
                    self.rows.get(self.selected).is_some_and(|row| {
                        matches!(&row.target, ActionTarget::Result { owner, item }
                        if owner == &input.executable.target.owner && item == &input.executable.target.item)
                    })
                })
                .map(|input| input.executable.clone()),
        };
        let binding = input.and_then(|key| {
            let definition = surface
                .descriptor
                .definitions
                .iter()
                .find(|d| d.item.stable_id == key.target.item)?;
            Some((
                key.clone(),
                definition.presentation(key.action.as_ref())?.schema.clone()?,
            ))
        });
        if let Some((key, schema)) = binding {
            let Some(revision) = next_inline_input_revision() else {
                self.standard_unavailable();
                return false;
            };
            let draft = self
                .inline_arguments
                .as_ref()
                .filter(|i| i.executable == key && schema.fingerprint().ok() == Some(i.fingerprint))
                .map(|i| i.draft.clone());
            self.open_inline_arguments(revision, key, schema, source, None);
            if let (Some(input), Some(draft)) = (&mut self.inline_arguments, draft) {
                input.draft = draft;
            }
        } else {
            self.inline_arguments = None;
        }
        drop(old);
        self.action_panel = None;
        self.reconcile_standard_activation();
        let entry = self.capture_entry();
        if navigation::retained_bytes(self.navigation_stack.iter().chain(std::iter::once(&entry)))
            > navigation::MAX_NAVIGATION_BYTES
        {
            self.standard_unavailable();
            return false;
        }
        self.dirty = true;
        true
    }

    /// A fresh projection of the sole P3 model, never a renderer/worker claim.
    pub fn standard_host_state(&self) -> HostViewState {
        HostViewState {
            selected: self.standard_selected_target(),
            scope: self.navigation_scope.as_ref().map(|scope| scope.id.clone()),
            input: self
                .inline_arguments
                .as_ref()
                .map(|input| input.executable.clone()),
        }
    }
    pub fn standard_refresh_request(&self, reason: ViewRefreshReason) -> Option<ViewRequest> {
        let surface = self.standard.as_ref()?;
        Some(ViewRequest {
            mode: ViewRequestMode::Refresh,
            route: surface.context.route.clone(),
            session: surface.context.session,
            previous_revision: Some(surface.context.revision),
            state: Some(self.standard_host_state()),
            query: self.query.clone(),
            scroll: self.standard_scroll_anchor(),
            reason,
        })
    }

    fn standard_scroll_anchor(&self) -> Option<ScrollAnchor> {
        let collection = self.standard.as_ref().map_or_else(
            || {
                matches!(
                    self.page_reopen,
                    Some(NavigationRoute::Standard { collection: true, .. })
                )
            },
            |surface| {
                matches!(
                    surface.descriptor.view,
                    StandardView::List(_) | StandardView::Grid(_)
                )
            },
        );
        if !collection {
            return None;
        }
        self.rows
            .get(self.navigation_scroll.row)
            .and_then(|row| match &row.target {
                ActionTarget::Result { owner, item } => {
                    let item = OwnedItem {
                        owner: owner.clone(),
                        item: item.clone(),
                    };
                    if let Some(surface) = &self.standard {
                        let collection = match &surface.descriptor.view {
                            StandardView::List(collection) => collection,
                            StandardView::Grid(grid) => &grid.collection,
                            _ => return None,
                        };
                        if !collection.items.iter().any(|presentation| {
                            presentation.target == item
                                && self
                                    .navigation_scope
                                    .as_ref()
                                    .is_none_or(|scope| presentation.scopes.contains(&scope.id))
                        }) {
                            return None;
                        }
                    }
                    Some(ScrollAnchor {
                        item,
                        row_offset: self.navigation_scroll.offset.max(0.0).min(u32::MAX as f32) as u32,
                    })
                }
                _ => None,
            })
    }

    pub(crate) fn standard_execute(&mut self, action: Option<ActionId>) -> Option<UiEffect> {
        let surface = self.standard.as_ref()?;
        let executable = if let StandardView::Form(form) = &surface.descriptor.view {
            ExecutableKey {
                target: form.submit.target.clone(),
                action,
            }
        } else if let Some(row) = self.rows.get(self.selected) {
            if !self.standard_selection_available() {
                return None;
            }
            let ActionTarget::Result { owner, item } = &row.target else {
                return None;
            };
            ExecutableKey {
                target: OwnedItem {
                    owner: owner.clone(),
                    item: item.clone(),
                },
                action,
            }
        } else {
            match &surface.descriptor.view {
                StandardView::Detail(detail) => {
                    let target = detail.target.clone()?;
                    let definition = surface.descriptor.definition(&target)?;
                    ExecutableKey {
                        target,
                        action: action.or_else(|| definition.primary_action.clone()),
                    }
                }
                _ => return None,
            }
        };
        self.activate_standard_executable(executable)
    }

    fn activate_standard_executable(&mut self, executable: ExecutableKey) -> Option<UiEffect> {
        let surface = self.standard.as_ref()?;
        if let StandardView::Form(form) = &surface.descriptor.view {
            if executable != form.submit {
                return None;
            }
        }
        if self.standard_selected_target().as_ref() != Some(&executable.target) {
            return None;
        }
        let (_, presentation) = surface.descriptor.executable(&executable)?;
        if let Some(schema) = presentation.schema.clone() {
            let source = PublicationIdentity::Suggestion {
                generation: Generation::from_raw(surface.context.revision.get()),
                producer: surface.producer,
            };
            let Some(revision) = next_inline_input_revision() else {
                self.set_navigation_notice(
                    "Command input identity limit reached; restart the launcher.".into(),
                );
                return None;
            };
            self.open_inline_arguments(revision, executable, schema, source, None);
            return None;
        }
        Some(UiEffect::ExecuteStandard {
            owner: surface.owner.clone(),
            context: surface.context.clone(),
            executable,
        })
    }
}

fn standard_surface_bytes(surface: &StandardSurface) -> usize {
    std::mem::size_of::<StandardSurface>()
        + 2 * std::mem::size_of::<usize>()
        + surface.owner.0.capacity()
        + surface.context.route.0.capacity()
}

fn executable_bytes(key: &ExecutableKey) -> usize {
    std::mem::size_of::<ExecutableKey>()
        + key.target.owner.0.capacity()
        + key.target.item.0.capacity()
        + key.action.as_ref().map_or(0, |action| action.0.capacity())
}

/// Compare the frozen executable's item, action and schema, not just its IDs.
fn unchanged_executable(old: &ViewDescriptor, new: &ViewDescriptor, key: &ExecutableKey) -> bool {
    let (Some((old_policy, old_binding)), Some((new_policy, new_binding))) =
        (old.executable(key), new.executable(key))
    else {
        return false;
    };
    let (Some(old), Some(new)) = (old.definition(&key.target), new.definition(&key.target)) else {
        return false;
    };
    let a = &old.item;
    let b = &new.item;
    a.stable_id == b.stable_id
        && a.plugin_id == b.plugin_id
        && a.category == b.category
        && a.label == b.label
        && a.description == b.description
        && a.target == b.target
        && a.search_terms == b.search_terms
        && a.icon_reference == b.icon_reference
        && a.argument_policy == b.argument_policy
        && a.hit_policy == b.hit_policy
        && a.score_hint == b.score_hint
        && a.metadata == b.metadata
        && a.actions.len() == b.actions.len()
        && a.actions.iter().zip(&b.actions).all(|(a, b)| {
            a.action_id == b.action_id
                && a.label == b.label
                && a.description == b.description
                && a.applicable_categories == b.applicable_categories
                && a.icon_reference == b.icon_reference
                && a.execution_policy == b.execution_policy
        })
        && old.actionless_policy == new.actionless_policy
        && old.primary_action == new.primary_action
        && old_policy == new_policy
        && old_binding.action == new_binding.action
        && old_binding.schema == new_binding.schema
        && old_binding.shortcut == new_binding.shortcut
        && old_binding.destructive == new_binding.destructive
}

pub(crate) fn action_tree(
    surface: &StandardSurface,
    row: &ResultRow,
) -> Result<Arc<ActionTree>, action_panel::ActionPanelRefusal> {
    use action_panel::{ActionEntry, ActionPresentation, ActionSection};
    fn entries(definition: &RouteItemDefinition, source: &[ActionMenuEntry]) -> Vec<ActionEntry> {
        source
            .iter()
            .filter_map(|entry| match entry {
                ActionMenuEntry::Action(binding) => {
                    if binding.action.is_none() {
                        return Some(ActionEntry::Actionless {
                            presentation: ActionPresentation {
                                label: "Run".into(),
                                shortcut: binding.shortcut.clone(),
                                destructive: binding.destructive,
                                ..Default::default()
                            },
                        });
                    }
                    let id = binding.action.as_ref()?;
                    let action = definition.action(id)?;
                    Some(ActionEntry::Plugin {
                        action: id.clone(),
                        presentation: ActionPresentation {
                            label: action.label.clone(),
                            description: action.description.clone(),
                            icon_reference: action.icon_reference.clone(),
                            shortcut: binding.shortcut.clone(),
                            destructive: binding.destructive,
                        },
                    })
                }
                ActionMenuEntry::Submenu {
                    id,
                    label,
                    description,
                    icon_reference,
                    entries: children,
                } => {
                    let children = entries(definition, children);
                    if children.is_empty() {
                        return None;
                    }
                    Some(ActionEntry::Submenu {
                        id: id.clone(),
                        presentation: ActionPresentation {
                            label: label.clone(),
                            description: description.clone(),
                            icon_reference: icon_reference.clone(),
                            ..Default::default()
                        },
                        entries: children,
                    })
                }
            })
            .collect()
    }
    let ActionTarget::Result { owner, item } = &row.target else {
        return Err(action_panel::ActionPanelRefusal::EmptyIdentity("route item"));
    };
    let definition = surface
        .descriptor
        .definition(&OwnedItem {
            owner: owner.clone(),
            item: item.clone(),
        })
        .ok_or(action_panel::ActionPanelRefusal::EmptyIdentity("route item"))?;
    let sections = definition
        .action_sections
        .iter()
        .map(|section| ActionSection {
            id: section.id.clone(),
            label: section.label.clone(),
            entries: entries(definition, &section.entries),
        })
        .filter(|section| !section.entries.is_empty())
        .collect();
    ActionTree::new(row, sections)
}

#[cfg(test)]
#[path = "standard_view_tests.rs"]
pub(crate) mod tests;

pub(crate) fn section_bounds(index: usize, count: usize, headers: &[SectionHeader]) -> (usize, usize) {
    let start = headers
        .iter()
        .filter(|header| header.row <= index)
        .map(|header| header.row)
        .max()
        .unwrap_or(0);
    let end = headers
        .iter()
        .filter(|header| header.row > index)
        .map(|header| header.row)
        .min()
        .unwrap_or(count);
    (start, end)
}

fn grid_neighbor(
    index: usize,
    count: usize,
    columns: usize,
    headers: &[SectionHeader],
    direction: i8,
) -> usize {
    if count == 0 {
        return 0;
    }
    if index >= count {
        return if direction > 0 { 0 } else { count - 1 };
    }
    let (start, end) = section_bounds(index, count, headers);
    let column = (index - start) % columns;
    match direction {
        1 if column + 1 < columns && index + 1 < end => index + 1,
        -1 if column > 0 => index - 1,
        2 if index - column + columns < end => (index + columns).min(end - 1),
        2 if end < count => {
            let (_, next_end) = section_bounds(end, count, headers);
            (end + column).min(next_end - 1)
        }
        -2 if index >= start + columns => index - columns,
        -2 if start > 0 => {
            let (previous_start, _) = section_bounds(start - 1, count, headers);
            let last_row = previous_start + (start - 1 - previous_start) / columns * columns;
            (last_row + column).min(start - 1)
        }
        _ => index,
    }
}

fn result_row(
    owner: &PluginId,
    definition: &RouteItemDefinition,
    context: &ViewExecutionContext,
) -> ResultRow {
    let item = &definition.item;
    ResultRow {
        target: ActionTarget::Result {
            owner: owner.clone(),
            item: item.stable_id.clone(),
        },
        source: Some(ResultSource {
            owner: owner.clone(),
            item: item.stable_id.clone(),
            kind: ResultSourceKind::Standard {
                logical_view: context.session.logical_view.get(),
                surface: context.session.surface.get(),
                revision: context.revision.get(),
            },
        }),
        command_state: None,
        label: item.label.clone(),
        description: item.description.clone(),
        icon_reference: item.icon_reference.clone(),
        icon: None,
        category: String::new(),
        plugin_name: owner.0.clone(),
        highlights: Vec::new(),
        argument_hint: None,
        status: None,
        default_action: definition
            .primary_action
            .as_ref()
            .and_then(|id| definition.action(id))
            .cloned(),
        alternate_actions: item
            .actions
            .iter()
            .filter(|action| Some(&action.action_id) != definition.primary_action.as_ref())
            .cloned()
            .collect(),
    }
}
