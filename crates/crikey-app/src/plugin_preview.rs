//! Selection-bound preview delivery with exact command/catalog provenance.
use crikey_core::{Generation, ItemId, PluginId, PreviewContent};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewIdentity {
    pub view: u64,
    pub query: Generation,
    pub plugin: PluginId,
    pub incarnation: u64,
    pub item: ItemId,
    /// Full row identity; commands sharing an item never share selection authority.
    pub target: crikey_ui::ActionTarget,
    /// Accepted provider publication, distinct from the producing incarnation.
    pub catalog_source: Option<crate::plugin_action::DeclaredCommandSource>,
    pub selection: u64,
    pub request: u64,
}
#[derive(Debug, Clone)]
pub struct PreviewRequest {
    pub identity: PreviewIdentity,
}
#[derive(Debug, Clone, PartialEq)]
pub enum PreviewState {
    Loading,
    Content(Arc<crikey_ui::PreparedPreview>),
    Empty,
    Unavailable(String),
    Error(String),
    Cancelled,
}
#[derive(Debug, Clone)]
pub struct PreviewUpdate {
    pub identity: PreviewIdentity,
    pub state: PreviewState,
}
pub(crate) type PreviewPublish = Arc<dyn Fn(PreviewUpdate) + Send + Sync>;
pub(crate) type PendingPreviews<C> = Arc<std::sync::Mutex<Vec<(PreviewIdentity, Arc<C>)>>>;
pub(crate) trait PreviewDispatcher: Send + Sync {
    /// Returns the item and its immutable producing worker identity together.
    fn lookup(
        &self,
        plugin: &PluginId,
        item: &ItemId,
        source: Option<&crate::plugin_action::DeclaredCommandSource>,
    ) -> Option<(crikey_core::Item, u64)>;
    fn origin(
        &self,
        plugin: &PluginId,
        item: &ItemId,
        source: Option<&crate::plugin_action::DeclaredCommandSource>,
    ) -> Option<u64>;
    fn incarnation(&self, plugin: &PluginId) -> Option<u64>;
    fn set_wake(&self, wake: Arc<dyn Fn() + Send + Sync>);
    fn submit(&self, request: PreviewRequest, publish: PreviewPublish) -> Result<(), String>;
    fn cancel(&self, identity: &PreviewIdentity);
    /// Foreground callbacks have priority over speculative requests on the
    /// provider's actual execution resources, not inferred owner prefixes.
    fn foreground_blocked(&self, owner: &PluginId) -> bool;
    fn foreground_conflicts(&self, preview_owner: &PluginId, action_owner: &PluginId) -> bool;
}

/// One active operation and one replace-latest selection. Terminal delivery
/// releases admission; cancellation alone never releases the provider slot.
pub struct PreviewService {
    endpoints: Vec<Arc<dyn PreviewDispatcher>>,
    selected: Option<crikey_ui::PreviewSelection>,
    binding: Option<(usize, u64)>,
    pending: Option<(crikey_ui::PreviewSelection, usize, crikey_core::Item, u64)>,
    active: Option<(PreviewIdentity, usize)>,
    presented: Option<(PreviewIdentity, usize, Arc<crikey_ui::PreparedPreview>)>,
    metadata: Option<Arc<PreviewContent>>,
    terminal: Arc<std::sync::Mutex<Option<PreviewUpdate>>>,
    publish: PreviewPublish,
    sequence: u64,
    retired_request: Option<u64>,
}
impl std::fmt::Debug for PreviewService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewService")
            .field("active", &self.active)
            .field("selected", &self.selected)
            .finish()
    }
}
impl PreviewService {
    pub fn new(
        native: &crate::NativeDriver,
        modern: &crate::ModernDriver,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let terminal = Arc::new(std::sync::Mutex::new(None));
        let sink = Arc::clone(&terminal);
        let endpoints = vec![native.preview_dispatcher(), modern.preview_dispatcher()];
        for endpoint in &endpoints {
            endpoint.set_wake(Arc::clone(&wake));
        }
        let publish: PreviewPublish = Arc::new(move |update| {
            *sink.lock().unwrap_or_else(|error| error.into_inner()) = Some(update);
            wake();
        });
        Self {
            endpoints,
            selected: None,
            binding: None,
            pending: None,
            active: None,
            presented: None,
            metadata: None,
            terminal,
            publish,
            sequence: 0,
            retired_request: None,
        }
    }

    /// Foreground work retires preview/link authority immediately, but keeps
    /// its active slot until the actual terminal delivery releases it.
    pub fn retire_for_foreground(&mut self, model: &mut crikey_ui::LauncherViewModel, owner: &PluginId) {
        if let Some((identity, endpoint)) = self.active.as_ref().filter(|(identity, endpoint)| {
            self.endpoints[*endpoint].foreground_conflicts(&identity.plugin, owner)
        }) {
            self.retired_request = Some(identity.request);
            self.endpoints[*endpoint].cancel(identity);
        }
        if let Some(selection) = self.selected.as_ref().filter(|selection| {
            self.binding.is_some_and(|(endpoint, _)| {
                self.endpoints[endpoint].foreground_conflicts(&selection.plugin, owner)
            })
        }) {
            self.presented = None;
            // Rebuild only the latest still-current intent on synchronize;
            // admission remains fenced until the real foreground release.
            self.binding = None;
            model.publish_preview(
                selection,
                crikey_ui::PreviewStatus::Unavailable("Preview paused for foreground action".into()),
            );
            self.selected = None;
        }
    }

    pub fn synchronize(&mut self, model: &mut crikey_ui::LauncherViewModel, search: &crate::SearchService) {
        use crikey_ui::PreviewStatus;
        let selection = model.preview_selection();
        // Only authoritative item+producer publication can establish/re-establish
        // a binding. A current worker alone is never proof that it owns the row.
        let published_binding = selection.as_ref().and_then(|selection| {
            let source = catalog_source(selection, search).ok()?;
            let mut matches = self.endpoints.iter().enumerate().filter_map(|(index, endpoint)| {
                endpoint
                    .origin(&selection.plugin, &selection.item, source)
                    .and_then(|incarnation| {
                        (endpoint.incarnation(&selection.plugin) == Some(incarnation))
                            .then_some((index, incarnation))
                    })
            });
            let first = matches.next();
            if matches.next().is_some() {
                None
            } else {
                first
            }
        });
        let newly_bound = published_binding.is_some() && published_binding != self.binding;
        if self.selected != selection || newly_bound {
            self.selected = selection.clone();
            self.binding = None;
            self.presented = None;
            self.metadata = None;
            self.pending = None;
            if let Some((identity, endpoint)) = &self.active {
                self.endpoints[*endpoint].cancel(identity);
            }
            if let Some(selection) = selection {
                let source = match catalog_source(&selection, search) {
                    Ok(source) => source,
                    Err(()) => {
                        model.publish_preview(
                            &selection,
                            PreviewStatus::Unavailable("Preview catalog source unavailable".into()),
                        );
                        return;
                    }
                };
                let mut matches = self.endpoints.iter().enumerate().filter_map(|(index, endpoint)| {
                    endpoint
                        .lookup(&selection.plugin, &selection.item, source)
                        .map(|item| (index, item))
                });
                let first = matches.next();
                if matches.next().is_some() {
                    model.publish_preview(
                        &selection,
                        PreviewStatus::Unavailable("Ambiguous preview ownership".into()),
                    );
                } else if let Some((endpoint, (item, incarnation))) = first {
                    if item.metadata.get("crikey.preview").map(String::as_str) == Some("deferred")
                        && self.endpoints[endpoint].incarnation(&selection.plugin) != Some(incarnation)
                    {
                        model.publish_preview(
                            &selection,
                            PreviewStatus::Unavailable("Preview owner unavailable".into()),
                        );
                    } else if item.metadata.get("crikey.preview").map(String::as_str) == Some("deferred") {
                        self.binding = Some((endpoint, incarnation));
                        model.publish_preview(&selection, PreviewStatus::Loading);
                        self.pending = Some((selection, endpoint, item, incarnation));
                    } else {
                        self.binding = Some((endpoint, incarnation));
                        self.publish_metadata(model, &selection, Some(&item));
                    }
                } else if matches!(
                    selection.source.kind,
                    crikey_ui::ResultSourceKind::DeclaredCatalog { .. }
                ) {
                    model.publish_preview(
                        &selection,
                        PreviewStatus::Unavailable("Preview catalog source unavailable".into()),
                    );
                } else {
                    let mut hits = search.results().iter().filter(|hit| {
                        hit.item.plugin_id == selection.plugin && hit.item.stable_id == selection.item
                    });
                    let item = hits.next().map(|hit| &hit.item);
                    if hits.next().is_some() {
                        model.publish_preview(
                            &selection,
                            PreviewStatus::Unavailable("Ambiguous preview ownership".into()),
                        );
                    } else {
                        self.publish_metadata(model, &selection, item);
                    }
                }
            }
        }
        if let Some(selection) = self
            .selected
            .as_ref()
            .filter(|selection| catalog_source(selection, search).is_err())
        {
            self.binding = None;
            self.presented = None;
            self.metadata = None;
            self.pending = None;
            model.publish_preview(
                selection,
                PreviewStatus::Unavailable("Preview catalog source unavailable".into()),
            );
        } else if self.metadata.is_some() {
            // Only host metadata is refreshed; deferred/pending/terminal content
            // and catalog-source failures retain their own authority.
            if let Some(selection) = self.selected.take() {
                self.publish_metadata(model, &selection, None);
                self.selected = Some(selection);
            }
        }
        // Retirement is a lifecycle event, not necessarily a terminal reply.
        // Revalidate retained content on every wake, even with no new selection.
        if self.presented.as_ref().is_some_and(|(identity, endpoint, _)| {
            self.endpoints[*endpoint].incarnation(&identity.plugin) != Some(identity.incarnation)
                || !self.selected.as_ref().is_some_and(|selection| {
                    catalog_source(selection, search)
                        .is_ok_and(|source| source == identity.catalog_source.as_ref())
                })
                || !self.source_is_current(identity, *endpoint)
        }) {
            self.presented = None;
            if let Some(selection) = &self.selected {
                model.publish_preview(
                    selection,
                    PreviewStatus::Unavailable("Preview owner unavailable".into()),
                );
            }
        }
        let update = self
            .terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(update) = update {
            if self
                .active
                .as_ref()
                .is_some_and(|(identity, _)| identity == &update.identity)
            {
                let (identity, endpoint) = self.active.take().unwrap();
                let retired = self.retired_request == Some(identity.request);
                if retired {
                    self.retired_request = None;
                }
                let origin = self.endpoints[endpoint].incarnation(&identity.plugin);
                // A dead worker can still finish its exact active request with a
                // failure. Content must never outlive its originating worker,
                // and a replacement incarnation cannot publish the old failure.
                let admissible_origin = self.source_is_current(&identity, endpoint)
                    && self.selected.as_ref().is_some_and(|selection| {
                        catalog_source(selection, search)
                            .is_ok_and(|source| source == identity.catalog_source.as_ref())
                    })
                    && (origin == Some(identity.incarnation)
                        || (origin.is_none()
                            && matches!(
                                &update.state,
                                PreviewState::Error(_)
                                    | PreviewState::Unavailable(_)
                                    | PreviewState::Cancelled
                            )));
                if !retired
                    && self.selected.as_ref().is_some_and(|selection| {
                        selection.view.get() == identity.view
                            && selection.generation == identity.query
                            && selection.plugin == identity.plugin
                            && selection.item == identity.item
                            && selection.revision == identity.selection
                            && selection.target == identity.target
                    })
                {
                    let state = if !admissible_origin {
                        PreviewStatus::Unavailable("Preview owner unavailable".into())
                    } else {
                        match update.state {
                            PreviewState::Content(content) => {
                                self.presented = Some((identity.clone(), endpoint, Arc::clone(&content)));
                                PreviewStatus::Content(content)
                            }
                            PreviewState::Empty => PreviewStatus::Empty,
                            PreviewState::Unavailable(reason) => PreviewStatus::Unavailable(reason),
                            PreviewState::Error(reason) => PreviewStatus::Error(reason),
                            PreviewState::Cancelled => PreviewStatus::Unavailable("Preview cancelled".into()),
                            PreviewState::Loading => PreviewStatus::Loading,
                        }
                    };
                    model.publish_preview(self.selected.as_ref().unwrap(), state);
                    if self.presented.is_some() {
                        model.set_preview_link_target(crikey_ui::PreviewLinkTarget {
                            selection: self.selected.as_ref().unwrap().clone(),
                            plugin: identity.plugin,
                            incarnation: identity.incarnation,
                            request: identity.request,
                        });
                    }
                }
            }
        }
        if let Some((identity, endpoint)) = &self.active {
            if self.endpoints[*endpoint].incarnation(&identity.plugin) != Some(identity.incarnation)
                || !self.selected.as_ref().is_some_and(|selection| {
                    selection.target == identity.target
                        && catalog_source(selection, search)
                            .is_ok_and(|source| source == identity.catalog_source.as_ref())
                        && self.source_is_current(identity, *endpoint)
                })
            {
                self.endpoints[*endpoint].cancel(identity);
                if let Some(selection) = &self.selected {
                    if selection.plugin == identity.plugin
                        && selection.item == identity.item
                        && selection.revision == identity.selection
                    {
                        model.publish_preview(
                            selection,
                            PreviewStatus::Unavailable("Preview owner unavailable".into()),
                        );
                    }
                }
            }
        }
        if self.active.is_none() {
            if let Some((selection, endpoint, item, incarnation)) = self.pending.take() {
                if self.endpoints[endpoint].foreground_blocked(&selection.plugin) {
                    self.pending = Some((selection, endpoint, item, incarnation));
                    return;
                }
                // The producing identity was captured atomically with the item.
                // A busy provider must not drop this selection, and a restarted
                // provider must not silently inherit its predecessor's item.
                let Some(sequence) = self.sequence.checked_add(1) else {
                    model.publish_preview(
                        &selection,
                        PreviewStatus::Error("Preview request identity exhausted".into()),
                    );
                    return;
                };
                self.sequence = sequence;
                let identity = PreviewIdentity {
                    view: selection.view.get(),
                    query: selection.generation,
                    plugin: item.plugin_id,
                    incarnation,
                    item: selection.item.clone(),
                    target: selection.target.clone(),
                    catalog_source: match catalog_source(&selection, search) {
                        Ok(source) => source.cloned(),
                        Err(()) => {
                            model.publish_preview(
                                &selection,
                                PreviewStatus::Unavailable("Preview catalog source unavailable".into()),
                            );
                            return;
                        }
                    },
                    selection: selection.revision,
                    request: sequence,
                };
                match self.endpoints[endpoint].submit(
                    PreviewRequest {
                        identity: identity.clone(),
                    },
                    Arc::clone(&self.publish),
                ) {
                    Ok(()) => self.active = Some((identity, endpoint)),
                    Err(reason) => {
                        model.publish_preview(&selection, PreviewStatus::Error(reason));
                    }
                }
            }
        }
    }

    pub fn open_link(
        &self,
        target: &crikey_ui::PreviewLinkTarget,
        url: &str,
        search: &crate::SearchService,
    ) -> Result<(), String> {
        crikey_core::preview::validate_preview_link(url).map_err(|error| error.to_string())?;
        let (identity, endpoint, content) = self.presented.as_ref().ok_or("Preview link has retired")?;
        if self.selected.as_ref() != Some(&target.selection)
            || target.plugin != identity.plugin
            || target.incarnation != identity.incarnation
            || target.request != identity.request
            || self.endpoints[*endpoint].incarnation(&identity.plugin) != Some(identity.incarnation)
            || !self.source_is_current(identity, *endpoint)
            || target.selection.target != identity.target
            || !catalog_source(&target.selection, search)
                .is_ok_and(|source| source == identity.catalog_source.as_ref())
        {
            return Err("Preview link owner has retired".into());
        }
        let declared = content
            .content
            .values
            .iter()
            .any(|value| value.kind == crikey_core::PreviewValueKind::Link && value.value == url)
            || content
                .blocks
                .iter()
                .any(|block| block.spans.iter().any(|span| span.link.as_deref() == Some(url)));
        if !declared {
            return Err("Link is not part of the current preview".into());
        }
        search
            .open_preview_url(&identity.plugin, url)
            .map_err(|error| error.to_string())
    }
    fn source_is_current(&self, identity: &PreviewIdentity, endpoint: usize) -> bool {
        identity.catalog_source.as_ref().is_none_or(|source| {
            self.endpoints[endpoint].origin(&identity.plugin, &identity.item, Some(source))
                == Some(identity.incarnation)
        })
    }

    fn publish_metadata(
        &mut self,
        model: &mut crikey_ui::LauncherViewModel,
        selection: &crikey_ui::PreviewSelection,
        _item: Option<&crikey_core::Item>,
    ) {
        use crikey_core::{PreviewText, PreviewValue, PreviewValueKind};
        let Some(row) = model.selected_preview_row() else {
            return;
        };
        if self.metadata.as_ref().is_some_and(|content| {
            let text = match &content.text {
                Some(crikey_core::PreviewText::Plain(text)) => text.as_str(),
                _ => "",
            };
            let mut values = content.values.iter();
            bounded_metadata_matches(&row.description, text)
                && [
                    ("Provider", row.plugin_name.as_str()),
                    ("Category", row.category.as_str()),
                ]
                .into_iter()
                .filter(|(_, value)| !value.is_empty())
                .all(|(label, source)| {
                    values.next().is_some_and(|value| {
                        value.label == label && bounded_metadata_matches(source, &value.value)
                    })
                })
                && values.next().is_none()
        }) {
            return;
        }
        // Host-known legacy content introduces no new plugin callback.
        let mut values = Vec::new();
        for (label, value) in [
            ("Provider", row.plugin_name.as_str()),
            ("Category", row.category.as_str()),
        ] {
            if !value.is_empty() {
                values.push(PreviewValue {
                    label: label.into(),
                    value: bounded_metadata_text(value),
                    kind: PreviewValueKind::Text,
                });
            }
        }
        let content = PreviewContent {
            text: (!row.description.is_empty())
                .then(|| PreviewText::Plain(bounded_metadata_text(&row.description))),
            image: None,
            values,
        };
        let had_content = self
            .metadata
            .as_ref()
            .is_some_and(|old| old.text.is_some() || !old.values.is_empty());
        let content = Arc::new(content);
        self.metadata = Some(Arc::clone(&content));
        if content.text.is_some() || !content.values.is_empty() {
            let prepared = crikey_ui::PreparedPreview {
                content,
                blocks: Arc::default(),
            };
            model.publish_preview(selection, crikey_ui::PreviewStatus::Content(Arc::new(prepared)));
        } else if had_content {
            model.publish_preview(selection, crikey_ui::PreviewStatus::Empty);
        }
    }
}
impl Drop for PreviewService {
    fn drop(&mut self) {
        if let Some((identity, endpoint)) = &self.active {
            self.endpoints[*endpoint].cancel(identity);
        }
    }
}

/// Catalog admission epochs are resolved by the composition ledger, never
/// interpreted as producing-worker incarnations.
fn catalog_source<'a>(
    selection: &crikey_ui::PreviewSelection,
    search: &'a crate::SearchService,
) -> Result<Option<&'a crate::plugin_action::DeclaredCommandSource>, ()> {
    if selection.source.owner != selection.plugin || selection.source.item != selection.item {
        return Err(());
    }
    match selection.source.kind {
        crikey_ui::ResultSourceKind::Ordinary => Ok(None),
        crikey_ui::ResultSourceKind::Standard { .. } => Err(()),
        crikey_ui::ResultSourceKind::DeclaredCatalog { epoch } => search
            .declared_catalog_source(&selection.plugin, epoch)
            .map(Some)
            .ok_or(()),
    }
}

fn bounded_metadata_text(value: &str) -> String {
    const LIMIT: usize = 8192;
    const ELLIPSIS: &str = "…";
    if value.len() <= LIMIT {
        return value.to_owned();
    }
    let mut end = LIMIT - ELLIPSIS.len();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = String::with_capacity(end + ELLIPSIS.len());
    bounded.push_str(&value[..end]);
    bounded.push_str(ELLIPSIS);
    bounded
}

fn bounded_metadata_matches(source: &str, rendered: &str) -> bool {
    if source.len() <= 8192 {
        return source == rendered;
    }
    let mut end = 8192 - "…".len();
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    rendered.strip_suffix('…') == Some(&source[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crikey_ui::{LauncherViewModel, PreviewStatus, ResultRow, UiCommand};
    use std::sync::Mutex;

    struct Endpoint {
        incarnation: Mutex<Option<u64>>,
    }

    impl PreviewDispatcher for Endpoint {
        fn lookup(
            &self,
            _: &PluginId,
            _: &ItemId,
            _: Option<&crate::plugin_action::DeclaredCommandSource>,
        ) -> Option<(crikey_core::Item, u64)> {
            None
        }
        fn origin(
            &self,
            _: &PluginId,
            _: &ItemId,
            _: Option<&crate::plugin_action::DeclaredCommandSource>,
        ) -> Option<u64> {
            None
        }
        fn incarnation(&self, _: &PluginId) -> Option<u64> {
            *self.incarnation.lock().unwrap()
        }
        fn set_wake(&self, _: Arc<dyn Fn() + Send + Sync>) {}
        fn submit(&self, _: PreviewRequest, _: PreviewPublish) -> Result<(), String> {
            panic!("these terminal-delivery fixtures have no pending request")
        }
        fn cancel(&self, _: &PreviewIdentity) {}
        fn foreground_blocked(&self, _: &PluginId) -> bool {
            false
        }
        fn foreground_conflicts(&self, preview_owner: &PluginId, action_owner: &PluginId) -> bool {
            preview_owner == action_owner
        }
    }

    fn active_preview() -> (PreviewService, LauncherViewModel, Arc<Endpoint>, PreviewIdentity) {
        let mut model = LauncherViewModel::new();
        model.activate();
        let _ = model.apply(UiCommand::SetQuery("preview".into()));
        let generation = crikey_core::GenerationTracker::new().advance();
        model.begin_generation(generation);
        model.publish(
            generation,
            vec![ResultRow {
                target: crikey_ui::ActionTarget::Result {
                    owner: PluginId("preview-plugin".into()),
                    item: ItemId("preview-item".into()),
                },
                source: Some(crikey_ui::ResultSource {
                    owner: PluginId("preview-plugin".into()),
                    item: ItemId("preview-item".into()),
                    kind: crikey_ui::ResultSourceKind::Ordinary,
                }),
                command_state: None,
                label: "Preview".into(),
                description: String::new(),
                icon_reference: None,
                icon: None,
                category: String::new(),
                plugin_name: String::new(),
                highlights: Vec::new(),
                argument_hint: None,
                status: None,
                default_action: None,
                alternate_actions: Vec::new(),
            }],
            false,
        );
        let selection = model.preview_selection().unwrap();
        assert!(model.publish_preview(&selection, PreviewStatus::Loading));
        let identity = PreviewIdentity {
            view: selection.view.get(),
            query: selection.generation,
            plugin: PluginId("preview-plugin".into()),
            incarnation: 7,
            item: selection.item.clone(),
            target: selection.target.clone(),
            catalog_source: None,
            selection: selection.revision,
            request: 11,
        };
        let endpoint = Arc::new(Endpoint {
            incarnation: Mutex::new(Some(identity.incarnation)),
        });
        let terminal = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&terminal);
        let publish: PreviewPublish = Arc::new(move |update| {
            *sink.lock().unwrap() = Some(update);
        });
        let service = PreviewService {
            endpoints: vec![endpoint.clone()],
            selected: Some(selection),
            binding: Some((0, identity.incarnation)),
            pending: None,
            active: Some((identity.clone(), 0)),
            presented: None,
            metadata: None,
            terminal,
            publish,
            sequence: identity.request,
            retired_request: None,
        };
        (service, model, endpoint, identity)
    }

    #[test]
    fn command_identity_changes_retire_links_even_with_the_same_item() {
        let (mut service, mut model, _, mut identity) = active_preview();
        let mut row = model.selected_preview_row().unwrap().clone();
        let owner = identity.plugin.clone();
        let command = |name: &str| {
            crikey_ui::ActionTarget::Command(crikey_core::CommandRef {
                provider_id: owner.clone(),
                command_id: name.into(),
            })
        };
        row.target = command("a");
        model.publish(identity.query, vec![row.clone()], false);
        let selection = model.preview_selection().unwrap();
        identity.target = selection.target.clone();
        identity.selection = selection.revision;
        service.selected = Some(selection);
        service.active = Some((identity.clone(), 0));
        (service.publish)(PreviewUpdate {
            identity: identity.clone(),
            state: PreviewState::Content(Arc::new(crikey_ui::PreparedPreview {
                content: Arc::new(PreviewContent {
                    text: None,
                    image: None,
                    values: vec![crikey_core::PreviewValue {
                        label: "Link".into(),
                        value: "https://example.com/command".into(),
                        kind: crikey_core::PreviewValueKind::Link,
                    }],
                }),
                blocks: Arc::default(),
            })),
        });
        let search = crate::SearchService::new(crate::App::new());
        service.synchronize(&mut model, &search);
        let old_link = model.frame().unwrap().preview.link_target.clone().unwrap();
        for name in ["b", "a"] {
            row.target = command(name);
            model.publish(identity.query, vec![row.clone()], false);
            service.synchronize(&mut model, &search);
            assert!(service.presented.is_none());
            assert!(model.frame().unwrap().preview.link_target.is_none());
            assert!(service
                .open_link(&old_link, "https://example.com/command", &search)
                .is_err());
        }
        assert_ne!(model.preview_selection().unwrap(), old_link.selection);
    }

    #[test]
    fn home_command_requires_accepted_catalog_source_instead_of_ordinary_fallback() {
        let (mut service, mut model, _, identity) = active_preview();
        let mut row = model.selected_preview_row().unwrap().clone();
        row.target = crikey_ui::ActionTarget::Command(crikey_core::CommandRef {
            provider_id: identity.plugin,
            command_id: "saved".into(),
        });
        row.source.as_mut().unwrap().kind = crikey_ui::ResultSourceKind::DeclaredCatalog { epoch: 99 };
        row.description = "Must not fall back to this description".into();
        let _ = model.apply(UiCommand::SetQuery(String::new()));
        let generation = Generation::from_raw(identity.query.get() + 1);
        model.begin_generation(generation);
        model.publish(generation, vec![row], false);
        assert!(
            model.preview_selection().is_some(),
            "home has a real admitted publication"
        );
        service.synchronize(&mut model, &crate::SearchService::new(crate::App::new()));
        assert_eq!(
            model.frame().unwrap().preview.status,
            Some(PreviewStatus::Unavailable(
                "Preview catalog source unavailable".into()
            ))
        );
        assert!(service.presented.is_none());
    }

    #[test]
    fn host_metadata_tracks_same_selection_republication_without_resetting_focus() {
        let (mut service, mut model, _, _) = active_preview();
        service.active = None;
        service.selected = None;
        service.binding = None;
        let search = crate::SearchService::new(crate::App::new());
        let initial = model.frame().unwrap();
        let mut row = initial.rows[0].clone();
        row.description = "First detail".into();
        row.plugin_name = "Shared value".into();
        model.publish(initial.generation, vec![row.clone()], false);
        service.synchronize(&mut model, &search);
        let selection = model.preview_selection().unwrap();
        model.apply(UiCommand::FocusPreview);
        model.frame().unwrap();

        row.description = "Updated detail".into();
        row.plugin_name.clear();
        row.category = "Shared value".into();
        model.publish(initial.generation, vec![row.clone()], false);
        service.synchronize(&mut model, &search);
        assert_eq!(model.preview_selection(), Some(selection));
        let updated = model.frame().unwrap();
        assert!(updated.preview.focused);
        let Some(PreviewStatus::Content(content)) = updated.preview.status else {
            panic!("updated host metadata must remain visible");
        };
        assert_eq!(
            content.content.text,
            Some(crikey_core::PreviewText::Plain("Updated detail".into()))
        );
        assert_eq!(content.content.values[0].label, "Category");
        service.synchronize(&mut model, &search);
        assert!(model.frame().is_none(), "unchanged metadata must not republish");

        row.description.clear();
        row.category.clear();
        model.publish(initial.generation, vec![row], false);
        service.synchronize(&mut model, &search);
        assert_eq!(model.frame().unwrap().preview.status, Some(PreviewStatus::Empty));
    }

    #[test]
    fn cosmetic_republication_preserves_deferred_and_terminal_preview_authority() {
        let content = Arc::new(crikey_ui::PreparedPreview {
            content: Arc::new(PreviewContent {
                text: Some(crikey_core::PreviewText::Plain("Canonical preview".into())),
                ..Default::default()
            }),
            blocks: Arc::default(),
        });
        for (state, expected) in [
            (PreviewState::Empty, PreviewStatus::Empty),
            (
                PreviewState::Error("Canonical failure".into()),
                PreviewStatus::Error("Canonical failure".into()),
            ),
            (
                PreviewState::Unavailable("Canonical unavailable".into()),
                PreviewStatus::Unavailable("Canonical unavailable".into()),
            ),
            (
                PreviewState::Content(Arc::clone(&content)),
                PreviewStatus::Content(Arc::clone(&content)),
            ),
        ] {
            let (mut service, mut model, _, identity) = active_preview();
            let selection = model.preview_selection().unwrap();
            let mut row = model.selected_preview_row().unwrap().clone();
            let search = crate::SearchService::new(crate::App::new());
            row.description = "Cosmetic description while loading".into();
            row.plugin_name = "Cosmetic provider".into();
            model.publish(identity.query, vec![row.clone()], false);
            service.synchronize(&mut model, &search);
            assert_eq!(model.preview_selection(), Some(selection.clone()));
            assert_eq!(model.frame().unwrap().preview.status, Some(PreviewStatus::Loading));
            assert_eq!(service.active.as_ref().map(|(active, _)| active), Some(&identity));
            assert!(service.metadata.is_none());

            (service.publish)(PreviewUpdate {
                identity: identity.clone(),
                state,
            });
            service.synchronize(&mut model, &search);
            assert_eq!(model.frame().unwrap().preview.status, Some(expected.clone()));
            assert!(service.active.is_none());

            row.description = "Cosmetic description after completion".into();
            row.plugin_name.clear();
            row.category = "Cosmetic category".into();
            model.publish(identity.query, vec![row], false);
            service.synchronize(&mut model, &search);
            assert_eq!(model.preview_selection(), Some(selection));
            assert_eq!(model.frame().unwrap().preview.status, Some(expected));
            assert!(service.metadata.is_none());
        }
    }

    #[test]
    fn current_terminal_failure_survives_origin_death() {
        for state in [
            PreviewState::Error("worker timed out".into()),
            PreviewState::Unavailable("worker exited".into()),
        ] {
            let (mut service, mut model, endpoint, identity) = active_preview();
            *endpoint.incarnation.lock().unwrap() = None;
            let expected = match &state {
                PreviewState::Error(reason) => PreviewStatus::Error(reason.clone()),
                PreviewState::Unavailable(reason) => PreviewStatus::Unavailable(reason.clone()),
                _ => unreachable!(),
            };
            (service.publish)(PreviewUpdate { identity, state });
            service.synchronize(&mut model, &crate::SearchService::new(crate::App::new()));
            assert_eq!(model.frame().unwrap().preview.status, Some(expected));
            assert!(
                service.active.is_none(),
                "terminal failure must release admission"
            );
            assert!(service.presented.is_none());
        }
    }

    #[test]
    fn old_terminal_cannot_release_admission_or_hide_origin_retirement() {
        for field in 0..9 {
            let (mut service, mut model, endpoint, mut stale) = active_preview();
            *endpoint.incarnation.lock().unwrap() = None;
            match field {
                0 => stale.view += 1,
                1 => stale.query = Generation::ZERO,
                2 => stale.plugin = PluginId("old-plugin".into()),
                3 => stale.incarnation -= 1,
                4 => stale.item = ItemId("old-item".into()),
                5 => stale.selection -= 1,
                6 => stale.request -= 1,
                7 => {
                    stale.target = crikey_ui::ActionTarget::Result {
                        owner: stale.plugin.clone(),
                        item: ItemId("other-target".into()),
                    }
                }
                8 => {
                    stale.catalog_source = Some(crate::plugin_action::DeclaredCommandSource {
                        instance: 1,
                        generation: stale.query,
                        producer: stale.incarnation,
                        publication: 1,
                    })
                }
                _ => unreachable!(),
            }
            (service.publish)(PreviewUpdate {
                identity: stale,
                state: PreviewState::Error("old worker failed".into()),
            });
            service.synchronize(&mut model, &crate::SearchService::new(crate::App::new()));
            assert_eq!(
                model.frame().unwrap().preview.status,
                Some(PreviewStatus::Unavailable("Preview owner unavailable".into()))
            );
            assert!(
                service.active.is_some(),
                "stale terminal must not release current admission"
            );
        }
    }

    #[test]
    fn retired_origin_cannot_deliver_content_or_a_replacement_workers_failure() {
        for replacement in [None, Some(8)] {
            let (mut service, mut model, endpoint, identity) = active_preview();
            *endpoint.incarnation.lock().unwrap() = replacement;
            let state = if replacement.is_some() {
                PreviewState::Error("retired worker failed".into())
            } else {
                PreviewState::Content(Arc::new(crikey_ui::PreparedPreview {
                    content: Arc::new(PreviewContent {
                        text: None,
                        image: None,
                        values: Vec::new(),
                    }),
                    blocks: Arc::default(),
                }))
            };
            (service.publish)(PreviewUpdate { identity, state });
            service.synchronize(&mut model, &crate::SearchService::new(crate::App::new()));
            assert_eq!(
                model.frame().unwrap().preview.status,
                Some(PreviewStatus::Unavailable("Preview owner unavailable".into()))
            );
            assert!(service.active.is_none());
            assert!(service.presented.is_none());
        }
    }

    #[test]
    fn presented_content_and_links_retire_without_terminal_update() {
        for replacement in [None, Some(8)] {
            let (mut service, mut model, endpoint, identity) = active_preview();
            let content = Arc::new(crikey_ui::PreparedPreview {
                content: Arc::new(PreviewContent {
                    text: Some(crikey_core::PreviewText::Plain("Presented preview".into())),
                    image: None,
                    values: vec![crikey_core::PreviewValue {
                        label: "Website".into(),
                        value: "https://example.com/preview".into(),
                        kind: crikey_core::PreviewValueKind::Link,
                    }],
                }),
                blocks: Arc::default(),
            });
            let retained_content = Arc::downgrade(&content);
            let expected_identity = identity.clone();
            (service.publish)(PreviewUpdate {
                identity,
                state: PreviewState::Content(content),
            });
            let search = crate::SearchService::new(crate::App::new());
            service.synchronize(&mut model, &search);
            assert!(service.presented.is_some());
            assert!(
                service.active.is_none(),
                "terminal content must release admission"
            );
            let presented_frame = model.frame().unwrap();
            let Some(PreviewStatus::Content(presented_content)) = &presented_frame.preview.status else {
                panic!("the frame must present the delivered content");
            };
            assert!(Arc::ptr_eq(
                presented_content,
                &retained_content.upgrade().unwrap()
            ));
            let target = presented_frame.preview.link_target.clone().unwrap();
            assert_eq!(target.selection, service.selected.clone().unwrap());
            assert_eq!(target.plugin, expected_identity.plugin);
            assert_eq!(target.incarnation, expected_identity.incarnation);
            assert_eq!(target.request, expected_identity.request);
            *endpoint.incarnation.lock().unwrap() = replacement;
            service.synchronize(&mut model, &search);
            assert!(service.presented.is_none());
            let retired_frame = model.frame().unwrap();
            assert!(retired_frame.preview.link_target.is_none());
            assert_eq!(
                retired_frame.preview.status,
                Some(PreviewStatus::Unavailable("Preview owner unavailable".into()))
            );
            assert_eq!(
                service.open_link(&target, "https://example.com/preview", &search),
                Err("Preview link has retired".into())
            );
            assert!(model
                .apply(UiCommand::OpenPreviewLink {
                    target: Box::new(target),
                    url: "https://example.com/preview".into(),
                })
                .is_none());
            // An already observed frame owns its snapshot, but neither the
            // service nor the model may retain retired content after it drops.
            drop(presented_frame);
            assert!(retained_content.upgrade().is_none());
        }
    }

    #[test]
    fn late_exact_origins_dispatch_shared_item_id_to_selected_native_or_modern_owner() {
        struct RoutingEndpoint {
            items: Vec<crikey_core::Item>,
            requests: Mutex<Vec<PluginId>>,
        }
        impl PreviewDispatcher for RoutingEndpoint {
            fn lookup(
                &self,
                owner: &PluginId,
                item: &ItemId,
                _: Option<&crate::plugin_action::DeclaredCommandSource>,
            ) -> Option<(crikey_core::Item, u64)> {
                self.items
                    .iter()
                    .find(|candidate| &candidate.plugin_id == owner && &candidate.stable_id == item)
                    .cloned()
                    .map(|item| (item, 7))
            }
            fn origin(
                &self,
                owner: &PluginId,
                item: &ItemId,
                _: Option<&crate::plugin_action::DeclaredCommandSource>,
            ) -> Option<u64> {
                self.items
                    .iter()
                    .any(|candidate| &candidate.plugin_id == owner && &candidate.stable_id == item)
                    .then_some(7)
            }
            fn incarnation(&self, owner: &PluginId) -> Option<u64> {
                self.items
                    .iter()
                    .any(|item| &item.plugin_id == owner)
                    .then_some(7)
            }
            fn set_wake(&self, _: Arc<dyn Fn() + Send + Sync>) {}
            fn submit(&self, request: PreviewRequest, publish: PreviewPublish) -> Result<(), String> {
                self.requests
                    .lock()
                    .unwrap()
                    .push(request.identity.plugin.clone());
                publish(PreviewUpdate {
                    identity: request.identity,
                    state: PreviewState::Empty,
                });
                Ok(())
            }
            fn cancel(&self, _: &PreviewIdentity) {}
            fn foreground_blocked(&self, _: &PluginId) -> bool {
                // This routing fixture admits previews only, never actions.
                false
            }
            fn foreground_conflicts(&self, preview_owner: &PluginId, action_owner: &PluginId) -> bool {
                preview_owner == action_owner
                    && self.items.iter().any(|item| &item.plugin_id == preview_owner)
            }
        }
        let owners = ["native.first", "native.second", "modern.third"].map(|owner| PluginId(owner.into()));
        let items: Vec<_> = owners
            .iter()
            .map(|owner| crikey_core::Item {
                stable_id: ItemId("shared".into()),
                plugin_id: owner.clone(),
                category: crikey_core::Category::Application,
                label: "Shared".into(),
                description: String::new(),
                target: String::new(),
                search_terms: Vec::new(),
                icon_reference: None,
                argument_policy: crikey_core::ArgumentPolicy::Forbidden,
                hit_policy: crikey_core::HitPolicy::Recorded,
                score_hint: 0,
                metadata: std::collections::BTreeMap::from([("crikey.preview".into(), "deferred".into())]),
                actions: Vec::new(),
            })
            .collect();
        let native = Arc::new(RoutingEndpoint {
            items: items[..2].to_vec(),
            requests: Mutex::new(Vec::new()),
        });
        let modern = Arc::new(RoutingEndpoint {
            items: items[2..].to_vec(),
            requests: Mutex::new(Vec::new()),
        });
        let (mut service, mut model, _, _) = active_preview();
        service.active = None;
        service.selected = None;
        let mut row = model.selected_preview_row().unwrap().clone();
        let generation = model.preview_selection().unwrap().generation;
        let search = crate::SearchService::new(crate::App::new());
        for owner in &owners {
            row.target = crikey_ui::ActionTarget::Result {
                owner: owner.clone(),
                item: ItemId("shared".into()),
            };
            row.source = Some(crikey_ui::ResultSource {
                owner: owner.clone(),
                item: ItemId("shared".into()),
                kind: crikey_ui::ResultSourceKind::Ordinary,
            });
            model.publish(generation, vec![row.clone()], false);
            service.endpoints.clear();
            service.synchronize(&mut model, &search);
            let unchanged = model.preview_selection().unwrap();
            assert!(service.binding.is_none());
            // Authoritative producer publication follows the row, without a
            // selection/query/view/revision change.
            service.endpoints = vec![native.clone(), modern.clone()];
            service.synchronize(&mut model, &search);
            service.synchronize(&mut model, &search);
            assert_eq!(model.preview_selection(), Some(unchanged));
            assert_eq!(model.frame().unwrap().preview.status, Some(PreviewStatus::Empty));
        }
        assert_eq!(*native.requests.lock().unwrap(), owners[..2]);
        assert_eq!(*modern.requests.lock().unwrap(), owners[2..]);
    }

    #[test]
    fn host_metadata_is_bounded_in_utf8_bytes_with_visible_truncation() {
        assert_eq!(bounded_metadata_text("short"), "short");
        let exact = "é".repeat(4096);
        assert_eq!(bounded_metadata_text(&exact), exact);
        let oversized = "界".repeat(8192);
        let bounded = bounded_metadata_text(&oversized);
        assert!(bounded.len() <= 8192);
        assert!(bounded.ends_with('…'));
        assert!(oversized.starts_with(bounded.trim_end_matches('…')));
    }

    #[test]
    fn exact_old_terminal_cannot_cross_selection_query_or_view_changes() {
        for boundary in 0..3 {
            let (mut service, mut model, endpoint, identity) = active_preview();
            *endpoint.incarnation.lock().unwrap() = None;
            let mut row = model.selected_preview_row().unwrap().clone();
            match boundary {
                0 => {
                    row.target = crikey_ui::ActionTarget::Result {
                        owner: identity.plugin.clone(),
                        item: ItemId("new-selection".into()),
                    };
                    row.source.as_mut().unwrap().item = ItemId("new-selection".into());
                    model.publish(identity.query, vec![row], false);
                }
                1 | 2 => {
                    if boundary == 2 {
                        model.dismiss();
                        model.activate();
                    }
                    let _ = model.apply(UiCommand::SetQuery("new query".into()));
                    let generations = crikey_core::GenerationTracker::new();
                    let _ = generations.advance();
                    let generation = generations.advance();
                    model.begin_generation(generation);
                    model.publish(generation, vec![row], false);
                }
                _ => unreachable!(),
            }
            (service.publish)(PreviewUpdate {
                identity,
                state: PreviewState::Error("old selection failed".into()),
            });
            service.synchronize(&mut model, &crate::SearchService::new(crate::App::new()));
            assert_eq!(model.frame().unwrap().preview.status, None);
            assert!(service.active.is_none());
            assert!(service.presented.is_none());
        }
    }

    #[test]
    fn foreground_retirement_fences_even_content_completed_before_cancellation() {
        let (mut service, mut model, _, identity) = active_preview();
        let content = Arc::new(
            crikey_ui::prepare_preview(Arc::new(PreviewContent {
                text: Some(crikey_core::PreviewText::Plain("must not revive".into())),
                ..Default::default()
            }))
            .unwrap(),
        );
        let retained = Arc::downgrade(&content);
        (service.publish)(PreviewUpdate {
            identity: identity.clone(),
            state: PreviewState::Content(content),
        });
        service.retire_for_foreground(&mut model, &identity.plugin);
        assert!(
            service.active.is_some(),
            "cancellation is not a fabricated terminal"
        );
        service.synchronize(&mut model, &crate::SearchService::new(crate::App::new()));
        assert!(service.active.is_none());
        assert!(service.presented.is_none());
        let frame = model.frame().unwrap();
        assert!(frame.preview.link_target.is_none());
        assert!(
            !matches!(&frame.preview.status, Some(crikey_ui::PreviewStatus::Content(content))
            if matches!(content.content.text.as_ref(), Some(crikey_core::PreviewText::Plain(text)) if text == "must not revive"))
        );
        assert!(retained.upgrade().is_none());
    }
}
